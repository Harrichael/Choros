use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::app::{App, NewFocus};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(3), // name
            Constraint::Min(1),    // repos
            Constraint::Length(1), // error
            Constraint::Length(1), // footer
        ])
        .split(area);

    let header = Line::from(vec![
        Span::styled(
            "choros ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("@ {}", app.space.display()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(" — new", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    draw_name_field(f, chunks[1], app);
    draw_repo_list(f, chunks[2], app);

    let err = app.error.clone().unwrap_or_default();
    f.render_widget(
        Paragraph::new(err).style(Style::default().fg(Color::Red)),
        chunks[3],
    );

    let footer = "Tab: switch focus   Space: toggle repo   b: landing queue   Enter: create   Esc: cancel";
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[4],
    );

    if let Some(what) = &app.working {
        draw_working(f, area, what);
    }
}

fn focus_style(app: &App, focus: NewFocus) -> Style {
    if app.focus == focus {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

fn draw_name_field(f: &mut Frame, area: Rect, app: &App) {
    let cursor = if app.focus == NewFocus::Name { "_" } else { "" };
    let block = Block::default()
        .title(" name ")
        .borders(Borders::ALL)
        .border_style(focus_style(app, NewFocus::Name));
    let p = Paragraph::new(format!("{}{}", app.name, cursor)).block(block);
    f.render_widget(p, area);
}

fn draw_repo_list(f: &mut Frame, area: Rect, app: &App) {
    let items: Vec<ListItem> = if app.registry.is_empty() {
        vec![ListItem::new(Span::styled(
            "(registry is empty — run `choros add <ssh-url>`)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.registry
            .iter()
            .zip(&app.repo_selected)
            .enumerate()
            .map(|(i, (r, selected))| {
                let mark = if *selected { "[x]" } else { "[ ]" };
                let mut spans = vec![Span::raw(mark), Span::raw("  "), Span::raw(r.clone())];
                if let Some(q) = app.queue_of(i) {
                    spans.push(Span::styled(format!("  → {q}"), Style::default().fg(Color::Cyan)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    };
    let mut list_state = ListState::default();
    if !app.registry.is_empty() {
        list_state.select(Some(app.repo_idx));
    }
    let repos = List::new(items)
        .block(
            Block::default()
                .title(" repos (Space toggles) ")
                .borders(Borders::ALL)
                .border_style(focus_style(app, NewFocus::Repos)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("> ");
    f.render_stateful_widget(repos, area, &mut list_state);
}

fn draw_working(f: &mut Frame, parent: Rect, what: &str) {
    let w = 60.min(parent.width.saturating_sub(2));
    let h = 5.min(parent.height.saturating_sub(2));
    let area = Rect::new(
        parent.x + (parent.width.saturating_sub(w)) / 2,
        parent.y + (parent.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, area);
    let p = Paragraph::new(Line::from(Span::styled(
        what.to_string(),
        Style::default().fg(Color::Yellow),
    )))
    .alignment(Alignment::Center)
    .block(Block::default().title(" Working ").borders(Borders::ALL));
    f.render_widget(p, area);
}
