//! Auto lands: `--deps auto`, or `deps = "auto"` in repos.toml, land on as
//! little of the queue as the commits need, and only when the repo's check
//! passes without the rest.

use super::*;

/// Two pieces of unrelated work: the second needs nothing of the first, so it
/// lands straight on main, beside it, and both can open PRs at once.
#[test]
fn unrelated_work_lands_beside_the_queue() {
    let w = World::new();
    w.repos_toml("[app]\ncheck = \"true\"\n");
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land(&one).unwrap();
    w.sync(&two);
    w.commit(&two, "b.txt", "b\n");

    let out = w.land_auto(&two).unwrap();

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
    w.repos_toml("[app]\ncheck = \"true\"\n");
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land(&one).unwrap();
    let three = w.workspace("three");
    w.commit(&three, "c.txt", "c\n");
    w.land(&three).unwrap();
    w.sync(&two);
    w.commit(&two, "a.txt", "a, changed\n");

    let out = w.land_auto(&two).unwrap();

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
    w.repos_toml("[app]\ncheck = \"true\"\n");
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land(&one).unwrap();
    let three = w.workspace("three");
    w.commit(&three, "c.txt", "c\n");
    w.land(&three).unwrap();
    w.sync(&two);
    git(&two.join(REPO), &["rm", "--quiet", "a.txt"]);
    w.commit(&two, "b.txt", "instead of a.txt\n");

    let out = w.land_auto(&two).unwrap();

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
/// adds without touching it. The check catches it, and the land goes on top
/// of everything queued instead, saying in one line which sets the check
/// failed without and where its whole output is.
#[test]
fn a_failing_check_lands_on_everything_queued() {
    let w = World::new();
    w.repos_toml("[app]\ncheck = \"echo no a.txt >&2; test -f a.txt\"\n");
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land(&one).unwrap();
    w.sync(&two);
    w.commit(&two, "b.txt", "uses a.txt\n");

    let out = w.land_auto(&two).unwrap();

    let st = w.state();
    assert_eq!(st.sets[&2].deps, [Dep::Set(1)]);
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt").as_deref(), Some("a\n"));
    let log = w.trunk.join(".choros-config/queues/appq/check-app.log");
    assert_eq!(
        out.landed[0].warnings,
        [format!(
            "`echo no a.txt >&2; test -f a.txt` failed without appq/landing-1; landed after it; full output: {}",
            log.display()
        )]
    );
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "no a.txt\n");
}

/// Auto without a check is refused, before anything lands; and a repo set to
/// auto in repos.toml needs one too.
#[test]
fn auto_needs_a_check() {
    let w = World::new();
    let one = w.workspace("one");
    w.commit(&one, "a.txt", "a\n");

    let err = w.land_auto(&one).unwrap_err().to_string();
    assert!(err.starts_with("--deps auto needs a check for app"), "{err}");

    w.repos_toml("[app]\ndeps = \"auto\"\n");
    let err = w.land(&one).unwrap_err().to_string();
    assert!(err.contains("app has deps = \"auto\" but no check"), "{err}");
    assert!(w.state().sets.is_empty());
}

/// `deps = "auto"` makes every plain land an auto land.
#[test]
fn repos_toml_makes_auto_the_default() {
    let w = World::new();
    w.repos_toml("[app]\ncheck = \"true\"\ndeps = \"auto\"\n");
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.land(&one).unwrap();
    w.sync(&two);
    w.commit(&two, "b.txt", "b\n");

    w.land(&two).unwrap();

    assert_eq!(w.state().sets[&2].deps, []);
}
