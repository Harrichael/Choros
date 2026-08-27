use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::choros::{self, ChorosInfo, ProgressSink};
use crate::registry;
use crate::ui;

pub struct App {
    pub root: PathBuf,
    pub choros_list: Vec<ChorosInfo>,
    pub registry: Vec<String>,
    pub choros_idx: usize,
    pub overlay: Option<Overlay>,
    pub status: String,
    pub work_rx: Option<Receiver<WorkMsg>>,
    pub should_quit: bool,
    pub single_shot: bool,
    pub result_path: Option<PathBuf>,
    /// Status to show when the pending background job succeeds.
    work_done: String,
}

pub enum Overlay {
    NewChoros(NewChorosState),
    AddRepos(AddReposState),
    ConfirmDelete { name: String },
    Detail(ChorosInfo),
    Working { what: String },
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum NewFocus {
    Name,
    Repos,
}

/// Cursor + checkbox state over a fixed list of registry repo names. Owns its
/// items rather than indexing into `App::registry` so a rescan can't leave the
/// checkboxes pointing at the wrong rows.
pub struct RepoPicker {
    pub items: Vec<String>,
    pub selected: Vec<bool>,
    pub idx: usize,
}

impl RepoPicker {
    pub fn new(items: Vec<String>) -> Self {
        let selected = vec![false; items.len()];
        Self {
            items,
            selected,
            idx: 0,
        }
    }

    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.idx = (self.idx + 1) % self.items.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.items.is_empty() {
            self.idx = self.idx.checked_sub(1).unwrap_or(self.items.len() - 1);
        }
    }

    pub fn toggle(&mut self) {
        if let Some(slot) = self.selected.get_mut(self.idx) {
            *slot = !*slot;
        }
    }

    pub fn is_selected(&self, i: usize) -> bool {
        self.selected.get(i).copied().unwrap_or(false)
    }

    pub fn chosen(&self) -> Vec<String> {
        self.items
            .iter()
            .enumerate()
            .filter(|(i, _)| self.is_selected(*i))
            .map(|(_, r)| r.clone())
            .collect()
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.next(),
            KeyCode::Char('k') | KeyCode::Up => self.prev(),
            KeyCode::Char(' ') => self.toggle(),
            _ => {}
        }
    }
}

pub struct NewChorosState {
    pub name: String,
    pub focus: NewFocus,
    pub repos: RepoPicker,
    pub error: Option<String>,
}

pub struct AddReposState {
    pub workspace: String,
    pub repos: RepoPicker,
    pub error: Option<String>,
}

pub enum WorkMsg {
    Status(String),
    Done(std::result::Result<ChorosInfo, String>),
}

impl App {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            choros_list: Vec::new(),
            registry: Vec::new(),
            choros_idx: 0,
            overlay: None,
            status: String::new(),
            work_rx: None,
            should_quit: false,
            single_shot: false,
            result_path: None,
            work_done: String::new(),
        }
    }

    pub fn new_work(root: PathBuf, prefill_name: Option<String>, focus: NewFocus) -> Self {
        let mut app = Self::new(root);
        app.single_shot = true;
        app.refresh();
        app.overlay = Some(Overlay::NewChoros(NewChorosState {
            name: prefill_name.unwrap_or_default(),
            focus,
            repos: RepoPicker::new(app.registry.clone()),
            error: None,
        }));
        app
    }

    pub fn new_add_repos(root: PathBuf, workspace: String, candidates: Vec<String>) -> Self {
        let mut app = Self::new(root);
        app.single_shot = true;
        app.refresh();
        app.overlay = Some(Overlay::AddRepos(AddReposState {
            workspace,
            repos: RepoPicker::new(candidates),
            error: None,
        }));
        app
    }

    pub fn refresh(&mut self) {
        match registry::scan(&self.root) {
            Ok(r) => self.registry = r,
            Err(e) => self.status = format!("registry scan failed: {e}"),
        }
        match choros::scan(&self.root) {
            Ok(c) => self.choros_list = c,
            Err(e) => self.status = format!("choros scan failed: {e}"),
        }
        if self.choros_idx >= self.choros_list.len() {
            self.choros_idx = self.choros_list.len().saturating_sub(1);
        }
    }

    pub fn selected_choros(&self) -> Option<&ChorosInfo> {
        self.choros_list.get(self.choros_idx)
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
    let mut done = false;
    let mut done_result: Option<std::result::Result<ChorosInfo, String>> = None;
    loop {
        match rx.try_recv() {
            Ok(WorkMsg::Status(s)) => {
                if let Some(Overlay::Working { what }) = app.overlay.as_mut() {
                    *what = s.clone();
                }
                app.status = s;
            }
            Ok(WorkMsg::Done(r)) => {
                done = true;
                done_result = Some(r);
                break;
            }
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                done = true;
                break;
            }
        }
    }
    if done {
        app.work_rx = None;
        app.overlay = None;
        match done_result {
            Some(Ok(info)) => {
                app.status = std::mem::take(&mut app.work_done);
                if app.single_shot {
                    app.result_path = Some(info.cd_target());
                    app.should_quit = true;
                }
            }
            Some(Err(e)) => app.status = format!("error: {e}"),
            None => app.status = "worker disconnected".into(),
        }
        app.refresh();
    }
}

fn handle_key(app: &mut App, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }

    let overlay = app.overlay.take();
    match overlay {
        // In `choros work` there is nothing to browse; a dismissed overlay means
        // the flow is over (or failed), so any key exits.
        None if app.single_shot => app.should_quit = true,
        None => handle_browsing(app, key),
        Some(Overlay::NewChoros(state)) => handle_new_choros(app, key, state),
        Some(Overlay::AddRepos(state)) => handle_add_repos(app, key, state),
        Some(Overlay::ConfirmDelete { name }) => handle_confirm_delete(app, key, name),
        Some(Overlay::Detail(info)) => handle_detail(app, key, info),
        Some(Overlay::Working { what }) => {
            // Working overlay: only Ctrl-C above can break it; otherwise restore.
            app.overlay = Some(Overlay::Working { what });
        }
    }
}

fn handle_browsing(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Char('j') | KeyCode::Down => {
            if !app.choros_list.is_empty() {
                app.choros_idx = (app.choros_idx + 1) % app.choros_list.len();
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if !app.choros_list.is_empty() {
                if app.choros_idx == 0 {
                    app.choros_idx = app.choros_list.len() - 1;
                } else {
                    app.choros_idx -= 1;
                }
            }
        }
        KeyCode::Char('n') => {
            if app.registry.is_empty() {
                app.status =
                    "registry is empty; run `choros add <ssh-url>` to add a repo".into();
            } else {
                app.overlay = Some(Overlay::NewChoros(NewChorosState {
                    name: String::new(),
                    focus: NewFocus::Name,
                    repos: RepoPicker::new(app.registry.clone()),
                    error: None,
                }));
            }
        }
        KeyCode::Char('d') => {
            if let Some(c) = app.selected_choros() {
                app.overlay = Some(Overlay::ConfirmDelete {
                    name: c.meta.name.clone(),
                });
            }
        }
        KeyCode::Enter => {
            if let Some(c) = app.selected_choros() {
                app.overlay = Some(Overlay::Detail(c.clone()));
            }
        }
        _ => {}
    }
}

fn handle_new_choros(app: &mut App, key: KeyEvent, mut state: NewChorosState) {
    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
            if app.single_shot {
                app.should_quit = true;
            }
            return;
        }
        KeyCode::Tab => {
            state.focus = match state.focus {
                NewFocus::Name => NewFocus::Repos,
                NewFocus::Repos => NewFocus::Name,
            };
        }
        KeyCode::Enter => {
            if let Err(e) = choros::validate_name(&app.root, &state.name) {
                state.error = Some(e.to_string());
                app.overlay = Some(Overlay::NewChoros(state));
                return;
            }
            let selected = state.repos.chosen();
            if selected.is_empty() {
                state.error = Some("select at least one repo".into());
                app.overlay = Some(Overlay::NewChoros(state));
                return;
            }
            spawn_create(app, state.name.clone(), selected);
            return;
        }
        _ => match state.focus {
            NewFocus::Name => match key.code {
                KeyCode::Char(c) => {
                    if !c.is_control() {
                        state.name.push(c);
                        state.error = None;
                    }
                }
                KeyCode::Backspace => {
                    state.name.pop();
                    state.error = None;
                }
                _ => {}
            },
            NewFocus::Repos => state.repos.handle_key(key),
        },
    }
    app.overlay = Some(Overlay::NewChoros(state));
}

fn handle_add_repos(app: &mut App, key: KeyEvent, mut state: AddReposState) {
    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
            if app.single_shot {
                app.should_quit = true;
            }
            return;
        }
        KeyCode::Enter => {
            let selected = state.repos.chosen();
            if selected.is_empty() {
                state.error = Some("select at least one repo".into());
                app.overlay = Some(Overlay::AddRepos(state));
                return;
            }
            spawn_add_repos(app, state.workspace.clone(), selected);
            return;
        }
        _ => state.repos.handle_key(key),
    }
    app.overlay = Some(Overlay::AddRepos(state));
}

fn handle_confirm_delete(app: &mut App, key: KeyEvent, name: String) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            let target = app
                .choros_list
                .iter()
                .find(|c| c.meta.name == name)
                .cloned();
            match target {
                Some(c) => match choros::delete(&c) {
                    Ok(()) => app.status = format!("deleted '{}'", name),
                    Err(e) => app.status = format!("delete failed: {e}"),
                },
                None => app.status = format!("choros '{}' not found", name),
            }
            app.refresh();
        }
        _ => {
            app.overlay = Some(Overlay::ConfirmDelete { name });
        }
    }
}

fn handle_detail(app: &mut App, _key: KeyEvent, _info: ChorosInfo) {
    app.overlay = None;
}

struct ChannelProgress(mpsc::Sender<WorkMsg>);

impl ProgressSink for ChannelProgress {
    fn status(&self, msg: String) {
        let _ = self.0.send(WorkMsg::Status(msg));
    }
}

fn spawn_work<F>(app: &mut App, what: String, done: String, job: F)
where
    F: FnOnce(&ChannelProgress) -> Result<ChorosInfo> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    app.work_rx = Some(rx);
    app.work_done = done;
    app.overlay = Some(Overlay::Working { what });

    thread::spawn(move || {
        let progress = ChannelProgress(tx.clone());
        let result = job(&progress);
        let _ = tx.send(WorkMsg::Done(result.map_err(|e| e.to_string())));
    });
}

fn spawn_create(app: &mut App, name: String, repos: Vec<String>) {
    let root = app.root.clone();
    spawn_work(
        app,
        format!("creating '{}'", name),
        format!("created choros '{}'", name),
        move |p| choros::create(&root, &name, &repos, p),
    );
}

fn spawn_add_repos(app: &mut App, name: String, repos: Vec<String>) {
    let root = app.root.clone();
    let what = format!("adding {} to '{}'", repos.join(", "), name);
    let done = format!("added {} to '{}'", repos.join(", "), name);
    spawn_work(app, what, done, move |p| {
        choros::add_repos(&root, &name, &repos, p)
    });
}
