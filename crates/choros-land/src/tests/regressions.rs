//! Stories that once went wrong (found in review), kept so they stay right.

use super::*;
use crate::state::{ParkOp, SetStatus};

fn rebase_continue(repo: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .env("GIT_EDITOR", "true")
        .args(["rebase", "--continue"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// A land whose sync conflicts stops and says to resolve and land again.
/// Doing exactly that — even after the set the workspace built on was
/// squash-merged — lands the new work, not an error about rewritten history.
#[test]
fn a_sync_conflict_resolved_by_hand_then_lands() {
    let w = World::new();
    let ws = w.workspace("ws");
    let repo = ws.join(REPO);
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    w.pass();
    w.push_to_main("b.txt", "main-b\n");
    w.commit(&ws, "b.txt", "mine-b\n");

    let err = w.land(&ws).unwrap_err();
    assert!(err.to_string().contains("rebase --continue"), "{err}");
    std::fs::write(repo.join("b.txt"), "merged\n").unwrap();
    git(&repo, &["add", "b.txt"]);
    rebase_continue(&repo);

    w.land(&ws).unwrap();
    let st = w.state();
    assert_eq!(w.own_commits(2), 1);
    assert_eq!(w.file_at(&st.sets[&2].tip, "b.txt").as_deref(), Some("merged\n"));
}

/// A landing folder's branch is already a landing-set. Landing from inside
/// it is refused rather than sending the PR's commits around the queue.
#[test]
fn landing_from_a_landing_folder_is_refused() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    let landing = w.land(&ws).unwrap().landed[0].landing.clone().unwrap();
    let lrepo = landing.join(REPO);
    std::fs::write(lrepo.join("a.txt"), "a, reviewed\n").unwrap();
    git(&lrepo, &["commit", "--quiet", "-am", "review fix"]);

    let req = LandRequest {
        cwd: lrepo,
        mode: LandMode::Continue,
        fork: None,
        on_conflict: None,
        title: None,
        body: None,
        sessions: Vec::new(),
        env_session: None,
    };
    let err = crate::land::land(&w.env(), &req, None).unwrap_err();
    assert!(err.to_string().contains("push review fixes"), "{err}");
    assert_eq!(w.file_at(&w.origin_main(), "a.txt"), None);
}

/// When the workspace's own earlier set leaves the merge head (its PR was
/// closed), the next land must not carry that set's commits along as its own.
#[test]
fn landing_after_ones_own_set_closed_leaves_its_commits_out() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.forge.close(w.state().sets[&1].pr.as_ref().unwrap().number);
    w.pass();

    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    let st = w.state();
    assert_eq!(w.own_commits(2), 1);
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt"), None);
    assert_eq!(w.file_at(&st.sets[&2].tip, "b.txt").as_deref(), Some("b\n"));
}

/// A re-sync's push reaches origin, then the process dies before saving.
/// The journal entry written before the push lets the next pass keep the
/// pushed tip instead of mistaking it for someone else's rewrite.
#[test]
fn a_crash_after_a_push_keeps_the_pushed_tip() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    {
        let env = w.env();
        let q = w.queue();
        let mut work = crate::engine::Work::begin(&q, env.agent, None, true).unwrap().unwrap();
        work.observe_prs(env.forge, &mut Default::default()).unwrap();
        let spec = work.desired_base(2).unwrap();
        assert!(work.resync(2, spec).unwrap());
        // dies here, never saving
    }

    w.pass();
    let st = w.state();
    assert!(st.sets[&2].park.is_none());
    assert!(st.sets[&2].pending.is_none());
    assert_eq!(st.sets[&2].base_sha, w.origin_main());
    assert!(st.sets[&2].pr.is_some());
}

/// origin refusing one push holds that set back, not the queue: the rest of
/// the pass runs, and the next pass gets the push through.
#[test]
fn a_refused_push_holds_back_one_set_only() {
    let w = World::new();
    let ws = w.workspace("ws");
    for f in ["a", "b", "c"] {
        w.commit(&ws, &format!("{f}.txt"), &format!("{f}\n"));
        w.land(&ws).unwrap();
    }
    let hook = w.origin.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\nwhile read o n r; do [ \"$r\" = refs/heads/appq/landing-2 ] && exit 1; done\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);

    let report = w.pass();
    assert_eq!(report.push_failed, [2]);
    assert_eq!(w.state().merge_head.as_ref().unwrap().composition.main, w.origin_main());

    std::fs::remove_file(&hook).unwrap();
    w.pass();
    let st = w.state();
    assert_eq!(st.sets[&2].remote.as_ref(), Some(&st.sets[&2].tip));
    assert!(st.sets[&2].pr.is_some());
}

/// A merge commit pushed onto a private set (a pull that merged, in its
/// landing folder) cannot be replayed with the set; the set parks instead
/// of breaking every later pass.
#[test]
fn a_merge_pushed_to_a_private_set_parks_it() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    let second = w.land(&ws).unwrap().landed[0].landing.clone().unwrap().join(REPO);
    w.push_to_main("c.txt", "c\n");
    git(&second, &["fetch", "--quiet", "origin"]);
    git(&second, &["merge", "--quiet", "--no-edit", "origin/main"]);
    git(&second, &["push", "--quiet"]);

    w.pass();
    assert_eq!(w.state().sets[&2].park.as_ref().map(|p| p.op), Some(ParkOp::PushedMerge));
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    w.pass();
}

/// Declining to resolve leaves no trace of an agent session that was never
/// started, so the next resolve does not try to resume a ghost.
#[test]
fn declining_a_resolution_records_no_agent_session() {
    struct Declines;
    impl ConflictHandler for Declines {
        fn resolve(&mut self, _c: &Conflict) -> Result<Handled> {
            Ok(Handled::Declined)
        }
        fn fork_conflict(&mut self, _f: u32, _w: &[u32]) -> Result<Option<OnConflict>> {
            Ok(None)
        }
    }
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "mine\n");
    w.land(&ws).unwrap();
    w.push_to_main("a.txt", "theirs\n");
    w.pass();
    crate::admin::sync(&w.env(), &w.queue(), Some(&mut Declines)).unwrap();
    assert!(w.state().agent.is_none());
}

/// From inside a session (no terminal), landing a fork behind a set it clashes
/// with moves the workspace onto that set for the session to resolve; done,
/// the same land goes through.
#[test]
fn a_fork_landed_behind_a_clash_without_a_terminal() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_with(&one, Some(vec![]), None, None).unwrap();
    w.commit(&two, "README", "two\n");

    let err = w
        .land_with(&two, Some(vec![]), Some(OnConflict::Depend), None)
        .unwrap_err();
    assert!(err.to_string().contains("rebase --continue"), "{err}");
    let repo = two.join(REPO);
    std::fs::write(repo.join("README"), "one, then two\n").unwrap();
    git(&repo, &["add", "README"]);
    rebase_continue(&repo);

    w.land_with(&two, Some(vec![]), Some(OnConflict::Depend), None).unwrap();
    let st = w.state();
    let n = *st.sets.keys().max().unwrap();
    assert!(st.sets[&n].deps.contains(&Dep::Set(1)));
    assert_eq!(w.own_commits(n), 1);
    assert_eq!(w.file_at(&st.sets[&n].tip, "README").as_deref(), Some("one, then two\n"));
}

/// A closed PR needs a decision, so the prompt says so.
#[test]
fn the_prompt_flags_a_closed_pr() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.forge.close(w.state().sets[&1].pr.as_ref().unwrap().number);
    w.pass();
    assert_eq!(crate::status::prompt_token(&ws.join(REPO)), "!resolve");
}

/// A workspace bound after it already had work shows ⇣sync; `choros sync`
/// clears it even when no rebase turns out to be needed.
#[test]
fn a_sync_with_nothing_to_move_still_clears_the_prompt() {
    let w = World::new();
    choros_core::choros::create(&w.root, "ws", &[REPO.to_string()], &()).unwrap();
    let ws = w.root.join("ws");
    w.commit(&ws, "a.txt", "a\n");
    crate::land::bind(&w.env(), &ws, REPO, Some(QUEUE)).unwrap();
    assert_eq!(crate::status::prompt_token(&ws.join(REPO)), "⇣sync");
    crate::land::sync_workspace(&w.env(), &ws.join(REPO)).unwrap();
    assert_eq!(crate::status::prompt_token(&ws.join(REPO)), "");
}

/// A closed PR reopened on GitHub returns to the queue, and what was waiting
/// behind it comes back into the merge head.
#[test]
fn a_reopened_pr_brings_its_subtree_back() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    let pr = w.state().sets[&1].pr.as_ref().unwrap().number;
    w.forge.close(pr);
    w.pass();
    w.forge.reopen(pr);
    w.pass();
    let st = w.state();
    assert_eq!(st.sets[&1].status, SetStatus::Queued);
    assert!(w.file_at(&st.merge_head.as_ref().unwrap().sha, "b.txt").is_some());
}

/// Three sets in a line, each squash-merged in turn, arrive on main intact.
#[test]
fn a_chain_of_three_squash_merges_cleanly() {
    let w = World::new();
    let ws = w.workspace("ws");
    for f in ["a", "b", "c"] {
        w.commit(&ws, &format!("{f}.txt"), &format!("{f}\n"));
        w.land(&ws).unwrap();
    }
    for n in 1..=3 {
        let pr = w.state().sets[&n].pr.as_ref().expect("at the front").number;
        w.forge.merge(pr, MergeMethod::Squash);
        w.pass();
        if n < 3 {
            assert_eq!(w.own_commits(n + 1), 1);
        }
    }
    let main = w.origin_main();
    for f in ["a", "b", "c"] {
        assert_eq!(w.file_at(&main, &format!("{f}.txt")).as_deref(), Some(&*format!("{f}\n")));
    }
    assert_eq!(w.state().merge_head.as_ref().unwrap().sha, main);
}

/// A pass opens a PR and dies before saving. GitHub refuses a second PR for
/// the same branch, so the next pass must find and adopt the one it opened.
#[test]
fn a_crash_after_opening_a_pr_adopts_it() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    {
        let q = w.queue();
        let mut work = crate::engine::Work::begin(&q, &w.agent, None, true).unwrap().unwrap();
        assert_eq!(work.pass(&w.forge).unwrap().opened, [2]);
        // dies here, never saving
    }

    let report = w.pass();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let pr = w.state().sets[&2].pr.as_ref().unwrap().number;
    assert_eq!(Some(pr), w.forge.pr_for("appq/landing-2"));
}

/// The queue recorded a land but the process died before the workspace did.
/// Landing again finds nothing new rather than queueing the commits twice.
#[test]
fn a_crash_between_the_two_saves_does_not_land_twice() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    let file = ws.join(crate::workspace::LANDING_FILE);
    let before = std::fs::read_to_string(&file).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    std::fs::write(&file, before).unwrap();

    let _ = w.land(&ws);
    assert_eq!(w.state().sets.len(), 2);
}

/// Main moving on by itself is no reason to rewrite private sets: that
/// would force-push every queued branch and rerun its CI for nothing.
#[test]
fn main_moving_alone_rewrites_no_private_set() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land(&ws).unwrap();
    w.push_to_main("x.txt", "x\n");
    assert!(w.pass().resynced.is_empty());
}

/// A fork kept beside a clash, landed from inside a session (no one to
/// resolve the merge head there), parks only itself; an unrelated land
/// goes ahead on a merge head without it.
#[test]
fn a_merge_head_clash_parks_the_fork_not_the_queue() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    let three = w.workspace("three");
    w.commit(&one, "README", "one\n");
    w.land(&one).unwrap();
    w.commit(&two, "README", "two\n");
    w.land_with(&two, Some(vec![]), Some(OnConflict::MergeHead), None).unwrap();
    let st = w.state();
    let fork = *st.sets.keys().max().unwrap();
    assert_eq!(st.sets[&fork].park.as_ref().map(|p| p.op), Some(ParkOp::MergeHead));

    w.commit(&three, "c.txt", "c\n");
    w.land(&three).unwrap();
    let st = w.state();
    let mh = &st.merge_head.as_ref().unwrap().sha;
    assert_eq!(w.file_at(mh, "README").as_deref(), Some("one\n"));
    assert!(w.file_at(mh, "c.txt").is_some());

    let mut h = Scripted {
        content: [("README".to_string(), "one and two\n".to_string())].into(),
        ..Default::default()
    };
    w.resolve(&mut h);
    let st = w.state();
    assert!(st.sets[&fork].park.is_none());
    assert_eq!(
        w.file_at(&st.merge_head.as_ref().unwrap().sha, "README").as_deref(),
        Some("one and two\n")
    );
}
