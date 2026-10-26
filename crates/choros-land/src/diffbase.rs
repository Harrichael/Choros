//! A queue's diffbase: PRs it builds on before they reach main, landed
//! through Choros or not. In each repo, the queue's sets, its merge head and
//! the workspaces synced onto it start from main with that repo's listed PRs
//! merged in, in list order; nothing in the queue opens a PR until every
//! listed PR has merged, and then its sets are replayed onto main.
//!
//! A conflict between the listed PRs parks the whole queue. Resolving it
//! (`choros queue resolve`) is recorded and replayed by rerere on every
//! rebuild, but never ships: when the PRs merge, main's own resolution of
//! them is what the sets are replayed onto.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use choros_core::registry;
use color_eyre::eyre::{Result, eyre};

use crate::admin;
use crate::engine::{Env, PassReport, Work};
use crate::forge::{Forge, PrState};
use crate::queue::Queue;
use crate::state::{BuiltDiffbase, DiffbaseConflict, DiffbasePark, PrSpec, SeenPr};

impl Work<'_, '_> {
    /// Look up each listed PR on GitHub, fetching the heads of open ones. A
    /// merged PR is not asked about again. One PR failing doesn't stop the
    /// others being looked at.
    pub fn observe_diffbase(&mut self, forge: &dyn Forge) -> Result<()> {
        let listed = self.q.config.diffbase()?;
        let keys: Vec<String> = listed.iter().map(PrSpec::to_string).collect();
        self.st.diffbase.prs.retain(|k, _| keys.contains(k));
        let mut refetched = BTreeSet::new();
        let mut failed = Vec::new();
        for pr in &listed {
            let merged = self
                .st
                .diffbase
                .prs
                .get(&pr.to_string())
                .is_some_and(|s| s.state == PrState::Merged);
            if merged {
                continue;
            }
            if let Err(e) = self.observe_pr(forge, pr, &mut refetched) {
                failed.push(format!("{pr}: {e}"));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(eyre!(failed.join("; ")))
        }
    }

    fn observe_pr(
        &mut self,
        forge: &dyn Forge,
        pr: &PrSpec,
        refetched: &mut BTreeSet<String>,
    ) -> Result<()> {
        self.ensure_repo(&pr.repo)?;
        let state = forge.pr_state(&self.q.sync_dir(&pr.repo), pr.number)?;
        let head = match state {
            PrState::Open => Some(self.fetch_head(&pr.repo, pr.number)?),
            _ => None,
        };
        // Bases built from here on must contain the merge.
        if state == PrState::Merged && refetched.insert(pr.repo.clone()) {
            self.q.fetch(&pr.repo)?;
        }
        self.st
            .diffbase
            .prs
            .insert(pr.to_string(), SeenPr { state, head });
        Ok(())
    }

    /// GitHub keeps every PR's head at `refs/pull/N/head`, forks' included.
    fn fetch_head(&self, repo: &str, number: u64) -> Result<String> {
        let git = self.git(repo);
        let local = format!("refs/choros/diffbase/{number}");
        git.fetch("origin", &[&format!("+refs/pull/{number}/head:{local}")])?;
        git.sha(&local)
    }

    /// Build each repo's diffbase whose inputs changed. A listed PR closed
    /// without merging, or one that won't merge onto those before it, parks
    /// the queue instead.
    pub fn rebuild_diffbase(&mut self) -> Result<()> {
        let listed = self.q.config.diffbase()?;
        let db = &mut self.st.diffbase;
        if let Some(closed) = listed.iter().find(|p| {
            db.prs
                .get(&p.to_string())
                .is_some_and(|s| s.state == PrState::Closed)
        }) {
            db.park = Some(DiffbasePark {
                pr: closed.to_string(),
                conflict: None,
            });
            return Ok(());
        }
        if db.park.as_ref().is_some_and(|p| p.conflict.is_none()) {
            db.park = None;
        }
        let repos: Vec<String> = self.st.repos.keys().cloned().collect();
        for repo in repos {
            self.rebuild_repo_diffbase(&repo, &listed)?;
            if self.st.diffbase.park.is_some() {
                break;
            }
        }
        Ok(())
    }

    fn rebuild_repo_diffbase(&mut self, repo: &str, listed: &[PrSpec]) -> Result<()> {
        let mine: Vec<&PrSpec> = listed.iter().filter(|p| p.repo == repo).collect();
        let seen = |pr: &PrSpec| self.st.diffbase.prs.get(&pr.to_string());
        // Building without a PR not yet seen would move the repo off it and
        // back again once it is.
        if mine.iter().any(|p| seen(p).is_none()) {
            return Ok(());
        }
        let heads: Vec<(String, String)> = mine
            .iter()
            .filter_map(|p| {
                let s = seen(p)?;
                let head = s.head.clone().filter(|_| s.state == PrState::Open)?;
                Some((p.to_string(), head))
            })
            .collect();
        let parked_here = self
            .st
            .diffbase
            .park
            .as_ref()
            .and_then(|p| p.conflict.as_ref().map(|c| (p, c)))
            .filter(|(p, _)| PrSpec::parse(&p.pr, None).is_ok_and(|s| s.repo == repo))
            .map(|(_, c)| c.on.clone());
        let built = self.st.repos[repo].diffbase.clone();
        let what = format!("{}/diffbase", self.q.name);
        if heads.is_empty() {
            if built.is_some() {
                self.st.repos.get_mut(repo).unwrap().diffbase = None;
                let _ = self
                    .git(repo)
                    .run(&["update-ref", "-d", &format!("refs/heads/{what}")]);
            }
            if parked_here.is_some() {
                self.st.diffbase.park = None;
            }
            return Ok(());
        }
        let behind = match &built {
            Some(b) => !self.has_merged_sets(repo, &b.main)?,
            None => false,
        };
        // The same PR heads keep the main they were built on, as a set keeps
        // its base: following main for its own sake would rewrite the whole
        // queue on every merge to main. A set of this queue merging is reason
        // enough, though; its dependents and the merge head need it.
        let main = match &built {
            Some(b) if b.heads == heads && !behind && parked_here.is_none() => return Ok(()),
            Some(b) if b.heads == heads && !behind => b.main.clone(),
            _ => self.main_sha(repo)?,
        };
        let key = format!(
            "{main};{}",
            heads
                .iter()
                .map(|(p, h)| format!("{p}@{h}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        if parked_here.as_deref() == Some(key.as_str()) && self.handler.is_none() {
            return Ok(());
        }
        let tips: Vec<String> = heads.iter().map(|(_, h)| h.clone()).collect();
        match self.build(repo, &main, &tips, &what)? {
            Ok(sha) => {
                self.git(repo)
                    .run(&["update-ref", &format!("refs/heads/{what}"), &sha])?;
                self.st.repos.get_mut(repo).unwrap().diffbase =
                    Some(BuiltDiffbase { main, heads, sha });
                if parked_here.is_some() {
                    self.st.diffbase.park = None;
                }
            }
            Err((i, files)) => {
                self.st.diffbase.park = Some(DiffbasePark {
                    pr: heads[i].0.clone(),
                    conflict: Some(DiffbaseConflict {
                        onto: heads[..i].iter().map(|(p, _)| p.clone()).collect(),
                        files,
                        on: key,
                    }),
                });
            }
        }
        Ok(())
    }

    /// Whether every set this queue has merged in `repo` is in `main`.
    fn has_merged_sets(&self, repo: &str, main: &str) -> Result<bool> {
        let git = self.git(repo);
        let points: BTreeSet<&String> = self
            .st
            .sets
            .values()
            .filter(|s| s.repo() == repo)
            .filter_map(|s| s.merged_into.as_ref())
            .collect();
        for p in points {
            if !git.is_ancestor(p, main)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Where the bases of `repo`'s sets start: its diffbase, or main.
    pub fn root(&self, repo: &str) -> Result<String> {
        match self.st.repos.get(repo).and_then(|r| r.diffbase.as_ref()) {
            Some(b) => Ok(b.sha.clone()),
            None => self.main_sha(repo),
        }
    }

    /// A listed PR hasn't merged (or hasn't been seen yet), so the queue's
    /// work isn't ready for main.
    pub fn diffbase_waiting(&self) -> Result<bool> {
        Ok(self.q.config.diffbase()?.iter().any(|p| {
            self.st
                .diffbase
                .prs
                .get(&p.to_string())
                .is_none_or(|s| s.state != PrState::Merged)
        }))
    }
}

/// Add `pr` to the queue's diffbase, editing its queue.toml in place, and
/// settle the queue on it. It must be open: a merged PR is already in main.
pub fn add(env: &Env, q: &Queue, pr: &str) -> Result<PassReport> {
    let spec = PrSpec::parse(pr, q.config.repo.as_deref())?;
    if q.config.diffbase()?.contains(&spec) {
        return Err(eyre!("{spec} is already in {}'s diffbase", q.name));
    }
    match env
        .forge
        .pr_state(&repo_dir(&q.trunk, &spec.repo)?, spec.number)?
    {
        PrState::Open => {}
        PrState::Merged => return Err(eyre!("{spec} has already merged; main has it")),
        PrState::Closed => return Err(eyre!("{spec} was closed without merging")),
    }
    edit_config(q, |list| list.push(spec.to_string()))?;
    admin::sync(env, &Queue::open(&q.trunk, &q.name)?, None)
}

/// Take `pr` out of the queue's diffbase; the queue moves off it on the pass
/// that follows.
pub fn remove(env: &Env, q: &Queue, pr: &str) -> Result<PassReport> {
    let spec = PrSpec::parse(pr, q.config.repo.as_deref())?;
    let mut found = false;
    edit_config(q, |list| {
        let before = list.len();
        list.retain(|s| PrSpec::parse(s, q.config.repo.as_deref()).ok().as_ref() != Some(&spec));
        found = list.len() != before;
    })?;
    if !found {
        return Err(eyre!("{spec} is not in {}'s diffbase", q.name));
    }
    admin::sync(env, &Queue::open(&q.trunk, &q.name)?, None)
}

fn repo_dir(trunk: &Path, repo: &str) -> Result<PathBuf> {
    let dir = registry::repo_path(trunk, repo);
    if !dir.exists() {
        return Err(eyre!(
            "{repo} is not in this trunk's registry; add it with `choros add <ssh-url>`"
        ));
    }
    Ok(dir)
}

/// Rewrite queue.toml's `diffbase` and nothing else (comments stay), under
/// the queue's lock so no pass reads it half-changed.
fn edit_config(q: &Queue, change: impl FnOnce(&mut Vec<String>)) -> Result<()> {
    let _lock = q.lock()?;
    let path = q.dir().join("queue.toml");
    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&path)?.parse()?;
    let mut list: Vec<String> = doc
        .get("diffbase")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    change(&mut list);
    if list.is_empty() {
        doc.remove("diffbase");
    } else {
        doc["diffbase"] = toml_edit::value(toml_edit::Array::from_iter(list));
    }
    crate::state::write_atomic(&path, doc.to_string().as_bytes())
}
