use super::*;
use crate::diffbase;
use crate::forge::PrState;

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
            .create_pr(&self.queue().sync_dir(), &req)
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
    diffbase::add(&w.env(), &w.queue(), &format!("#{lib}")).unwrap();
    let ws = w.workspace("ws");
    assert_eq!(
        std::fs::read_to_string(ws.join(REPO).join("lib.txt")).unwrap(),
        "lib\n"
    );

    w.commit(&ws, "uses.txt", "uses lib\n");
    w.land(&ws).unwrap();
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
    assert!(st.diffbase.built.is_none(), "back on main");
}

/// Two listed PRs that edit the same line park the whole queue, naming the
/// pair. Resolved once, the resolution is reused when a PR gets new commits;
/// it never ships, so nothing opens while the PRs are unmerged.
#[test]
fn listed_prs_that_conflict_park_the_queue_until_resolved() {
    let w = World::new();
    let a = w.outside_pr("a", "x.txt", "a\n");
    let b = w.outside_pr("b", "x.txt", "b\n");
    diffbase::add(&w.env(), &w.queue(), &format!("#{a}")).unwrap();
    let report = diffbase::add(&w.env(), &w.queue(), &format!("#{b}")).unwrap();
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
    let built = st.diffbase.built.unwrap();
    assert_eq!(w.file_at(&built.sha, "x.txt").as_deref(), Some("a and b\n"));

    w.push_branch("b", "y.txt", "more b\n");
    let report = w.pass();
    assert!(
        !report.diffbase_parked,
        "the recorded resolution is replayed"
    );
    let rebuilt = w.state().diffbase.built.unwrap();
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

/// A PR in another repo can't be built on, only waited for: the queue's work
/// stays on main but opens no PR until it merges. A listed PR closed without
/// merging parks the queue until it is reopened.
#[test]
fn other_repos_prs_hold_the_queue_and_a_closed_one_parks_it() {
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
    w.land(&ws).unwrap();
    let st = w.state();
    assert!(
        st.diffbase.built.is_none(),
        "nothing of another repo's goes in the base"
    );
    assert_eq!(st.sets[&1].base_sha, w.origin_main());
    assert!(st.sets[&1].pr.is_none(), "held until lib's PR merges");

    w.forge.close(theirs);
    assert!(w.pass().diffbase_parked);
    assert!(w.state().diffbase.park.unwrap().conflict.is_none());
    w.forge.reopen(theirs);
    assert!(!w.pass().diffbase_parked);

    w.forge.merge(theirs, MergeMethod::Merge);
    let report = w.pass();
    assert_eq!(report.opened, [1]);
    assert_eq!(
        w.state().diffbase.prs[&format!("lib#{theirs}")].state,
        PrState::Merged
    );
}

/// Adding and removing a PR edits only the `diffbase` line of queue.toml,
/// and adding refuses what can't be built on.
#[test]
fn the_diffbase_is_edited_in_place_and_checked() {
    let w = World::new();
    let path = w.queue().dir().join("queue.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("# mine, keep me\n{text}")).unwrap();
    let pr = w.outside_pr("p", "p.txt", "p\n");

    diffbase::add(&w.env(), &w.queue(), &format!("#{pr}")).unwrap();
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

    diffbase::remove(&w.env(), &w.queue(), &format!("#{pr}")).unwrap();
    assert!(w.queue().config.diffbase.is_empty());
    assert!(w.state().diffbase.built.is_none());

    w.forge.merge(pr, MergeMethod::Merge);
    let merged = diffbase::add(&w.env(), &w.queue(), &format!("#{pr}")).unwrap_err();
    assert!(merged.to_string().contains("already merged"), "{merged}");
}
