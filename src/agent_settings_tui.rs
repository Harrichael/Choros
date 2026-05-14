use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::Duration;

use color_eyre::eyre::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::agent_settings::{load_template, load_workspace, save_template, DiffSource};
use crate::json_diff::{apply_additions, diff_additions, Addition};

pub struct SettingsApp {
    pub workspace: PathBuf,
    pub root: PathBuf,
    pub source: DiffSource,
    pub additions: Vec<Addition>,
    pub selected: Vec<bool>,
    pub cursor: usize,
    pub status: String,
    pub should_quit: bool,
}

impl SettingsApp {
    pub fn new(workspace: PathBuf, root: PathBuf) -> Result<Self> {
        let mut app = Self {
            workspace,
            root,
            source: DiffSource::Both,
            additions: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
            status: String::new(),
            should_quit: false,
        };
        app.refresh()?;
        Ok(app)
    }

    fn refresh(&mut self) -> Result<()> {
        let template = load_template(&self.root)?;
        let workspace = load_workspace(&self.workspace, self.source)?;
        let additions = diff_additions(&template, &workspace);
        self.selected = vec![true; additions.len()];
        self.additions = additions;
        if self.cursor >= self.additions.len() {
            self.cursor = self.additions.len().saturating_sub(1);
        }
        Ok(())
    }

    fn cycle_source(&mut self) -> Result<()> {
        self.source = self.source.next();
        self.refresh()?;
        self.status = format!("source: {}", self.source.label());
        Ok(())
    }

    fn toggle(&mut self) {
        if let Some(s) = self.selected.get_mut(self.cursor) {
            *s = !*s;
        }
    }

    fn promote(&mut self) -> Result<()> {
        let picks: Vec<Addition> = self
            .additions
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected.get(*i).copied().unwrap_or(false))
            .map(|(_, a)| a.clone())
            .collect();
        if picks.is_empty() {
            self.status = "nothing selected".into();
            return Ok(());
        }
        let mut template = load_template(&self.root)?;
        apply_additions(&mut template, &picks);
        save_template(&self.root, &template)?;
        let n = picks.len();
        self.refresh()?;
        self.status = format!(
            "promoted {n} {} to template",
            if n == 1 { "entry" } else { "entries" }
        );
        Ok(())
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = self.additions.len();
        if len == 0 {
            return;
        }
        let cur = self.cursor as isize;
        let new = cur + delta;
        let len_i = len as isize;
        let wrapped = ((new % len_i) + len_i) % len_i;
        self.cursor = wrapped as usize;
    }
}

pub fn run(workspace: PathBuf, root: PathBuf) -> Result<()> {
    let mut app = SettingsApp::new(workspace, root)?;

    let tty = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .wrap_err("opening /dev/tty for TUI rendering")?;

    enable_raw_mode()?;
    let mut backend_writer = tty;
    execute!(backend_writer, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(backend_writer);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut SettingsApp) -> Result<()>
where
    <B as Backend>::Error: Send + Sync + 'static,
{
    while !app.should_quit {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                handle_key(app, key)?;
            }
        }
    }
    Ok(())
}

fn handle_key(app: &mut SettingsApp, key: KeyEvent) -> Result<()> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return Ok(());
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1),
        KeyCode::Char(' ') => app.toggle(),
        KeyCode::Tab => app.cycle_source()?,
        KeyCode::Enter => app.promote()?,
        _ => {}
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &SettingsApp) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title
            Constraint::Length(1), // source line
            Constraint::Min(1),    // list
            Constraint::Length(1), // status
            Constraint::Length(1), // footer
        ])
        .split(area);

    let title = Line::from(vec![
        Span::styled(
            "choros agent save settings ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("@ {}", app.workspace.display()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(title), chunks[0]);

    let source_line = Line::from(vec![
        Span::styled("diff source: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            app.source.label(),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  (Tab to cycle)", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(source_line), chunks[1]);

    draw_list(f, chunks[2], app);

    let status_p = Paragraph::new(Line::from(Span::styled(
        app.status.clone(),
        Style::default().fg(Color::Yellow),
    )));
    f.render_widget(status_p, chunks[3]);

    let footer = "j/k: move  Space: toggle  Tab: cycle source  Enter: promote selected  q/Esc: quit";
    let footer_p = Paragraph::new(Line::from(Span::styled(
        footer,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(footer_p, chunks[4]);
}

fn draw_list(f: &mut Frame, area: Rect, app: &SettingsApp) {
    let title = format!(" Additions (in workspace, not in template) — {} ", app.additions.len());
    if app.additions.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no new entries — workspace settings match template",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::default().title(title).borders(Borders::ALL));
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .additions
        .iter()
        .enumerate()
        .map(|(i, add)| {
            let mark = if app.selected.get(i).copied().unwrap_or(false) {
                "[x]"
            } else {
                "[ ]"
            };
            ListItem::new(Line::from(vec![
                Span::raw(mark),
                Span::raw("  "),
                Span::styled(add.pretty_path(), Style::default().fg(Color::Cyan)),
                Span::raw(" = "),
                Span::styled(add.pretty_value(), Style::default().fg(Color::White)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.cursor));
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, area, &mut state);
}
