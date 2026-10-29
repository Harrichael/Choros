use super::*;
use crate::state::{ParkOp, SetStatus};

/// Two workspaces land in turn. The first set has nothing ahead of it, so it
/// goes straight to GitHub as a draft PR; the second lands on the merge head,
/// behind the first, and stays private. Merging the first (a merge commit)
/// re-syncs the second onto main and opens its PR, carrying only its own
/// commit.
#[test]
fn landing_two_and_merging_the_front_opens_the_next() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");

    w.commit(&one, "a.txt", "a\n");
    w.land_on_head(&one).unwrap();
    git(&two.join(REPO), &["pull", "--quiet", "--rebase", "origin", "main"]);
    w.commit(&two, "b.txt", "b\n");
    w.land_on_head(&two).unwrap();

    let st = w.state();
    assert!(st.sets[&1].pr.is_some(), "the first set is at the front");
    assert_eq!(st.sets[&2].deps, [Dep::Set(1)]);
    assert!(st.sets[&2].pr.is_none(), "the second stays private");
    let (_, _, draft) = w.forge.pr_text(st.sets[&1].pr.as_ref().unwrap().number);
    assert!(draft);

    w.forge.merge(st.sets[&1].pr.as_ref().unwrap().number, MergeMethod::Merge);
    let report = w.pass();

    let st = w.state();
    assert_eq!(st.sets[&1].status, SetStatus::Merged);
    assert_eq!(report.opened, [2]);
    assert_eq!(w.own_commits(2), 1);
    assert_eq!(st.sets[&2].base_sha, w.origin_main());
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt").as_deref(), Some("a\n"));
    assert_eq!(w.file_at(&st.sets[&2].tip, "b.txt").as_deref(), Some("b\n"));
}

/// A squash merge rewrites the front's commits into one new commit on main.
/// The set behind it still re-syncs cleanly: its own commits are known
/// exactly, so it is replayed onto main without dragging the pre-squash
/// commits along.
#[test]
fn a_squash_merged_front_re_syncs_the_set_behind_it() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a1\n");
    w.commit(&ws, "a.txt", "a2\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();

    let pr = w.state().sets[&1].pr.as_ref().unwrap().number;
    w.forge.merge(pr, MergeMethod::Squash);
    w.pass();

    let st = w.state();
    assert_eq!(st.sets[&2].base_sha, w.origin_main());
    assert_eq!(w.own_commits(2), 1, "only b's commit, not a's two pre-squash commits");
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt").as_deref(), Some("a2\n"));
    assert!(st.sets[&2].pr.is_some());
}

/// A set that depends on two forks keeps both after one of them is
/// squash-merged: its new base is main (which has the merged one's content)
/// merged with the other fork's tip.
#[test]
fn a_set_on_two_forks_keeps_both_when_one_squash_merges() {
    let w = World::new();
    let x = w.workspace("x");
    let y = w.workspace("y");
    let z = w.workspace("z");
    w.commit(&x, "x.txt", "x\n");
    w.land_with(&x, Some(vec![]), None, None).unwrap();
    w.commit(&y, "y.txt", "y\n");
    w.land_with(&y, Some(vec![]), None, None).unwrap();
    w.commit(&z, "z.txt", "z\n");
    w.land_with(&z, Some(vec![Dep::Set(1), Dep::Set(2)]), None, None).unwrap();

    let st = w.state();
    assert!(st.sets[&1].pr.is_some() && st.sets[&2].pr.is_some(), "forks on main are both at the front");
    w.forge.merge(st.sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    w.pass();

    let st = w.state();
    let z_tip = &st.sets[&3].tip;
    assert_eq!(w.file_at(z_tip, "x.txt").as_deref(), Some("x\n"));
    assert_eq!(w.file_at(z_tip, "y.txt").as_deref(), Some("y\n"));
    assert_eq!(w.file_at(z_tip, "z.txt").as_deref(), Some("z\n"));
    assert!(st.sets[&3].pr.is_none(), "y has not merged yet");
}

/// Review fixes pushed from a front set's landing folder become part of the
/// set, and the set queued behind it re-syncs on top of them.
#[test]
fn review_fixes_on_the_front_carry_into_the_set_behind() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    let first = w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();

    let landing = first.landed[0].landing.clone().unwrap().join(REPO);
    std::fs::write(landing.join("a.txt"), "a, reviewed\n").unwrap();
    git(&landing, &["commit", "--quiet", "-am", "address review"]);
    git(&landing, &["push", "--quiet"]);
    w.pass();

    let st = w.state();
    assert_eq!(w.file_at(&st.sets[&1].tip, "a.txt").as_deref(), Some("a, reviewed\n"));
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt").as_deref(), Some("a, reviewed\n"));
    assert_eq!(w.own_commits(1), 2, "the review fix is a new commit, nothing rewritten");
}

/// Two forks that edit the same line cannot sit side by side silently. With
/// no way to ask, the land stops and says so; told to depend, the second
/// lands behind the first instead.
#[test]
fn a_conflicting_fork_is_settled_before_it_goes_public() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_with(&one, Some(vec![]), None, None).unwrap();
    w.commit(&two, "README", "two\n");

    let err = w.land_with(&two, Some(vec![]), None, None).unwrap_err();
    assert!(err.to_string().contains("--on-conflict"), "{err}");
    assert!(w.forge.pr_for("appq/landing-2").is_none());

    w.land_with(&two, Some(vec![]), Some(OnConflict::Depend), Some(&mut Scripted::default()))
        .unwrap();
    let st = w.state();
    let n = *st.sets.keys().max().unwrap();
    assert!(st.sets[&n].deps.contains(&Dep::Set(1)));
    assert!(st.sets[&n].pr.is_none(), "behind landing-1, so not public");
}

/// Two forks editing the same line, kept beside each other: the clash is
/// resolved once, in the merge head. Returns the second fork's number.
fn conflicting_forks_side_by_side(w: &World) -> u32 {
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "README", "one\n");
    w.land_with(&one, Some(vec![]), None, None).unwrap();
    w.commit(&two, "README", "two\n");
    let mut h = Scripted {
        content: [("README".to_string(), "one and two\n".to_string())].into(),
        ..Default::default()
    };
    w.land_with(&two, Some(vec![]), Some(OnConflict::MergeHead), Some(&mut h)).unwrap();
    let st = w.state();
    let mh = st.repos[REPO].merge_head.as_ref().unwrap();
    assert_eq!(w.file_at(&mh.sha, "README").as_deref(), Some("one and two\n"));
    assert_eq!(st.repos[REPO].resolutions.len(), 1);
    *st.sets.keys().max().unwrap()
}

/// When the first of two clashing forks merges, main must be merged into the
/// second (public) one, and the same clash comes back. git's rerere replays
/// the merge-head resolution, so the daemon settles it without asking.
#[test]
fn a_merge_head_resolution_replays_when_the_clash_returns() {
    let w = World::new();
    let n2 = conflicting_forks_side_by_side(&w);
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Merge);
    let report = w.pass();

    assert_eq!(report.updated_public, [n2]);
    let st = w.state();
    assert!(st.sets[&n2].park.is_none());
    assert_eq!(w.file_at(&st.sets[&n2].tip, "README").as_deref(), Some("one and two\n"));
}

/// When rerere cannot replay it (here its cache is gone), the daemon parks,
/// and whoever resolves is shown how the same files were resolved before.
#[test]
fn a_merge_head_resolution_is_shown_when_it_cannot_replay() {
    let w = World::new();
    let n2 = conflicting_forks_side_by_side(&w);
    std::fs::remove_dir_all(w.queue().sync_dir(REPO).join(".git/rr-cache")).unwrap();
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Merge);
    assert_eq!(w.pass().parked, [n2]);

    let mut h = Scripted::default();
    w.resolve(&mut h);
    assert_eq!(h.seen.len(), 1);
    assert_eq!(h.seen[0].files, ["README"]);
    assert!(h.seen[0].recorded > 0, "the earlier resolution is handed over");
    assert!(w.state().sets[&n2].park.is_none());
}

/// A public set is only added to. Main moving on is no reason to touch it;
/// main conflicting with it is, and then main is merged in — a new commit,
/// never a rebase.
#[test]
fn a_public_set_takes_main_in_only_when_it_conflicts() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "mine\n");
    w.land_on_head(&ws).unwrap();
    let tip = w.state().sets[&1].tip.clone();

    w.push_to_main("other.txt", "unrelated\n");
    w.pass();
    assert_eq!(w.state().sets[&1].tip, tip, "unrelated main commits leave it alone");

    w.push_to_main("a.txt", "theirs\n");
    let report = w.pass();
    assert_eq!(report.parked, [1]);
    let mut h = Scripted::default();
    w.resolve(&mut h);

    let st = w.state();
    let new_tip = &st.sets[&1].tip;
    assert_ne!(new_tip, &tip);
    let parents = git(&w.queue().sync_dir(REPO), &["rev-list", "--parents", "-n", "1", new_tip]);
    assert_eq!(parents.split(' ').count(), 3, "a merge commit on top of the old tip");
    assert!(git(&w.queue().sync_dir(REPO), &["merge-base", "--is-ancestor", &tip, new_tip]).is_empty());
}

/// A PR closed without merging parks itself and everything behind it, out
/// of the merge head. Dropping it lets the rest go on without its content.
#[test]
fn a_closed_pr_parks_its_subtree_until_dropped() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();

    w.forge.close(w.state().sets[&1].pr.as_ref().unwrap().number);
    w.pass();
    let st = w.state();
    assert_eq!(st.sets[&1].status, SetStatus::Closed);
    assert_eq!(st.repos[REPO].merge_head.as_ref().unwrap().sha, w.origin_main(), "neither is in the merge head");

    w.drop_set(1);
    let st = w.state();
    assert!(st.sets[&2].pr.is_some(), "landing-2 moved to the front");
    assert_eq!(w.file_at(&st.sets[&2].tip, "a.txt"), None);
    assert_eq!(w.file_at(&st.sets[&2].tip, "b.txt").as_deref(), Some("b\n"));
}

/// The daemon's pass is level-triggered: if it dies after noticing a merge
/// but before acting on it, with a cherry-pick left half done in the sync
/// clone, the next pass still finishes the job.
#[test]
fn a_pass_interrupted_midway_heals_on_the_next() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);

    {
        let env = w.env();
        let q = w.queue();
        let mut work = crate::engine::Work::begin(&q, env.agent, None, true).unwrap().unwrap();
        work.observe_prs(env.forge, &mut Default::default()).unwrap();
        work.save().unwrap();
        let sync = q.sync_dir(REPO);
        git(&sync, &["checkout", "--quiet", "--detach", "HEAD"]);
        std::fs::write(sync.join("README"), "junk\n").unwrap();
        // The crash: a dirty tree mid-way through.
    }

    w.pass();
    let st = w.state();
    assert_eq!(st.sets[&1].status, SetStatus::Merged);
    assert!(st.sets[&2].pr.is_some());
    assert_eq!(st.sets[&2].base_sha, w.origin_main());
}

/// Two lands racing into one queue serialize on its lock: the second sees
/// the first in the merge head and lands behind it.
#[test]
fn concurrent_lands_serialize() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    w.commit(&one, "a.txt", "a\n");
    w.commit(&two, "b.txt", "b\n");
    std::thread::scope(|scope| {
        let handles: Vec<_> = [one, two]
            .into_iter()
            .map(|ws| {
                let w = &w;
                scope.spawn(move || {
                    let req = LandRequest {
                        cwd: ws.join(REPO),
                        mode: LandMode::Continue,
                        deps: crate::land::Deps::Head,
                        force: false,
                        on_conflict: None,
                        title: None,
                        body: None,
                        labels: Vec::new(),
                        sessions: Vec::new(),
                        env_session: None,
                    };
                    w.land_req(&req, None).map(|_| ()).map_err(|e| e.to_string())
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
    });

    let st = w.state();
    assert_eq!(st.sets.len(), 2);
    let later = st.sets.values().find(|s| !s.deps.is_empty()).expect("one landed behind the other");
    let earlier = st.sets.values().find(|s| s.deps.is_empty()).unwrap();
    assert_eq!(later.deps, [Dep::Set(earlier.n)]);
    let mh = st.repos[REPO].merge_head.as_ref().unwrap();
    assert!(w.file_at(&mh.sha, "a.txt").is_some() && w.file_at(&mh.sha, "b.txt").is_some());
}

/// A workspace that merged main into itself still lands only its own work:
/// the sync before landing straightens the merge out.
#[test]
fn a_workspace_that_merged_main_lands_only_its_own_commits() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "mine.txt", "mine\n");
    w.push_to_main("theirs.txt", "theirs\n");
    let repo = ws.join(REPO);
    git(&repo, &["fetch", "--quiet", "origin"]);
    git(&repo, &["merge", "--quiet", "--no-edit", "origin/main"]);

    w.land_on_head(&ws).unwrap();
    let st = w.state();
    assert_eq!(w.own_commits(1), 1);
    assert_eq!(w.file_at(&st.sets[&1].tip, "theirs.txt").as_deref(), Some("theirs\n"));
}

/// The prompt token, and `choros repo ls` through `needs_sync`, tell a
/// workspace when the queue moved under it, and `choros repo sync` clears it.
#[test]
fn the_prompt_says_sync_until_the_workspace_is_synced() {
    let w = World::new();
    let one = w.workspace("one");
    let two = w.workspace("two");
    let needs_sync = || crate::status::needs_sync(&w.trunk, &two, REPO).unwrap();
    assert_eq!(crate::status::prompt_token(&two.join(REPO)), "");
    assert!(!needs_sync());

    w.commit(&one, "a.txt", "a\n");
    w.land_on_head(&one).unwrap();
    assert_eq!(crate::status::prompt_token(&two.join(REPO)), "⇣sync");
    assert!(needs_sync());

    crate::land::sync_workspace(&w.env(), &two.join(REPO)).unwrap();
    assert_eq!(crate::status::prompt_token(&two.join(REPO)), "");
    assert!(!needs_sync());
    assert!(two.join(REPO).join("a.txt").exists());
}

/// A land leaves a landing folder on the set's branch, a workspace of its
/// own, with the landing session forked into it.
#[test]
fn a_land_makes_a_landing_with_the_session_forked_into_it() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    let out = w.land_on_head(&ws).unwrap();

    let landing = out.landed[0].landing.clone().unwrap();
    assert_eq!(landing, w.trunk.join("landing/appq/landing-1"));
    assert_eq!(git(&landing.join(REPO), &["branch", "--show-current"]), "appq/landing-1");
    assert!(landing.join(".choros-meta.toml").exists());
    let forks = w.agent.forks.lock().unwrap();
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].0, landing);
    assert_eq!(forks[0].1, "source-session");
    assert_eq!(w.state().sets[&1].sessions[0].fork, forks[0].2);
}

/// With no queue, landing puts the work straight on main.
#[test]
fn instant_mode_lands_on_main() {
    let w = World::new();
    choros_core::choros::create(&w.trunk, "ws", &[REPO.to_string()], &()).unwrap();
    let ws = w.trunk.join("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    let main = w.origin_main();
    assert_eq!(git(&w.origin, &["show", &format!("{main}:a.txt")]), "a");
    assert!(w.land_on_head(&ws).is_err(), "nothing left to land");
}

/// With no queue and a PR as the route, a PR that can't merge yet is
/// remembered: landing again retries it rather than opening another with the
/// same commits, and once it merges only the newer work lands. One closed
/// without merging leaves its commits to land again.
#[test]
fn an_instant_pr_that_cannot_merge_yet_is_finished_later() {
    let w = World::new();
    choros_core::choros::create(&w.trunk, "ws", &[REPO.to_string()], &()).unwrap();
    let ws = w.trunk.join("ws");
    let mut wl = crate::workspace::read(&ws).unwrap();
    wl.instant = Some(crate::instant::Via::Pr);
    crate::workspace::write(&ws, &wl).unwrap();
    let on_main = |file: &str| {
        let main = w.origin_main();
        Command::new("git")
            .arg("-C")
            .arg(&w.origin)
            .args(["cat-file", "-e", &format!("{main}:{file}")])
            .status()
            .unwrap()
            .success()
    };

    *w.forge.refuse_merges.lock().unwrap() = true;
    w.commit(&ws, "a.txt", "a\n");
    let err = w.land_on_head(&ws).unwrap_err().to_string();
    assert!(err.contains("PR #1") && err.contains("approving review"), "{err}");
    w.commit(&ws, "b.txt", "b\n");
    assert!(w.land_on_head(&ws).is_err());
    assert_eq!(w.forge.pr_count(), 1, "no second PR for a.txt");

    *w.forge.refuse_merges.lock().unwrap() = false;
    w.land_on_head(&ws).unwrap();
    assert!(on_main("a.txt") && on_main("b.txt"));
    assert_eq!(w.forge.pr_count(), 2, "#1 for a.txt, then #2 for b.txt alone");
    assert!(w.land_on_head(&ws).is_err(), "nothing left to land");

    *w.forge.refuse_merges.lock().unwrap() = true;
    w.commit(&ws, "c.txt", "c\n");
    assert!(w.land_on_head(&ws).is_err());
    w.forge.close(3);
    *w.forge.refuse_merges.lock().unwrap() = false;
    w.land_on_head(&ws).unwrap();
    assert!(on_main("c.txt"));
    assert_eq!(w.forge.pr_count(), 4);
}

/// An agent that lands without a PR body is shown the repo's template and
/// what a body is for, and nothing lands; given one, its PR carries it. A
/// person landing without a body gets the template as the PR's body.
#[test]
fn a_pr_body_follows_the_repos_template() {
    let w = World::new();
    *w.forge.template.lock().unwrap() = Some("## Purpose\n\n## Test Plan\n".into());
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");

    let mut req = w.request(&ws, crate::land::Deps::Head, None);
    req.env_session = Some("agent-session".into());
    let err = w.land_req(&req, None).unwrap_err().to_string();
    assert!(err.contains("what changed and why") && err.contains("## Purpose"), "{err}");
    assert!(w.state().sets.is_empty(), "nothing landed");

    req.body = Some("## Purpose\n\nwrite a\n".into());
    w.land_req(&req, None).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();
    w.pass();
    let pr = |n: u32| w.state().sets[&n].pr.as_ref().unwrap().number;
    assert_eq!(w.forge.pr_text(pr(1)).1, "## Purpose\n\nwrite a\n");
    w.forge.merge(pr(1), MergeMethod::Merge);
    w.pass();
    assert_eq!(w.forge.pr_text(pr(2)).1, "## Purpose\n\n## Test Plan\n");
}

/// An instant PR that merges on GitHub after its land is logged by the
/// daemon, with the land's labels, though no one lands from that workspace
/// again, even once it is archived, and though another PR can't be checked
/// at all. A land that does come later finishes the workspace's own record
/// without logging the PR twice.
#[test]
fn an_instant_pr_merged_after_its_land_is_logged_without_another_land() {
    let w = World::new();
    let instant_ws = |name: &str| {
        choros_core::choros::create(&w.trunk, name, &[REPO.to_string()], &()).unwrap();
        let ws = w.trunk.join(name);
        let mut wl = crate::workspace::read(&ws).unwrap();
        wl.instant = Some(crate::instant::Via::Pr);
        crate::workspace::write(&ws, &wl).unwrap();
        ws
    };
    let kept = instant_ws("kept");
    let shelved = instant_ws("shelved");

    *w.forge.refuse_merges.lock().unwrap() = true;
    for (ws, file, task) in [(&kept, "a.txt", "tasks/a"), (&shelved, "b.txt", "tasks/b")] {
        w.commit(ws, file, "x\n");
        let mut req = w.request(ws, crate::land::Deps::Head, None);
        req.labels = vec![task.into()];
        assert!(w.land_req(&req, None).is_err());
    }
    choros_core::choros::archive_at(&shelved).unwrap();
    let gone = instant_ws("gone");
    let mut wl = crate::workspace::read(&gone).unwrap();
    wl.repo.entry(REPO.to_string()).or_default().instant_pr = Some(crate::workspace::InstantPr {
        number: 99,
        head: String::new(),
        labels: Vec::new(),
        sessions: Vec::new(),
    });
    crate::workspace::write(&gone, &wl).unwrap();
    let report = crate::instant::log_merged(&w.trunk, &w.forge).unwrap();
    assert!(report.logged.is_empty(), "still open");
    assert_eq!(report.errors.len(), 1, "#99 is unknown to GitHub");

    w.forge.merge(1, MergeMethod::Merge);
    w.forge.merge(2, MergeMethod::Merge);
    let report = crate::instant::log_merged(&w.trunk, &w.forge).unwrap();
    assert_eq!(report.errors.len(), 1);
    let mut logged: Vec<_> = report.logged.iter().map(|l| (l.pr, l.labels.clone(), l.workspace.clone())).collect();
    logged.sort();
    assert_eq!(
        logged,
        [
            (Some(1), vec!["tasks/a".to_string()], kept.clone()),
            (Some(2), vec!["tasks/b".to_string()], shelved.clone()),
        ]
    );
    assert!(crate::instant::log_merged(&w.trunk, &w.forge).unwrap().logged.is_empty(), "logged once");

    w.land_on_head(&kept).unwrap();
    assert_eq!(crate::instant::read(&w.trunk).unwrap().len(), 2, "the land doesn't log #1 again");
    assert!(w.land_on_head(&kept).is_err(), "nothing left to land");
}

/// A set's commits whose change reached main some other way are dropped
/// when it is re-synced, so its PR lists only what it still changes; a set
/// left with nothing is merged without a PR at all.
#[test]
fn work_already_on_main_drops_out_of_its_set() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b\n");
    w.commit(&ws, "c.txt", "c\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "d.txt", "d\n");
    w.land_on_head(&ws).unwrap();
    w.pass();

    w.push_to_main("b.txt", "b\n");
    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    w.pass();
    assert_eq!(w.own_commits(2), 1, "b.txt is already on main");
    let pr = w.state().sets[&2].pr.as_ref().unwrap().number;
    assert_eq!(w.forge.pr_text(pr).0, "write c.txt");

    w.push_to_main("d.txt", "d\n");
    w.forge.merge(pr, MergeMethod::Squash);
    let report = w.pass();
    let set = &w.state().sets[&3];
    assert_eq!(set.status, SetStatus::Merged);
    assert!(set.pr.is_none());
    assert_eq!(report.already_on_main, [3]);
}

/// Parked sets show up as `!resolve` until resolved.
#[test]
fn the_prompt_flags_a_parked_queue() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "mine\n");
    w.land_on_head(&ws).unwrap();
    w.push_to_main("a.txt", "theirs\n");
    w.pass();
    assert_eq!(w.state().sets[&1].park.as_ref().map(|p| p.op), Some(ParkOp::MainIntoPublic));
    assert_eq!(crate::status::prompt_token(&ws.join(REPO)), "!resolve");
}

/// Landing folders keep up: a clean one follows its set when the set is
/// re-synced, and a merged set's folder moves to the archive.
#[test]
fn landing_folders_follow_their_sets_and_retire_on_merge() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    let first = w.land_on_head(&ws).unwrap().landed[0].landing.clone().unwrap();
    w.commit(&ws, "b.txt", "b\n");
    let second = w.land_on_head(&ws).unwrap().landed[0].landing.clone().unwrap();

    w.forge.merge(w.state().sets[&1].pr.as_ref().unwrap().number, MergeMethod::Squash);
    w.pass();

    assert!(!first.exists());
    assert!(w.trunk.join(".choros-config/archive/landing/appq/landing-1").exists());
    let tip = w.state().sets[&2].tip.clone();
    assert_eq!(git(&second.join(REPO), &["rev-parse", "HEAD"]), tip);
}

/// A dropped set's commits go back to its workspace: the next land queues
/// them again rather than finding nothing to land.
#[test]
fn a_dropped_set_lands_again_from_its_workspace() {
    let w = World::new();
    let first = w.workspace("first");
    w.commit(&first, "a.txt", "a\n");
    w.land_on_head(&first).unwrap();
    let ws = w.workspace("ws");
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();

    let dropped = w.drop_set(2);
    assert_eq!(dropped.returned_to, Some(ws.join(REPO)));

    w.land_on_head(&ws).unwrap();
    let st = w.state();
    assert_eq!(st.sets[&2].status, SetStatus::Dropped);
    assert_eq!(w.file_at(&st.sets[&3].tip, "b.txt").as_deref(), Some("b\n"));
}

/// Syncing after a land moves the workspace past its commits, which then
/// live only in the merge head; a drop takes them out of that. They still
/// come back to the workspace, under the work done since.
#[test]
fn a_dropped_set_comes_back_after_the_workspace_moved_on() {
    let w = World::new();
    let first = w.workspace("first");
    w.commit(&first, "a.txt", "a\n");
    w.land_on_head(&first).unwrap();
    let ws = w.workspace("ws");
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();
    let other = w.workspace("other");
    w.commit(&other, "c.txt", "c\n");
    w.land_on_head(&other).unwrap();
    w.sync(&ws);
    w.commit(&ws, "b.txt", "b, then more\n");

    w.drop_set(2);
    w.sync(&ws);

    assert_eq!(std::fs::read_to_string(ws.join(REPO).join("b.txt")).unwrap(), "b, then more\n");
    w.land_on_head(&ws).unwrap();
    let st = w.state();
    assert_eq!(w.file_at(&st.sets[&4].tip, "b.txt").as_deref(), Some("b, then more\n"));
    assert_eq!(w.own_commits(4), 2, "the dropped commit and the one after it");
}

/// Landing again from a workspace makes a new set, so the land says where
/// changes to its still-open one go.
#[test]
fn landing_again_points_at_the_open_set() {
    let w = World::new();
    let first = w.workspace("first");
    w.commit(&first, "a.txt", "a\n");
    w.land_on_head(&first).unwrap();
    let ws = w.workspace("ws");
    w.commit(&ws, "b.txt", "b\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "b.txt", "b, reviewed\n");

    let out = w.land_on_head(&ws).unwrap();

    assert_eq!(
        out.landed[0].notes,
        ["appq/landing-2 is still open; to change it, work in its landing folder \
          (`choros landing open 2 --queue appq`)"
            .to_string()]
    );
}

/// A set names only what it sits directly on. Three lands in a row make a
/// chain, not a fan: the third depends on the second alone, which already
/// carries the first. A fork picking a set along with one beneath it keeps
/// only the upper one.
#[test]
fn a_set_depends_only_on_what_it_sits_directly_on() {
    let w = World::new();
    let ws = w.workspace("ws");
    let other = w.workspace("other");
    for (i, file) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
        w.commit(&ws, file, &format!("{i}\n"));
        w.land_on_head(&ws).unwrap();
    }
    let st = w.state();
    assert_eq!(st.sets[&2].deps, [Dep::Set(1)]);
    assert_eq!(st.sets[&3].deps, [Dep::Set(2)]);

    w.commit(&other, "d.txt", "d\n");
    w.land_with(&other, Some(vec![Dep::Set(1), Dep::Set(2)]), None, None).unwrap();
    assert_eq!(w.state().sets[&4].deps, [Dep::Set(2)]);
}

/// A pass notes where the front's PR review stands: a draft at first, then
/// ready, then waiting on its author once a reviewer has the last word.
#[test]
fn a_pass_notes_the_front_prs_review() {
    let w = World::new();
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    let pr = w.state().sets[&1].pr.as_ref().unwrap().number;
    let review = || w.state().sets[&1].review.clone().unwrap();

    w.pass();
    assert_eq!(review(), PrReview { draft: true, unanswered: false, approved: false });

    w.forge.mark_ready(pr);
    w.forge.set_unanswered(pr, true);
    w.pass();
    assert_eq!(review(), PrReview { draft: false, unanswered: true, approved: false });
}
