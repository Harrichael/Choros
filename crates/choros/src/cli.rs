use std::io::IsTerminal;
use std::path::PathBuf;

use choros_core::space;
use color_eyre::eyre::eyre;
use color_eyre::Result;

use crate::agent;
use crate::app::{self, NewFocus};
use crate::choros::{self, ProgressSink};
use crate::{git, registry};

const SHELL_INIT: &str = r#"# choros shell integration — works in bash and zsh
choros() {
  local target
  case "$1" in
    work)
      target="$(command choros "$@")" || return $?
      [ -n "$target" ] && cd "$target"
      ;;
    branch)
      target="$(command choros "$@")" || return $?
      case "$2" in
        archive)
          [ -n "$target" ] && printf '%s\n' "$target"
          __choros_leave "$target"
          ;;
        bind) ;;
        *) [ -n "$target" ] && cd "$target" ;;
      esac
      ;;
    archive|land)
      target="$(command choros "$@")" || return $?
      [ -n "$target" ] && printf '%s\n' "$target"
      __choros_leave "$target"
      ;;
    *)
      command choros "$@"
      ;;
  esac
}

# Archiving the folder you stand in moves it out from under the shell; go to
# the nearest space above it that still exists instead.
__choros_leave() {
  [ -d "$PWD" ] && return
  [ -n "$1" ] || return
  local trunk="${1%/.choros-config/archive/*}"
  local dir="$trunk/$(dirname "${1#*/.choros-config/archive/}")"
  while [ ! -d "$dir" ] && [ "$dir" != "$trunk" ]; do dir="$(dirname "$dir")"; done
  cd "$dir"
}
"#;

const USAGE: &str = "\
choros — multi-repo work-environment manager

A trunk is the folder `choros init` ran in. Branch-spaces are folders in it that
group workspaces (and further branch-spaces); workspaces live in either.

SETUP (once per machine or trunk)
    choros init                     make this folder a trunk: .choros-config/ with its registry
                                    and templates, registered for chorosd
    choros add <ssh-url>            clone a repo into the trunk's registry (ssh URLs only)
    choros shell-init               shell function for `eval \"$(choros shell-init)\"`, so work,
                                    branch and archive can move your shell

SPACES (in the trunk or a branch-space; acts where you are)
    choros work [NAME [REPO…]]      create a workspace here: name + repo picker (b picks a repo's
                                    queue, defaulting to the branch-space's); with REPOs, no picker
    choros branch NAME [--no-pick]  create a branch-space here; the picker sets which queue from
                                    further up its new workspaces land into
    choros branch bind [REPO] QUEUE|none
                                    in a branch-space: change that default for every repo, or one
                                    (new workspaces only). A queue scoped to a branch-space needs
                                    none of this: its workspaces land into it
    choros branch archive [NAME] [--force]
                                    archive a branch-space and everything in it; refuses while a
                                    workspace has uncommitted or unlanded work, unless --force
    choros ls [--all]               workspaces here and below, as a tree, most recent first,
                                    with what each space has landing; --all: the whole trunk
    choros archive [NAME]           archive a workspace (to the same path under
                                    .choros-config/archive/); NAME defaults to the one you're in

INSIDE A WORKSPACE
    choros repo ls                  the repos cloned into it
    choros repo add REPO [REPO…]    clone more registry repos in, on its branch
    choros repo sync                rebase unlanded commits onto the queue's current merge head
    choros repo bind REPO QUEUE|none
                                    land REPO into QUEUE (none: straight to main), whatever queue
                                    is scoped to the workspace
    choros land continue|archive    land this repo (or, at the workspace root, each repo as its own
                                    set) into its queue, or straight to main when it has none; `archive` then
                                    archives the workspace. --fork [--deps SET,…|none] lands beside
                                    the queue instead of on top; --title/--body-file set the PR text
    choros landing open N [--queue Q] [--session ID]
                                    open the landing's forked Claude Code session

QUEUES (anywhere in a trunk; narrowed to where you are)
    choros status [--prompt|--all]  landing queues and their sets; --prompt prints one token
                                    (⇣sync / !resolve) for shell prompts
    choros queue [NAME]             TUI: a queue's landing-sets; edit dependencies, drop, sync
    choros queue ls [--all]         each queue: its scope, repos, open sets, diffbase
    choros queue new [-i] NAME [--workspace PATH | --branch PATH]
                                    create a landing queue for every repo, optionally only for one
                                    workspace or branch-space (path below the trunk), whose
                                    workspaces then land into it unasked; -i asks for each
                                    setting, defaults in parens (NAME optional then)
    choros queue sync [NAME]        run the daemon's pass now
    choros queue resolve NAME [--agent trust|review|manual]
                                    retry parked conflicts, asking how to resolve each
    choros queue drop NAME N        remove a queued landing-set
    choros queue doctor NAME        compare the queue's state with origin, then sync
    choros queue base NAME          the PRs the queue builds on before they reach main
    choros queue base add|rm NAME REPO#N
                                    build on a PR in review in its repo, or stop. Nothing in the
                                    queue opens a PR until every listed one merges

AGENTS
    choros agent ls                 resumable claude / cursor sessions under the current directory
    choros agent save settings      TUI: promote this workspace's claude settings to the template

    choros -h | --help              show this message
";

pub fn print_usage() {
    print!("{USAGE}");
}

pub fn emit_shell_init() {
    print!("{SHELL_INIT}");
}

pub fn run_init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let registry_dir = registry::init_dirs(&cwd)?;
    let templates_dir = agent::init_templates(&cwd)?;
    if let Some(config) = choros_core::trunks::config_home() {
        choros_core::trunks::register(&config, &cwd)?;
    }
    eprintln!("initialized {}", registry_dir.display());
    eprintln!("initialized {}", templates_dir.display());
    Ok(())
}

pub fn run_agent(args: Vec<String>) -> Result<()> {
    let mut iter = args.into_iter();
    let verb = iter.next();
    let object = iter.next();
    let tail: Vec<String> = iter.collect();
    match (verb.as_deref(), object.as_deref()) {
        (Some("save"), Some("settings")) => run_agent_save_settings(tail),
        (Some("ls"), None) => run_agent_ls(Vec::new()),
        (Some("ls"), Some(extra)) => {
            let mut all = vec![extra.to_string()];
            all.extend(tail);
            run_agent_ls(all)
        }
        _ => {
            eprintln!("usage: choros agent save settings\n       choros agent ls");
            std::process::exit(2);
        }
    }
}

fn run_agent_ls(args: Vec<String>) -> Result<()> {
    if !args.is_empty() {
        eprintln!(
            "`choros agent ls` takes no arguments, got: {}",
            args.join(" ")
        );
        std::process::exit(2);
    }
    let cwd = std::env::current_dir()?;
    agent::ls::run(&cwd)
}

fn run_agent_save_settings(args: Vec<String>) -> Result<()> {
    if !args.is_empty() {
        eprintln!(
            "`choros agent save settings` takes no arguments, got: {}",
            args.join(" ")
        );
        std::process::exit(2);
    }
    let cwd = std::env::current_dir()?;
    let (trunk, name) = choros::resolve_target(&cwd, None)?;
    let workspace = trunk.join(&name);
    crate::settings_tui::run(workspace, trunk)
}

pub fn run_repo(args: Vec<String>) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["ls"] => {}
        ["add", repos @ ..] if !repos.is_empty() => {}
        ["sync"] => return crate::land_cli::run_sync(Vec::new()),
        ["bind", repo, queue] => return crate::land_cli::run_repo_bind_here(repo, queue),
        _ => {
            eprintln!(
                "usage: choros repo ls | repo add REPO [REPO…] | repo sync | repo bind REPO QUEUE|none"
            );
            std::process::exit(2);
        }
    }
    let cwd = std::env::current_dir()?;
    let (space, name) = choros::resolve_target(&cwd, None)?;
    if a[0] == "add" {
        let repos = &args[1..];
        choros::add_repos(&space, &name, repos, &StderrProgress)?;
        for repo in repos {
            eprintln!("added {repo}");
        }
        return Ok(());
    }
    let meta = choros::read_meta(&space.join(&name))?;
    for repo in meta.repos {
        println!("{repo}");
    }
    Ok(())
}

pub fn run_add(args: Vec<String>) -> Result<()> {
    let mut iter = args.into_iter();
    let url = match (iter.next(), iter.next()) {
        (Some(url), None) => url,
        _ => {
            eprintln!("usage: choros add <ssh-url>");
            std::process::exit(2);
        }
    };
    let cwd = std::env::current_dir()?;
    let (name, path) = registry::add(&cwd, &url)?;
    eprintln!("added {name}");
    println!("{}", path.display());
    Ok(())
}

pub fn run_ls(args: Vec<String>) -> Result<()> {
    let all = match args.as_slice() {
        [] => false,
        [a] if a == "--all" => true,
        _ => {
            eprintln!("usage: choros ls [--all]");
            std::process::exit(2);
        }
    };
    let cwd = std::env::current_dir()?;
    crate::ls::run(&cwd, all)
}

pub fn run_archive(args: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let name_arg = args.into_iter().next();
    let (space, name) = choros::resolve_target(&cwd, name_arg.as_deref())?;
    let dst = choros::archive(&space, &name)?;
    println!("{}", dst.display());
    Ok(())
}

/// The trunk or branch-space a new workspace or branch-space goes in.
fn space_here() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    space::find_space(&cwd)
        .ok_or_else(|| eyre!("not in a choros trunk (no .choros-config above here); run `choros init` first"))
}

pub fn run_work(args: Vec<String>) -> Result<()> {
    let space = space_here()?;
    let mut iter = args.into_iter();
    let name = iter.next();
    let repos: Vec<String> = iter.collect();
    if let (Some(name), false) = (&name, repos.is_empty()) {
        return run_work_noninteractive(space, name.clone(), repos);
    }
    let focus = if name.is_some() {
        NewFocus::Repos
    } else {
        NewFocus::Name
    };
    let mut app = app::App::new(space, name, focus);
    run_tui(&mut app)?;
    if let Some(path) = app.created_path {
        println!("{}", path.display());
    }
    Ok(())
}

fn run_work_noninteractive(space: PathBuf, name: String, repos: Vec<String>) -> Result<()> {
    let trunk = space::find_trunk(&space).ok_or_else(|| eyre!("{space:?} is not under a trunk"))?;
    choros::validate_name(&space, &name)?;
    for r in &repos {
        registry::ensure_repo_exists(&trunk, r)?;
    }
    let progress = StderrProgress;
    let info = choros::create(&space, &name, &repos, &progress)?;
    let scoped = choros_land::workspace::space_queue(&trunk, &space::rel(&trunk, &space)?)?;
    for repo in &repos {
        let queue = match choros_land::branch::default_binding(&space, repo)? {
            Some(queue) => queue,
            None => scoped.clone(),
        };
        match &queue {
            Some(q) => progress.status(format!("{repo} lands into {q}; moving it onto the merge head…")),
            // Straight to main where a queue is scoped here has to be said outright.
            None if scoped.is_none() => continue,
            None => {}
        }
        crate::land_cli::bind_new(&info.path, repo, queue.as_deref())?;
    }
    println!("{}", info.cd_target().display());
    Ok(())
}

pub fn run_branch(args: Vec<String>) -> Result<()> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["bind", rest @ ..] if matches!(rest.len(), 1 | 2) => {
            let here = space_here()?;
            let (repo, queue) = match rest {
                [queue] => (None, *queue),
                [repo, queue] => (Some(*repo), *queue),
                _ => unreachable!(),
            };
            let queue = (queue != "none").then_some(queue);
            choros_land::branch::set_binding(&here, repo, queue)?;
            let what = repo.unwrap_or("every repo");
            match queue {
                Some(q) => eprintln!("new workspaces here land {what} into {q}"),
                None => eprintln!("new workspaces here land {what} straight to main"),
            }
            Ok(())
        }
        ["archive", rest @ ..] => {
            let (name, force) = match rest {
                [] => (None, false),
                ["--force"] => (None, true),
                [n] => (Some(*n), false),
                [n, "--force"] | ["--force", n] => (Some(*n), true),
                _ => usage_branch(),
            };
            let here = space_here()?;
            let dir = match name {
                Some(n) => here.join(n),
                None => here,
            };
            if !space::is_branch_space(&dir) {
                return Err(eyre!("{} is not a branch-space", dir.display()));
            }
            if !force {
                let unfinished = unfinished_work(&dir)?;
                if !unfinished.is_empty() {
                    return Err(eyre!(
                        "work that would be shelved unlanded:\n  {}\nland or commit it first, or pass --force",
                        unfinished.join("\n  ")
                    ));
                }
            }
            let dst = space::archive_branch(&dir)?;
            println!("{}", dst.display());
            Ok(())
        }
        [name] | [name, "--no-pick"] => {
            let parent = space_here()?;
            let dir = space::create_branch(&parent, name)?;
            if a.len() == 1 && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
                pick_branch_defaults(&dir)?;
            }
            eprintln!("created branch-space {}", dir.display());
            println!("{}", dir.display());
            Ok(())
        }
        _ => usage_branch(),
    }
}

fn usage_branch() -> ! {
    eprintln!("usage: choros branch NAME [--no-pick] | branch bind [REPO] QUEUE|none | branch archive [NAME] [--force]");
    std::process::exit(2);
}

/// Every workspace under `dir` with uncommitted changes or unlanded commits,
/// one line each.
fn unfinished_work(dir: &std::path::Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for ws in space::tree(dir)?.all_workspaces() {
        let label = ws.path.strip_prefix(dir).unwrap_or(&ws.path).display().to_string();
        let changed: usize = ws
            .meta
            .repos
            .iter()
            .map(|r| git::changed_count(&ws.path.join(r)).unwrap_or(0))
            .sum();
        if changed > 0 {
            out.push(format!("{label}: {changed} uncommitted change(s)"));
        }
        match choros_land::land::unlanded(&ws.path) {
            Ok(repos) => {
                for (repo, n) in repos {
                    out.push(format!("{label}/{repo}: {n} unlanded commit(s)"));
                }
            }
            Err(e) => out.push(format!("{label}: couldn't tell what's unlanded ({e})")),
        }
    }
    Ok(out)
}

/// Let the new branch-space choose a queue from further up for its
/// workspaces to land into. A queue scoped to it later needs no choosing.
fn pick_branch_defaults(dir: &std::path::Path) -> Result<()> {
    let trunk = space::find_trunk(dir).ok_or_else(|| eyre!("{dir:?} is not under a trunk"))?;
    let rel = space::rel(&trunk, dir)?;
    let mut options: Vec<String> = Vec::new();
    for name in choros_land::queue::list(&trunk)? {
        if choros_land::queue::Queue::open(&trunk, &name)?.config.scope.admits_space(&rel) {
            options.push(name);
        }
    }
    if options.is_empty() {
        return Ok(());
    }
    let Some(chosen) = crate::queue_tui::pick(
        " default queue for new workspaces here (Enter with none: decide later) ",
        &options,
        &[],
    )?
    else {
        return Ok(());
    };
    match chosen.as_slice() {
        [] => Ok(()),
        [i] => choros_land::branch::set_binding(dir, None, Some(&options[*i])),
        _ => Err(eyre!("pick one queue; set others per repo with `choros branch bind REPO QUEUE`")),
    }
}

struct StderrProgress;

impl ProgressSink for StderrProgress {
    fn status(&self, msg: String) {
        eprintln!("{msg}");
    }
}

fn run_tui(app: &mut app::App) -> Result<()> {
    crate::tui::with_tui(|terminal| app::run(terminal, app))
}
