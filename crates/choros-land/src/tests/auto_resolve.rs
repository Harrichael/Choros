//! The daemon settles conflicts with the resync agent on its own, within
//! the queue's limits, and trusts a fix only once the repo's check passes.

use super::*;

/// Two forks editing the same line, landed beside each other with no one
/// to resolve the clash: the second is parked out of the merge head.
fn clash(w: &World) -> u32 {
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_with(&one, Some(vec![]), None, None).unwrap();
    w.commit(&two, "README", "two\n");
    w.land_with(&two, Some(vec![]), Some(OnConflict::MergeHead), None)
        .unwrap();
    let n = *w.state().sets.keys().max().unwrap();
    assert!(
        w.state().sets[&n].park.is_some(),
        "no one at the land to resolve it"
    );
    n
}

/// A clash parked at land time gets the daemon's try: the agent, on the
/// queue's model, settles it, the check passes, and the merge head holds
/// the fix.
#[test]
fn the_daemon_settles_a_clash_with_the_agent() {
    let w = World::new();
    let n = clash(&w);
    *w.agent.fix.lock().unwrap() = Some("one and two\n".into());

    let notes = w.daemon_pass();

    let st = w.state();
    assert!(st.sets[&n].park.is_none());
    let mh = st.repos[REPO].merge_head.as_ref().unwrap();
    assert_eq!(
        w.file_at(&mh.sha, "README").as_deref(),
        Some("one and two\n")
    );
    assert_eq!(*w.agent.models.lock().unwrap(), ["opus"]);
    assert_eq!(notes.len(), 1);
    assert!(
        notes[0].starts_with("the agent settled building appq/merge-head (README)"),
        "{notes:?}"
    );
}

/// A fix the check fails stays parked, with the fix and the check's output
/// kept to look at; and the conflict had its one try, so the next pass
/// leaves it be.
#[test]
fn a_fix_the_check_fails_stays_parked_and_isnt_tried_again() {
    let w = World::new();
    let n = clash(&w);
    w.repos_toml("[app]\ncheck = \"! grep -q broken README\"\n");
    *w.agent.fix.lock().unwrap() = Some("broken\n".into());

    let notes = w.daemon_pass();

    assert!(w.state().sets[&n].park.is_some());
    let log = w
        .trunk
        .join(".choros-config/queues/appq/auto-resolve-app.log");
    assert!(notes[0].contains("failed on the agent's fix"), "{notes:?}");
    let kept = std::fs::read_to_string(&log).unwrap();
    assert!(kept.contains("+broken"), "{kept}");

    assert!(w.daemon_pass().is_empty());
    assert_eq!(w.agent.models.lock().unwrap().len(), 1, "one try");
}

/// Past the day's runs, or with auto_resolve off, the clash waits for
/// `choros queue resolve`, and the agent isn't started.
#[test]
fn limits_leave_conflicts_parked() {
    let w = World::new();
    let n = clash(&w);
    let config = w.queue().dir().join("queue.toml");
    let toml = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, toml.replace("daily_runs = 20", "daily_runs = 0")).unwrap();
    *w.agent.fix.lock().unwrap() = Some("one and two\n".into());

    let notes = w.daemon_pass();
    assert!(
        notes[0].contains("the agent already ran 0 times today"),
        "{notes:?}"
    );

    std::fs::write(
        &config,
        toml.replace("auto_resolve = true", "auto_resolve = false"),
    )
    .unwrap();
    w.daemon_pass();

    assert!(w.state().sets[&n].park.is_some());
    assert!(w.agent.models.lock().unwrap().is_empty());
}
