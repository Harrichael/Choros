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

/// Two pieces of work that edit the same lines, neither built on the other.
/// They can't both be at the front: whichever merged second would conflict
/// on GitHub. So the second lands behind the first, and the queue notes
/// that only the lines put it there. A rule that keeps both sides' lines
/// settles the pair as it lands, with no one asked.
#[test]
fn work_that_only_clashes_lands_behind() {
    let w = World::new();
    let mut config = w.queue().config;
    config.merge.rules.push(crate::state::MergeRule {
        files: "README".into(),
        union: true,
        regenerate: None,
    });
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_plain(&one).unwrap();
    w.commit(&two, "README", "two\n");

    let out = w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&2].deps, [Dep::Set(1)]);
    assert_eq!(st.sets[&2].lines_only, [1]);
    assert!(st.sets[&2].park.is_none());
    assert!(st.sets[&1].pr.is_some() && st.sets[&2].pr.is_none(), "one at the front");
    assert_eq!(
        out.landed[0].notes[0],
        "on appq/landing-1: they change the same lines (README), so it goes behind"
    );
    assert_eq!(w.file_at(&st.sets[&2].tip, "README").as_deref(), Some("one\ntwo\n"));
}

/// A clash no tool can put together takes judgment, and the one landing has
/// it: the land moves the workspace onto the queue and stops on the
/// conflict there, for whoever wrote the commits to settle.
#[test]
fn work_that_clashes_past_any_tool_is_settled_where_it_was_written() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_plain(&one).unwrap();
    w.commit(&two, "README", "two\n");

    let err = w.land_plain(&two).unwrap_err().to_string();

    assert!(err.contains("rebasing app onto appq's merge head conflicts in: README"), "{err}");
    assert_eq!(w.state().sets.len(), 1);
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

/// A workspace's earlier set is parked out of the merge head, and its next
/// commits build on that set's lines. They land on it, not on a merge head
/// that goes without it.
#[test]
fn a_land_goes_on_its_own_parked_set() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_plain(&one).unwrap();
    w.commit(&two, "README", "two\n");
    w.land_with(&two, Some(vec![]), Some(OnConflict::MergeHead), None).unwrap();
    assert!(w.state().sets[&2].park.is_some());
    w.commit(&two, "README", "two, more\n");

    w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&3].deps, [Dep::Set(2)]);
    assert_eq!(w.file_at(&st.sets[&3].tip, "README").as_deref(), Some("two, more\n"));
}

/// Going behind a set for its lines pulls in nothing else: another set that
/// touches the same file elsewhere is no part of it.
#[test]
fn a_settled_clash_takes_in_only_the_set_it_clashes_with() {
    let w = World::new();
    let lines = |first: &str, last: &str| format!("{first}\n2\n3\n4\n5\n6\n7\n8\n9\n{last}\n");
    w.push_to_main("NOTES", &lines("1", "10"));
    let mut config = w.queue().config;
    config.merge.rules.push(crate::state::MergeRule {
        files: "NOTES".into(),
        union: true,
        regenerate: None,
    });
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    let (zed, one, two) = (w.workspace("zed"), w.workspace("one"), w.workspace("two"));
    w.commit(&zed, "NOTES", &lines("1", "zed"));
    w.land_plain(&zed).unwrap();
    w.commit(&one, "NOTES", &lines("one", "10"));
    w.land_plain(&one).unwrap();
    assert_eq!(w.state().sets[&2].deps, [], "they meet in no line");
    w.commit(&two, "NOTES", &lines("two", "10"));

    w.land_plain(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&3].deps, [Dep::Set(2)]);
    assert_eq!(st.sets[&3].lines_only, [2]);
}

/// A set that went behind another for its lines landed as it was written.
/// When the other is dropped, nothing of it stays behind in the set.
#[test]
fn dropping_what_a_set_went_behind_for_its_lines_leaves_none_of_it() {
    let w = World::new();
    w.push_to_main("NOTES", "1\n2\n");
    let mut config = w.queue().config;
    config.merge.rules.push(crate::state::MergeRule {
        files: "NOTES".into(),
        union: true,
        regenerate: None,
    });
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    let (one, two) = (w.workspace("one"), w.workspace("two"));
    w.commit(&one, "NOTES", "one\n2\n");
    w.land_plain(&one).unwrap();
    w.commit(&two, "NOTES", "two\n2\n");
    w.land_plain(&two).unwrap();
    let sat = w.state().sets[&2].tip.clone();
    assert_eq!(w.file_at(&sat, "NOTES").as_deref(), Some("one\ntwo\n2\n"));

    w.forge.close(w.forge.pr_for(&format!("{QUEUE}/landing-1")).unwrap());
    w.pass();
    w.drop_set(1);
    w.pass();

    let set = w.state().sets[&2].clone();
    assert!(set.park.is_none() && set.lines_only.is_empty());
    assert_eq!(w.file_at(&set.tip, "NOTES").as_deref(), Some("two\n2\n"));
}

/// Two sets that meet only in their lines could go in either order. The
/// more urgent goes first while the other is only queued; a set with a PR
/// is public and keeps its place whatever lands.
#[test]
fn the_more_urgent_of_two_that_meet_in_their_lines_goes_first() {
    let w = World::new();
    w.push_to_main("NOTES", "1\n2\n");
    let mut config = w.queue().config;
    config.merge.rules.push(crate::state::MergeRule {
        files: "NOTES".into(),
        union: true,
        regenerate: None,
    });
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    let (front, later, urgent) = (w.workspace("front"), w.workspace("later"), w.workspace("urgent"));
    w.commit(&front, "NOTES", "front\n2\n");
    w.land_plain(&front).unwrap();
    w.commit(&later, "NOTES", "later\n2\n");
    w.land_plain(&later).unwrap();
    assert!(w.state().sets[&1].pr.is_some() && w.state().sets[&2].pr.is_none());
    w.commit(&urgent, "NOTES", "urgent\n2\n");

    let req = LandRequest {
        priority: Some(0),
        ..w.request(&urgent, Deps::Least, None)
    };
    w.land_req(&req, None).unwrap();
    w.pass();

    let st = w.state();
    assert_eq!(st.sets[&3].deps, [Dep::Set(1)], "behind the set with a PR");
    assert_eq!(st.sets[&2].deps, [Dep::Set(3)], "ahead of the one only queued");
    assert!(st.sets.values().all(|s| s.park.is_none()));
    assert_eq!(w.file_at(&st.sets[&3].tip, "NOTES").as_deref(), Some("front\nurgent\n2\n"));
    assert_eq!(w.file_at(&st.sets[&2].tip, "NOTES").as_deref(), Some("front\nurgent\nlater\n2\n"));
}

/// An urgent land going ahead of a line of queued sets goes ahead of the
/// whole line, and still behind the set with a PR at the bottom of it: it
/// never ends up at the front beside a set it clashes with.
#[test]
fn an_urgent_land_goes_ahead_of_a_whole_line_and_behind_the_pr_under_it() {
    let w = World::new();
    w.push_to_main("NOTES", "1\n2\n");
    let mut config = w.queue().config;
    config.merge.rules.push(crate::state::MergeRule {
        files: "NOTES".into(),
        union: true,
        regenerate: None,
    });
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    for name in ["front", "a", "b", "c"] {
        let ws = w.workspace(name);
        w.commit(&ws, "NOTES", &format!("{name}\n2\n"));
        w.land_plain(&ws).unwrap();
    }
    let urgent = w.workspace("urgent");
    w.commit(&urgent, "NOTES", "urgent\n2\n");

    let req = LandRequest {
        priority: Some(0),
        ..w.request(&urgent, Deps::Least, None)
    };
    w.land_req(&req, None).unwrap();
    w.pass();

    let st = w.state();
    let deps = |n: u32| st.sets[&n].deps.clone();
    assert_eq!((deps(5), deps(2), deps(3), deps(4)), (vec![Dep::Set(1)], vec![Dep::Set(5)], vec![Dep::Set(2)], vec![Dep::Set(3)]));
    assert!(st.sets[&5].pr.is_none() && st.sets.values().all(|s| s.park.is_none()));
    assert!(st.sets.values().all(|s| s.lines_only.iter().all(|d| s.deps.contains(&Dep::Set(*d)))));
}

/// A land that would go ahead of a set for its lines, and turns out to
/// need that set for its check, goes behind it: the queue never holds two
/// sets that each wait on the other.
#[test]
fn a_land_that_needs_the_set_it_would_go_ahead_of_goes_behind_it() {
    let w = World::new();
    w.push_to_main("NOTES", "1\n2\n");
    w.repos_toml("[app]\ncheck = \"test -f a.txt -o ! -f b.txt\"\n");
    let mut config = w.queue().config;
    config.merge.rules.push(crate::state::MergeRule {
        files: "NOTES".into(),
        union: true,
        regenerate: None,
    });
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    let (y, m, other, urgent) = (w.workspace("y"), w.workspace("m"), w.workspace("other"), w.workspace("urgent"));
    w.commit(&y, "c.txt", "c\n");
    w.land_plain(&y).unwrap();
    w.commit(&m, "NOTES", "m\n2\n");
    w.commit(&m, "a.txt", "a\n");
    w.land_req(&w.request(&m, Deps::Picked(vec![Dep::Set(1)]), None), None).unwrap();
    w.commit(&other, "d.txt", "d\n");
    w.land_plain(&other).unwrap();
    w.commit(&urgent, "NOTES", "urgent\n2\n");
    w.commit(&urgent, "b.txt", "uses a\n");

    let req = LandRequest {
        priority: Some(0),
        ..w.request(&urgent, Deps::Least, None)
    };
    w.land_req(&req, None).unwrap();

    let st = w.state();
    assert!(!crate::graph::has_cycle(&st));
    assert_eq!(st.sets[&4].deps, [Dep::Set(2)]);
    assert_eq!(st.sets[&2].deps, [Dep::Set(1)]);
}
