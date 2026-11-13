//! Finding mapped copies the member does not track yet, such as those made before
//! housekeeping existed. A directory counts only if its mark names a calling member
//! and a caller path that map to exactly that directory.
use crate::{
    housekeeping::stores,
    scheduling::timestamp::Timestamp,
    workspace::{
        copy,
        location::{CopyLocation, Locations, Setting},
    },
};
use std::{path::Path, time::SystemTime};

/// Marked mapped copies under the workspace root, each with the latest time
/// Elsecall wrote to it, as the best guess of its last use.
pub fn discover(locations: &Locations) -> Vec<(CopyLocation, Timestamp)> {
    let mapped = Locations {
        setting: Setting::Mapped,
        ..locations.clone()
    };
    let mut found = Vec::new();
    let Ok(members) = std::fs::read_dir(&locations.root) else {
        return found;
    };
    for member in members.flatten() {
        let name = member.file_name();
        let Some(member_name) = name.to_str().filter(|n| !n.starts_with('.')) else {
            continue;
        };
        if !member.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let Ok(copies) = std::fs::read_dir(member.path()) else {
            continue;
        };
        for entry in copies.flatten() {
            let root = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Some((calling_member, caller_root)) = copy::owner(&root) else {
                continue;
            };
            let Ok(mut location) = mapped.locate(&calling_member, &caller_root, &caller_root)
            else {
                continue;
            };
            let Some(store) = copy::store_of(&root)
                .filter(|store| stores::is_store_of(&locations.root, &calling_member, store))
            else {
                continue;
            };
            if calling_member != member_name || location.root != root {
                continue;
            }
            location.store = store;
            let last_written = [
                location.state_file.clone(),
                root.join(".git/index"),
                root.clone(),
            ]
            .iter()
            .find_map(|path| modified(path));
            found.push((location, last_written.unwrap_or_else(Timestamp::now)));
        }
    }
    found
}

fn modified(path: &Path) -> Option<Timestamp> {
    std::fs::symlink_metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .map(|time: SystemTime| Timestamp::from(time))
}
