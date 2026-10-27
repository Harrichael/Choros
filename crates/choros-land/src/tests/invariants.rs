//! What must hold after anything a story does, checked by [`World`] after
//! each land, pass, sync and drop, so every story tests them, not only the
//! ones written about them.

use std::collections::BTreeMap;
use std::path::Path;

use super::*;
use crate::git::Git;
use crate::land::changed_lines;
use crate::state::SetStatus;

type Lines = BTreeMap<String, Vec<String>>;

impl World {
    /// What a land is about to send: the repo's unlanded changes, and the
    /// commits of dropped sets handed back to it. None when main was merged
    /// in by hand: the land straightens that out, and the range carries
    /// main's changes too.
    pub(super) fn unlanded_changes(&self, ws: &Path, repo: &str) -> Option<Lines> {
        let rl = crate::workspace::read(ws).unwrap().repo.get(repo).cloned().unwrap_or_default();
        let wgit = Git::new(&ws.join(repo));
        // A rebase finished by hand counts from where it was headed.
        let through = match &rl.pending {
            Some(p) if wgit.is_ancestor(p, "HEAD").unwrap() => p.clone(),
            _ => crate::land::landed_through(ws, repo).unwrap(),
        };
        if wgit.has_merges(&through, "HEAD").unwrap() {
            return None;
        }
        let mut lines = changed_lines(&wgit, (&through, "HEAD")).unwrap();
        for r in &rl.returned {
            let sync = Git::new(&Queue::open(&self.trunk, &r.queue).unwrap().sync_dir(repo));
            for (file, more) in changed_lines(&sync, (&r.base, &r.tip)).unwrap() {
                let have = lines.entry(file).or_default();
                have.extend(more);
                have.sort();
            }
        }
        Some(lines)
    }

    /// A landed set changes exactly what its commits changed in the
    /// workspace, apart from files a conflict handler rewrote. A clean
    /// replay can still lose a hunk; this is where that shows.
    pub(super) fn check_landed(&self, out: &LandOutcome, before: &BTreeMap<String, Option<Lines>>, resolved: &[String]) {
        for l in &out.landed {
            let Some(n) = l.n else { continue };
            let Some(before) = &before[&l.repo] else { continue };
            let q = Queue::open(&self.trunk, l.queue.as_deref().unwrap()).unwrap();
            let st = q.read_state().unwrap();
            let set = &st.sets[&n];
            let sync = Git::new(&q.sync_dir(&l.repo));
            let landed = changed_lines(&sync, (&set.base_sha, &set.tip)).unwrap();
            for file in before.keys().chain(landed.keys()) {
                if resolved.contains(file) {
                    continue;
                }
                assert_eq!(
                    before.get(file),
                    landed.get(file),
                    "landing-{n} changes {file} other than the workspace's commits did"
                );
            }
        }
        self.check();
    }

    /// Checks that hold at any moment.
    pub(super) fn check(&self) {
        self.check_dropped_work_is_kept();
    }

    /// Dropping a set takes it out of the queue, not out of the workspace
    /// that made it: its changes are on their way back there, or back.
    fn check_dropped_work_is_kept(&self) {
        let st = self.state();
        for set in st.sets.values().filter(|s| s.status == SetStatus::Dropped) {
            let (ws, repo) = (&set.source.workspace, &set.source.repo);
            if !ws.join(repo).exists() {
                continue;
            }
            let rl = crate::workspace::read(ws).unwrap().repo.get(repo).cloned().unwrap_or_default();
            if rl.returned.iter().chain(&rl.pending_returned).any(|r| r.n == set.n) {
                continue;
            }
            let sync = self.queue().sync_dir(repo);
            let dropped = patch_ids(&sync, &format!("{}..{}", set.base_sha, set.tip));
            let start = git(&ws.join(repo), &["merge-base", "HEAD", "refs/remotes/origin/main"]);
            let has = patch_ids(&ws.join(repo), &format!("{start}..HEAD"));
            for (id, commit) in &dropped {
                assert!(
                    has.contains_key(id),
                    "{ws:?} lost dropped landing-{}'s commit {commit}",
                    set.n
                );
            }
        }
    }
}

/// Each commit in `range`, by what it changes (`git patch-id`), which a
/// rebase keeps.
fn patch_ids(dir: &Path, range: &str) -> BTreeMap<String, String> {
    use std::io::Write;
    let log = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "--no-merges", "-p", range])
        .output()
        .unwrap()
        .stdout;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["patch-id", "--stable"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&log).unwrap();
    let out = child.wait_with_output().unwrap().stdout;
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(id, c)| (id.to_string(), c.to_string()))
        .collect()
}
