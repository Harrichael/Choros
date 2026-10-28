use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::path::Path;

use crate::app::{App, NewFocus, Popup};

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

    match &app.popup {
        Some(Popup::Working(what)) => draw_working(f, area, what),
        Some(Popup::Failed { during, error, kept }) => {
            draw_failed(f, area, during, error, kept.as_deref())
        }
        None => {}
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

/// Sized to the message, up to nearly the whole screen. A message too long
/// for that (pnpm's output can run to pages) keeps its head and tail: the
/// outermost cause says what choros was doing, the last lines say why the
/// tool gave up.
fn draw_failed(f: &mut Frame, parent: Rect, during: &str, error: &[String], kept: Option<&Path>) {
    let dim = Style::default().fg(Color::DarkGray);
    let w = 100.min(parent.width.saturating_sub(4));
    let inner_w = usize::from(w.saturating_sub(2)).max(1);
    let rows = |lines: &[Line]| -> usize { lines.iter().map(|l| l.width().max(1).div_ceil(inner_w)).sum() };

    let mut head = Vec::new();
    if !during.is_empty() {
        head.push(Line::from(vec![Span::styled("last step: ", dim), Span::raw(during.to_string())]));
        head.push(Line::default());
    }
    let mut body: Vec<Line> = Vec::new();
    for (i, cause) in error.iter().enumerate() {
        let style = if i == 0 {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        body.extend(cause.lines().map(|l| Line::styled(l.to_string(), style)));
    }
    let left = match kept {
        Some(path) => format!("{} is still there.", path.display()),
        None => "Nothing was left behind.".to_string(),
    };
    let foot = vec![
        Line::default(),
        Line::styled(left, dim),
        Line::styled("Enter/Esc: back to the picker", dim),
    ];

    let max_h = usize::from(parent.height.saturating_sub(2));
    let budget = max_h.saturating_sub(2 + rows(&head) + rows(&foot));
    if rows(&body) > budget && body.len() > 2 {
        let first = body.remove(0);
        let mut tail = Vec::new();
        let mut used = rows(std::slice::from_ref(&first)) + 1;
        while let Some(line) = body.pop() {
            let r = rows(std::slice::from_ref(&line));
            if used + r > budget {
                body.push(line);
                break;
            }
            used += r;
            tail.push(line);
        }
        tail.reverse();
        let hidden = Line::styled(format!("… {} lines hidden …", body.len()), dim);
        body = std::iter::once(first).chain(std::iter::once(hidden)).chain(tail).collect();
    }

    let lines: Vec<Line> = head.into_iter().chain(body).chain(foot).collect();
    let h = (rows(&lines) + 2).min(max_h) as u16;
    let area = Rect::new(
        parent.x + (parent.width.saturating_sub(w)) / 2,
        parent.y + (parent.height.saturating_sub(h)) / 2,
        w,
        h,
    );
    f.render_widget(Clear, area);
    let p = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" Failed ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red)),
    );
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn screen(error: &[String]) -> String {
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| draw_failed(f, f.area(), "pnpm install in web…", error, None)).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// pnpm's output runs far past a screen. The popup keeps what choros was
    /// doing (the outermost cause) and how the tool's output ended, and says
    /// how much it dropped in between.
    #[test]
    fn a_long_failure_keeps_its_first_line_and_its_tail() {
        let output: Vec<String> = (1..=60).map(|i| format!("progress line {i}")).collect();
        let error = vec![
            "setting up workspace caches".to_string(),
            format!("pnpm install failed in /ws/web:\n{}\nexec: corepack: not found", output.join("\n")),
        ];
        let shown = screen(&error);
        assert!(shown.contains("last step: pnpm install in web…"));
        assert!(shown.contains("setting up workspace caches"));
        assert!(shown.contains("lines hidden"));
        assert!(shown.contains("progress line 60"));
        assert!(shown.contains("exec: corepack: not found"));
        assert!(!shown.contains("progress line 1 "));
        assert!(shown.contains("Nothing was left behind."));
    }
}
