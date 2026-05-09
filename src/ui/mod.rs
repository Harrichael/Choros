use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, NewFocus, NewWsState, Overlay};

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();

    if app.single_shot {
        if let Some(Overlay::NewWorkspace(state)) = app.overlay.as_ref() {
            draw_work_screen(f, area, app, state);
            return;
        }
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    draw_header(f, chunks[0], app);
    draw_main(f, chunks[1], app);
    draw_status(f, chunks[2], app);
    draw_footer(f, chunks[3]);

    if let Some(overlay) = app.overlay.as_ref() {
        match overlay {
            Overlay::NewWorkspace(state) => draw_new_workspace(f, area, app, state),
            Overlay::ConfirmDelete { name } => draw_confirm_delete(f, area, name),
            Overlay::Detail(ws) => draw_detail(f, area, ws),
            Overlay::Working { what } => draw_working(f, area, what),
        }
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let line = Line::from(vec![
        Span::styled(
            "workspaces ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("@ {}", app.root.display()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_main(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    let ws_items: Vec<ListItem> = if app.workspaces.is_empty() {
        vec![ListItem::new(Span::styled(
            "(no workspaces — press n to create one)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.workspaces
            .iter()
            .map(|ws| {
                let line = Line::from(vec![
                    Span::raw(ws.meta.name.clone()),
                    Span::raw("  "),
                    Span::styled(
                        format!("[{}]", ws.meta.repos.join(", ")),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect()
    };

    let mut state = ListState::default();
    if !app.workspaces.is_empty() {
        state.select(Some(app.workspace_idx));
    }
    let ws_list = List::new(ws_items)
        .block(Block::default().title(" Workspaces ").borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(ws_list, cols[0], &mut state);

    let reg_items: Vec<ListItem> = if app.registry.is_empty() {
        vec![ListItem::new(Span::styled(
            "(empty — clone repos into .ws-config/registry/)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.registry
            .iter()
            .map(|r| ListItem::new(Span::raw(r.clone())))
            .collect()
    };
    let reg_list = List::new(reg_items)
        .block(Block::default().title(" Registry ").borders(Borders::ALL));
    f.render_widget(reg_list, cols[1]);
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let p = Paragraph::new(Line::from(Span::styled(
        app.status.clone(),
        Style::default().fg(Color::Yellow),
    )));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, area: Rect) {
    let text = "n: new  d: delete  Enter: detail  r: rescan  q: quit";
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(p, area);
}

fn centered_rect(parent: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(parent.width.saturating_sub(2));
    let h = h.min(parent.height.saturating_sub(2));
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    Rect::new(x, y, w, h)
}

fn draw_work_screen(f: &mut Frame, area: Rect, app: &App, state: &NewWsState) {
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
            "workspaces ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("@ {}", app.root.display()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(" — new workspace", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    let name_style = if state.focus == NewFocus::Name {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let cursor = if state.focus == NewFocus::Name {
        "_"
    } else {
        ""
    };
    let name_block = Block::default()
        .title(" name ")
        .borders(Borders::ALL)
        .border_style(name_style);
    let name_p = Paragraph::new(format!("{}{}", state.name, cursor)).block(name_block);
    f.render_widget(name_p, chunks[1]);

    let repos_style = if state.focus == NewFocus::Repos {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let items: Vec<ListItem> = app
        .registry
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mark = if state.repo_selected.get(i).copied().unwrap_or(false) {
                "[x]"
            } else {
                "[ ]"
            };
            ListItem::new(Line::from(vec![
                Span::raw(mark),
                Span::raw("  "),
                Span::raw(r.clone()),
            ]))
        })
        .collect();
    let mut list_state = ListState::default();
    if !items.is_empty() {
        list_state.select(Some(state.repo_idx));
    }
    let repos = List::new(items)
        .block(
            Block::default()
                .title(" repos (Space toggles) ")
                .borders(Borders::ALL)
                .border_style(repos_style),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("> ");
    f.render_stateful_widget(repos, chunks[2], &mut list_state);

    let err = state.error.clone().unwrap_or_default();
    let err_p = Paragraph::new(err).style(Style::default().fg(Color::Red));
    f.render_widget(err_p, chunks[3]);

    let footer = "Tab: switch focus   Space: toggle repo   Enter: create   Esc: cancel";
    let footer_p = Paragraph::new(Line::from(Span::styled(
        footer,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(footer_p, chunks[4]);
}

fn draw_new_workspace(f: &mut Frame, parent: Rect, app: &App, state: &NewWsState) {
    let area = centered_rect(parent, 70, 20);
    f.render_widget(Clear, area);

    let block = Block::default()
        .title(" New workspace (Tab switches focus, Esc cancels, Enter creates) ")
        .borders(Borders::ALL);
    f.render_widget(block, area);

    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // name
            Constraint::Min(1),    // repos
            Constraint::Length(1), // error
        ])
        .split(inner);

    let name_style = if state.focus == NewFocus::Name {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let cursor = if state.focus == NewFocus::Name {
        "_"
    } else {
        ""
    };
    let name_block = Block::default()
        .title(" name ")
        .borders(Borders::ALL)
        .border_style(name_style);
    let name_p = Paragraph::new(format!("{}{}", state.name, cursor)).block(name_block);
    f.render_widget(name_p, chunks[0]);

    let repos_style = if state.focus == NewFocus::Repos {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let items: Vec<ListItem> = app
        .registry
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mark = if state.repo_selected.get(i).copied().unwrap_or(false) {
                "[x]"
            } else {
                "[ ]"
            };
            ListItem::new(Line::from(vec![
                Span::raw(mark),
                Span::raw("  "),
                Span::raw(r.clone()),
            ]))
        })
        .collect();
    let mut list_state = ListState::default();
    if !items.is_empty() {
        list_state.select(Some(state.repo_idx));
    }
    let repos = List::new(items)
        .block(
            Block::default()
                .title(" repos (Space toggles) ")
                .borders(Borders::ALL)
                .border_style(repos_style),
        )
        .highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("> ");
    f.render_stateful_widget(repos, chunks[1], &mut list_state);

    let err = state.error.clone().unwrap_or_default();
    let err_p = Paragraph::new(err).style(Style::default().fg(Color::Red));
    f.render_widget(err_p, chunks[2]);
}

fn draw_confirm_delete(f: &mut Frame, parent: Rect, name: &str) {
    let area = centered_rect(parent, 60, 7);
    f.render_widget(Clear, area);
    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("Delete workspace '{}' and all its clones?", name),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("y/Enter to confirm,  any other key to cancel"),
    ];
    let p = Paragraph::new(text)
        .alignment(Alignment::Center)
        .block(Block::default().title(" Confirm delete ").borders(Borders::ALL));
    f.render_widget(p, area);
}

fn draw_detail(f: &mut Frame, parent: Rect, ws: &crate::workspace::WorkspaceInfo) {
    let area = centered_rect(parent, 70, 12);
    f.render_widget(Clear, area);
    let lines = vec![
        Line::from(vec![
            Span::styled("name: ", Style::default().fg(Color::DarkGray)),
            Span::raw(ws.meta.name.clone()),
        ]),
        Line::from(vec![
            Span::styled("path: ", Style::default().fg(Color::DarkGray)),
            Span::raw(ws.path.display().to_string()),
        ]),
        Line::from(vec![
            Span::styled("created: ", Style::default().fg(Color::DarkGray)),
            Span::raw(ws.meta.created_at.clone()),
        ]),
        Line::from(vec![
            Span::styled("repos: ", Style::default().fg(Color::DarkGray)),
            Span::raw(ws.meta.repos.join(", ")),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let p = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().title(" Workspace ").borders(Borders::ALL));
    f.render_widget(p, area);
}

fn draw_working(f: &mut Frame, parent: Rect, what: &str) {
    let area = centered_rect(parent, 60, 5);
    f.render_widget(Clear, area);
    let p = Paragraph::new(Line::from(Span::styled(
        what.to_string(),
        Style::default().fg(Color::Yellow),
    )))
    .alignment(Alignment::Center)
    .block(Block::default().title(" Working ").borders(Borders::ALL));
    f.render_widget(p, area);
}
