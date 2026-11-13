//! Real filesystem and SQLite boundaries, including writers in separate processes.
mod processes;
use elsecall::{
    scheduling::{records::Records, timestamp::Timestamp},
    service_log::{Log, Settings},
};
use std::{fs, process::Stdio};

#[test]
#[ignore = "subprocess entry point"]
fn log_writer_child() {
    let log = Log::new(
        std::env::var_os("LOG_TEST_PATH").unwrap().into(),
        Settings {
            max_bytes: 1024,
            keep: 100,
        },
    );
    let writer = std::env::var("LOG_TEST_WRITER").unwrap();
    for n in 0..100 {
        log.append(format!("writer={writer} record={n:03} complete\n").as_bytes())
            .unwrap();
    }
}

#[test]
fn concurrent_processes_reopen_the_current_file_and_keep_every_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("service.log");
    let mut children: Vec<_> = (0..4)
        .map(|n| {
            crate::processes::command(std::env::current_exe().unwrap())
                .args(["--exact", "log_writer_child", "--ignored"])
                .env("LOG_TEST_PATH", &path)
                .env("LOG_TEST_WRITER", n.to_string())
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in &mut children {
        assert!(child.wait().unwrap().success());
    }
    let mut lines = Vec::new();
    for entry in fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() == "service.log.lock" {
            continue;
        }
        let text = fs::read_to_string(entry.path()).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.len() <= 1024);
        lines.extend(text.lines().map(str::to_owned));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(entry.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let expected: std::collections::BTreeSet<_> = (0..4)
        .flat_map(|w| (0..100).map(move |n| format!("writer={w} record={n:03} complete")))
        .collect();
    assert_eq!(lines.len(), 400);
    assert_eq!(
        lines.into_iter().collect::<std::collections::BTreeSet<_>>(),
        expected
    );
}

#[test]
fn expired_records_return_substantial_slack_and_wal_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.sqlite");
    let records = Records::open(&path).unwrap();
    // Populate at the SQLite boundary: expiry does not decode record content.
    // Expression indexes require valid JSON; a JSON string is still not a Record.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "INSERT INTO requests (run_id, label, state, requested_at, run_ms, completed, record)
        VALUES ('old', NULL, 'ended', 0, NULL, 0, json_quote(hex(zeroblob(10000000))));
        INSERT INTO requests (run_id, label, state, requested_at, run_ms, completed, record)
        VALUES ('live', NULL, 'running', 0, NULL, 0, '{}');",
        )
        .unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    assert!(fs::metadata(&path).unwrap().len() > 16 * 1024 * 1024);
    records.prune(Timestamp::now()).unwrap();
    assert!(fs::metadata(&path).unwrap().len() < 1024 * 1024);
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM requests WHERE run_id = 'live'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert!(
        fs::metadata(path.with_extension("sqlite-wal"))
            .unwrap()
            .len()
            < 1024 * 1024
    );
}

#[test]
fn backup_retention_removes_content_and_empty_directories_and_applies_lowered_limits_on_open() {
    use elsecall::{
        config_sync::{
            compare::Change,
            replica::{Layout, Replica},
            state::{Backup, State},
            store::FileStore,
            synced_path::SyncedPath,
        },
        private_files,
    };
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("state");
    let layout = Layout::for_home(home.path(), &data);
    private_files::directory(&layout.state).unwrap();
    let store = std::sync::Arc::new(FileStore::open(&layout.state.join("objects")).unwrap());
    let mut state = State::default();
    for n in 0..5 {
        let location = layout.state.join(format!("backups/{n}/.config/demo/{n}"));
        private_files::directory(location.parent().unwrap()).unwrap();
        fs::write(&location, format!("secret-{n}")).unwrap();
        state.add_backup(Backup {
            at: Timestamp::from_millis(n),
            path: SyncedPath::parse("~/.config/demo").unwrap(),
            file: format!("~/.config/demo/{n}"),
            change: Change::Removed,
            size: 8,
            fingerprint: None,
            location,
        });
    }
    state.save(&layout.state.join("state.json")).unwrap();
    // A process killed during an atomic write can leave unregistered content.
    private_files::directory(&layout.state.join("backups/100/sub")).unwrap();
    fs::write(
        layout.state.join("backups/100/sub/scratch"),
        "orphan-secret",
    )
    .unwrap();
    // A recent unregistered whole tree may be the only local recovery copy.
    let recent = layout.state.join(format!(
        "backups/{}/demo.previous",
        Timestamp::now().millis()
    ));
    private_files::directory(&recent).unwrap();
    fs::write(recent.join("local-only"), "recover-me").unwrap();
    let directory = layout.state.clone();
    let _replica = Replica::open_with_backup_limit(layout, store, 2).unwrap();
    let state = State::load(&directory.join("state.json")).unwrap();
    assert!(!directory.join("backups/100").exists());
    assert_eq!(
        fs::read_to_string(recent.join("local-only")).unwrap(),
        "recover-me"
    );
    assert_eq!(state.backups.len(), 2);
    assert_eq!(state.pruned_backups, 3);
    for n in 0..3 {
        assert!(!directory.join(format!("backups/{n}")).exists());
    }
    assert_eq!(
        fs::read_to_string(&state.backups[0].location).unwrap(),
        "secret-3"
    );
    assert_eq!(
        fs::read_to_string(&state.backups[1].location).unwrap(),
        "secret-4"
    );
}

#[test]
fn retention_removes_recorded_cross_volume_backup_without_removing_its_neighbors() {
    use elsecall::{
        config_sync::{
            compare::Change,
            replica::{Layout, Replica},
            state::{Backup, State},
            store::FileStore,
            synced_path::SyncedPath,
        },
        private_files,
    };
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("state");
    let layout = Layout::for_home(home.path(), &data);
    private_files::directory(&layout.state).unwrap();
    let store = std::sync::Arc::new(FileStore::open(&layout.state.join("objects")).unwrap());
    // Simulate the persisted location returned by disk::write_tree's cross-volume
    // fallback, at the filesystem boundary, with real files and no module stubs.
    let beside = home
        .path()
        .join(".config/demo.elsecall-kept-0123456789abcdef");
    private_files::directory(&beside).unwrap();
    fs::write(beside.join("old"), "local-only").unwrap();
    fs::write(home.path().join(".config/neighbor"), "keep").unwrap();
    let mut state = State::default();
    let backup = |at, location| Backup {
        at: Timestamp::from_millis(at),
        path: SyncedPath::parse("~/.config/demo").unwrap(),
        file: "~/.config/demo".into(),
        change: Change::Removed,
        size: 0,
        fingerprint: None,
        location,
    };
    state.add_backup(backup(1, beside.clone()));
    let latest = layout.state.join("backups/2/demo.previous");
    private_files::directory(&latest).unwrap();
    state.add_backup(backup(2, latest.clone()));
    state.save(&layout.state.join("state.json")).unwrap();
    let state_file = layout.state.join("state.json");
    let _replica = Replica::open_with_backup_limit(layout, store, 1).unwrap();
    assert!(!beside.exists());
    assert!(latest.exists());
    assert_eq!(
        fs::read_to_string(home.path().join(".config/neighbor")).unwrap(),
        "keep"
    );
    assert_eq!(State::load(&state_file).unwrap().pruned_backups, 1);
}
