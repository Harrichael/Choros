//! `choros land`, `sync`, `status`, `queue …` and `landing open`: the CLI
//! over the landing queue engine.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use choros_land::agent::{Claude, ConflictBrief};
use choros_land::engine::{Conflict, ConflictHandler, Env, Handled, OnConflict, PassReport};
use choros_land::forge::Gh;
use choros_land::land::{self, LandRequest};
use choros_land::queue::{self, Queue};
use choros_land::state::{Dep, LandMode, Scope};
use choros_land::{admin, status};
use color_eyre::eyre::{Result, eyre};

use crate::choros;

/// What the engine needs, owned, so an `Env` can borrow it.
struct Tools {
    gh: Gh,
    claude: Claude,
}

impl Tools {
    fn for_root(root: &Path) -> Tools {
        let gh_dir = root.join(choros::CONFIG_DIR).join("gh");
        Tools {
            gh: Gh {
                config_dir: gh_dir.exists().then_some(gh_dir),
            },
            claude: Claude,
        }
    }

    fn env(&self) -> Env<'_> {
        Env {
            forge: &self.gh,
            agent: &self.claude,
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }
}

fn root_here() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    choros::find_root(&cwd).ok_or_else(|| eyre!("not under a choros root (no .choros-config above here)"))
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

fn usage(msg: &str) -> ! {
    eprintln!("usage: {msg}");
    std::process::exit(2);
}

pub fn run_land(args: Vec<String>) -> Result<()> {
    let mut it = args.into_iter();
    let mode = match it.next().as_deref() {
        Some("continue") => LandMode::Continue,
        Some("archive") => LandMode::Archive,
        _ => usage("choros land continue|archive [--fork [--deps SET,…|none]] [--on-conflict depend|merge-head] [--title T] [--body-file F] [--session ID]…"),
    };
    let mut fork = false;
    let mut deps: Option<Vec<Dep>> = None;
    let mut on_conflict = None;
    let mut title = None;
    let mut body = None;
    let mut sessions = Vec::new();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--fork" => fork = true,
            "--deps" => deps = Some(parse_deps(&it.next().unwrap_or_default())?),
            "--on-conflict" => {
                on_conflict = Some(match it.next().as_deref() {
                    Some("depend") => OnConflict::Depend,
                    Some("merge-head") => OnConflict::MergeHead,
                    other => return Err(eyre!("--on-conflict takes depend or merge-head, got {other:?}")),
                })
            }
            "--title" => title = it.next(),
            "--body-file" => {
                let path = it.next().ok_or_else(|| eyre!("--body-file needs a path"))?;
                body = Some(std::fs::read_to_string(path)?);
            }
            "--session" => sessions.push(it.next().ok_or_else(|| eyre!("--session needs an id"))?),
            other => return Err(eyre!("unknown option {other}")),
        }
    }
    let cwd = std::env::current_dir()?;
    let (root, ws) = choros::find_workspace(&cwd)?;
    let fork = if fork {
        Some(match deps {
            Some(d) => d,
            None if interactive() => pick_fork_deps(&root, &ws, &cwd)?,
            None => return Err(eyre!("--fork without a terminal needs --deps (SET,… or none)")),
        })
    } else {
        None
    };

    let tools = Tools::for_root(&root);
    let mut handler = Interactive::default();
    let req = LandRequest {
        cwd,
        mode,
        fork,
        on_conflict,
        title,
        body,
        sessions,
    };
    let handler: Option<&mut (dyn ConflictHandler + 'static)> =
        if interactive() { Some(&mut handler) } else { None };
    let out = land::land(&tools.env(), &req, handler)?;
    for l in &out.landed {
        match (&l.queue, l.n) {
            (Some(q), Some(n)) => {
                let st = Queue::open(&root, q)?.read_state()?;
                let where_ = match st.sets.get(&n).and_then(|s| s.pr.as_ref()) {
                    Some(pr) => format!("at the front: {}", pr.url),
                    None => "queued".to_string(),
                };
                eprintln!("{} → {q}/landing-{n}, {where_}", l.repo);
                if let Some(dir) = &l.landing {
                    eprintln!("    landing: {}", dir.display());
                }
                if !l.forked_sessions.is_empty() {
                    eprintln!("    sessions forked: {}", l.forked_sessions.join(", "));
                }
            }
            _ => eprintln!("{} → merged to main", l.repo),
        }
        for w in &l.warnings {
            eprintln!("    warning: {w}");
        }
    }
    // stdout carries only the archived path, for the shell function to leave
    // a workspace that has moved.
    if let Some(dst) = out.archived_to {
        println!("{}", dst.display());
    }
    Ok(())
}

/// `1,2`, `landing-1`, `other-queue/landing-3`, or `none`.
fn parse_deps(s: &str) -> Result<Vec<Dep>> {
    if s == "none" || s.is_empty() {
        return Ok(Vec::new());
    }
    s.split(',')
        .map(|part| {
            let part = part.trim();
            let num = |x: &str| -> Result<u32> {
                x.trim_start_matches("landing-")
                    .parse()
                    .map_err(|_| eyre!("not a landing-set: {part:?}"))
            };
            match part.split_once('/') {
                Some((queue, n)) => Ok(Dep::Cross {
                    queue: queue.to_string(),
                    n: num(n)?,
                }),
                None => Ok(Dep::Set(num(part)?)),
            }
        })
        .collect()
}

fn pick_fork_deps(root: &Path, ws: &Path, cwd: &Path) -> Result<Vec<Dep>> {
    let wl = choros_land::workspace::read(ws)?;
    let repo = cwd
        .strip_prefix(ws)
        .ok()
        .and_then(|r| r.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_string);
    let queue = repo
        .as_ref()
        .and_then(|r| wl.repo.get(r))
        .or_else(|| wl.repo.values().find(|r| r.queue.is_some()))
        .and_then(|r| r.queue.clone())
        .ok_or_else(|| eyre!("no repo here lands into a queue"))?;
    let q = Queue::open(root, &queue)?;
    let st = q.read_state()?;
    let items: Vec<(Dep, String)> = st
        .sets
        .values()
        .filter(|s| s.is_open())
        .map(|s| (Dep::Set(s.n), crate::queue_tui::describe(&q, s)))
        .collect();
    let chosen = crate::queue_tui::pick(
        &format!(" {queue}: what does this fork depend on? (none = straight to main) "),
        &items.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>(),
        &[],
    )?
    .ok_or_else(|| eyre!("cancelled"))?;
    Ok(chosen.into_iter().map(|i| items[i].0.clone()).collect())
}

pub fn run_sync(args: Vec<String>) -> Result<()> {
    if !args.is_empty() {
        usage("choros sync");
    }
    let cwd = std::env::current_dir()?;
    let root = root_here()?;
    let tools = Tools::for_root(&root);
    let synced = land::sync_workspace(&tools.env(), &cwd)?;
    if synced.is_empty() {
        eprintln!("already on the merge head");
    } else {
        eprintln!("synced onto the merge head: {}", synced.join(", "));
    }
    Ok(())
}

pub fn run_status(args: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    if args.first().map(String::as_str) == Some("--prompt") {
        print!("{}", status::prompt_token(&cwd));
        return Ok(());
    }
    if !args.is_empty() {
        usage("choros status [--prompt]");
    }
    let config = choros_core::roots::config_home();
    let report = status::report(&cwd, config.as_deref())?;
    if report.is_empty() {
        println!("no landing queues here");
        return Ok(());
    }
    for q in report {
        println!("{} ({}) @ {}", q.name, q.repo, q.root.display());
        if let Some(p) = &q.merge_head_park {
            println!("  ! merge head parked: conflicts in {}", p.files.join(", "));
        }
        for s in &q.sets {
            if s.state == "merged" {
                continue;
            }
            let mut line = format!("  {:<8} {}", s.state, s.branch);
            if !s.deps.is_empty() {
                line.push_str(&format!("  after {}", s.deps.join(", ")));
            }
            if let Some(pr) = &s.pr {
                line.push_str(&format!("  {pr}"));
            }
            if let Some(p) = &s.park {
                line.push_str(&format!("  ! parked ({:?}) {}", p.op, p.files.join(", ")));
            } else if s.blocked {
                line.push_str("  (waiting on a parked or closed set)");
            }
            if !s.conflicts_with.is_empty() {
                let with: Vec<String> = s.conflicts_with.iter().map(|n| format!("landing-{n}")).collect();
                line.push_str(&format!("  clashes with {}", with.join(", ")));
            }
            if s.landing_dirty {
                line.push_str("  (landing folder has uncommitted changes)");
            }
            println!("{line}");
        }
        let merged = q.sets.iter().filter(|s| s.state == "merged").count();
        if merged > 0 {
            println!("  ({merged} merged)");
        }
        for (ws, repo) in &q.needs_sync {
            println!("  ⇣ {} ({repo}) needs `choros sync`", ws.display());
        }
    }
    Ok(())
}

pub fn run_queue(args: Vec<String>) -> Result<()> {
    let root = root_here()?;
    let tools = Tools::for_root(&root);
    let env = tools.env();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] => crate::queue_tui::run(&root, None, &env),
        ["new", name, repo, rest @ ..] => {
            let scope = match rest {
                [] => Scope::Root,
                ["--workspace", ws] => Scope::Workspace(ws.to_string()),
                _ => usage("choros queue new NAME REPO [--workspace WS]"),
            };
            let q = Queue::create(&root, name, repo, scope)?;
            warn_push_workflows(&q);
            eprintln!("created queue {name} for {repo}; bind a workspace with `choros queue bind {repo} {name}`");
            Ok(())
        }
        ["bind", repo, queue] => {
            let cwd = std::env::current_dir()?;
            let (_, ws) = choros::find_workspace(&cwd)?;
            let queue = (*queue != "none").then_some(*queue);
            land::bind(&env, &ws, repo, queue)?;
            match queue {
                Some(q) => eprintln!("{repo} now lands into {q}"),
                None => eprintln!("{repo} now lands straight to main"),
            }
            Ok(())
        }
        ["sync", rest @ ..] => {
            let names = match rest {
                [] => queue::list(&root)?,
                [name] => vec![name.to_string()],
                _ => usage("choros queue sync [NAME]"),
            };
            for name in names {
                let report = admin::sync(&env, &Queue::open(&root, &name)?, None)?;
                print_report(&name, &report);
            }
            Ok(())
        }
        ["resolve", name, rest @ ..] => {
            let preset = match rest {
                [] => None,
                ["--agent", "trust"] => Some('a'),
                ["--agent", "review"] => Some('b'),
                ["--agent", "manual"] => Some('c'),
                _ => usage("choros queue resolve NAME [--agent trust|review|manual]"),
            };
            if preset.is_none() && !interactive() {
                return Err(eyre!("no terminal to ask in; pass --agent trust|review|manual"));
            }
            let mut handler = Interactive { preset };
            let report = admin::sync(&env, &Queue::open(&root, name)?, Some(&mut handler))?;
            print_report(name, &report);
            Ok(())
        }
        ["drop", name, n] => {
            let n: u32 = n.trim_start_matches("landing-").parse()?;
            let report = admin::drop_set(&env, &Queue::open(&root, name)?, n)?;
            print_report(name, &report);
            Ok(())
        }
        ["doctor", name] => {
            for note in admin::doctor(&env, &Queue::open(&root, name)?)? {
                println!("{note}");
            }
            Ok(())
        }
        [name] => crate::queue_tui::run(&root, Some(name), &env),
        _ => usage(
            "choros queue [NAME] | queue new NAME REPO [--workspace WS] | queue bind REPO QUEUE|none \
             | queue sync [NAME] | queue resolve NAME | queue drop NAME N | queue doctor NAME",
        ),
    }
}

fn print_report(name: &str, r: &PassReport) {
    let list = |v: &[u32]| v.iter().map(|n| format!("landing-{n}")).collect::<Vec<_>>().join(", ");
    for (what, v) in [
        ("merged", &r.merged),
        ("closed", &r.closed),
        ("re-synced", &r.resynced),
        ("took main in", &r.updated_public),
        ("opened PRs for", &r.opened),
        ("parked", &r.parked),
    ] {
        if !v.is_empty() {
            eprintln!("{name}: {what} {}", list(v));
        }
    }
    if r.merge_head_parked {
        eprintln!("{name}: merge head parked; run `choros queue resolve {name}`");
    }
}

/// Pushing every private branch runs `on: push` workflows on each re-sync.
fn warn_push_workflows(q: &Queue) {
    let workflows = q.sync_dir().join(".github/workflows");
    let Ok(entries) = std::fs::read_dir(workflows) else { return };
    let on_push = entries.flatten().any(|e| {
        std::fs::read_to_string(e.path())
            .map(|s| s.lines().any(|l| l.trim_start().starts_with("push")))
            .unwrap_or(false)
    });
    if on_push {
        eprintln!(
            "note: {} has `on: push` workflows; they will run on every re-sync of a queued branch. \
             Set [push] private_sets = false in its queue.toml to keep queued branches local.",
            q.config.repo
        );
    }
}

pub fn run_landing(args: Vec<String>) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let (n, rest) = match a.as_slice() {
        ["open", n, rest @ ..] => (n.trim_start_matches("landing-").parse::<u32>()?, rest),
        _ => usage("choros landing open N [--queue Q] [--session ID]"),
    };
    let root = root_here()?;
    let mut queue = None;
    let mut session = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i] {
            "--queue" => queue = rest.get(i + 1).map(|s| s.to_string()),
            "--session" => session = rest.get(i + 1).map(|s| s.to_string()),
            other => return Err(eyre!("unknown option {other}")),
        }
        i += 2;
    }
    let queue = match queue {
        Some(q) => q,
        None => match queue::list(&root)?.as_slice() {
            [only] => only.clone(),
            _ => return Err(eyre!("which queue? pass --queue")),
        },
    };
    let q = Queue::open(&root, &queue)?;
    let (dir, fork) = admin::landing_session(&q, n, session.as_deref())?;
    let mut cmd = Command::new("claude");
    cmd.current_dir(&dir);
    if let Some(f) = &fork {
        cmd.args(["--resume", f]);
    }
    eprintln!("opening {} {}", dir.display(), fork.as_deref().unwrap_or("(new session)"));
    let status = cmd.status()?;
    if !status.success() {
        return Err(eyre!("claude exited with {status}"));
    }
    Ok(())
}

/// Asks on every conflict, as a/b/c — unless a choice was given up front.
#[derive(Default)]
struct Interactive {
    preset: Option<char>,
}

fn ask(prompt: &str) -> Result<String> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

impl ConflictHandler for Interactive {
    fn resolve(&mut self, c: &Conflict) -> Result<Handled> {
        eprintln!("\nconflict in {} while {}:", c.queue, c.operation);
        for f in &c.files {
            eprintln!("    {f}");
        }
        if !c.recorded.is_empty() {
            eprintln!("({} earlier resolution(s) of these files will be shown to the agent)", c.recorded.len());
        }
        let choice = match self.preset {
            Some(p) => p,
            None => ask(
                "  a) trust the agent and push\n  b) agent resolves, show me the diff first\n  c) I'll drive (opens Claude Code here)\n  s) skip: leave it parked\n> ",
            )?
            .chars()
            .next()
            .unwrap_or('s'),
        };
        let brief = || ConflictBrief {
            dir: c.dir,
            operation: &c.operation,
            files: &c.files,
            recorded: &c.recorded,
            session: c.slot,
        };
        match choice {
            'a' => {
                c.agent.resolve(&brief())?;
                Ok(Handled::Resolved { used_session: true })
            }
            'b' => {
                c.agent.resolve(&brief())?;
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(c.dir)
                    .args(["--no-pager", "diff", "--cached"])
                    .status();
                match ask("push this resolution? [y] push / [i] intervene / [s] skip > ")?.as_str() {
                    "y" | "" => Ok(Handled::Resolved { used_session: true }),
                    "i" => drive(c).map(|_| Handled::Resolved { used_session: true }),
                    _ => Ok(Handled::Declined),
                }
            }
            'c' => drive(c).map(|_| Handled::Resolved { used_session: true }),
            _ => Ok(Handled::Declined),
        }
    }

    fn fork_conflict(&mut self, fork: u32, with: &[u32]) -> Result<Option<OnConflict>> {
        let with: Vec<String> = with.iter().map(|n| format!("landing-{n}")).collect();
        eprintln!("landing-{fork} clashes with {} in the merge head.", with.join(", "));
        Ok(match ask("  d) land behind them\n  m) stay beside them; resolve in the merge head\n  c) cancel\n> ")?.as_str() {
            "d" => Some(OnConflict::Depend),
            "m" => Some(OnConflict::MergeHead),
            _ => None,
        })
    }
}

/// Open Claude Code on the resync agent's session in the sync clone, with the
/// conflict in progress, and let the user drive.
fn drive(c: &Conflict) -> Result<()> {
    let mut cmd = Command::new("claude");
    cmd.current_dir(c.dir);
    if c.slot.fresh {
        cmd.args(["--session-id", &c.slot.id]);
    } else {
        cmd.args(["--resume", &c.slot.id]);
    }
    cmd.arg(format!(
        "A conflict is in progress here while {}. Help me resolve it; stage the result \
         but do not commit, continue or push — Choros finishes the operation.",
        c.operation
    ));
    cmd.status()?;
    Ok(())
}

/// Bind a just-created workspace's repo to a queue (the `work` picker's `b`).
pub fn bind_new(ws: &Path, repo: &str, queue: &str) -> Result<()> {
    let root = choros::find_root(ws).ok_or_else(|| eyre!("{ws:?} is not under a choros root"))?;
    let tools = Tools::for_root(&root);
    land::bind(&tools.env(), ws, repo, Some(queue))
}
