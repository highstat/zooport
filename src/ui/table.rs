//! The main port table.
//!
//! Rows are built as owned values for the duration of one frame. The table
//! borrows `App` immutably; selection and scroll offset live in a local
//! `TableState`, so rendering never mutates application state.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, HighlightSpacing, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use super::dim_style;
use crate::app::{App, TunnelState};
use crate::scanner::PortEntry;

const COLUMN_TITLES: [&str; 7] = ["SEL", "PORT", "PID", "PROCESS", "CONTEXT", "MEM", "TUNNEL"];

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows: Vec<Row<'static>> = app
        .visible_entries()
        .map(|entry| table_row(app, entry))
        .collect();

    if rows.is_empty() {
        let message = if app.total_len() == 0 {
            if app.last_scan().is_some() {
                "no listening ports"
            } else {
                "scanning sockets..."
            }
        } else {
            "no ports match the filter"
        };

        frame.render_widget(
            Paragraph::new(Line::from(message).centered()).style(dim_style()),
            area,
        );
        return;
    }

    let header = Row::new(COLUMN_TITLES.into_iter().map(Cell::from)).style(
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );

    // PROCESS absorbs extra width. TUNNEL needs a fixed 12 columns so
    // `starting...` is never truncated at the minimum terminal width.
    let widths = [
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Length(7),
        Constraint::Fill(1),
        Constraint::Length(18),
        Constraint::Length(9),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(Style::new().fg(app.accent()).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);

    let selected = (app.visible_len() > 0).then_some(app.selected());
    let mut state = TableState::new().with_selected(selected);
    frame.render_stateful_widget(table, area, &mut state);
}

fn table_row(app: &App, entry: &PortEntry) -> Row<'static> {
    let state = app.tunnel_state(entry.port);
    let tunnel = state.map_or("none", TunnelState::label);
    let tunnel_style = match state {
        Some(TunnelState::Active { .. }) => {
            Style::new().fg(app.accent()).add_modifier(Modifier::BOLD)
        }
        Some(TunnelState::Starting | TunnelState::Stopping) | None => dim_style(),
    };
    let marked = app.is_marked(entry);
    let mark_style = if marked {
        Style::new().fg(app.accent()).add_modifier(Modifier::BOLD)
    } else {
        dim_style()
    };

    Row::new(vec![
        Cell::from(if marked { "●" } else { " " }).style(mark_style),
        Cell::from(entry.port.to_string()).style(text_style().add_modifier(Modifier::BOLD)),
        Cell::from(entry.pid.to_string()).style(dim_style()),
        Cell::from(entry.process_name.clone()).style(text_style()),
        Cell::from(entry.context.text()).style(text_style()),
        Cell::from(format!("{:.1} MB", entry.memory_mb())).style(dim_style()),
        Cell::from(tunnel).style(tunnel_style),
    ])
}

fn text_style() -> Style {
    Style::new().fg(Color::Reset)
}
