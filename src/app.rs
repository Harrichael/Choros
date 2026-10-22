use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::Backend;

use crate::choros::{self, ChorosInfo, ProgressSink};
use crate::registry;
use crate::ui;

/// The `choros work` picker: a name, a set of registry repos, and — once
/// Enter is pressed — a progress popup over the same picker until the
/// workspace exists.
pub struct App {
    pub root: PathBuf,
    pub registry: Vec<String>,
    pub name: String,
    pub focus: NewFocus,
    pub repo_selected: Vec<bool>,
    pub repo_idx: usize,
    pub error: Option<String>,
    /// The latest progress line while a create runs; `None` when idle.
    pub working: Option<String>,
    work_rx: Option<Receiver<WorkMsg>>,
    pub should_quit: bool,
    pub created_path: Option<PathBuf>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum NewFocus {
    Name,
    Repos,
}

enum WorkMsg {
    Status(String),
    Done(std::result::Result<ChorosInfo, String>),
}

impl App {
    pub fn new(root: PathBuf, prefill_name: Option<String>, focus: NewFocus) -> Self {
        let (registry, error) = match registry::scan(&root) {
            Ok(r) => (r, None),
            Err(e) => (Vec::new(), Some(format!("registry scan failed: {e}"))),
        };
        Self {
            root,
            repo_selected: vec![false; registry.len()],
            registry,
            name: prefill_name.unwrap_or_default(),
            focus,
            repo_idx: 0,
            error,
            working: None,
            work_rx: None,
            should_quit: false,
            created_path: None,
        }
    }
}

pub fn run<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()>
where
    <B as Backend>::Error: Send + Sync + 'static,
{
    while !app.should_quit {
        drain_worker(app);

        terminal.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                handle_key(app, key);
            }
        }
    }
    Ok(())
}

fn drain_worker(app: &mut App) {
    let Some(rx) = app.work_rx.as_ref() else {
        return;
    };
    let done = loop {
        match rx.try_recv() {
            Ok(WorkMsg::Status(s)) => app.working = Some(s),
            Ok(WorkMsg::Done(r)) => break Some(r),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => break None,
        }
    };
    app.work_rx = None;
    app.working = None;
    match done {
        Some(Ok(info)) => {
            app.created_path = Some(info.cd_target());
            app.should_quit = true;
        }
        Some(Err(e)) => app.error = Some(e),
        None => app.error = Some("worker disconnected".into()),
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }
    if app.working.is_some() {
        return;
    }
    match key.code {
        KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab => {
            app.focus = match app.focus {
                NewFocus::Name => NewFocus::Repos,
                NewFocus::Repos => NewFocus::Name,
            };
        }
        KeyCode::Enter => submit(app),
        _ => match app.focus {
            NewFocus::Name => match key.code {
                KeyCode::Char(c) if !c.is_control() => {
                    app.name.push(c);
                    app.error = None;
                }
                KeyCode::Backspace => {
                    app.name.pop();
                    app.error = None;
                }
                _ => {}
            },
            NewFocus::Repos => match key.code {
                KeyCode::Char('j') | KeyCode::Down if !app.registry.is_empty() => {
                    app.repo_idx = (app.repo_idx + 1) % app.registry.len();
                }
                KeyCode::Char('k') | KeyCode::Up if !app.registry.is_empty() => {
                    app.repo_idx = app
                        .repo_idx
                        .checked_sub(1)
                        .unwrap_or(app.registry.len() - 1);
                }
                KeyCode::Char(' ') => {
                    if let Some(slot) = app.repo_selected.get_mut(app.repo_idx) {
                        *slot = !*slot;
                    }
                }
                _ => {}
            },
        },
    }
}

fn submit(app: &mut App) {
    if let Err(e) = choros::validate_name(&app.root, &app.name) {
        app.error = Some(e.to_string());
        return;
    }
    let repos: Vec<String> = app
        .repo_selected
        .iter()
        .zip(&app.registry)
        .filter(|(s, _)| **s)
        .map(|(_, r)| r.clone())
        .collect();
    if repos.is_empty() {
        app.error = Some("select at least one repo".into());
        return;
    }

    let (tx, rx) = mpsc::channel();
    let root = app.root.clone();
    let name = app.name.clone();
    app.error = None;
    app.working = Some(format!("creating '{name}'"));
    app.work_rx = Some(rx);
    thread::spawn(move || {
        let progress = ChannelProgress(tx.clone());
        let result = choros::create(&root, &name, &repos, &progress);
        let _ = tx.send(WorkMsg::Done(result.map_err(|e| e.to_string())));
    });
}

struct ChannelProgress(mpsc::Sender<WorkMsg>);

impl ProgressSink for ChannelProgress {
    fn status(&self, msg: String) {
        let _ = self.0.send(WorkMsg::Status(msg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn press(app: &mut App, code: KeyCode) {
        handle_key(
            app,
            KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            },
        );
    }

    /// A create that fails lands back in the picker with the error shown and
    /// the user's name and picks intact, rather than quitting or losing them.
    /// The registry repo here has no `origin`, so cloning it fails.
    #[test]
    fn a_failed_create_returns_to_the_picker() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join(".choros-config/registry/orphan");
        std::fs::create_dir_all(&repo).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&repo)
            .status()
            .unwrap();
        assert!(ok.success());

        let mut app = App::new(
            tmp.path().to_path_buf(),
            Some("PROJ-1".into()),
            NewFocus::Repos,
        );
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Enter);
        assert!(app.working.is_some());
        while app.work_rx.is_some() {
            drain_worker(&mut app);
            thread::sleep(Duration::from_millis(10));
        }

        assert!(!app.should_quit);
        assert!(app.working.is_none());
        assert!(app.error.is_some());
        assert_eq!(app.name, "PROJ-1");
        assert_eq!(app.repo_selected, [true]);
        assert!(!tmp.path().join("PROJ-1").exists());
    }
}
