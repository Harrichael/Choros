//! The queue TUI (`choros queue -i [NAME]`) and the multi-select picker it shares
//! with `land -i`.

use std::path::Path;
use std::time::Duration;

use choros_land::admin;
use choros_land::engine::Env;
use choros_core::space;
use choros_land::queue::Queue;
use choros_land::state::{Dep, LandingSet, QueueState, SetStatus};
use color_eyre::eyre::{Result, eyre};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use crate::tui::with_tui;

/// One line about a set: its branch, state, what it waits on, and where it
/// came from.
pub fn describe(q: &Queue, s: &LandingSet) -> String {
    let state = match (&s.status, &s.pr, &s.park) {
        (_, _, Some(p)) => format!("parked ({:?})", p.op),
        (SetStatus::Queued, Some(_), _) => "front".into(),
        (status, _, _) => format!("{status:?}").to_lowercase(),
    };
    let deps: Vec<String> = s
        .deps
        .iter()
        .map(|d| match d {
            Dep::Set(n) => format!("{n}"),
            Dep::Cross { queue, n } => format!("{queue}/{n}"),
        })
        .collect();
    let from = space::rel(&q.trunk, &s.source.workspace).unwrap_or_else(|_| "?".into());
    let title = s.title.clone().unwrap_or_default();
    format!(
        "{:<18} {:<10} {:<14} after [{}]  from {from}  {title}",
        s.branch(&q.name),
        s.repo(),
        state,
        deps.join(",")
    )
}

/// Toggle any of `labels`; Enter confirms (an empty choice is allowed), Esc
/// cancels.
pub fn pick(title: &str, labels: &[String], preselected: &[usize]) -> Result<Option<Vec<usize>>> {
    let mut chosen: Vec<bool> = (0..labels.len()).map(|i| preselected.contains(&i)).collect();
    let mut idx = 0usize;
    with_tui(|terminal| loop {
        terminal.draw(|f| {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(f.area());
            let items: Vec<ListItem> = if labels.is_empty() {
                vec![ListItem::new("(nothing queued)")]
            } else {
                labels
                    .iter()
                    .zip(&chosen)
                    .map(|(l, c)| ListItem::new(format!("{} {l}", if *c { "[x]" } else { "[ ]" })))
                    .collect()
            };
            let mut state = ListState::default();
            if !labels.is_empty() {
                state.select(Some(idx));
            }
            let list = List::new(items)
                .block(Block::default().title(title.to_string()).borders(Borders::ALL))
                .highlight_style(Style::default().bg(Color::DarkGray))
                .highlight_symbol("> ");
            f.render_stateful_widget(list, rows[0], &mut state);
            f.render_widget(
                Paragraph::new(Span::styled(
                    "Space: toggle   Enter: confirm   Esc: cancel",
                    Style::default().fg(Color::DarkGray),
                )),
                rows[1],
            );
        })?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Enter => {
                return Ok(Some(
                    chosen.iter().enumerate().filter(|(_, c)| **c).map(|(i, _)| i).collect(),
                ));
            }
            KeyCode::Char('j') | KeyCode::Down if !labels.is_empty() => idx = (idx + 1) % labels.len(),
            KeyCode::Char('k') | KeyCode::Up if !labels.is_empty() => {
                idx = idx.checked_sub(1).unwrap_or(labels.len() - 1)
            }
            KeyCode::Char(' ') => {
                if let Some(c) = chosen.get_mut(idx) {
                    *c = !*c;
                }
            }
            _ => {}
        }
    })
}

enum Action {
    Edit(u32),
    Drop(u32),
    Sync,
    Quit,
}

/// With no `name`, picks among `offered` (the queues in view from here).
pub fn run(trunk: &Path, name: Option<&str>, offered: &[String], env: &Env) -> Result<()> {
    let name = match name {
        Some(n) => n.to_string(),
        None => match offered {
            [] => return Err(eyre!("no queues yet; create one with `choros queue new NAME REPO`")),
            [only] => only.clone(),
            many => {
                let labels: Vec<String> = many.to_vec();
                let picked = pick(" which queue? (pick one) ", &labels, &[])?
                    .and_then(|v| v.first().copied())
                    .ok_or_else(|| eyre!("cancelled"))?;
                labels[picked].clone()
            }
        },
    };
    let q = Queue::open(trunk, &name)?;
    let mut message = String::new();
    loop {
        let st = q.read_state()?;
        let open: Vec<u32> = st.sets.values().filter(|s| s.is_open() || s.status == SetStatus::Closed).map(|s| s.n).collect();
        let action = browse(&q, &st, &open, &message)?;
        message = match action {
            Action::Quit => return Ok(()),
            Action::Sync => match admin::sync(env, &q, None) {
                Ok(r) => format!("synced: {} re-synced, {} PRs opened, {} parked", r.resynced.len(), r.opened.len(), r.parked.len()),
                Err(e) => format!("error: {e}"),
            },
            Action::Drop(n) => match admin::drop_set(env, &q, n) {
                Ok(d) => match crate::land_cli::dropped_note(&d) {
                    Some(note) => format!("dropped landing-{n}; {note}"),
                    None => format!("dropped landing-{n}"),
                },
                Err(e) => format!("error: {e}"),
            },
            Action::Edit(n) => {
                let others: Vec<&LandingSet> = st.sets.values().filter(|s| s.is_open() && s.n != n).collect();
                let labels: Vec<String> = others.iter().map(|s| describe(&q, s)).collect();
                let current: Vec<usize> = others
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| st.sets[&n].deps.contains(&Dep::Set(s.n)))
                    .map(|(i, _)| i)
                    .collect();
                match pick(&format!(" landing-{n} depends on: "), &labels, &current)? {
                    None => String::new(),
                    Some(chosen) => {
                        let mut deps: Vec<Dep> = chosen.into_iter().map(|i| Dep::Set(others[i].n)).collect();
                        // Cross-queue edges are kept; this picker only shows this queue.
                        deps.extend(st.sets[&n].deps.iter().filter(|d| matches!(d, Dep::Cross { .. })).cloned());
                        match admin::set_deps(env, &q, n, deps) {
                            Ok(_) => format!("landing-{n}'s dependencies updated"),
                            Err(e) => format!("error: {e}"),
                        }
                    }
                }
            }
        };
    }
}

fn browse(q: &Queue, st: &QueueState, open: &[u32], message: &str) -> Result<Action> {
    let mut idx = 0usize;
    let mut confirm_drop = false;
    with_tui(|terminal| loop {
        terminal.draw(|f| {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)])
                .split(f.area());
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("choros queue ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("{} ({})", q.name, st.repos.keys().cloned().collect::<Vec<_>>().join(", ")),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])),
                rows[0],
            );
            let items: Vec<ListItem> = if open.is_empty() {
                vec![ListItem::new("(nothing queued)")]
            } else {
                open.iter().map(|n| ListItem::new(describe(q, &st.sets[n]))).collect()
            };
            let mut state = ListState::default();
            if !open.is_empty() {
                state.select(Some(idx));
            }
            f.render_stateful_widget(
                List::new(items)
                    .block(Block::default().borders(Borders::ALL))
                    .highlight_style(Style::default().bg(Color::DarkGray))
                    .highlight_symbol("> "),
                rows[1],
                &mut state,
            );
            let status = if confirm_drop {
                "drop this landing-set? y to confirm".to_string()
            } else {
                message.to_string()
            };
            f.render_widget(Paragraph::new(Span::styled(status, Style::default().fg(Color::Yellow))), rows[2]);
            f.render_widget(
                Paragraph::new(Span::styled(
                    "e: edit dependencies   x: drop   s: sync now   q: quit",
                    Style::default().fg(Color::DarkGray),
                )),
                rows[3],
            );
        })?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let selected = open.get(idx).copied();
        if confirm_drop {
            confirm_drop = false;
            if let (KeyCode::Char('y'), Some(n)) = (key.code, selected) {
                return Ok(Action::Drop(n));
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
            KeyCode::Char('s') => return Ok(Action::Sync),
            KeyCode::Char('e') => {
                if let Some(n) = selected {
                    return Ok(Action::Edit(n));
                }
            }
            KeyCode::Char('x') if selected.is_some() => confirm_drop = true,
            KeyCode::Char('j') | KeyCode::Down if !open.is_empty() => idx = (idx + 1) % open.len(),
            KeyCode::Char('k') | KeyCode::Up if !open.is_empty() => idx = idx.checked_sub(1).unwrap_or(open.len() - 1),
            _ => {}
        }
    })
}
