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
use choros_land::state::{Dep, LandMode, PrSpec, QueueConfig, Scope};
use choros_land::{admin, diffbase, status};
use color_eyre::eyre::{Result, eyre};

use crate::choros;
use choros_core::space;

/// What the engine needs, owned, so an `Env` can borrow it.
struct Tools {
    gh: Gh,
    claude: Claude,
}

impl Tools {
    fn for_trunk(trunk: &Path) -> Tools {
        let gh_dir = trunk.join(choros::CONFIG_DIR).join("gh");
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

fn trunk_here() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    space::find_trunk(&cwd).ok_or_else(|| eyre!("not under a choros trunk (no .choros-config above here)"))
}

/// The queues `choros queue` and `landing open` offer from here, narrowed
/// like `choros status`.
fn visible_here(trunk: &Path) -> Result<Vec<String>> {
    let cwd = std::env::current_dir()?;
    Ok(status::visible_queues(&cwd, None, false)?
        .into_iter()
        .filter(|(t, _)| t == trunk)
        .map(|(_, q)| q)
        .collect())
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
    let (trunk, ws) = choros::find_workspace(&cwd)?;
    let fork = if fork {
        Some(match deps {
            Some(d) => d,
            None if interactive() => pick_fork_deps(&trunk, &ws, &cwd)?,
            None => return Err(eyre!("--fork without a terminal needs --deps (SET,… or none)")),
        })
    } else {
        None
    };

    let tools = Tools::for_trunk(&trunk);
    let mut handler = Interactive::default();
    let req = LandRequest {
        cwd,
        mode,
        fork,
        on_conflict,
        title,
        body,
        sessions,
        env_session: std::env::var("CLAUDE_CODE_SESSION_ID").ok(),
    };
    let handler: Option<&mut (dyn ConflictHandler + 'static)> =
        if interactive() { Some(&mut handler) } else { None };
    let out = land::land(&tools.env(), &req, handler)?;
    for l in &out.landed {
        match (&l.queue, l.n) {
            (Some(q), Some(n)) => {
                let st = Queue::open(&trunk, q)?.read_state()?;
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

fn pick_fork_deps(trunk: &Path, ws: &Path, cwd: &Path) -> Result<Vec<Dep>> {
    let repo = cwd
        .strip_prefix(ws)
        .ok()
        .and_then(|r| r.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .map(str::to_string);
    let repos = match repo {
        Some(r) => vec![r],
        None => choros::read_meta(ws)?.repos,
    };
    let mut queue = None;
    for r in &repos {
        if let Some(q) = choros_land::workspace::queue_for(trunk, ws, r)? {
            queue = Some(q);
            break;
        }
    }
    let queue = queue.ok_or_else(|| eyre!("no repo here lands into a queue"))?;
    let q = Queue::open(trunk, &queue)?;
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
        usage("choros repo sync");
    }
    let cwd = std::env::current_dir()?;
    let trunk = trunk_here()?;
    let tools = Tools::for_trunk(&trunk);
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
    let all = match args.as_slice() {
        [] => false,
        [a] if a == "--all" => true,
        _ => usage("choros status [--prompt | --all]"),
    };
    let config = choros_core::trunks::config_home();
    let report = status::report(&cwd, config.as_deref(), all)?;
    if report.is_empty() {
        println!("no landing queues here");
        return Ok(());
    }
    for q in report {
        let repos = if q.repos.is_empty() { "nothing landed yet".to_string() } else { q.repos.join(", ") };
        println!("{} ({repos}) @ {}", q.name, q.trunk.display());
        for s in &q.sets {
            if s.state == "merged" {
                continue;
            }
            let mut line = format!("  {:<8} {} ({})", s.state, s.branch, s.repo);
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
        print_diffbase(&q);
        for (ws, repo) in &q.needs_sync {
            println!("  ⇣ {} ({repo}) needs `choros repo sync`", ws.display());
        }
    }
    Ok(())
}

pub fn run_queue(args: Vec<String>) -> Result<()> {
    let trunk = trunk_here()?;
    let tools = Tools::for_trunk(&trunk);
    let env = tools.env();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        [] => crate::queue_tui::run(&trunk, None, &visible_here(&trunk)?, &env),
        ["new", rest @ ..] if rest.contains(&"-i") => {
            let rest: Vec<&str> = rest.iter().copied().filter(|a| *a != "-i").collect();
            new_interactive(&env, &trunk, &rest)
        }
        ["new", name, rest @ ..] => {
            let scope = match rest {
                [] => Scope::Trunk,
                [flag, path] => scope_arg(&trunk, flag, path)?,
                _ => usage("choros queue new [-i] NAME [--workspace PATH | --branch PATH]"),
            };
            let q = Queue::create(&trunk, name, QueueConfig::new(scope))?;
            created(&trunk, &q);
            Ok(())
        }
        ["ls", rest @ ..] => {
            let names = match rest {
                [] => visible_here(&trunk)?,
                ["--all"] => queue::list(&trunk)?,
                _ => usage("choros queue ls [--all]"),
            };
            print_queues(&trunk, &names)
        }
        ["bind", repo, queue] => run_repo_bind(&env, repo, queue),
        ["base", "add", name, pr] => {
            let q = Queue::open(&trunk, name)?;
            let report = diffbase::add(&env, &q, pr)?;
            print_report(name, &report);
            let status = status::queue_status(&trunk, name)?;
            print_diffbase(&status);
            let conflict = status
                .diffbase_park
                .as_ref()
                .is_some_and(|p| p.conflict.is_some());
            if conflict
                && interactive()
                && !ask("resolve it now? [Y/n] ")?
                    .trim()
                    .eq_ignore_ascii_case("n")
            {
                let mut handler = Interactive { preset: None };
                let report = admin::sync(&env, &Queue::open(&trunk, name)?, Some(&mut handler))?;
                print_report(name, &report);
                print_diffbase(&status::queue_status(&trunk, name)?);
            }
            Ok(())
        }
        ["base", "rm", name, pr] => {
            let report = diffbase::remove(&env, &Queue::open(&trunk, name)?, pr)?;
            print_report(name, &report);
            print_diffbase(&status::queue_status(&trunk, name)?);
            Ok(())
        }
        ["base", name] => {
            let status = status::queue_status(&trunk, name)?;
            if status.diffbase.is_empty() {
                println!("{name} builds on each repo's main");
            }
            print_diffbase(&status);
            Ok(())
        }
        ["sync", rest @ ..] => {
            let names = match rest {
                [] => queue::list(&trunk)?,
                [name] => vec![name.to_string()],
                _ => usage("choros queue sync [NAME]"),
            };
            for name in names {
                let report = admin::sync(&env, &Queue::open(&trunk, &name)?, None)?;
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
            let report = admin::sync(&env, &Queue::open(&trunk, name)?, Some(&mut handler))?;
            print_report(name, &report);
            Ok(())
        }
        ["drop", name, n] => {
            let n: u32 = n.trim_start_matches("landing-").parse()?;
            let report = admin::drop_set(&env, &Queue::open(&trunk, name)?, n)?;
            print_report(name, &report);
            Ok(())
        }
        ["doctor", name] => {
            for note in admin::doctor(&env, &Queue::open(&trunk, name)?)? {
                println!("{note}");
            }
            Ok(())
        }
        [name] => crate::queue_tui::run(&trunk, Some(name), &[], &env),
        _ => usage(
            "choros queue [NAME] | queue ls [--all] \
             | queue new [-i] NAME [--workspace PATH | --branch PATH] | queue sync [NAME] | queue resolve NAME | queue drop NAME N | queue doctor NAME \
             | queue base NAME | queue base add|rm NAME REPO#N",
        ),
    }
}

/// `--workspace PATH` or `--branch PATH`, checked against the trunk.
fn scope_arg(trunk: &Path, flag: &str, path: &str) -> Result<Scope> {
    let path = path.trim_end_matches('/');
    let below = Path::new(path)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    match flag.trim_start_matches("--") {
        "workspace" if below && trunk.join(path).join(choros::META_FILE).exists() => {
            Ok(Scope::Workspace(path.to_string()))
        }
        "branch" if below && space::is_branch_space(&trunk.join(path)) => Ok(Scope::Branch(path.to_string())),
        "workspace" => Err(eyre!("no workspace {path:?} in {}; give its path below the trunk", trunk.display())),
        "branch" => Err(eyre!("no branch-space {path:?} in {}; give its path below the trunk", trunk.display())),
        _ => usage("choros queue new [-i] NAME [--workspace PATH | --branch PATH]"),
    }
}

fn scope_text(scope: &Scope) -> String {
    match scope {
        Scope::Trunk => "trunk".into(),
        Scope::Workspace(p) => format!("workspace {p}"),
        Scope::Branch(p) => format!("branch {p}"),
    }
}

/// `queue new -i`: a question per setting, its default in parens. What the
/// command line gave is the default; otherwise the scope is the branch-space
/// you're in.
fn new_interactive(env: &Env, trunk: &Path, args: &[&str]) -> Result<()> {
    if !interactive() {
        return Err(eyre!("no terminal to ask in; drop -i"));
    }
    let (given, scope) = match args {
        [rest @ .., flag, path] if flag.starts_with("--") => (rest, Some(scope_arg(trunk, flag, path)?)),
        rest if rest.len() <= 1 => (rest, None),
        _ => usage("choros queue new -i [NAME] [--workspace PATH | --branch PATH]"),
    };
    let scope = match scope {
        Some(s) => s,
        None => match space::find_space(&std::env::current_dir()?) {
            Some(here) if space::is_branch_space(&here) => Scope::Branch(space::rel(trunk, &here)?),
            _ => Scope::Trunk,
        },
    };
    let name = question("queue name", given.first().copied(), |a| {
        Queue::check_name(trunk, a).map(|_| a.to_string())
    })?;
    let scope = question(
        "scope: trunk, branch PATH or workspace PATH",
        Some(&scope_text(&scope)),
        |a| {
            let scope = match a.split_once(' ') {
                _ if a == "trunk" => Scope::Trunk,
                Some((flag, path)) => scope_arg(trunk, flag, path.trim())?,
                None => return Err(eyre!("answer trunk, branch PATH or workspace PATH")),
            };
            Queue::check_scope(trunk, &scope).map(|_| scope)
        },
    )?;
    let mut config = QueueConfig::new(scope);
    config.push.merge_head = yes_no("push the merge head to origin", config.push.merge_head)?;
    config.push.private_sets = yes_no(
        "push landing branches before they reach the front",
        config.push.private_sets,
    )?;
    config.pr.draft = yes_no("open PRs as drafts", config.pr.draft)?;
    config.agent.rotate_after = question(
        "resync-agent runs before it starts a fresh session",
        Some(&config.agent.rotate_after.to_string()),
        |a| a.parse().map_err(|_| eyre!("answer a number")),
    )?;
    let prs = question("PRs to build on until they merge, as repo#N", Some("none"), |a| {
        a.split([' ', ','])
            .filter(|s| !s.is_empty() && *s != "none")
            .map(|s| PrSpec::parse(s, None).map(|p| p.to_string()))
            .collect::<Result<Vec<_>>>()
    })?;

    let q = Queue::create(trunk, &name, config)?;
    created(trunk, &q);
    for pr in prs {
        match diffbase::add(env, &Queue::open(trunk, &name)?, &pr) {
            Ok(report) => print_report(&name, &report),
            Err(e) => eprintln!("not building on {pr}: {e}; add it later with `choros queue base add {name} {pr}`"),
        }
    }
    print_diffbase(&status::queue_status(trunk, &name)?);
    Ok(())
}

/// Say what a new queue takes, and what pushing its branches will set off.
fn created(trunk: &Path, q: &Queue) {
    let name = &q.name;
    match &q.config.scope {
        Scope::Trunk => eprintln!("created queue {name}; bind a workspace's repos with `choros repo bind REPO {name}`"),
        scope => eprintln!("created queue {name}; workspaces in {} land into it", scope_text(scope)),
    }
    if !q.config.push.private_sets {
        return;
    }
    let noisy: Vec<String> = choros_core::registry::scan(trunk)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| queue::push_workflows(trunk, r))
        .collect();
    if !noisy.is_empty() {
        eprintln!(
            "note: {} {} `on: push` workflows; they will run on every re-sync of a queued branch. \
             Set [push] private_sets = false in its queue.toml to keep queued branches local.",
            noisy.join(", "),
            if noisy.len() == 1 { "has" } else { "have" }
        );
    }
}

/// Ask until `parse` takes the answer; an empty one takes `default`.
fn question<T>(prompt: &str, default: Option<&str>, parse: impl Fn(&str) -> Result<T>) -> Result<T> {
    let shown = default.map(|d| format!(" ({d})")).unwrap_or_default();
    loop {
        let answer = read_answer(&format!("{prompt}{shown}: "))?
            .ok_or_else(|| eyre!("no answer to {prompt:?}"))?;
        let answer = match (answer.as_str(), default) {
            ("", Some(d)) => d,
            ("", None) => continue,
            (a, _) => a,
        };
        match parse(answer) {
            Ok(v) => return Ok(v),
            Err(e) => eprintln!("  {e}"),
        }
    }
}

fn yes_no(prompt: &str, default: bool) -> Result<bool> {
    question(&format!("{prompt}?"), Some(if default { "yes" } else { "no" }), |a| {
        match a.to_lowercase().as_str() {
            "y" | "yes" => Ok(true),
            "n" | "no" => Ok(false),
            _ => Err(eyre!("answer yes or no")),
        }
    })
}

/// One line per queue: its scope, repos, open sets and diffbase.
fn print_queues(trunk: &Path, names: &[String]) -> Result<()> {
    if names.is_empty() {
        println!("no landing queues here");
        return Ok(());
    }
    let mut rows = vec![["QUEUE", "SCOPE", "REPOS", "OPEN", "BUILDS ON"].map(String::from)];
    let or_dash = |v: Vec<String>| if v.is_empty() { "-".to_string() } else { v.join(", ") };
    for name in names {
        let q = Queue::open(trunk, name)?;
        let st = q.read_state()?;
        let open = st.sets.values().filter(|s| s.is_open()).count();
        rows.push([
            name.clone(),
            scope_text(&q.config.scope),
            or_dash(st.repos.keys().cloned().collect()),
            open.to_string(),
            or_dash(q.config.diffbase.clone()),
        ]);
    }
    let widths: Vec<usize> = (0..5).map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0)).collect();
    for row in &rows {
        let cells: Vec<String> = row.iter().zip(&widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
        println!("{}", cells.join("  ").trim_end());
    }
    Ok(())
}

/// The PRs a queue builds on, and why it's parked on them if it is.
fn print_diffbase(q: &status::QueueStatus) {
    if q.diffbase.is_empty() {
        return;
    }
    let prs: Vec<String> = q
        .diffbase
        .iter()
        .map(|d| format!("{} ({})", d.pr, d.state))
        .collect();
    println!("  diffbase: {}", prs.join(", "));
    match &q.diffbase_park {
        Some(p) => match &p.conflict {
            Some(c) => {
                let mut onto = vec!["main".to_string()];
                onto.extend(c.onto.iter().cloned());
                println!(
                    "  ! parked: {} conflicts with {} in {}; `choros queue resolve {}`",
                    p.pr,
                    onto.join(" + "),
                    c.files.join(", "),
                    q.name
                );
            }
            None => println!(
                "  ! parked: {} was closed without merging; reopen it, or `choros queue base rm {} {}`",
                p.pr, q.name, p.pr
            ),
        },
        None if q.diffbase.iter().any(|d| d.state != "merged") => {
            println!("  (no PR opens here until these merge)");
        }
        None => {}
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
    if r.diffbase_parked {
        eprintln!("{name}: parked on its diffbase; nothing else moved");
    }
    for e in &r.errors {
        eprintln!("{name}: {e}");
    }
}

pub fn run_landing(args: Vec<String>) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    let (n, rest) = match a.as_slice() {
        ["open", n, rest @ ..] => (n.trim_start_matches("landing-").parse::<u32>()?, rest),
        _ => usage("choros landing open N [--queue Q] [--session ID]"),
    };
    let trunk = trunk_here()?;
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
        None => match visible_here(&trunk)?.as_slice() {
            [only] => only.clone(),
            _ => return Err(eyre!("which queue? pass --queue")),
        },
    };
    let q = Queue::open(&trunk, &queue)?;
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
    Ok(read_answer(prompt)?.unwrap_or_default().to_lowercase())
}

/// The trimmed line typed after `prompt`; `None` once stdin has closed.
fn read_answer(prompt: &str) -> Result<Option<String>> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
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

/// `choros repo bind REPO QUEUE|none`: where this workspace lands `repo`.
pub fn run_repo_bind(env: &Env, repo: &str, queue: &str) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (_, ws) = choros::find_workspace(&cwd)?;
    let queue = (queue != "none").then_some(queue);
    land::bind(env, &ws, repo, queue)?;
    match queue {
        Some(q) => eprintln!("{repo} now lands into {q}"),
        None => eprintln!("{repo} now lands straight to main"),
    }
    Ok(())
}

pub fn run_repo_bind_here(repo: &str, queue: &str) -> Result<()> {
    let trunk = trunk_here()?;
    let tools = Tools::for_trunk(&trunk);
    run_repo_bind(&tools.env(), repo, queue)
}

/// Bind a just-created workspace's repo to a queue (the `work` picker's `b`,
/// or its branch-space's default).
pub fn bind_new(ws: &Path, repo: &str, queue: Option<&str>) -> Result<()> {
    let trunk = space::find_trunk(ws).ok_or_else(|| eyre!("{ws:?} is not under a choros trunk"))?;
    let tools = Tools::for_trunk(&trunk);
    land::bind(&tools.env(), ws, repo, queue)
}
