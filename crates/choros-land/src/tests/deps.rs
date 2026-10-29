//! What a land builds on, and the check that vouches for it. A plain land
//! takes as little of the queue as its commits need; whatever it builds on,
//! the set must pass the repo's check on its own tip, short of `--force`.

use super::*;
use crate::land::Deps;

/// Two pieces of unrelated work: the second needs nothing of the first, so it
/// lands straight on main, beside it, and both can open PRs at once.
#[test]
fn unrelated_work_lands_beside_the_queue() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land_on_head(&one).unwrap();
    w.sync(&two);
    w.commit(&two, "b.txt", "b\n");

    let out = w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&2].deps, []);
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt"), None);
    assert_eq!(w.file_at(&st.sets[&2].tip, "b.txt").as_deref(), Some("b\n"));
    assert!(st.sets[&2].pr.is_some(), "on main, it is at the front");
    let notes = &out.landed[0].notes;
    assert_eq!(notes, &["ahead of appq/landing-1; `true` passed without them".to_string()]);
}

/// Work that edits what a queued set wrote can't replay without it, so it
/// depends on that set, and says why.
#[test]
fn work_on_what_a_set_wrote_lands_behind_it() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land_on_head(&one).unwrap();
    let three = w.workspace("three");
    w.commit(&three, "c.txt", "c\n");
    w.land_on_head(&three).unwrap();
    w.sync(&two);
    w.commit(&two, "a.txt", "a, changed\n");

    let out = w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&3].deps, [Dep::Set(1)]);
    assert_eq!(w.file_at(&st.sets[&3].tip, "c.txt"), None, "landing-2 is skipped");
    assert_eq!(
        out.landed[0].notes,
        [
            "on appq/landing-1: its commits build on it (a.txt)".to_string(),
            "ahead of appq/landing-2; `true` passed without them".to_string(),
        ]
    );
}

/// Work that takes back what a queued set added replays clean without it,
/// the deletion "already done", and would quietly lose that part. It lands
/// behind the set instead, whole.
#[test]
fn work_that_undoes_a_set_lands_behind_it() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land_on_head(&one).unwrap();
    let three = w.workspace("three");
    w.commit(&three, "c.txt", "c\n");
    w.land_on_head(&three).unwrap();
    w.sync(&two);
    git(&two.join(REPO), &["rm", "--quiet", "a.txt"]);
    w.commit(&two, "b.txt", "instead of a.txt\n");

    let out = w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&3].deps, [Dep::Set(1)]);
    assert_eq!(w.file_at(&st.sets[&3].tip, "a.txt"), None, "removed, as written");
    assert_eq!(
        out.landed[0].notes,
        [
            "on appq/landing-1: its commits change what it wrote (a.txt)".to_string(),
            "ahead of appq/landing-2; `true` passed without them".to_string(),
        ]
    );
}

/// No conflict doesn't mean no need: these commits use a file a queued set
/// adds without touching it. The check fails on main alone, so the land
/// tries each line of queued work in turn, newest first, and takes the one
/// it passes with, still ahead of the line it doesn't need.
#[test]
fn a_failing_check_finds_the_set_the_commits_need() {
    let w = World::new();
    w.repos_toml(USES_A);
    let one = w.workspace("one");
    let two = w.workspace("two");
    let three = w.workspace("three");
    w.commit(&one, "a.txt", "a\n");
    w.land_plain(&one).unwrap();
    w.commit(&three, "c.txt", "c\n");
    w.land_plain(&three).unwrap();
    w.commit(&two, "b.txt", "uses a.txt\n");

    let out = w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&2].deps, [], "two lines of work");
    assert_eq!(st.sets[&3].deps, [Dep::Set(1)]);
    assert_eq!(w.file_at(&st.sets[&3].tip, "c.txt"), None, "landing-2 is skipped");
    assert_eq!(
        out.landed[0].notes,
        [
            "on appq/landing-1: the check fails without it".to_string(),
            format!("ahead of appq/landing-2; `{CHECK}` passed without them"),
        ]
    );
}

/// Along one line of queued work the land takes only as far as the set the
/// check starts passing at, and stays ahead of the rest.
#[test]
fn along_a_line_the_land_stops_where_the_check_passes() {
    let w = World::new();
    w.repos_toml(USES_A);
    let two = w.workspace("two");
    for (ws, file) in [("w1", "c.txt"), ("w2", "a.txt"), ("w3", "d.txt")] {
        let ws = w.workspace(ws);
        w.commit(&ws, file, "x\n");
        w.land_on_head(&ws).unwrap();
    }
    w.commit(&two, "b.txt", "uses a.txt\n");

    let out = w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&4].deps, [Dep::Set(2)]);
    assert_eq!(w.file_at(&st.sets[&4].tip, "d.txt"), None, "landing-3 is skipped");
    assert_eq!(
        out.landed[0].notes,
        [
            "on appq/landing-2: the check fails without it".to_string(),
            format!("ahead of appq/landing-3; `{CHECK}` passed without them"),
        ]
    );
}

/// Work that writes b.txt needs a.txt; anything else passes.
const CHECK: &str = "test -f a.txt -o ! -f b.txt";
const USES_A: &str = "[app]\ncheck = \"test -f a.txt -o ! -f b.txt\"\n";

/// A check that fails however much of the queue is under it refuses the
/// land, whatever it builds on, and keeps the output; `--force` lands it
/// unchecked.
#[test]
fn a_failing_check_refuses_the_land_short_of_force() {
    let w = World::new();
    w.repos_toml("[app]\ncheck = \"echo broken >&2; false\"\n");
    let one = w.workspace("one");
    w.commit(&one, "a.txt", "a\n");
    let log = w.trunk.join(".choros-config/queues/appq/check-app.log");

    for (deps, on) in [(Deps::Least, "on the merge head"), (Deps::Head, "on the merge head"), (Deps::Picked(vec![]), "on main")] {
        let err = w.land_req(&w.request(&one, deps, None), None).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "`echo broken >&2; false` failed on app's commits {on}, so they didn't land; full output: {}. \
                 Fix it, or pass --force to land without the check",
                log.display()
            )
        );
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "broken\n");
        assert!(w.state().sets.is_empty());
    }

    let mut req = w.request(&one, Deps::Least, None);
    req.force = true;
    w.land_req(&req, None).unwrap();
    assert_eq!(w.state().sets.len(), 1);
}

/// Every queued repo needs a check, set before anything lands; `--force`
/// lands without one.
#[test]
fn a_queued_repo_needs_a_check() {
    let w = World::new();
    w.repos_toml("");
    let one = w.workspace("one");
    w.commit(&one, "a.txt", "a\n");

    let err = w.land_plain(&one).unwrap_err().to_string();
    assert!(err.starts_with("app has no check to land with"), "{err}");
    assert!(w.state().sets.is_empty());

    let mut req = w.request(&one, Deps::Least, None);
    req.force = true;
    w.land_req(&req, None).unwrap();
    assert_eq!(w.state().sets.len(), 1);
}

/// The check runs with the queue let go, so a slow one holds up no one
/// else: here it takes the queue's lock itself, and can.
#[test]
fn the_check_runs_with_the_queue_let_go() {
    let w = World::new();
    let lock = w.trunk.join(".choros-config/queues/appq/lock");
    w.repos_toml(&format!(
        "[app]\ncheck = \"perl -MFcntl=:flock -e 'open(F, \\\">>\\\", shift) or die; flock(F, LOCK_EX|LOCK_NB) or die \\\"held\\\"' {}\"\n",
        lock.display()
    ));
    let one = w.workspace("one");
    w.commit(&one, "a.txt", "a\n");

    w.land_plain(&one).unwrap();

    assert_eq!(w.state().sets.len(), 1);
}
