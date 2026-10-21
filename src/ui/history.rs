use std::time::{Duration, SystemTime};

use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, HighlightSpacing, Paragraph, Row, Table, TableState,
};
use ratatui::Frame;

use super::dim_style;
use crate::app::{App, TunnelHistoryStatus};

const HISTORY_WIDTH: u16 = 112;
const HISTORY_HEIGHT: u16 = 18;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let width = HISTORY_WIDTH.min(area.width.saturating_sub(4));
    let desired_height = if app.tunnel_history().is_empty() {
        6
    } else {
        u16::try_from(app.tunnel_history().len())
            .unwrap_or(u16::MAX)
            .saturating_add(4)
    };
    let height = desired_height
        .min(HISTORY_HEIGHT)
        .min(area.height.saturating_sub(2));

    if width == 0 || height == 0 {
        return;
    }

    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(vertical);

    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(dim_style())
        .title(Span::styled(
            " tunnel history ",
            Style::new().fg(app.accent()).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(
            "[j/k] browse  [c] clear  [h] close",
            dim_style(),
        ));

    if app.tunnel_history().is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from("no tunnel history").alignment(Alignment::Center))
                .style(dim_style())
                .block(block),
            popup,
        );
        return;
    }

    let now = SystemTime::now();
    let rows: Vec<Row<'static>> = app
        .tunnel_history()
        .iter()
        .map(|entry| {
            let url = entry.url.as_deref().unwrap_or("-");
            Row::new(vec![
                Cell::from(format!(":{}", entry.port))
                    .style(Style::new().fg(Color::Reset).add_modifier(Modifier::BOLD)),
                Cell::from(entry.status.label()).style(status_style(entry.status, app.accent())),
                Cell::from(format!(
                    "{} ago",
                    format_duration(relative(now, entry.started_at))
                ))
                .style(dim_style()),
                Cell::from(format_duration(
                    entry
                        .ended_at
                        .unwrap_or(now)
                        .duration_since(entry.started_at)
                        .unwrap_or_default(),
                ))
                .style(dim_style()),
                Cell::from(url.to_owned()).style(Style::new().fg(Color::Reset)),
            ])
        })
        .collect();

    let header = Row::new(["PORT", "STATUS", "STARTED", "DURATION", "URL"].map(Cell::from)).style(
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let widths = [
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(10),
        Constraint::Fill(1),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(Style::new().fg(app.accent()).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always)
        .block(block);

    let mut state = TableState::new().with_selected(Some(app.history_selected()));
    frame.render_stateful_widget(table, popup, &mut state);
}

fn status_style(status: TunnelHistoryStatus, accent: Color) -> Style {
    match status {
        TunnelHistoryStatus::Active => Style::new().fg(accent).add_modifier(Modifier::BOLD),
        TunnelHistoryStatus::Failed => Style::new().fg(Color::Reset).add_modifier(Modifier::BOLD),
        TunnelHistoryStatus::Starting
        | TunnelHistoryStatus::Stopping
        | TunnelHistoryStatus::Stopped => dim_style(),
    }
}

fn relative(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_short_and_long_durations() {
        assert_eq!(format_duration(Duration::from_secs(9)), "9s");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m");
        assert_eq!(format_duration(Duration::from_secs(7_200)), "2h");
        assert_eq!(format_duration(Duration::from_secs(172_800)), "2d");
    }
}
