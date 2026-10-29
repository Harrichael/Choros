use super::*;
use crate::diffbase;
use crate::forge::PrState;
use crate::state::BuiltDiffbase;

/// `repo`'s built diffbase in `st`, if it has one.
fn diffbase_of(st: &QueueState, repo: &str) -> Option<BuiltDiffbase> {
    st.repos.get(repo).and_then(|r| r.diffbase.clone())
}

impl World {
    /// A PR from outside the queue: a branch on origin writing `file`.
    fn outside_pr(&self, branch: &str, file: &str, content: &str) -> u64 {
        self.push_branch(branch, file, content);
        let req = PrRequest {
            head: branch,
            base: "main",
            title: branch,
            body: "",
            draft: false,
        };
        self.forge
            .create_pr(&self.trunk.join(".choros-config/registry").join(REPO), &req)
            .unwrap()
            .number
    }

    /// Another commit on `branch` (made from main if it doesn't exist yet).
    fn push_branch(&self, branch: &str, file: &str, content: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("w");
        git(
            tmp.path(),
            &["clone", "--quiet", self.origin.to_str().unwrap(), "w"],
        );
        let exists = Command::new("git")
            .arg("-C")
            .arg(&work)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("origin/{branch}"),
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if exists {
            git(&work, &["checkout", "--quiet", branch]);
        } else {
            git(&work, &["checkout", "--quiet", "-b", branch]);
        }
        std::fs::write(work.join(file), content).unwrap();
        git(&work, &["add", file]);
        git(
            &work,
            &[
                "commit",
                "--quiet",
                "-m",
                &format!("{branch} writes {file}"),
            ],
        );
        git(&work, &["push", "--quiet", "origin", branch]);
    }
}

/// Work that needs a PR still in review: the queue's diffbase lists it, so a
/// fresh workspace starts with it, and a set landed there builds on it but
/// stays private. Once the PR is squash-merged, the set is replayed onto main
/// with only its own commit, and its PR opens.
#[test]
fn work_built_on_a_pr_in_review_goes_to_main_after_it() {
    let w = World::new();
    let lib = w.outside_pr("lib", "lib.txt", "lib\n");
    diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{lib}")).unwrap();
    let ws = w.workspace("ws");
    assert_eq!(
        std::fs::read_to_string(ws.join(REPO).join("lib.txt")).unwrap(),
        "lib\n"
    );

    w.commit(&ws, "uses.txt", "uses lib\n");
    w.land_on_head(&ws).unwrap();
    let st = w.state();
    assert!(st.sets[&1].pr.is_none(), "waits for the PR it builds on");
    assert_eq!(
        w.file_at(&st.sets[&1].tip, "lib.txt").as_deref(),
        Some("lib\n")
    );

    w.forge.merge(lib, MergeMethod::Squash);
    let report = w.pass();
    let st = w.state();
    assert_eq!(report.opened, [1]);
    assert_eq!(st.sets[&1].base_sha, w.origin_main());
    assert_eq!(w.own_commits(1), 1, "only the set's commit, not the PR's");
    assert!(diffbase_of(&st, REPO).is_none(), "back on main");
}

/// Two listed PRs that edit the same line park the whole queue, naming the
/// pair. Resolved once, the resolution is reused when a PR gets new commits;
/// it never ships, so nothing opens while the PRs are unmerged.
#[test]
fn listed_prs_that_conflict_park_the_queue_until_resolved() {
    let w = World::new();
    let a = w.outside_pr("a", "x.txt", "a\n");
    let b = w.outside_pr("b", "x.txt", "b\n");
    diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{a}")).unwrap();
    let report = diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{b}")).unwrap();
    assert!(report.diffbase_parked);
    let park = w.state().diffbase.park.unwrap();
    assert_eq!(park.pr, format!("{REPO}#{b}"));
    let conflict = park.conflict.unwrap();
    assert_eq!(conflict.onto, [format!("{REPO}#{a}")]);
    assert_eq!(conflict.files, ["x.txt"]);
    assert!(
        w.pass().diffbase_parked,
        "still parked with no one to resolve"
    );

    let mut handler = Scripted::default();
    handler.content.insert("x.txt".into(), "a and b\n".into());
    w.resolve(&mut handler);
    let st = w.state();
    assert!(st.diffbase.park.is_none());
    let built = diffbase_of(&st, REPO).unwrap();
    assert_eq!(w.file_at(&built.sha, "x.txt").as_deref(), Some("a and b\n"));

    w.push_branch("b", "y.txt", "more b\n");
    let report = w.pass();
    assert!(
        !report.diffbase_parked,
        "the recorded resolution is replayed"
    );
    let rebuilt = diffbase_of(&w.state(), REPO).unwrap();
    assert_ne!(rebuilt.sha, built.sha);
    assert_eq!(
        w.file_at(&rebuilt.sha, "x.txt").as_deref(),
        Some("a and b\n")
    );
    assert_eq!(
        w.file_at(&rebuilt.sha, "y.txt").as_deref(),
        Some("more b\n")
    );
}

/// A PR in another repo is built on in that repo only: work in this one
/// stays on main, but opens no PR until it merges.
#[test]
fn other_repos_prs_hold_the_queue_until_they_merge() {
    let w = World::new();
    let registry = w.trunk.join(".choros-config/registry");
    git(
        &registry,
        &["clone", "--quiet", w.origin.to_str().unwrap(), "lib"],
    );
    let theirs = w.outside_pr("their-change", "theirs.txt", "theirs\n");
    diffbase::add(&w.env(), &w.queue(), &format!("lib#{theirs}")).unwrap();

    let ws = w.workspace("ws");
    w.commit(&ws, "mine.txt", "mine\n");
    w.land_on_head(&ws).unwrap();
    let st = w.state();
    assert!(
        diffbase_of(&st, REPO).is_none(),
        "nothing of another repo's goes in this one's base"
    );
    let lib = diffbase_of(&st, "lib").unwrap();
    assert_eq!(w.file_at_in("lib", &lib.sha, "theirs.txt").as_deref(), Some("theirs\n"));
    assert_eq!(st.sets[&1].base_sha, w.origin_main());
    assert!(st.sets[&1].pr.is_none(), "held until lib's PR merges");

    w.forge.merge(theirs, MergeMethod::Merge);
    let report = w.pass();
    assert_eq!(report.opened, [1]);
    assert_eq!(
        w.state().diffbase.prs[&format!("lib#{theirs}")].state,
        PrState::Merged
    );
}

/// A listed PR closed without merging is left out, as if taken off the list:
/// its code leaves the base, so new workspaces start without it, queued work
/// is replayed off it, and nothing waits for it. Reopened, it comes back.
#[test]
fn a_closed_pr_leaves_the_base_until_reopened() {
    let w = World::new();
    let dropped = w.outside_pr("dropped", "dropped.txt", "dropped\n");
    diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{dropped}")).unwrap();
    let ws = w.workspace("ws");
    w.commit(&ws, "mine.txt", "mine\n");
    w.land_on_head(&ws).unwrap();
    assert!(w.state().sets[&1].pr.is_none(), "waits for the PR");

    w.forge.close(dropped);
    let report = w.pass();

    let st = w.state();
    assert!(!report.diffbase_parked);
    assert!(diffbase_of(&st, REPO).is_none(), "back on main");
    assert_eq!(w.file_at(&st.sets[&1].tip, "dropped.txt"), None);
    assert_eq!(w.file_at(&st.sets[&1].tip, "mine.txt").as_deref(), Some("mine\n"));
    assert!(st.sets[&1].pr.is_some(), "nothing waits for a closed PR");
    let fresh = w.workspace("fresh");
    assert!(!fresh.join(REPO).join("dropped.txt").exists());

    w.forge.reopen(dropped);
    w.pass();
    let db = diffbase_of(&w.state(), REPO).expect("built on it again");
    assert_eq!(w.file_at(&db.sha, "dropped.txt").as_deref(), Some("dropped\n"));
}

/// Adding and removing PRs edits only the `diffbase` line of queue.toml, and
/// adding refuses what can't be built on. Several go at once, all or none.
#[test]
fn the_diffbase_is_edited_in_place_and_checked() {
    let w = World::new();
    let path = w.queue().dir().join("queue.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("# mine, keep me\n{text}")).unwrap();
    let pr = w.outside_pr("p", "p.txt", "p\n");

    diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{pr}")).unwrap();
    assert_eq!(w.queue().config.diffbase, [format!("{REPO}#{pr}")]);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# mine, keep me\n")
    );
    let again = diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{pr}")).unwrap_err();
    assert!(again.to_string().contains("already"), "{again}");
    assert!(diffbase::add(&w.env(), &w.queue(), "nope").is_err());
    assert!(
        diffbase::add(&w.env(), &w.queue(), "ghost#1").is_err(),
        "not in the registry"
    );

    diffbase::remove(&w.env(), &w.queue(), &format!("{REPO}#{pr}")).unwrap();
    assert!(w.queue().config.diffbase.is_empty());
    assert!(diffbase_of(&w.state(), REPO).is_none());

    let (p2, p3) = (w.outside_pr("p2", "p2.txt", "2\n"), w.outside_pr("p3", "p3.txt", "3\n"));
    assert!(
        diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{p2},{p3} ghost#1")).is_err(),
        "one bad, none added"
    );
    assert!(w.queue().config.diffbase.is_empty());
    diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{p2},{p3}")).unwrap();
    assert_eq!(w.queue().config.diffbase, [format!("{REPO}#{p2}"), format!("{REPO}#{p3}")]);
    diffbase::remove(&w.env(), &w.queue(), &format!("{REPO}#{p2},{p3}")).unwrap();
    assert!(w.queue().config.diffbase.is_empty());

    w.forge.merge(pr, MergeMethod::Merge);
    let merged = diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{pr}")).unwrap_err();
    assert!(merged.to_string().contains("already merged"), "{merged}");
}

/// A set whose PR was open before the diffbase was listed can still merge.
/// The set behind it then needs that work, so the diffbase moves to the main
/// that has it, even though the listed PR hasn't changed.
#[test]
fn a_set_merging_under_a_diffbase_moves_it_to_the_new_main() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();
    let lib = w.outside_pr("lib", "lib.txt", "lib\n");
    diffbase::add(&w.env(), &w.queue(), &format!("{REPO}#{lib}")).unwrap();
    let before = diffbase_of(&w.state(), REPO).unwrap();

    let one = w.state().sets[&1].pr.as_ref().unwrap().number;
    w.forge.merge(one, MergeMethod::Squash);
    w.pass();
    let st = w.state();
    let built = diffbase_of(&st, REPO).unwrap();
    assert_ne!(built.main, before.main);
    assert_eq!(built.main, w.origin_main());
    let tip = &st.sets[&2].tip;
    assert_eq!(w.file_at(tip, "a.txt").as_deref(), Some("a\n"));
    assert_eq!(w.file_at(tip, "lib.txt").as_deref(), Some("lib\n"));
    assert_eq!(w.own_commits(2), 1);
}
