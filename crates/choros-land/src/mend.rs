//! Settling a conflicted file without judgment: from its three versions
//! alone, by a means that gives the same answer every time. What can't be
//! settled so goes on to the agent or to a person.
//!
//! In order: the answer this queue already has for these same three
//! versions, whoever first gave it; a rule in `queue.toml` (`union`: keep
//! both sides' lines); a merge of the file's syntax tree by Mergiraf, when
//! it is installed. A `regenerate` rule is not settled here: its command
//! needs the whole tree, so the engine runs it once the rest is settled.
//!
//! Only regular files are settled here: a symlink's or a submodule's
//! "content" is not text to merge. Everything works on blobs, so it can
//! answer for a conflict in progress in a worktree and for one only
//! foreseen, and changes nothing until the engine takes the answer.

use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use choros_core::process::output_limited;
use color_eyre::eyre::{Result, eyre};
use globset::GlobBuilder;

use crate::git::Git;
use crate::state::{MergeConfig, MergeRule};
use crate::store::Store;

/// One conflicted file: each side's blob and mode, `None` where a side has
/// no such file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sides {
    pub path: String,
    pub base: Option<Side>,
    pub ours: Option<Side>,
    pub theirs: Option<Side>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    pub mode: String,
    pub blob: String,
}

impl Sides {
    pub fn regular(&self) -> bool {
        [&self.base, &self.ours, &self.theirs]
            .into_iter()
            .flatten()
            .all(|s| matches!(s.mode.as_str(), "100644" | "100755"))
    }

    /// The mode the settled file takes: the side's that changed it, as git
    /// merges modes.
    pub fn mode(&self) -> &str {
        fn mode(s: &Option<Side>) -> Option<&str> {
            s.as_ref().map(|s| s.mode.as_str())
        }
        match (mode(&self.base), mode(&self.ours), mode(&self.theirs)) {
            (base, Some(ours), Some(theirs)) if Some(ours) == base => theirs,
            (_, Some(ours), _) => ours,
            (_, None, theirs) => theirs.unwrap_or("100644"),
        }
    }
}

/// How a file was settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mended {
    pub path: String,
    /// The blob it comes to; `None`: the file is gone.
    pub result: Option<String>,
    pub by: By,
    /// This queue had the answer already.
    pub remembered: bool,
}

/// What settled a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum By {
    /// The tool and its version.
    Mergiraf(String),
    Union,
    /// The rule's command.
    Regenerate(String),
    Agent,
    Hand,
    /// Git's own memory of a fix made here before, whoever made it.
    Rerere,
}

impl By {
    /// Whether the answer follows from the file's three versions alone. One
    /// that does can be trusted wherever they meet again; one that took
    /// judgment, or the rest of the tree, was right for the tree it was
    /// given in.
    pub fn from_the_file_alone(&self) -> bool {
        matches!(self, By::Mergiraf(_) | By::Union)
    }

    pub fn name(&self) -> &'static str {
        match self {
            By::Mergiraf(_) => "mergiraf",
            By::Union => "union",
            By::Regenerate(_) => "regenerate",
            By::Agent => "agent",
            By::Hand => "hand",
            By::Rerere => "rerere",
        }
    }

    pub fn tool(&self) -> &str {
        match self {
            By::Mergiraf(tool) | By::Regenerate(tool) => tool,
            _ => "",
        }
    }

    pub fn named(name: &str, tool: String) -> Result<By> {
        Ok(match name {
            "mergiraf" => By::Mergiraf(tool),
            "union" => By::Union,
            "regenerate" => By::Regenerate(tool),
            "agent" => By::Agent,
            "hand" => By::Hand,
            "rerere" => By::Rerere,
            other => {
                return Err(eyre!(
                    "queue.db names an unknown way to settle a file: {other}"
                ));
            }
        })
    }
}

impl MergeConfig {
    pub fn rule_for(&self, path: &str) -> Option<&MergeRule> {
        self.rules
            .iter()
            .find(|r| r.matcher().is_ok_and(|m| m.is_match(path)))
    }

    /// Refuse what would otherwise be passed over in silence.
    pub fn validate(&self) -> Result<()> {
        for rule in &self.rules {
            rule.matcher()
                .map_err(|e| eyre!("[[merge.rule]] files = \"{}\": {e}", rule.files))?;
            if rule.union == rule.regenerate.is_some() {
                return Err(eyre!(
                    "[[merge.rule]] for \"{}\" needs one of `union = true` or `regenerate`",
                    rule.files
                ));
            }
        }
        Ok(())
    }
}

impl MergeRule {
    /// A pattern with no `/` names a file wherever it is.
    fn matcher(&self) -> Result<globset::GlobMatcher, globset::Error> {
        let pattern = if self.files.contains('/') {
            self.files.clone()
        } else {
            format!("**/{}", self.files)
        };
        Ok(GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()?
            .compile_matcher())
    }
}

/// Settle `sides` if it can be done without judgment. Writes blobs, and
/// nothing else.
pub fn mend(
    git: &Git,
    config: &MergeConfig,
    store: &Store,
    repo: &str,
    sides: &Sides,
) -> Result<Option<Mended>> {
    if !sides.regular() {
        return Ok(None);
    }
    if let Some(known) = store.resolution(repo, sides)? {
        // The blob may have gone with a commit git cleaned up.
        if known.result.as_ref().is_none_or(|b| git.has_object(b)) {
            return Ok(Some(known));
        }
    }
    let rule = config.rule_for(&sides.path);
    // Past here both sides must have the file: neither a union nor a tree
    // merge can say whether a file one side deleted should stay.
    let (Some(ours), Some(theirs)) = (&sides.ours, &sides.theirs) else {
        return Ok(None);
    };
    let union = rule.is_some_and(|r| r.union);
    let mergiraf = mergiraf().filter(|_| config.structural && rule.is_none());
    if !union && mergiraf.is_none() {
        return Ok(None);
    }
    let dir = tempfile::tempdir()?;
    let file = |name: &str, blob: Option<&String>| -> Result<std::path::PathBuf> {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            blob.map(|b| git.blob(b)).transpose()?.unwrap_or_default(),
        )?;
        Ok(path)
    };
    let base = file("base", sides.base.as_ref().map(|s| &s.blob))?;
    let (ours, theirs) = (
        file("ours", Some(&ours.blob))?,
        file("theirs", Some(&theirs.blob))?,
    );
    let merged = dir.path().join("merged");
    let by = if union {
        let out = Command::new("git")
            .args(["merge-file", "--union", "-p"])
            .args([&ours, &base, &theirs])
            .output()?;
        if !out.status.success() {
            return Ok(None);
        }
        std::fs::write(&merged, out.stdout)?;
        By::Union
    } else {
        let out = Command::new("mergiraf")
            .arg("merge")
            .args([&base, &ours, &theirs])
            .args(["--path-name", &sides.path, "--output"])
            .arg(&merged)
            .output()?;
        // Mergiraf exits 0 with the file whole; 1 with conflicts left in
        // it, or for a file it can't read as a tree.
        if !out.status.success() || !merged.exists() {
            return Ok(None);
        }
        By::Mergiraf(mergiraf.unwrap_or_default().to_string())
    };
    Ok(Some(Mended {
        path: sides.path.clone(),
        result: Some(git.write_blob(&merged)?),
        by,
        remembered: false,
    }))
}

/// Mergiraf's name and version, when it is installed.
pub fn mergiraf() -> Option<&'static str> {
    static FOUND: OnceLock<Option<String>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            let out = Command::new("mergiraf").arg("--version").output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        })
        .as_deref()
}

/// A package manager fetching over a live connection gets this long.
const REGENERATE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Run a `regenerate` rule's command where the conflict is in progress and
/// stage what it makes of `path`, starting from one side's file. The
/// caller puts back whatever else the command touched.
pub fn regenerate(git: &Git, path: &str, command: &str) -> Result<Mended> {
    git.run(&["checkout", "--theirs", "--", path])?;
    let mut cmd = Command::new("sh");
    // Its own process group, so stopping it stops what it started too.
    cmd.args(["-c", command])
        .current_dir(git.dir())
        .process_group(0);
    let out = output_limited(cmd, REGENERATE_LIMIT)?;
    if !out.status.success() {
        return Err(eyre!(
            "`{command}` failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if !git.dir().join(path).is_file() {
        return Err(eyre!("`{command}` left no {path}"));
    }
    git.run(&["add", "--", path])?;
    Ok(Mended {
        path: path.to_string(),
        result: git.rev_parse_any(&format!(":0:{path}"))?,
        by: By::Regenerate(command.to_string()),
        remembered: false,
    })
}
