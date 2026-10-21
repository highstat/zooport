//! QR modal rendering.
//!
//! A terminal cell is roughly twice as tall as it is wide. Pairing two QR
//! module rows into one cell with the Unicode half-block characters therefore
//! restores a 1:1 module aspect ratio:
//!
//! ```text
//! top dark, bottom light -> ▀
//! top light, bottom dark -> ▄
//! both dark              -> █
//! both light             -> (space)
//! ```
//!
//! Dark modules are drawn over an explicit white background so the code scans
//! correctly on both light and dark terminal themes.

use qrcode::types::QrError;
use qrcode::{Color, QrCode};
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color as TermColor, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;

use super::dim_style;

const QUIET_ZONE_MODULES: usize = 4;
const MESSAGE_WIDTH: u16 = 52;
const MESSAGE_HEIGHT: u16 = 6;

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, port: u16, url: &str, accent: TermColor) {
    let rendered = match render(url) {
        Ok(rendered) => rendered,
        Err(error) => {
            render_message(
                frame,
                area,
                &format!("could not build QR code: {error}"),
                " qr error ",
                accent,
            );
            return;
        }
    };

    let content_width = u16::try_from(rendered.width).unwrap_or(u16::MAX);
    let content_height = u16::try_from(rendered.rows.len()).unwrap_or(u16::MAX);
    let popup_width = content_width.saturating_add(2);
    let popup_height = content_height.saturating_add(2);

    if popup_width > area.width || popup_height > area.height {
        render_message(
            frame,
            area,
            &format!("QR needs a {popup_width}x{popup_height} terminal; resize and try again"),
            " qr ",
            accent,
        );
        return;
    }

    let [vertical] = Layout::vertical([Constraint::Length(popup_height)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(popup_width)])
        .flex(Flex::Center)
        .areas(vertical);

    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(dim_style())
        .title(Span::styled(
            format!(" tunnel qr :{port} "),
            Style::new().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled("[q] close", dim_style()));

    let lines: Vec<Line<'static>> = rendered
        .rows
        .into_iter()
        .map(|row| {
            Line::from(Span::styled(
                row,
                Style::new().fg(TermColor::Black).bg(TermColor::White),
            ))
        })
        .collect();

    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

struct QrRender {
    rows: Vec<String>,
    /// Width in modules, including the quiet zone. This is also the character
    /// width of every row.
    width: usize,
}

fn render(url: &str) -> Result<QrRender, QrError> {
    let code = QrCode::new(url.as_bytes())?;
    let modules = code.to_colors();
    let module_width = code.width();
    let width = module_width + QUIET_ZONE_MODULES * 2;
    let mut rows = Vec::with_capacity(width.div_ceil(2));

    for top_y in (0..width).step_by(2) {
        let mut row = String::with_capacity(width);

        for x in 0..width {
            let top = module_is_dark(&modules, module_width, x, top_y);
            let bottom = top_y + 1 < width && module_is_dark(&modules, module_width, x, top_y + 1);

            row.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }

        rows.push(row);
    }

    Ok(QrRender { rows, width })
}

fn module_is_dark(modules: &[Color], module_width: usize, x: usize, y: usize) -> bool {
    let Some(module_x) = x.checked_sub(QUIET_ZONE_MODULES) else {
        return false;
    };
    let Some(module_y) = y.checked_sub(QUIET_ZONE_MODULES) else {
        return false;
    };

    if module_x >= module_width || module_y >= module_width {
        return false;
    }

    modules[module_y * module_width + module_x] == Color::Dark
}

fn render_message(
    frame: &mut Frame<'_>,
    area: Rect,
    message: &str,
    title: &str,
    accent: TermColor,
) {
    let width = MESSAGE_WIDTH.min(area.width);
    let height = MESSAGE_HEIGHT.min(area.height);

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
            title.to_owned(),
            Style::new().fg(accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled("[q] close", dim_style()));

    frame.render_widget(
        Paragraph::new(Line::from(message.to_owned()).alignment(Alignment::Center))
            .style(Style::new().fg(TermColor::Reset))
            .block(block),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_URL: &str = "https://calm-river-dawn.trycloudflare.com";

    #[test]
    fn renders_half_block_matrix_with_square_modules() {
        let rendered = render(TEST_URL).expect("QR generation");

        assert!(!rendered.rows.is_empty());
        assert_eq!(rendered.rows.len(), rendered.width.div_ceil(2));

        for row in &rendered.rows {
            assert_eq!(row.chars().count(), rendered.width);
            assert!(row
                .chars()
                .all(|character| matches!(character, ' ' | '▀' | '▄' | '█')));
        }

        // Two character rows cover the four-module quiet zone.
        let quiet_rows = QUIET_ZONE_MODULES.div_ceil(2);
        for row in &rendered.rows[..quiet_rows] {
            assert!(row.chars().all(|character| character == ' '));
        }
        for row in &rendered.rows[rendered.rows.len() - quiet_rows..] {
            assert!(row.chars().all(|character| character == ' '));
        }

        assert!(rendered
            .rows
            .iter()
            .any(|row| row.contains('▀') || row.contains('▄') || row.contains('█')));
    }

    #[test]
    fn matrix_width_stays_within_standard_qr_versions() {
        let rendered = render(TEST_URL).expect("QR generation");

        // QR versions 1..=40 are 21, 25, ... 177 modules wide. Adding the
        // 4-module quiet zone on both sides must preserve that arithmetic.
        assert!(rendered.width >= 21 + QUIET_ZONE_MODULES * 2);
        assert_eq!((rendered.width - QUIET_ZONE_MODULES * 2) % 4, 1);
    }
}
