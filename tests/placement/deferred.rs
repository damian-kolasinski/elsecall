use super::*;

#[test]
fn deferrable_placement_promotes_once_then_uses_the_first_available_member() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    let here = gated_work(&alpha.member, &repo, &["--pinned", "--size", "2"]);
    let there = gated_work(&bravo.member, &repo, &["--pinned", "--size", "2"]);
    let mut child = spawn(
        &alpha.member,
        &repo,
        &[
            "--placeable",
            "--size",
            "2",
            "--deferrable",
            "--deadline",
            "1s",
            "--at-deadline",
            "start",
            "--label",
            "deferred placement",
            "--",
            "/bin/sh",
            "-c",
            "cat notes.txt && echo ran >> ../count && echo $ELSECALL_MEMBER && wc -l < ../count && exit 17",
        ],
    );
    until(
        "the master to queue the request",
        crate::processes::WAIT,
        || listed(&alpha.member, "deferred placement").is_some(),
    );
    std::thread::sleep(Duration::from_millis(1300));
    let queued = listed(&alpha.member, "deferred placement").unwrap();
    assert_eq!(queued["state"], "queued", "{queued}");
    release_work(there);
    until(
        "the promoted command to finish on the free worker",
        crate::processes::WAIT,
        || child.try_wait().unwrap().is_some(),
    );
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(17), "{result:?}");
    assert_eq!(
        String::from_utf8_lossy(&result.stdout)
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["uncommitted", "bravo", "1"]
    );
    let id = queued["run_id"].as_str().unwrap();
    let caller = explain(&alpha.member, id);
    let worker = explain(&bravo.member, id);
    assert_eq!(caller["urgency"]["deferrable"]["at_deadline"], "start");
    assert_eq!(worker["urgency"], caller["urgency"]);
    assert_eq!(caller["placed"]["member"], "bravo");
    assert!(worker["hold_limit"].is_null());
    release_work(here);
}

#[test]
fn deferrable_placement_skips_without_executing_when_all_members_are_busy() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    let here = gated_work(&alpha.member, &repo, &["--pinned", "--size", "2"]);
    let there = gated_work(&bravo.member, &repo, &["--pinned", "--size", "2"]);
    for target in [&["--placeable"][..], &["--member", "bravo"][..]] {
        let mut args = target.to_vec();
        args.extend([
            "--size",
            "2",
            "--deferrable",
            "--deadline",
            "1s",
            "--at-deadline",
            "skip",
            "--",
            "/bin/echo",
            "unexpected execution",
        ]);
        let result = run(&alpha.member, &repo, &args);
        assert_eq!(result.status.code(), Some(125), "{result:?}");
        assert!(result.stdout.is_empty());
        let error = envelope(&result);
        assert_eq!(error["error"]["code"], "deadline_skipped", "{error}");
        let record = explain(&alpha.member, error["error"]["run_id"].as_str().unwrap());
        assert_eq!(record["decision"]["verdict"], "deadline_skipped");
        assert!(record["started_at"].is_null());
    }
    release_work(there);
    release_work(here);
}

#[test]
fn deferrable_remote_queue_yields_to_interactive_work_until_promoted() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    for (deadline, expected) in [("5m", "interactive"), ("1s", "deferred")] {
        let held = gated_work(&bravo.member, &repo, &["--pinned", "--size", "2"]);
        let path = repo.join(format!("order-{deadline}"));
        let order = path.to_str().unwrap();
        let mut deferred = vec![
            "--deferrable",
            "--deadline",
            deadline,
            "--at-deadline",
            "start",
        ];
        let dlabel = format!("background {deadline}");
        let ilabel = format!("foreground {deadline}");
        let command = ["--", "/bin/sh", "-c", "echo \"$1\" >> \"$2\"", "sh"];
        deferred.extend(["--member", "bravo", "--size", "2", "--label", &dlabel]);
        deferred.extend(command);
        deferred.extend(["deferred", order]);
        let background = spawn(&alpha.member, &repo, &deferred);
        until(
            "the background request to queue",
            crate::processes::WAIT,
            || listed(&alpha.member, &dlabel).is_some(),
        );
        let mut foreground = vec!["--member", "bravo", "--size", "2", "--label", &ilabel];
        foreground.extend(command);
        foreground.extend(["interactive", order]);
        let interactive = spawn(&alpha.member, &repo, &foreground);
        until(
            "the interactive request to queue",
            crate::processes::WAIT,
            || listed(&alpha.member, &ilabel).is_some(),
        );
        if deadline == "1s" {
            std::thread::sleep(Duration::from_millis(1300));
        }
        release_work(held);
        for child in [interactive, background] {
            let result = child.wait_with_output().unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        let order = std::fs::read_to_string(path).unwrap();
        assert_eq!(order.lines().count(), 2);
        assert_eq!(order.lines().next(), Some(expected), "{order}");
    }
}

#[test]
fn deferrable_placement_grants_a_cold_worker_before_a_short_admission_deadline() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (_temp, repo) = repository();
    let here = gated_work(&alpha.member, &repo, &["--pinned", "--size", "2"]);
    crate::processes::workload_policy(
        alpha.member.home.path(),
        Some(&repo),
        r#"[[labels]]
match = "background job"
urgency = "deferrable"
placement = "placeable"
deadline = "15s"
at_deadline = "skip"
size = 2
"#,
    );
    let (id, member) = placed(&alpha.member, &repo, &["--label", "background job"]);
    assert_eq!(member, "bravo");
    let replay = alpha.member.ok(&["replay"]);
    assert_eq!(replay["decisions"]["replayed"], 1, "{replay}");
    assert_eq!(replay["decisions"]["flipped"], 0, "{replay}");
    assert_eq!(
        explain(&bravo.member, &id)["urgency"]["deferrable"]["at_deadline"],
        "skip"
    );
    let trace = _temp.path().join("recorded.json");
    let trace_path = trace.to_str().unwrap();
    let exported = alpha
        .member
        .ok(&["trace", "export", "--output", trace_path]);
    assert_eq!(exported["incomplete"], false, "{exported}");
    let recorded: Value = serde_json::from_slice(&std::fs::read(trace).unwrap()).unwrap();
    assert!(
        recorded["requests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["run_id"] == id && r["deferrable"] == true && r["placement"] == "placeable")
    );
    release_work(here);
}

#[test]
fn deferrable_placement_observes_an_expired_deadline_on_local_fallback() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let (_temp, repo) = repository();
    let result = run(
        &alpha.member,
        &repo,
        &[
            "--placeable",
            "--deferrable",
            "--deadline",
            "2000-01-01T00:00:00Z",
            "--at-deadline",
            "skip",
            "--",
            "/bin/echo",
            "ran",
        ],
    );
    assert_eq!(result.status.code(), Some(125), "{result:?}");
    assert!(result.stdout.is_empty());
    assert_eq!(envelope(&result)["error"]["code"], "deadline_skipped");
}

#[test]
fn skipped_late_acceptance_never_executes_and_the_worker_releases_its_reservation() {
    let alpha = Machine::start("alpha", 2, 0.0, 1.0);
    let bravo = Machine::start("bravo", 2, 0.0, 1.0);
    enrol(&alpha.member, &bravo.member);
    let (temp, repo) = repository();
    let marker = temp.path().join("executed");
    let gate = crate::sync_gate::SyncGate::start_offer(&bravo.member);
    gate.enroll(&alpha.member, &bravo.member);
    gate.arm();
    let child = spawn(
        &alpha.member,
        &repo,
        &[
            "--member",
            "bravo",
            "--size",
            "2",
            "--deferrable",
            "--deadline",
            "3s",
            "--at-deadline",
            "skip",
            "--label",
            "late offer",
            "--",
            "/usr/bin/touch",
            marker.to_str().unwrap(),
        ],
    );
    gate.wait();
    let held = bravo.member.ok(&["runs", "--limit", "1"]);
    assert_eq!(held["member"]["in_use"], 2.0, "{held}");
    let result = child.wait_with_output().unwrap();
    gate.release();
    assert_eq!(result.status.code(), Some(125), "{result:?}");
    let error = envelope(&result);
    assert_eq!(error["error"]["code"], "deadline_skipped", "{error}");
    let id = error["error"]["run_id"].as_str().unwrap();
    until(
        "the unattached reservation to lapse",
        crate::processes::WAIT,
        || explain(&bravo.member, id)["outcome"]["code"] == "cancelled",
    );
    let worker = explain(&bravo.member, id);
    assert_eq!(worker["placed"]["reached"], "placed");
    assert_eq!(
        bravo.member.ok(&["runs", "--limit", "1"])["member"]["in_use"],
        0.0
    );
    let caller = explain(&alpha.member, id);
    assert_eq!(caller["decision"]["verdict"], "deadline_skipped");
    assert!(
        caller["started_at"].is_null() && caller["placed"].is_null(),
        "{caller}"
    );
    assert!(!marker.exists(), "the skipped command executed");
}
