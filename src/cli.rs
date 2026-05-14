use std::fs::OpenOptions;
use std::path::PathBuf;

use color_eyre::eyre::{Context, Result};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::agent_ls;
use crate::agent_settings_tui;
use crate::app::{self, NewFocus};
use crate::choros::{self, ProgressSink};
use crate::registry;

const SHELL_INIT: &str = r#"# choros shell integration — works in bash and zsh
choros() {
  if [ "$1" = "work" ]; then
    local target
    target="$(command choros "$@")" || return $?
    [ -n "$target" ] && cd "$target"
  else
    command choros "$@"
  fi
}
"#;

const USAGE: &str = "\
choros — multi-repo work-environment manager

USAGE:
    choros                          launch the full TUI in the current directory
    choros work                     fast-create: name + repo multi-select, exits on create
    choros work NAME                fast-create with name pre-filled
    choros work NAME REPO [REPO…]   non-interactive create
                                    (detects Rust / JS toolchains in cloned repos and wires up
                                    a shared build cache at .choros-config/store/)
    choros archive [NAME]           archive a workspace (move it under .choros-config/archive/);
                                    NAME defaults to the workspace containing the current directory
    choros agent save settings      open a TUI in the current workspace; diff workspace claude
                                    settings against the template and promote selected entries
    choros agent ls                 list resumable claude / cursor sessions recorded under the
                                    current directory (matches the cwd of `claude --resume`)
    choros init                     create .choros-config/ and .choros-config/registry/ in the current directory
    choros shell-init               emit shell-integration function for `eval \"$(choros shell-init)\"`
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
    let templates_dir = choros::init_templates(&cwd)?;
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
    agent_ls::run(&cwd)
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
    let (root, name) = choros::resolve_target(&cwd, None)?;
    let workspace = root.join(&name);
    agent_settings_tui::run(workspace, root)
}

pub fn run_archive(args: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let name_arg = args.into_iter().next();
    let (root, name) = choros::resolve_target(&cwd, name_arg.as_deref())?;
    let dst = choros::archive(&root, &name)?;
    if cwd.starts_with(root.join(&name)) {
        eprintln!(
            "warning: archived workspace contained your current directory; cd somewhere else"
        );
    }
    println!("{}", dst.display());
    Ok(())
}

pub fn run_full_tui() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let mut app = app::App::new(cwd);
    app.refresh();
    run_tui(&mut app)?;
    Ok(())
}

pub fn run_work(args: Vec<String>) -> Result<()> {
    let cwd = std::env::current_dir()?;
    match args.len() {
        0 => {
            let mut app = app::App::new_work(cwd, None, NewFocus::Name);
            run_tui(&mut app)?;
            if let Some(path) = app.created_path {
                println!("{}", path.display());
            }
            Ok(())
        }
        1 => {
            let name = args.into_iter().next().unwrap();
            let mut app = app::App::new_work(cwd, Some(name), NewFocus::Repos);
            run_tui(&mut app)?;
            if let Some(path) = app.created_path {
                println!("{}", path.display());
            }
            Ok(())
        }
        _ => {
            let mut iter = args.into_iter();
            let name = iter.next().unwrap();
            let repos: Vec<String> = iter.collect();
            run_work_noninteractive(cwd, name, repos)
        }
    }
}

fn run_work_noninteractive(root: PathBuf, name: String, repos: Vec<String>) -> Result<()> {
    choros::validate_name(&root, &name)?;
    for r in &repos {
        registry::ensure_repo_exists(&root, r)?;
    }
    let progress = StderrProgress;
    let info = choros::create(&root, &name, &repos, &progress)?;
    println!("{}", info.cd_target().display());
    Ok(())
}

struct StderrProgress;

impl ProgressSink for StderrProgress {
    fn status(&self, msg: String) {
        eprintln!("{msg}");
    }
}

fn run_tui(app: &mut app::App) -> Result<()> {
    // Write the TUI to /dev/tty rather than stdout, so the shell-init wrapper
    // (`target="$(choros work …)"`) can capture the result path on stdout
    // without swallowing the terminal rendering.
    let mut tty = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .wrap_err("opening /dev/tty for TUI rendering")?;

    enable_raw_mode()?;
    execute!(tty, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(tty);
    let mut terminal = Terminal::new(backend)?;

    let result = app::run(&mut terminal, app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}
