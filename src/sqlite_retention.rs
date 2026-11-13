//! Reclaim retained SQLite files after bulk expiry, without vacuuming for every
//! row. Ordinary freed pages are reused; substantial slack is returned to disk.
use rusqlite::Connection;

pub fn reclaim(connection: &Connection) -> rusqlite::Result<()> {
    let pages: i64 = connection.pragma_query_value(None, "page_count", |row| row.get(0))?;
    let free: i64 = connection.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let size: i64 = connection.pragma_query_value(None, "page_size", |row| row.get(0))?;
    if free * size >= 16 * 1024 * 1024 && free * 4 >= pages {
        connection.execute_batch("VACUUM;")?;
    }
    // A concurrent reader may leave a busy checkpoint; it is retried on the next
    // retention pass. SQLite's normal autocheckpoint continues between passes.
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
}
