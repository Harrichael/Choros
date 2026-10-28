use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::Backend;

use crate::choros::{self, ChorosInfo, ProgressSink};
use choros_core::space;
use crate::registry;
use crate::ui;

/// The `choros work -i` picker: a name, a set of registry repos, and — once
/// Enter is pressed — a progress popup over the same picker until the
/// workspace exists.
pub struct App {
    /// Where the workspace is made: the trunk or a branch-space.
    pub space: PathBuf,
    pub registry: Vec<String>,
    pub name: String,
    pub focus: NewFocus,
    pub repo_selected: Vec<bool>,
    /// Per registry repo: the landing queues that take workspaces from this
    /// space, and which one the new workspace lands into (0 = none, i.e.
    /// instant mode; preset from the branch-space's default, else the queue
    /// scoped to it).
    pub queues: Vec<Vec<String>>,
    pub queue_choice: Vec<usize>,
    /// The queue scoped to this space, which a repo lands into unless it is
    /// bound otherwise.
    scoped: Option<String>,
    pub repo_idx: usize,
    pub error: Option<String>,
    pub popup: Option<Popup>,
    work_rx: Option<Receiver<WorkMsg>>,
    pub should_quit: bool,
    pub created_path: Option<PathBuf>,
}

/// The box over the picker once Enter is pressed.
pub enum Popup {
    /// The latest progress line while a create runs.
    Working(String),
    /// A create that failed stays up until dismissed, so the cause can be read.
    Failed {
        /// The last progress line before the failure, so the step in the user's terms.
        during: String,
        /// The error chain, outermost first.
        error: Vec<String>,
        /// Where the workspace would have been, if a failure left it there.
        kept: Option<PathBuf>,
    },
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum NewFocus {
    Name,
    Repos,
}

enum WorkMsg {
    Status(String),
    Done(std::result::Result<ChorosInfo, Vec<String>>),
}

impl App {
    pub fn new(space: PathBuf, prefill_name: Option<String>, focus: NewFocus) -> Self {
        let trunk = space::find_trunk(&space).unwrap_or_else(|| space.clone());
        let (registry, error) = match registry::scan(&trunk) {
            Ok(r) => (r, None),
            Err(e) => (Vec::new(), Some(format!("registry scan failed: {e}"))),
        };
        let rel = space::rel(&trunk, &space).unwrap_or_default();
        let admitted = queues_for(&trunk, &rel);
        let queues: Vec<Vec<String>> = registry.iter().map(|_| admitted.clone()).collect();
        let scoped = choros_land::workspace::space_queue(&trunk, &rel).ok().flatten();
        let position = |q: &str| admitted.iter().position(|x| x == q).map_or(0, |i| i + 1);
        let queue_choice = registry
            .iter()
            .map(|repo| match choros_land::branch::default_binding(&space, repo) {
                Ok(Some(Some(q))) => position(&q),
                Ok(Some(None)) => 0,
                _ => scoped.as_deref().map_or(0, position),
            })
            .collect();
        Self {
            queue_choice,
            queues,
            scoped,
            space,
            repo_selected: vec![false; registry.len()],
            registry,
            name: prefill_name.unwrap_or_default(),
            focus,
            repo_idx: 0,
            error,
            popup: None,
            work_rx: None,
            should_quit: false,
            created_path: None,
        }
    }
}

fn queues_for(trunk: &std::path::Path, space_rel: &str) -> Vec<String> {
    choros_land::queue::list(trunk)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| {
            choros_land::queue::Queue::open(trunk, name).is_ok_and(|q| q.config.scope.admits_space(space_rel))
        })
        .collect()
}

impl App {
    /// The queue the highlighted-or-given repo will land into.
    pub fn queue_of(&self, i: usize) -> Option<&str> {
        let choice = *self.queue_choice.get(i)?;
        choice.checked_sub(1).and_then(|c| self.queues[i].get(c)).map(String::as_str)
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
            Ok(WorkMsg::Status(s)) => app.popup = Some(Popup::Working(s)),
            Ok(WorkMsg::Done(r)) => break Some(r),
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => break None,
        }
    };
    app.work_rx = None;
    let error = match done {
        Some(Ok(info)) => {
            app.popup = None;
            app.created_path = Some(info.cd_target());
            app.should_quit = true;
            return;
        }
        Some(Err(chain)) => chain,
        None => vec!["the worker thread stopped without a result".into()],
    };
    let during = match app.popup.take() {
        Some(Popup::Working(s)) => s,
        _ => String::new(),
    };
    let target = app.space.join(&app.name);
    app.popup = Some(Popup::Failed {
        during,
        error,
        kept: target.exists().then_some(target),
    });
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }
    match app.popup {
        Some(Popup::Working(_)) => return,
        Some(Popup::Failed { .. }) => {
            if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                app.popup = None;
            }
            return;
        }
        None => {}
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
                KeyCode::Char('b') => {
                    let i = app.repo_idx;
                    if let (Some(c), Some(qs)) = (app.queue_choice.get_mut(i), app.queues.get(i)) {
                        *c = (*c + 1) % (qs.len() + 1);
                    }
                }
                _ => {}
            },
        },
    }
}

fn submit(app: &mut App) {
    if let Err(e) = choros::validate_name(&app.space, &app.name) {
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
    // Straight to main where a queue is scoped here has to be said outright.
    let bindings: Vec<(String, Option<String>)> = (0..app.registry.len())
        .filter(|i| app.repo_selected[*i])
        .filter_map(|i| match app.queue_of(i) {
            Some(q) => Some((app.registry[i].clone(), Some(q.to_string()))),
            None => app.scoped.as_ref().map(|_| (app.registry[i].clone(), None)),
        })
        .collect();
    if repos.is_empty() {
        app.error = Some("select at least one repo".into());
        return;
    }

    let (tx, rx) = mpsc::channel();
    let space = app.space.clone();
    let name = app.name.clone();
    app.error = None;
    app.popup = Some(Popup::Working(format!("creating '{name}'")));
    app.work_rx = Some(rx);
    thread::spawn(move || {
        let progress = ChannelProgress(tx.clone());
        let result = choros::create(&space, &name, &repos, &progress).and_then(|info| {
            for (repo, queue) in &bindings {
                if let Some(queue) = queue {
                    progress.status(format!("{repo} lands into {queue}; moving it onto the merge head…"));
                }
                crate::land_cli::bind_new(&info.path, repo, queue.as_deref())?;
            }
            Ok(info)
        });
        let _ = tx.send(WorkMsg::Done(
            result.map_err(|e| e.chain().map(|cause| cause.to_string()).collect()),
        ));
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

    /// A create that fails keeps its popup up, now saying what went wrong:
    /// the step it got to and the whole error chain, not just the outermost
    /// line. Dismissing it returns to the picker with the name and picks
    /// intact, and nothing left on disk. The registry repo here has no
    /// `origin`, so cloning it fails.
    #[test]
    fn a_failed_create_explains_itself_then_returns_to_the_picker() {
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
        assert!(matches!(app.popup, Some(Popup::Working(_))));
        while app.work_rx.is_some() {
            drain_worker(&mut app);
            thread::sleep(Duration::from_millis(10));
        }

        let Some(Popup::Failed { during, error, kept }) = &app.popup else {
            panic!("expected the failure popup");
        };
        assert_eq!(during, "cloning orphan…");
        assert!(error.concat().contains("No such remote 'origin'"), "{error:?}");
        assert!(kept.is_none());
        assert!(!tmp.path().join("PROJ-1").exists());

        press(&mut app, KeyCode::Esc);
        assert!(app.popup.is_none());
        assert!(!app.should_quit);
        assert_eq!(app.name, "PROJ-1");
        assert_eq!(app.repo_selected, [true]);
    }
}
