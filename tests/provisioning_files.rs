#![cfg(target_os = "macos")]
//! Real filesystem and persisted receipts; no internal module substitutes.
use elsecall::provisioning_files::{ExistingFile, FileMode, Ownership, RelativePath, Session};
use std::{
    ffi::CString,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
};

fn path(value: &str) -> RelativePath {
    value.to_owned().try_into().unwrap()
}
fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    fs::set_permissions(parent.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let root = parent.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let receipt = parent.path().join("restore-secrets");
    (parent, root, receipt)
}
fn set_attribute(path: &Path, value: &[u8]) {
    let name = CString::new("dev.elsecall.test").unwrap();
    let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: valid names and a sized input buffer; this touches only test files.
    assert_eq!(
        unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        },
        0
    );
}
fn attribute(path: &Path) -> Vec<u8> {
    let name = CString::new("dev.elsecall.test").unwrap();
    let path = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let mut value = vec![0u8; 128];
    // SAFETY: valid names and a sized writable buffer.
    let count = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    assert!(count >= 0);
    value.truncate(count as usize);
    value
}

#[test]
fn installs_restores_and_repeats_without_leaving_files_or_receipts() {
    let (_parent, root, receipt) = fixture();
    fs::write(root.join("config"), b"original configuration").unwrap();
    fs::set_permissions(root.join("config"), fs::Permissions::from_mode(0o640)).unwrap();
    set_attribute(&root.join("config"), b"original metadata");
    let before = fs::metadata(root.join("config")).unwrap();
    fs::write(root.join("preexisting"), b"reuse unchanged").unwrap();
    for _ in 0..2 {
        let mut session = create(&root, &receipt).unwrap();
        assert_eq!(
            session.directory(path("bin")).unwrap(),
            Ownership::Introduced
        );
        assert_eq!(
            session
                .put(
                    path("bin/elsecall"),
                    b"verified executable",
                    FileMode::Executable,
                    ExistingFile::Refuse
                )
                .unwrap(),
            Ownership::Introduced
        );
        session
            .link(path("elsecall"), path("bin/elsecall"))
            .unwrap();
        assert_eq!(
            fs::read(root.join("elsecall")).unwrap(),
            b"verified executable"
        );
        assert_eq!(
            session
                .put(
                    path("config"),
                    b"managed configuration",
                    FileMode::Private,
                    ExistingFile::RestoreOnDetach
                )
                .unwrap(),
            Ownership::Restorable
        );
        assert_eq!(
            session
                .put(
                    path("preexisting"),
                    b"never written",
                    FileMode::Private,
                    ExistingFile::Reuse
                )
                .unwrap(),
            Ownership::Reused
        );
        assert_eq!(fs::metadata(&receipt).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(receipt.join("receipt.json")).unwrap().mode() & 0o777,
            0o600
        );
        let status = serde_json::to_string(&session.cleanup().unwrap()).unwrap();
        assert_eq!(status, r#"{"clean":true,"pending":[]}"#);
        assert!(!status.contains("original"));
        session.finalization().unwrap().run().unwrap();
        assert!(!receipt.exists());
        assert!(!root.join("bin").exists());
        assert!(!root.join("elsecall").is_symlink());
        assert_eq!(
            fs::read(root.join("config")).unwrap(),
            b"original configuration"
        );
        assert_eq!(attribute(&root.join("config")), b"original metadata");
        let after = fs::metadata(root.join("config")).unwrap();
        assert_eq!(
            (
                before.dev(),
                before.ino(),
                before.uid(),
                before.gid(),
                before.mode(),
                before.mtime(),
                before.mtime_nsec()
            ),
            (
                after.dev(),
                after.ino(),
                after.uid(),
                after.gid(),
                after.mode(),
                after.mtime(),
                after.mtime_nsec()
            )
        );
        assert_eq!(
            fs::read(root.join("preexisting")).unwrap(),
            b"reuse unchanged"
        );
    }
}

#[test]
fn resumes_after_publication_and_refuses_concurrent_or_new_work_during_cleanup() {
    let (_parent, root, receipt) = fixture();
    fs::write(root.join("config"), b"prior").unwrap();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("config"),
            b"installed",
            FileMode::Private,
            ExistingFile::RestoreOnDetach,
        )
        .unwrap();
    assert!(Session::resume(&receipt).is_err());
    drop(session);
    let mut session = Session::resume(&receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    assert!(
        session
            .put(
                path("later"),
                b"refused",
                FileMode::Private,
                ExistingFile::Refuse
            )
            .is_err()
    );
    drop(session);
    let mut session = Session::resume(&receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
    assert_eq!(fs::read(root.join("config")).unwrap(), b"prior");
}

#[test]
fn preserves_replaced_and_modified_objects_reports_incomplete_and_can_retry() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session.directory(path("managed")).unwrap();
    session
        .put(
            path("managed/file"),
            b"managed",
            FileMode::Private,
            ExistingFile::Refuse,
        )
        .unwrap();
    // Same bytes at the same path cannot pass as the inode we introduced.
    fs::rename(root.join("managed/file"), root.join("held")).unwrap();
    fs::write(root.join("managed/file"), b"managed").unwrap();
    let cleanup = session.cleanup().unwrap();
    assert!(!cleanup.clean);
    assert!(!cleanup.pending.is_empty());
    assert_eq!(fs::read(root.join("managed/file")).unwrap(), b"managed");
    assert!(receipt.exists());
    fs::remove_file(root.join("managed/file")).unwrap();
    fs::rename(root.join("held"), root.join("managed/file")).unwrap();
    set_attribute(&root.join("managed/file"), b"user-added metadata");
    assert!(!session.cleanup().unwrap().clean);
    // Independent files still get cleaned; the changed object stays for resolution.
    assert!(root.join("managed/file").exists());
}

#[test]
fn does_not_delete_user_files_in_introduced_directories_or_modify_reused_files() {
    let (_parent, root, receipt) = fixture();
    fs::write(root.join("preexisting"), b"original").unwrap();
    let mut session = create(&root, &receipt).unwrap();
    session.directory(path("managed")).unwrap();
    session
        .put(
            path("managed/owned"),
            b"managed",
            FileMode::Private,
            ExistingFile::Refuse,
        )
        .unwrap();
    session
        .put(
            path("preexisting"),
            b"never written",
            FileMode::Private,
            ExistingFile::Reuse,
        )
        .unwrap();
    fs::write(root.join("preexisting"), b"user edit").unwrap();
    fs::write(root.join("managed/user-file"), b"user work").unwrap();
    assert!(!session.cleanup().unwrap().clean);
    assert!(!root.join("managed/owned").exists());
    assert_eq!(fs::read(root.join("preexisting")).unwrap(), b"user edit");
    assert_eq!(
        fs::read(root.join("managed/user-file")).unwrap(),
        b"user work"
    );
    fs::remove_file(root.join("managed/user-file")).unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn refuses_symlink_ancestors_unsafe_modes_hard_links_and_scope_replacement() {
    let (_parent, root, receipt) = fixture();
    fs::create_dir(root.join("outside")).unwrap();
    symlink(root.join("outside"), root.join("redirect")).unwrap();
    let mut session = create(&root, &receipt).unwrap();
    assert!(
        session
            .put(
                path("redirect/file"),
                b"refused",
                FileMode::Private,
                ExistingFile::Refuse
            )
            .is_err()
    );
    assert!(!root.join("outside/file").exists());
    fs::write(root.join("original"), b"preserve").unwrap();
    fs::hard_link(root.join("original"), root.join("alias")).unwrap();
    assert!(
        session
            .put(
                path("original"),
                b"refused",
                FileMode::Private,
                ExistingFile::RestoreOnDetach
            )
            .is_err()
    );
    assert!(session.cleanup().unwrap().clean);
    drop(session);
    let held = root.with_file_name("previous-home");
    fs::rename(&root, &held).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(Session::resume(&receipt).is_err());
    fs::remove_dir(&root).unwrap();
    fs::rename(&held, &root).unwrap();
    let session = Session::resume(&receipt).unwrap();
    session.finalization().unwrap().run().unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(create(&root, &receipt).is_err());
}

#[test]
fn edited_prior_storage_is_never_restored_over_an_installed_file() {
    let (_parent, root, receipt) = fixture();
    fs::write(root.join("config"), b"original").unwrap();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("config"),
            b"installed",
            FileMode::Private,
            ExistingFile::RestoreOnDetach,
        )
        .unwrap();
    fs::write(receipt.join("staged-0"), b"edited backup").unwrap();
    assert!(!session.cleanup().unwrap().clean);
    assert_eq!(fs::read(root.join("config")).unwrap(), b"installed");
    assert_eq!(
        fs::read(receipt.join("staged-0")).unwrap(),
        b"edited backup"
    );
    assert!(session.finalization().is_err());
}

#[test]
fn acl_grants_cannot_expose_or_tamper_with_private_receipt_storage() {
    let (parent, root, receipt) = fixture();
    let grant = |path: &Path| {
        assert!(
            std::process::Command::new("/bin/chmod")
                .args([
                    "+a",
                    "everyone allow read,write,execute,file_inherit,directory_inherit"
                ])
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    };
    grant(parent.path());
    assert!(create(&root, &receipt).is_err());
    assert!(!receipt.exists());
    assert!(
        std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(parent.path())
            .status()
            .unwrap()
            .success()
    );
    let mut session = create(&root, &receipt).unwrap();
    grant(&receipt);
    assert!(
        session
            .put(
                path("never-created"),
                b"private",
                FileMode::Private,
                ExistingFile::Refuse
            )
            .is_err()
    );
    assert!(session.cleanup().is_err());
    drop(session);
    assert!(Session::resume(&receipt).is_err());
    assert!(
        std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(&receipt)
            .status()
            .unwrap()
            .success()
    );
    let mut session = Session::resume(&receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn untracked_storage_prevents_clean_acknowledgment_and_finalization_is_restartable() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    fs::write(receipt.join("user-work"), b"preserve").unwrap();
    assert!(!session.cleanup().unwrap().clean);
    assert_eq!(fs::read(receipt.join("user-work")).unwrap(), b"preserve");
    fs::remove_file(receipt.join("user-work")).unwrap();
    assert!(session.cleanup().unwrap().clean);
    let ticket = session.finalization().unwrap();
    let private_journal = serde_json::to_vec(&ticket).unwrap();
    let mut ticket: elsecall::provisioning_files::Finalization =
        serde_json::from_slice(&private_journal).unwrap();
    // A lost ticket before the coordinator saved it can be recreated without
    // mutating the finalizing receipt or invalidating the original ticket.
    let resumed = Session::resume(&receipt).unwrap();
    let recreated = resumed.finalization().unwrap();
    assert_eq!(serde_json::to_vec(&recreated).unwrap(), private_journal);
    // Interruption after deleting the last in-directory evidence. The coordinator
    // still holds its ticket, which authenticates the remaining empty directory.
    fs::remove_file(receipt.join("receipt.json")).unwrap();
    fs::remove_file(receipt.join("lock")).unwrap();
    ticket.run().unwrap();
    ticket.run().unwrap();
    assert!(!receipt.exists());
}

#[test]
fn abandoned_partial_staging_recovers_and_repeated_updates_restore_the_baseline() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("binary"),
            b"first",
            FileMode::Executable,
            ExistingFile::Refuse,
        )
        .unwrap();
    session
        .put(
            path("binary"),
            b"second",
            FileMode::Executable,
            ExistingFile::RestoreOnDetach,
        )
        .unwrap();
    drop(session);
    // Model a process stopped while writing a reserved scratch slot, before
    // publication. Only the private filesystem boundary is manipulated.
    let record_path = receipt.join("receipt.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    record["entries"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "ownership":"changed", "path":"unpublished", "prior":null,
            "installed":null, "staging_kind":"file", "captured":false, "restored":false
        }));
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::write(receipt.join("staged-2"), b"partial bytes").unwrap();
    let mut session = Session::resume(&receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    assert!(!root.join("binary").exists());
    assert!(!root.join("unpublished").exists());
    session.finalization().unwrap().run().unwrap();
}

fn create(root: &Path, receipt: &Path) -> elsecall::failure::Result<Session> {
    elsecall::provisioning_files::Creation::prepare(root, receipt)?.start()
}

#[test]
fn an_unexplained_missing_target_remains_pending_instead_of_claiming_removal() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("managed"),
            b"introduced",
            FileMode::Private,
            ExistingFile::Refuse,
        )
        .unwrap();
    fs::rename(root.join("managed"), root.join("moved-by-user")).unwrap();
    assert!(!session.cleanup().unwrap().clean);
    assert_eq!(fs::read(root.join("moved-by-user")).unwrap(), b"introduced");
    fs::rename(root.join("moved-by-user"), root.join("managed")).unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn moving_receipt_storage_never_counts_as_verified_finalization() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    let mut ticket = session.finalization().unwrap();
    let moved = receipt.with_file_name("moved-receipt");
    fs::rename(&receipt, &moved).unwrap();
    assert!(ticket.run().is_err());
    assert!(moved.join("receipt.json").exists());
    fs::rename(&moved, &receipt).unwrap();
    ticket.run().unwrap();
    let committed = serde_json::to_vec(&ticket).unwrap();
    let mut ticket: elsecall::provisioning_files::Finalization =
        serde_json::from_slice(&committed).unwrap();
    ticket.run().unwrap();
}

#[test]
fn creation_ticket_recovers_before_first_receipt_commit_and_refuses_unrelated_data() {
    let (_parent, root, receipt) = fixture();
    let creation = elsecall::provisioning_files::Creation::prepare(&root, &receipt).unwrap();
    let durable = serde_json::to_vec(&creation).unwrap();
    let creation: elsecall::provisioning_files::Creation =
        serde_json::from_slice(&durable).unwrap();
    // Process termination after mkdir, before a complete lock or receipt exists.
    fs::create_dir(&receipt).unwrap();
    fs::set_permissions(&receipt, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(receipt.join("unrelated"), b"preserve").unwrap();
    assert!(creation.start().is_err());
    assert_eq!(fs::read(receipt.join("unrelated")).unwrap(), b"preserve");
    fs::remove_file(receipt.join("unrelated")).unwrap();
    // An interrupted initial receipt write may also be only partially encoded.
    fs::write(receipt.join("receipt.next"), b"{partial").unwrap();
    fs::set_permissions(
        receipt.join("receipt.next"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut session = creation.start().unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn interrupted_directory_quarantine_returns_user_work_to_its_original_path() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session.directory(path("managed")).unwrap();
    fs::write(root.join("managed/user-file"), b"user work").unwrap();
    drop(session);
    // Simulate termination after the atomic quarantine but before empty-only
    // removal detects the user's file and rolls the directory back.
    fs::rename(root.join("managed"), receipt.join("staged-0")).unwrap();
    let mut session = Session::resume(&receipt).unwrap();
    assert!(!session.cleanup().unwrap().clean);
    assert_eq!(
        fs::read(root.join("managed/user-file")).unwrap(),
        b"user work"
    );
    fs::remove_file(root.join("managed/user-file")).unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn unpublished_staged_inode_can_be_removed_while_preserving_changed_destination() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("managed"),
            b"introduced",
            FileMode::Private,
            ExistingFile::Refuse,
        )
        .unwrap();
    drop(session);
    // The durable pre-publication record and staged inode are also the state of
    // an exclusive publication refused because a destination appeared meanwhile.
    fs::rename(root.join("managed"), receipt.join("staged-0")).unwrap();
    fs::write(root.join("managed"), b"unrelated user work").unwrap();
    let mut session = Session::resume(&receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    assert_eq!(
        fs::read(root.join("managed")).unwrap(),
        b"unrelated user work"
    );
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn committed_capture_recovers_after_private_slot_deletion() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("managed"),
            b"introduced",
            FileMode::Private,
            ExistingFile::Refuse,
        )
        .unwrap();
    drop(session);
    fs::rename(root.join("managed"), receipt.join("staged-0")).unwrap();
    let record_path = receipt.join("receipt.json");
    let mut record: serde_json::Value =
        serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
    record["entries"][0]["captured"] = serde_json::json!(true);
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::remove_file(receipt.join("staged-0")).unwrap();
    let mut session = Session::resume(&receipt).unwrap();
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}

#[test]
fn concurrent_path_replacements_never_delete_unrelated_inodes() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };
    const COUNT: usize = 32;
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    for index in 0..COUNT {
        session
            .put(
                path(&format!("managed-{index}")),
                b"introduced",
                FileMode::Private,
                ExistingFile::Refuse,
            )
            .unwrap();
        fs::write(
            root.join(format!("user-{index}")),
            format!("user work {index}"),
        )
        .unwrap();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let started = Arc::new(Barrier::new(2));
    let writer = std::thread::spawn({
        let stop = stop.clone();
        let started = started.clone();
        let root = root.clone();
        move || {
            let names: Vec<_> = (0..COUNT)
                .map(|index| {
                    (
                        CString::new(
                            root.join(format!("managed-{index}"))
                                .as_os_str()
                                .as_encoded_bytes(),
                        )
                        .unwrap(),
                        CString::new(
                            root.join(format!("user-{index}"))
                                .as_os_str()
                                .as_encoded_bytes(),
                        )
                        .unwrap(),
                    )
                })
                .collect();
            started.wait();
            while !stop.load(Ordering::Acquire) {
                for (managed, user) in &names {
                    // SAFETY: valid path strings; all files are isolated test data.
                    // Missing paths are expected while cleanup quarantines them.
                    unsafe {
                        libc::renameatx_np(
                            libc::AT_FDCWD,
                            managed.as_ptr(),
                            libc::AT_FDCWD,
                            user.as_ptr(),
                            libc::RENAME_SWAP,
                        );
                    }
                }
            }
        }
    });
    started.wait();
    let cleanup = session.cleanup();
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    // Races can honestly leave pending cleanup, but cannot destroy user inodes.
    assert!(cleanup.is_ok());
    let mut preserved = std::collections::HashSet::new();
    for directory in [&root, &receipt] {
        for item in fs::read_dir(directory).unwrap() {
            let item = item.unwrap();
            if item.file_type().unwrap().is_file() {
                let bytes = fs::read(item.path()).unwrap();
                if bytes.starts_with(b"user work ") {
                    preserved.insert(bytes);
                }
            }
        }
    }
    assert_eq!(preserved.len(), COUNT);
}

#[test]
fn symlink_metadata_edits_are_preserved_even_when_target_and_mtime_match() {
    let (_parent, root, receipt) = fixture();
    let mut session = create(&root, &receipt).unwrap();
    session
        .put(
            path("binary"),
            b"introduced",
            FileMode::Executable,
            ExistingFile::Refuse,
        )
        .unwrap();
    session.link(path("link"), path("binary")).unwrap();
    let before = fs::symlink_metadata(root.join("link")).unwrap();
    let name = CString::new("dev.elsecall.test").unwrap();
    let link = CString::new(root.join("link").as_os_str().as_encoded_bytes()).unwrap();
    let value = b"user metadata";
    // SAFETY: valid link/name and sized input buffer; never follows the link.
    assert_eq!(
        unsafe {
            libc::setxattr(
                link.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                libc::XATTR_NOFOLLOW,
            )
        },
        0
    );
    let after = fs::symlink_metadata(root.join("link")).unwrap();
    assert_eq!(
        (before.mtime(), before.mtime_nsec()),
        (after.mtime(), after.mtime_nsec())
    );
    assert!(!session.cleanup().unwrap().clean);
    assert!(
        fs::symlink_metadata(root.join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // Removing only the deliberate edit restores the recorded identity for retry.
    // SAFETY: valid link/name, no-follow removal of this test's attribute.
    assert_eq!(
        unsafe { libc::removexattr(link.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) },
        0
    );
    assert!(session.cleanup().unwrap().clean);
    session.finalization().unwrap().run().unwrap();
}
