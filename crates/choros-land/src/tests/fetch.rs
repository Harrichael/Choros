//! Fetching origin outside the queue's lock. A fetch runs unlocked into a
//! staging area, and the next locked step takes what it saw, except where
//! the queue itself pushed after the fetch began.

use super::*;

fn tracking(w: &World, branch: &str) -> String {
    git(
        &w.queue().sync_dir(REPO),
        &["rev-parse", &format!("refs/remotes/origin/{branch}")],
    )
}

fn landing_1() -> String {
    format!("{QUEUE}/landing-1")
}

/// A fetch that began before the queue's own push saw the branch where it
/// was. Taking it after the push must not put the branch back.
#[test]
fn a_fetch_older_than_the_queues_own_push_does_not_undo_it() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    let q = w.queue();
    let sync = q.sync_dir(REPO);
    q.prefetch(REPO).unwrap();

    let lock = q.lock().unwrap();
    let old = tracking(&w, &landing_1());
    let tree = git(&sync, &["rev-parse", &format!("{old}^{{tree}}")]);
    let pushed = git(
        &sync,
        &["commit-tree", &tree, "-p", &old, "-m", "the queue moves it"],
    );
    git(
        &sync,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("{pushed}:refs/heads/{}", landing_1()),
        ],
    );
    q.take_fetched(REPO, false).unwrap();
    drop(lock);

    assert_eq!(tracking(&w, &landing_1()), pushed);
}

/// What others push is what a fetch is for: the next locked step sees it,
/// branches deleted on origin included.
#[test]
fn what_others_push_reaches_the_next_step() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    git(&w.origin, &["branch", "gone", "main"]);
    let take = |w: &World| {
        let q = w.queue();
        q.prefetch(REPO).unwrap();
        let _lock = q.lock().unwrap();
        q.take_fetched(REPO, false).unwrap();
    };
    take(&w);
    assert_eq!(tracking(&w, "gone"), w.origin_main());

    w.push_onto(1, writes("review.txt", "fix\n"));
    w.push_to_main("m.txt", "m\n");
    git(&w.origin, &["branch", "-D", "gone"]);
    take(&w);

    assert_eq!(tracking(&w, &landing_1()), git(&w.origin, &["rev-parse", &landing_1()]));
    assert_eq!(tracking(&w, "main"), w.origin_main());
    assert!(git(&w.queue().sync_dir(REPO), &["branch", "-r", "--list", "origin/gone"]).is_empty());
}

/// One command's steps share its fetch while it is recent; the next
/// command, as a `choros queue sync` typed after a push, fetches anew.
#[test]
fn a_command_reuses_its_own_fetch_and_the_next_fetches_anew() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    let step = |q: &Queue| {
        q.prefetch(REPO).unwrap();
        let _lock = q.lock().unwrap();
        q.take_fetched(REPO, false).unwrap();
    };

    let q = w.queue();
    step(&q);
    let before = w.origin_main();
    w.push_to_main("m.txt", "m\n");
    step(&q);
    assert_eq!(tracking(&w, "main"), before);

    step(&w.queue());
    assert_eq!(tracking(&w, "main"), w.origin_main());
}

/// A push origin took, from a run that died before it could confirm it,
/// is kept when origin has it. That holds while another process is busy
/// fetching, though what this step's own fetch saw predates the push.
#[test]
fn a_push_that_landed_is_kept_while_another_fetch_runs() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    let q = w.queue();
    q.prefetch(REPO).unwrap();

    // The run's push reached origin; its record still says pending.
    w.push_onto(1, writes("pushed.txt", "p\n"));
    let pushed = git(&w.origin, &["rev-parse", &landing_1()]);
    {
        let _lock = q.lock().unwrap();
        let mut st = q.read_state().unwrap();
        let set = st.sets.get_mut(&1).unwrap();
        set.pending = Some(crate::state::PendingPush {
            old_tip: set.tip.clone(),
            old_base: set.base.clone(),
            old_base_sha: set.base_sha.clone(),
        });
        set.tip = pushed.clone();
        q.write_state(&st).unwrap();
    }

    let other = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(q.dir().join(format!("fetch-{REPO}.lock")))
        .unwrap();
    other.lock().unwrap();
    let work = crate::engine::Work::begin(&q, &w.agent, None, true).unwrap().unwrap();

    assert_eq!(work.st.sets[&1].tip, pushed);
    assert_eq!(work.st.sets[&1].remote.as_deref(), Some(pushed.as_str()));
}
