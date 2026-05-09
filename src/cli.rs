use std::io;
use std::path::PathBuf;

use color_eyre::eyre::Result;
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{self, NewFocus};
use crate::registry;
use crate::workspace::{self, ProgressSink};

const SHELL_INIT: &str = r#"# wspace shell integration — works in bash and zsh
wspace() {
  if [ "$1" = "work" ]; then
    local target
    target="$(command wspace "$@")" || return $?
    [ -n "$target" ] && cd "$target"
  else
    command wspace "$@"
  fi
}
"#;

const USAGE: &str = "\
wspace — multi-repo workspace manager

USAGE:
    wspace                          launch the full TUI in the current directory
    wspace work                     fast-create: name + repo multi-select, exits on create
    wspace work NAME                fast-create with name pre-filled
    wspace work NAME REPO [REPO…]   non-interactive create
    wspace init                     create .ws-config/ and .ws-config/registry/ in the current directory
    wspace shell-init               emit shell-integration function for `eval \"$(wspace shell-init)\"`
    wspace -h | --help              show this message
";

pub fn print_usage() {
    print!("{USAGE}");
}

pub fn emit_shell_init() {
    print!("{SHELL_INIT}");
}

pub fn run_init() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let dir = registry::init_dirs(&cwd)?;
    eprintln!("initialized {}", dir.display());
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
    workspace::validate_name(&root, &name)?;
    for r in &repos {
        registry::ensure_repo_exists(&root, r)?;
    }
    let progress = StderrProgress;
    let info = workspace::create(&root, &name, &repos, &progress)?;
    println!("{}", info.path.display());
    Ok(())
}

struct StderrProgress;

impl ProgressSink for StderrProgress {
    fn status(&self, msg: String) {
        eprintln!("{msg}");
    }
}

fn run_tui(app: &mut app::App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = app::run(&mut terminal, app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}
