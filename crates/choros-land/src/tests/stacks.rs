//! Stacked PRs: with `stack.enabled`, sets behind the front get their PRs
//! early, linked into GitHub stacks, so review runs in parallel while
//! merging stays in order.

use super::*;
use crate::state::SetStatus;

impl World {
    fn pr(&self, n: u32) -> u64 {
        self.state().sets[&n].pr.as_ref().unwrap_or_else(|| panic!("landing-{n} has no PR")).number
    }

    /// A line of `count` sets, each landed on the one before.
    fn line(&self, count: usize) -> PathBuf {
        let ws = self.workspace("ws");
        for i in 1..=count {
            self.commit(&ws, &format!("f{i}.txt"), &format!("{i}\n"));
            self.land_on_head(&ws).unwrap();
        }
        ws
    }

    /// A reviewer's fix, pushed to a set's branch from elsewhere.
    fn push_fix(&self, n: u32, file: &str, content: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("w");
        let branch = format!("{QUEUE}/landing-{n}");
        git(tmp.path(), &["clone", "--quiet", "--branch", &branch, self.origin.to_str().unwrap(), "w"]);
        std::fs::write(work.join(file), content).unwrap();
        git(&work, &["add", file]);
        git(&work, &["commit", "--quiet", "-m", &format!("review fix {file}")]);
        git(&work, &["push", "--quiet", "origin", &branch]);
    }

    fn origin_tip(&self, n: u32) -> String {
        git(&self.origin, &["rev-parse", &format!("{QUEUE}/landing-{n}")])
    }
}

/// Three sets in a line. The first is at the front, on main; the two behind
/// it open at once as PRs on the branch below them, linked bottom to top
/// into one stack, and status calls them stacked.
#[test]
fn a_line_behind_the_front_opens_as_one_stack() {
    let w = World::new();
    w.stack(5);
    w.line(3);

    assert_eq!(w.forge.base_of(w.pr(1)), "main");
    assert_eq!(w.forge.base_of(w.pr(2)), format!("{QUEUE}/landing-1"));
    assert_eq!(w.forge.base_of(w.pr(3)), format!("{QUEUE}/landing-2"));
    assert_eq!(w.forge.stack_of(w.pr(1)), Some(vec![w.pr(1), w.pr(2), w.pr(3)]));
    let status = crate::status::queue_status(&w.trunk, QUEUE).unwrap();
    let states: Vec<&str> = status.sets.iter().map(|s| s.state).collect();
    assert_eq!(states, ["front", "stacked", "stacked"]);
    assert!(status.sets.iter().all(|s| !s.unlinked));
}

/// A set may list both a set and one built on it, as older lands stored
/// them. It still sits on one set, so it stacks, and after a merge GitHub's
/// rebase of it is taken as usual.
#[test]
fn a_set_listing_a_set_and_one_built_on_it_still_stacks() {
    let w = World::new();
    w.line(3);
    let mut st = w.state();
    st.sets.get_mut(&3).unwrap().deps = vec![Dep::Set(1), Dep::Set(2)];
    w.queue().write_state(&st).unwrap();
    w.stack(5);

    w.pass();
    let one = w.pr(1);
    assert_eq!(w.forge.stack_of(one), Some(vec![one, w.pr(2), w.pr(3)]));

    w.forge.merge(one, MergeMethod::Squash);
    w.pass();
    w.forge.restack(one);
    let report = w.pass();
    assert_eq!(report.adopted, [2, 3]);
    assert!(w.state().sets.values().all(|s| s.park.is_none()));
}

/// The cap counts every open PR in the repo, but holds back only stacking:
/// sets at the front always open. With a cap of 2, a line of three stacks
/// one; three sets with nothing ahead of them all open.
#[test]
fn the_cap_holds_back_stacking_not_the_front() {
    let w = World::new();
    w.stack(2);
    w.line(3);
    let st = w.state();
    assert!(st.sets[&2].pr.is_some());
    assert!(st.sets[&3].pr.is_none(), "the cap is reached");

    let v = World::new();
    v.stack(2);
    for name in ["a", "b", "c"] {
        let ws = v.workspace(name);
        v.commit(&ws, &format!("{name}.txt"), "x\n");
        v.land_with(&ws, Some(vec![]), None, None).unwrap();
    }
    assert!(v.state().sets.values().all(|s| s.pr.is_some()), "front PRs open past the cap");
}

/// Two sets sit on the front one; only one can continue its stack. The one
/// with more work built on it is the critical path, so it stacks, though
/// it landed later; the other waits for the front, as without stacks.
#[test]
fn the_longest_line_continues_the_stack() {
    let w = World::new();
    w.stack(5);
    let one = w.workspace("one");
    w.commit(&one, "a.txt", "a\n");
    w.land_on_head(&one).unwrap();
    let short = w.workspace("short");
    let long = w.workspace("long");
    // Stacking waits until all three are queued: otherwise the first to
    // land would take the slot before the longer line existed.
    let mut config = w.queue().config;
    config.stack.enabled = false;
    crate::state::write_config(&w.queue().dir().join("queue.toml"), &config).unwrap();
    w.commit(&short, "b.txt", "b\n");
    w.land_with(&short, Some(vec![Dep::Set(1)]), None, None).unwrap();
    w.commit(&long, "c.txt", "c\n");
    w.land_with(&long, Some(vec![Dep::Set(1)]), None, None).unwrap();
    w.commit(&long, "d.txt", "d\n");
    w.land_with(&long, Some(vec![Dep::Set(3)]), None, None).unwrap();
    assert_eq!(w.state().sets[&4].deps, [Dep::Set(3)]);
    w.stack(5);

    w.pass();

    let st = w.state();
    assert!(st.sets[&2].pr.is_none(), "the shorter line waits");
    assert_eq!(w.forge.stack_of(w.pr(1)), Some(vec![w.pr(1), w.pr(3), w.pr(4)]));
}

/// A review fix lands low in the stack. The PRs above are replayed onto it
/// and force-pushed, so the stack stays one straight line; the PR it
/// landed on only gained a commit.
#[test]
fn a_fix_low_in_a_stack_reaches_every_pr_above() {
    let w = World::new();
    w.stack(5);
    w.line(3);
    let front = w.origin_tip(1);

    w.push_fix(1, "fix.txt", "fixed\n");
    let report = w.pass();

    assert_eq!(report.resynced, [2, 3]);
    let st = w.state();
    assert!(git_is_ancestor(&w.origin, &front, &w.origin_tip(1)), "the front only gained a commit");
    assert_eq!(w.file_at(&st.sets[&3].tip, "fix.txt").as_deref(), Some("fixed\n"));
    assert_eq!(st.sets[&3].tip, w.origin_tip(3));
    assert_eq!(w.own_commits(3), 1);
}

/// The front is squash-merged. GitHub rebases the stack above on its own,
/// so the pass that sees the merge leaves those branches alone; once
/// GitHub has moved them, they are taken as they are, and the next pass
/// rewrites nothing.
#[test]
fn after_a_merge_github_rebases_the_stack_and_choros_adopts_it() {
    let w = World::new();
    w.stack(5);
    w.line(3);
    let (one, two, three) = (w.pr(1), w.pr(2), w.pr(3));
    let before = (w.origin_tip(2), w.origin_tip(3));

    w.forge.merge(one, MergeMethod::Squash);
    let report = w.pass();
    assert_eq!(report.merged, [1]);
    assert!(report.resynced.is_empty(), "GitHub's to move");
    assert_eq!((w.origin_tip(2), w.origin_tip(3)), before);

    w.forge.restack(one);
    let report = w.pass();
    assert_eq!(report.adopted, [2, 3]);
    assert!(report.resynced.is_empty());
    assert_eq!(w.forge.base_of(two), "main");
    assert_eq!(w.own_commits(2), 1, "the merged set's commits are gone");
    let status = crate::status::queue_status(&w.trunk, QUEUE).unwrap();
    let states: Vec<&str> = status.sets.iter().map(|s| s.state).collect();
    assert_eq!(states, ["merged", "front", "stacked"]);

    let tips = (w.origin_tip(2), w.origin_tip(3));
    let report = w.pass();
    assert!(report.resynced.is_empty() && report.updated_public.is_empty());
    assert_eq!((w.origin_tip(2), w.origin_tip(3)), tips);
    assert_eq!(w.forge.stack_of(three).map(|s| s.len()), Some(3));
}

/// A merge commit, unlike a squash, leaves the front's own commits on main,
/// so the branch GitHub rebases above it may still hold them. That is
/// GitHub's rebase all the same, and is adopted.
#[test]
fn a_stack_rebased_after_a_merge_commit_is_adopted_too() {
    let w = World::new();
    w.stack(5);
    w.line(3);
    let one = w.pr(1);

    w.forge.merge(one, MergeMethod::Merge);
    w.pass();
    w.forge.restack(one);
    let report = w.pass();

    assert_eq!(report.adopted, [2, 3]);
    assert!(report.parked.is_empty());
    assert!(w.state().sets.values().all(|s| s.park.is_none()));
}

/// A set's change reaches main some other way, so GitHub's rebase after
/// the front merges leaves its PR with nothing of its own. GitHub counts
/// that PR as merged and rebases the one above straight onto main; the
/// pass takes it all as it is.
#[test]
fn a_pr_emptied_by_githubs_rebase_counts_as_merged() {
    let w = World::new();
    w.stack(5);
    w.line(4);
    let (one, two, three, four) = (w.pr(1), w.pr(2), w.pr(3), w.pr(4));
    w.push_to_main("f2.txt", "2\n");

    w.forge.merge(one, MergeMethod::Squash);
    w.pass();
    w.forge.restack(one);
    let report = w.pass();

    let st = w.state();
    assert_eq!(st.sets[&2].status, SetStatus::Merged);
    assert_eq!(report.adopted, [3, 4]);
    assert!(report.resynced.is_empty());
    assert_eq!(w.forge.base_of(three), "main");
    assert_eq!(w.forge.stack_of(three), Some(vec![one, two, three, four]));
    assert_eq!(w.own_commits(3), 1);
    assert_eq!(st.sets[&4].tip, w.origin_tip(4));
}

/// A review fix on the front does what the set stacked on it did, so
/// replaying that set leaves it empty. GitHub would close its PR and then
/// refuse to merge the stack through it, so it is never pushed: the set
/// folds into the front, which takes on its tasks (labels), and the PR
/// above moves down onto the front, linked again.
#[test]
fn a_set_emptied_mid_stack_folds_into_the_one_below() {
    let w = World::new();
    w.stack(5);
    let ws = w.workspace("ws");
    for i in 1..=3 {
        w.commit(&ws, &format!("f{i}.txt"), &format!("{i}\n"));
        let mut req = w.request(&ws, crate::land::Deps::Head, None);
        req.labels = vec![format!("task/{i}")];
        req.sessions = vec![format!("session-{i}")];
        w.land_req(&req, None).unwrap();
    }
    let (one, two, three) = (w.pr(1), w.pr(2), w.pr(3));

    w.push_fix(1, "f2.txt", "2\n");
    let report = w.pass();

    let st = w.state();
    assert_eq!(report.folded, [2]);
    assert_eq!(st.sets[&2].status, SetStatus::Dropped);
    assert_eq!(st.sets[&1].labels, ["task/1", "task/2"]);
    let sessions: Vec<&str> = st.sets[&1].sessions.iter().map(|s| s.source.as_str()).collect();
    assert_eq!(sessions, ["session-1", "session-2"]);
    assert_eq!(w.forge.pr_state(&w.origin, two).unwrap(), PrState::Closed);
    assert_eq!(w.forge.base_of(three), format!("{QUEUE}/landing-1"));
    assert_eq!(w.forge.stack_of(three), Some(vec![one, three]));
    assert_eq!(st.sets[&3].deps, [Dep::Set(1)]);
    assert_eq!(w.file_at(&st.sets[&3].tip, "f2.txt").as_deref(), Some("2\n"));
    assert_eq!(st.sets[&3].tip, w.origin_tip(3));
    assert!(git(&w.origin, &["branch", "--list", &format!("{QUEUE}/landing-2")]).is_empty());
}

/// An unlinked PR gets no rebase from GitHub after the set below merges,
/// so Choros replays it onto main; its change already reached main some
/// other way, so nothing is left. Its PR is closed, and it is merged.
#[test]
fn a_stacked_set_emptied_onto_main_is_merged() {
    let w = World::new();
    w.stack(5);
    *w.forge.refuse_links.lock().unwrap() = true;
    w.line(2);
    let two = w.pr(2);
    w.push_to_main("f2.txt", "2\n");

    w.forge.merge(w.pr(1), MergeMethod::Squash);
    let report = w.pass();

    assert_eq!(w.state().sets[&2].status, SetStatus::Merged);
    assert!(report.already_on_main.contains(&2));
    assert_eq!(w.forge.pr_state(&w.origin, two).unwrap(), PrState::Closed);
}

/// When GitHub hasn't moved the stack by the pass after the merge, Choros
/// replays it itself.
#[test]
fn a_stack_github_left_alone_is_replayed_a_pass_later() {
    let w = World::new();
    w.stack(5);
    w.line(2);
    w.forge.merge(w.pr(1), MergeMethod::Squash);
    w.pass();

    let report = w.pass();

    assert_eq!(report.resynced, [2]);
    assert_eq!(w.state().sets[&2].base_sha, w.origin_main());
    assert_eq!(w.own_commits(2), 1);
}

/// `gh stack link` fails, so the stacked PR sits on its branch unlinked,
/// and status says so. GitHub won't move an unlinked PR when the set below
/// merges, so Choros replays it onto main and points the PR there.
#[test]
fn an_unlinked_pr_is_moved_to_main_by_choros() {
    let w = World::new();
    w.stack(5);
    *w.forge.refuse_links.lock().unwrap() = true;
    w.line(2);
    let status = crate::status::queue_status(&w.trunk, QUEUE).unwrap();
    assert!(status.sets[1].unlinked);

    w.forge.merge(w.pr(1), MergeMethod::Squash);
    let report = w.pass();

    assert_eq!(report.resynced, [2]);
    assert_eq!(w.forge.base_of(w.pr(2)), "main");
    assert_eq!(w.own_commits(2), 1);
    let status = crate::status::queue_status(&w.trunk, QUEUE).unwrap();
    assert_eq!(status.sets[1].state, "front");
}

/// A link that failed is retried by the next pass.
#[test]
fn a_failed_link_is_retried() {
    let w = World::new();
    w.stack(5);
    *w.forge.refuse_links.lock().unwrap() = true;
    w.line(2);
    *w.forge.refuse_links.lock().unwrap() = false;

    w.pass();

    assert_eq!(w.forge.stack_of(w.pr(2)), Some(vec![w.pr(1), w.pr(2)]));
}

/// The middle PR of a stack is closed and its set dropped. GitHub keeps a
/// closed PR in its stack and won't move a stacked PR's base, so the stack
/// is dissolved: the PR above moves down onto the front, is replayed
/// without the dropped work, and is linked again; the dropped branch goes
/// last, since GitHub would close a PR still based on it.
#[test]
fn dropping_a_closed_set_mid_stack_mends_the_stack() {
    let w = World::new();
    w.stack(5);
    w.line(3);
    let (one, two, three) = (w.pr(1), w.pr(2), w.pr(3));

    w.forge.close(two);
    w.pass();
    assert_eq!(w.state().sets[&2].status, SetStatus::Closed);
    w.drop_set(2);

    let st = w.state();
    assert_eq!(w.forge.base_of(three), format!("{QUEUE}/landing-1"));
    assert_eq!(w.forge.stack_of(three), Some(vec![one, three]));
    assert_eq!(w.file_at(&st.sets[&3].tip, "f2.txt"), None);
    assert_eq!(w.file_at(&st.sets[&3].tip, "f3.txt").as_deref(), Some("3\n"));
    assert_eq!(st.sets[&3].tip, w.origin_tip(3));
    let gone = Command::new("git")
        .arg("-C")
        .arg(&w.origin)
        .args(["rev-parse", "--verify", "--quiet", &format!("{QUEUE}/landing-2")])
        .status()
        .unwrap();
    assert!(!gone.success(), "the dropped branch is deleted");
}

/// A closed PR holds what builds on it, stacked or not, until it is
/// reopened or dropped; and while it is still in GitHub's stack, another
/// set doesn't take its place there beside it.
#[test]
fn a_closed_pr_in_a_stack_holds_its_line() {
    let w = World::new();
    w.stack(5);
    let ws = w.line(3);
    let tip = w.origin_tip(3);
    let other = w.workspace("other");
    w.commit(&other, "g.txt", "g\n");
    w.land_with(&other, Some(vec![Dep::Set(1)]), None, None).unwrap();
    assert!(w.state().sets[&4].pr.is_none(), "landing-2 holds the stack's next place");

    w.forge.close(w.pr(2));
    w.pass();

    let st = w.state();
    assert_eq!(st.sets[&2].status, SetStatus::Closed);
    assert_eq!(w.origin_tip(3), tip, "landing-3 waits");
    assert!(st.sets[&4].pr.is_none(), "nothing opens beside the closed PR");
    drop(ws);
}

/// Dropping the bottom of a stack: the PR above moves onto main, without
/// the dropped work.
#[test]
fn dropping_the_bottom_of_a_stack_moves_the_next_pr_onto_main() {
    let w = World::new();
    w.stack(5);
    w.line(3);
    let (two, three) = (w.pr(2), w.pr(3));

    w.forge.close(w.pr(1));
    w.pass();
    w.drop_set(1);

    let st = w.state();
    assert_eq!(w.forge.base_of(two), "main");
    assert_eq!(w.forge.stack_of(two), Some(vec![two, three]));
    assert_eq!(w.file_at(&st.sets[&2].tip, "f1.txt"), None);
    assert_eq!(w.file_at(&st.sets[&2].tip, "f2.txt").as_deref(), Some("2\n"));
    assert_eq!(st.sets[&2].base_sha, w.origin_main());
    let status = crate::status::queue_status(&w.trunk, QUEUE).unwrap();
    let states: Vec<&str> = status.sets.iter().filter(|s| s.n != 1).map(|s| s.state).collect();
    assert_eq!(states, ["front", "stacked"]);
}

/// The PR above the dropped bottom changed the dropped work, so it can't
/// be replayed onto main and parks. Its PR stays on the dropped set's
/// branch meanwhile, so that branch stays too: GitHub would close the PR.
#[test]
fn a_dropped_branch_stays_while_a_pr_is_still_based_on_it() {
    let w = World::new();
    w.stack(5);
    let ws = w.workspace("ws");
    w.commit(&ws, "a.txt", "a\n");
    w.land_on_head(&ws).unwrap();
    w.commit(&ws, "a.txt", "a, more\n");
    w.land_on_head(&ws).unwrap();
    let two = w.pr(2);

    w.forge.close(w.pr(1));
    w.pass();
    w.drop_set(1);

    assert!(w.state().sets[&2].park.is_some());
    assert_eq!(w.forge.pr_state(&w.origin, two).unwrap(), PrState::Open);
    assert!(!git(&w.origin, &["branch", "--list", &format!("{QUEUE}/landing-1")]).is_empty());
}

fn git_is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-base", "--is-ancestor", a, b])
        .status()
        .unwrap()
        .success()
}
