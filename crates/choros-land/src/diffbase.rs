//! A queue's diffbase: PRs it builds on before they reach main, landed
//! through Choros or not. The queue's sets, its merge head and the workspaces
//! synced onto it all start from main with this repo's listed PRs merged in,
//! in list order; nothing in the queue opens a PR until every listed PR
//! (other repos' included) has merged, and then its sets are replayed onto
//! main.
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
    /// Look up each listed PR on GitHub, fetching the heads of this repo's
    /// open ones. A merged PR is not asked about again. One PR failing
    /// doesn't stop the others being looked at.
    pub fn observe_diffbase(&mut self, forge: &dyn Forge) -> Result<()> {
        let listed = self.q.config.diffbase()?;
        let keys: Vec<String> = listed.iter().map(PrSpec::to_string).collect();
        self.st.diffbase.prs.retain(|k, _| keys.contains(k));
        let mut refetched = false;
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

    fn observe_pr(&mut self, forge: &dyn Forge, pr: &PrSpec, refetched: &mut bool) -> Result<()> {
        let own = pr.repo == self.q.config.repo;
        let dir = if own {
            self.git.dir().to_path_buf()
        } else {
            repo_dir(&self.q.trunk, &pr.repo)?
        };
        let state = forge.pr_state(&dir, pr.number)?;
        let head = match (&state, own) {
            (PrState::Open, true) => Some(self.fetch_head(pr.number)?),
            _ => None,
        };
        // Bases built from here on must contain the merge.
        if state == PrState::Merged && own && !*refetched {
            self.q.fetch()?;
            *refetched = true;
        }
        self.st
            .diffbase
            .prs
            .insert(pr.to_string(), SeenPr { state, head });
        Ok(())
    }

    /// GitHub keeps every PR's head at `refs/pull/N/head`, forks' included.
    fn fetch_head(&self, number: u64) -> Result<String> {
        let local = format!("refs/choros/diffbase/{number}");
        self.git
            .fetch("origin", &[&format!("+refs/pull/{number}/head:{local}")])?;
        self.git.sha(&local)
    }

    /// Build the diffbase if its inputs changed: main and this repo's open
    /// listed PRs. A listed PR closed without merging, or one that won't
    /// merge onto those before it, parks the queue instead.
    pub fn rebuild_diffbase(&mut self) -> Result<()> {
        let listed = self.q.config.diffbase()?;
        let seen = |pr: &PrSpec| self.st.diffbase.prs.get(&pr.to_string());
        if let Some(closed) = listed
            .iter()
            .find(|p| seen(p).is_some_and(|s| s.state == PrState::Closed))
        {
            self.st.diffbase.park = Some(DiffbasePark {
                pr: closed.to_string(),
                conflict: None,
            });
            return Ok(());
        }
        // Building without a PR not yet seen would move the queue off it
        // and back again once it is.
        let own = |p: &&PrSpec| p.repo == self.q.config.repo;
        if listed.iter().filter(own).any(|p| seen(p).is_none()) {
            return Ok(());
        }
        let heads: Vec<(String, String)> = listed
            .iter()
            .filter(own)
            .filter_map(|p| {
                let s = seen(p)?;
                let head = s.head.clone().filter(|_| s.state == PrState::Open)?;
                Some((p.to_string(), head))
            })
            .collect();
        let behind = match &self.st.diffbase.built {
            Some(b) => !self.has_merged_sets(&b.main)?,
            None => false,
        };
        let db = &mut self.st.diffbase;
        if db.park.as_ref().is_some_and(|p| p.conflict.is_none()) {
            db.park = None;
        }
        if heads.is_empty() {
            if db.built.take().is_some() {
                let _ = self.git.run(&[
                    "update-ref",
                    "-d",
                    &format!("refs/heads/{}/diffbase", self.q.name),
                ]);
            }
            self.st.diffbase.park = None;
            return Ok(());
        }
        // The same PR heads keep the main they were built on, as a set keeps
        // its base: following main for its own sake would rewrite the whole
        // queue on every merge to main. A set of this queue merging is reason
        // enough, though; its dependents and the merge head need it.
        let main = match &db.built {
            Some(b) if b.heads == heads && !behind && db.park.is_none() => return Ok(()),
            Some(b) if b.heads == heads && !behind => b.main.clone(),
            _ => self.main_sha()?,
        };
        let key = format!(
            "{main};{}",
            heads
                .iter()
                .map(|(p, h)| format!("{p}@{h}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let unchanged = self
            .st
            .diffbase
            .park
            .as_ref()
            .and_then(|p| p.conflict.as_ref())
            .is_some_and(|c| c.on == key);
        if unchanged && self.handler.is_none() {
            return Ok(());
        }
        let tips: Vec<String> = heads.iter().map(|(_, h)| h.clone()).collect();
        let what = format!("{}/diffbase", self.q.name);
        match self.build(&main, &tips, &what)? {
            Ok(sha) => {
                self.git
                    .run(&["update-ref", &format!("refs/heads/{what}"), &sha])?;
                self.st.diffbase.built = Some(BuiltDiffbase { main, heads, sha });
                self.st.diffbase.park = None;
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

    /// Whether every set this queue has merged is in `main`.
    fn has_merged_sets(&self, main: &str) -> Result<bool> {
        let points: BTreeSet<&String> = self
            .st
            .sets
            .values()
            .filter_map(|s| s.merged_into.as_ref())
            .collect();
        for p in points {
            if !self.git.is_ancestor(p, main)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Where every set's base starts: the diffbase, or main.
    pub fn root(&self) -> Result<String> {
        match &self.st.diffbase.built {
            Some(b) => Ok(b.sha.clone()),
            None => self.main_sha(),
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
    let spec = PrSpec::parse(pr, &q.config.repo)?;
    if q.config.diffbase()?.contains(&spec) {
        return Err(eyre!("{spec} is already in {}'s diffbase", q.name));
    }
    let dir = if spec.repo == q.config.repo {
        q.sync_dir()
    } else {
        repo_dir(&q.trunk, &spec.repo)?
    };
    match env.forge.pr_state(&dir, spec.number)? {
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
    let spec = PrSpec::parse(pr, &q.config.repo)?;
    let mut found = false;
    edit_config(q, |list| {
        let before = list.len();
        list.retain(|s| PrSpec::parse(s, &q.config.repo).ok().as_ref() != Some(&spec));
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
