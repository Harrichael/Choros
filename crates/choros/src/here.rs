//! `choros here`: what this folder is to choros, and where a land from it
//! goes, for someone (or an agent) dropped into it cold.

use std::collections::BTreeSet;
use std::path::Path;

use choros_core::space;
use choros_land::queue::Queue;
use choros_land::repos;
use choros_land::workspace;
use color_eyre::eyre::{Result, eyre};

use crate::{choros, registry};

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let trunk = space::find_trunk(&cwd)
        .ok_or_else(|| eyre!("not under a choros trunk (no .choros-config above here)"))?;
    let Some(space) = space::find_space(&cwd) else {
        println!("here: inside {}, choros's own files; nothing lands from here", choros::CONFIG_DIR);
        return Ok(());
    };
    let ws = choros::find_workspace(&cwd).ok().map(|(_, ws)| ws).filter(|ws| ws.starts_with(&trunk));

    let repos = match &ws {
        Some(ws) => {
            if let Some(l) = workspace::read(ws)?.landing {
                let branch = choros_land::state::branch(&l.queue, l.n);
                println!("here: landing folder of {branch}, landed from {}", l.parent.display());
                println!("commits pushed to {branch} from here join the set; `choros land` refuses here");
                println!("trunk: {}", trunk.display());
                print_queue_state(&trunk, &BTreeSet::from([l.queue]))?;
                return Ok(());
            }
            let rel = space::rel(&trunk, ws)?;
            let meta = choros::read_meta(ws)?;
            let below = cwd.strip_prefix(ws).unwrap_or(Path::new(""));
            let inside = below
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .and_then(|first| meta.repos.iter().find(|r| r.as_str() == first).cloned());
            let mut what = format!("workspace {rel}");
            if let Some(repo) = &inside {
                let up = vec![".."; below.components().count()].join("/");
                what = format!("repo {repo}, a clone in {what} (the workspace root is {up})");
            }
            match space_name(&trunk, &space)? {
                Some(b) => println!("here: {what}, in branch-space {b}"),
                None => println!("here: {what}"),
            }
            println!("trunk: {}", trunk.display());
            let repos = match inside {
                Some(repo) => {
                    println!("a land from here lands {repo}:");
                    vec![repo]
                }
                None => {
                    println!("a land from here lands each repo as its own set:");
                    meta.repos
                }
            };
            let mut out = Vec::new();
            for repo in repos {
                let queue = workspace::queue_for(&trunk, ws, &repo)?;
                out.push((repo, queue));
            }
            out
        }
        None => {
            match space_name(&trunk, &space)? {
                Some(b) => println!("here: branch-space {b}, no workspace (`choros work NAME REPO…` makes one here)"),
                None => println!("here: the trunk, no workspace (`choros work NAME REPO…` makes one here)"),
            }
            println!("trunk: {}", trunk.display());
            println!("a new workspace here lands:");
            let mut out = Vec::new();
            for repo in registry::scan(&trunk)? {
                let queue = workspace::queue_for_new(&trunk, &space, &repo)?;
                out.push((repo, queue));
            }
            out
        }
    };

    for (repo, queue) in &repos {
        let into = match queue {
            Some(q) => format!("into {q}"),
            None => "straight to main".to_string(),
        };
        let check = match repos::read(&trunk, repo) {
            Ok(c) => match (c.check, queue) {
                (Some(check), _) => format!("check `{check}`"),
                (None, Some(_)) => "no check, so lands need --force".to_string(),
                (None, None) => "no check".to_string(),
            },
            Err(e) => e.to_string(),
        };
        println!("  {repo}: {into}; {check}");
    }
    let queues: BTreeSet<String> = repos.into_iter().filter_map(|(_, q)| q).collect();
    print_queue_state(&trunk, &queues)?;
    let settings = repos::path(&trunk);
    if settings.exists() {
        println!("repo settings: {}", settings.display());
    } else {
        println!("repo settings: none yet; they go in {} (`choros help agent` shows how)", settings.display());
    }
    Ok(())
}

/// The branch-space `space` is, below the trunk; `None` for the trunk.
fn space_name(trunk: &Path, space: &Path) -> Result<Option<String>> {
    space::is_branch_space(space).then(|| space::rel(trunk, space)).transpose()
}

fn print_queue_state(trunk: &Path, queues: &BTreeSet<String>) -> Result<()> {
    for q in queues {
        let dir = Queue::open(trunk, q)?.dir();
        println!("queue {q}: state in {} (`choros queue {q}` reads it)", dir.join("state.json").display());
    }
    Ok(())
}
