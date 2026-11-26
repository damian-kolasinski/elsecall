use super::{
    ReleaseGate, TestMember, enrol, git, processes, repository, run, spawn, stdout, until,
};
use std::path::Path;

#[test]
fn a_label_reuses_one_warm_slot_across_worktrees_and_keeps_caller_identity() {
    let alpha = TestMember::start("alpha");
    let bravo =
        TestMember::start_with("bravo", "[workspace]\nwarm_slot_label = 'app build'\n", &[]);
    enrol(&alpha, &bravo);
    let repo = repository();
    crate::processes::workload_policy(
        alpha.home.path(),
        None,
        "[outputs]\npaths = ['build/logs', 'src']\n",
    );
    let sibling = repo.root.with_file_name("sibling");
    git(
        &repo.root,
        &[
            "worktree",
            "add",
            "--detach",
            sibling.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::write(sibling.join("src/lib.txt"), "sibling edit\n").unwrap();
    let build = |cwd: &Path, label: &str, command: &str| {
        stdout(&run(
            &alpha,
            cwd,
            &[
                "--member", "bravo", "--label", label, "--", "/bin/sh", "-ec", command,
            ],
            b"",
        ))
    };
    let first = build(
        &repo.root,
        "app build",
        "mkdir -p build/logs; echo first > build/logs/first; echo product > build/marker; stat -f '%i:%m' .gitignore; pwd",
    );
    let second = build(
        &sibling,
        "app build",
        "test -f build/marker && test ! -e notes.txt && test \"$(cat src/lib.txt)\" = 'sibling edit' || exit 1; mkdir -p build/logs; echo second > build/logs/second; stat -f '%i:%m' .gitignore; pwd; echo \"$ELSECALL_CALLER_WORKSPACE\"",
    );
    assert_eq!(second, format!("{first}{}\n", sibling.display()));
    assert!(repo.root.join("build/logs/first").is_file());
    assert!(sibling.join("build/logs/second").is_file());
    assert!(!sibling.join("build/logs/first").exists());
    let gate = ReleaseGate::new(bravo.home(), "hold-slot");
    let holding = spawn(
        &alpha,
        &repo.root,
        &[
            "--member",
            "bravo",
            "--label",
            "app build",
            "--",
            "/bin/sh",
            "-ec",
            "touch \"$HOME/slot-entered\"; while test -f \"$HOME/hold-slot\"; do sleep 0.05; done; pwd",
        ],
        &[],
    );
    until("worker to hold the slot", processes::WAIT, || {
        bravo.home().join("slot-entered").exists()
    });
    let busy = build(&sibling, "app build", "test ! -e build/marker; pwd");
    gate.open();
    assert_eq!(
        stdout(&holding.wait_with_output().unwrap()).trim(),
        first.lines().last().unwrap()
    );
    let ordinary = build(&sibling, "app test", "test ! -e build/marker; pwd");
    assert_eq!(busy, ordinary);
    assert_ne!(ordinary.trim(), first.lines().last().unwrap());
    let other = repository();
    let separate = build(&other.root, "app build", "test ! -e build/marker; pwd");
    assert_ne!(separate.trim(), first.lines().last().unwrap());
}
