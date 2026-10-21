//! Rendering layer.
//!
//! The UI is intentionally a pure function of `&App`. It never spawns tasks,
//! never mutates state, and never holds a lock. The event loop draws only when
//! `App` reports a dirty frame.

mod history;
mod qr;
mod table;

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, StatusLevel, TunnelState};

const MIN_WIDTH: u16 = 76;
const MIN_HEIGHT: u16 = 13;
const HELP_WIDTH: u16 = 66;
const HELP_HEIGHT: u16 = 22;
const NOTICE_WIDTH: u16 = 84;
const NOTICE_HEIGHT: u16 = 8;

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "terminal too small (minimum {MIN_WIDTH}x{MIN_HEIGHT})"
            ))
            .alignment(Alignment::Center)
            .style(dim_style()),
            area,
        );
        return;
    }

    let [header, body, inspect, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(area);

    render_header(frame, header, app);
    table::draw(frame, body, app);
    render_inspect(frame, inspect, app);
    render_status(frame, status, app);
    render_footer(frame, footer, app.accent());

    if let Some(notice) = app.notice() {
        render_notice(frame, area, notice, app.accent());
    } else if app.help_visible() {
        render_help(frame, area, app.accent());
    } else if let Some((port, url)) = app.qr_tunnel() {
        qr::draw(frame, area, port, url, app.accent());
    } else if app.history_visible() {
        history::draw(frame, area, app);
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(9)]).areas(area);

    let mut title = vec![
        Span::styled("ZOOPORT", text_style().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(format!("({} listening)", app.total_len()), dim_style()),
    ];
    if app.marked_count() > 0 {
        title.push(Span::raw("  "));
        title.push(Span::styled(
            format!("({} selected)", app.marked_count()),
            accent_style(app.accent()).add_modifier(Modifier::BOLD),
        ));
    }
    let title = Line::from(title);

    frame.render_widget(Paragraph::new(title), left);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled("[?] Help", text_style())))
            .alignment(Alignment::Right),
        right,
    );
}

fn render_inspect(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (cmd, cwd) = match app.selected_entry() {
        Some(entry) => (entry.cmdline_string(), entry.cwd_display()),
        None => ("-".to_owned(), "-".to_owned()),
    };
    let (tunnel, tunnel_style) = tunnel_text(app.selected_tunnel(), app.accent());

    let lines = vec![
        labeled_line("cmd", cmd, text_style()),
        labeled_line("cwd", cwd, text_style()),
        labeled_line("tunnel", tunnel, tunnel_style),
    ];

    let block = Block::new()
        .borders(Borders::TOP)
        .border_type(BorderType::Plain)
        .border_style(dim_style())
        .title(Span::styled(
            " inspect ",
            accent_style(app.accent()).add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn tunnel_text(state: Option<&TunnelState>, accent: Color) -> (String, Style) {
    match state {
        Some(TunnelState::Active { url }) => (
            url.clone(),
            accent_style(accent).add_modifier(Modifier::BOLD),
        ),
        Some(TunnelState::Starting) => ("starting...".to_owned(), dim_style()),
        Some(TunnelState::Stopping) => ("stopping...".to_owned(), dim_style()),
        None => ("none".to_owned(), dim_style()),
    }
}

fn labeled_line(label: &'static str, value: String, value_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("> {label}: "), dim_style()),
        Span::styled(value, value_style),
    ])
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(32)]).areas(area);

    let (text, style) = if app.is_filtering() {
        (
            format!("filter> {}_", app.filter()),
            text_style().add_modifier(Modifier::BOLD),
        )
    } else if let Some(busy) = app.busy() {
        (
            format!("{} in progress...", busy.description()),
            text_style(),
        )
    } else if let Some(error) = app.scan_error() {
        (
            format!("scan error: {error}"),
            text_style().add_modifier(Modifier::BOLD),
        )
    } else if let Some(status) = app.status() {
        (
            status.text.clone(),
            match status.level {
                StatusLevel::Info | StatusLevel::Warning => text_style(),
                StatusLevel::Error => text_style().add_modifier(Modifier::BOLD),
            },
        )
    } else if app.is_refreshing() {
        ("refreshing sockets...".to_owned(), dim_style())
    } else {
        ("ready".to_owned(), dim_style())
    };

    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), left);

    let scan_age = app
        .last_scan()
        .map(|instant| format!("scan {}s ago", instant.elapsed().as_secs()))
        .unwrap_or_else(|| "scanning...".to_owned());
    let docker = if app.docker_available() { "on" } else { "off" };
    let right_text = if app.is_refreshing() {
        format!("refreshing | docker:{docker}")
    } else {
        format!("{scan_age} | docker:{docker}")
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(right_text, dim_style())))
            .alignment(Alignment::Right),
        right,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, accent: Color) {
    let key = |label: &'static str| {
        Span::styled(label, accent_style(accent).add_modifier(Modifier::BOLD))
    };
    let hint = |label: &'static str| Span::styled(label, dim_style());

    let lines = vec![
        Line::from(vec![
            key("[j/k]"),
            hint(" nav  "),
            key("[Space]"),
            hint(" select  "),
            key("[x]"),
            hint(" kill  "),
            key("[r]"),
            hint(" restart  "),
            key("[t]"),
            hint(" tunnel  "),
            key("[R]"),
            hint(" refresh"),
        ]),
        Line::from(vec![
            key("[e]"),
            hint(" export  "),
            key("[h]"),
            hint(" history  "),
            key("[/]"),
            hint(" filter  "),
            key("[q]"),
            hint(" qr-code  "),
            key("[?]"),
            hint(" help"),
        ]),
    ];

    frame.render_widget(
        Paragraph::new(lines).style(Style::new().fg(Color::DarkGray)),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect, accent: Color) {
    let height = HELP_HEIGHT.min(area.height);
    let width = HELP_WIDTH.min(area.width);

    if width == 0 || height == 0 {
        return;
    }

    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);

    frame.render_widget(Clear, popup);

    let section = |text: &'static str| {
        Line::from(Span::styled(
            text,
            text_style().add_modifier(Modifier::BOLD),
        ))
    };
    let item = |keys: &'static str, description: &'static str| {
        Line::from(vec![
            Span::styled(format!("{keys:<18}"), text_style()),
            Span::styled(description, dim_style()),
        ])
    };

    let lines = vec![
        section("NAVIGATE"),
        item("j / k, up / down", "move selection"),
        item("g / G, home / end", "first / last port"),
        item("Space", "mark / unmark selected port"),
        section("PROCESS"),
        item("x", "terminate marked ports or selection"),
        item("r", "restart with original cmd/cwd/env"),
        section("TUNNEL"),
        item("t", "toggle cloudflared quick tunnel"),
        item("q", "show QR for active tunnel"),
        section("EXPORT"),
        item("e", "copy filtered ports as Markdown"),
        section("HISTORY"),
        item("h", "open tunnel history"),
        item("c", "clear history while it is open"),
        section("VIEW"),
        item("/", "filter (docker:, python:, rust:)"),
        item("F5 / R", "refresh now"),
        item("Esc / Ctrl-C", "exit filter or quit"),
        item("? / Esc", "close this help"),
    ];

    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(dim_style())
        .title(Span::styled(
            " help ",
            accent_style(accent).add_modifier(Modifier::BOLD),
        ));

    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

fn render_notice(frame: &mut Frame<'_>, area: Rect, message: &str, accent: Color) {
    use ratatui::widgets::Wrap;

    let width = NOTICE_WIDTH.min(area.width);
    let height = NOTICE_HEIGHT.min(area.height);

    if width == 0 || height == 0 {
        return;
    }

    let [vertical] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [popup] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(vertical);

    frame.render_widget(Clear, popup);

    let block = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(dim_style())
        .title(Span::styled(
            " notice ",
            accent_style(accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled("[Esc] close", dim_style()));

    let lines = vec![
        Line::raw(""),
        Line::from(message.to_owned())
            .alignment(Alignment::Center)
            .style(text_style().add_modifier(Modifier::BOLD)),
        Line::raw(""),
        Line::from("[Enter] dismiss")
            .alignment(Alignment::Center)
            .style(dim_style()),
    ];

    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        popup,
    );
}

fn text_style() -> Style {
    Style::new().fg(Color::Reset)
}

fn accent_style(accent: Color) -> Style {
    Style::new().fg(accent)
}

pub(super) fn dim_style() -> Style {
    Style::new().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::config::{Config, DEFAULT_ACCENT};
    use crate::scanner::{PortEntry, RuntimeContext, RuntimeKind, ScanSnapshot, Transport};
    use crate::tunnel::TunnelEvent;

    fn port_entry(port: u16, pid: u32, name: &str, kind: RuntimeKind) -> PortEntry {
        PortEntry {
            port,
            pid,
            process_name: name.to_owned(),
            context: RuntimeContext { kind, detail: None },
            memory_bytes: 64 * 1024 * 1024,
            cmdline: vec![name.to_owned(), "--serve".to_owned()],
            cwd: Some(PathBuf::from("/tmp/zooport-test")),
            exe: None,
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            transport: Transport::Tcp,
        }
    }

    fn sample_app() -> App {
        let mut app = App::new();
        app.apply_scan(ScanSnapshot {
            entries: vec![
                port_entry(3000, 42, "node", RuntimeKind::Node),
                port_entry(8080, 43, "docker-proxy", RuntimeKind::Docker),
            ],
            generated_at: Instant::now(),
            docker_available: true,
        });
        app
    }

    fn render_text(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw succeeds");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_shell_table_and_inspection() {
        let app = sample_app();
        let text = render_text(&app, 100, 24);

        assert!(text.contains("ZOOPORT"));
        assert!(text.contains("(2 listening)"));
        assert!(text.contains("PORT"));
        assert!(text.contains("PROCESS"));
        assert!(text.contains("node"));
        assert!(text.contains("docker-proxy"));
        assert!(text.contains("inspect"));
        assert!(text.contains("/tmp/zooport-test"));
        assert!(text.contains("[j/k] nav"));
        assert!(text.contains("[q] qr-code"));
    }

    #[test]
    fn opens_help_overlay() {
        let mut app = sample_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));

        let text = render_text(&app, 100, 24);

        assert!(text.contains("NAVIGATE"));
        assert!(text.contains("restart with original cmd/cwd/env"));
        assert!(text.contains("toggle cloudflared quick tunnel"));
        assert!(text.contains("close this help"));
    }

    #[test]
    fn renders_selection_count_and_marked_row_marker() {
        let mut app = sample_app();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        let text = render_text(&app, 100, 24);

        assert!(text.contains("(1 selected)"));
        assert!(text.contains("●"));
    }

    #[test]
    fn custom_accent_from_config_is_used_everywhere() {
        let config = Config::from_toml("accent = \"#3366ff\"\n");
        let mut app = App::with_config(config);
        app.apply_scan(ScanSnapshot {
            entries: vec![port_entry(3000, 42, "node", RuntimeKind::Node)],
            generated_at: Instant::now(),
            docker_available: false,
        });

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw succeeds");

        assert!(terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .any(|cell| cell.fg == Color::Rgb(51, 102, 255)));
    }

    #[test]
    fn renders_tunnel_history_modal() {
        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Starting { port: 3000 });
        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });
        app.apply_tunnel(TunnelEvent::Stopped {
            port: 3000,
            message: None,
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));

        let text = render_text(&app, 120, 32);

        assert!(text.contains("tunnel history"));
        assert!(text.contains(":3000"));
        assert!(text.contains("stopped"));
        assert!(text.contains("https://calm-river.trycloudflare.com"));
        assert!(text.contains("[c] clear"));
    }

    #[test]
    fn renders_missing_cloudflared_notice() {
        use crate::app::CLOUDFLARED_MISSING_NOTICE;

        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::CloudflaredMissing { port: 3000 });

        let text = render_text(&app, 120, 32);

        assert!(text.contains(CLOUDFLARED_MISSING_NOTICE));
        assert!(text.contains("[Esc] close"));
        assert!(text.contains("[Enter] dismiss"));
    }

    #[test]
    fn palette_is_transparent_and_uses_only_one_warm_accent() {
        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw succeeds");

        let allowed = [Color::Reset, Color::DarkGray, DEFAULT_ACCENT];
        for cell in terminal.backend().buffer().content() {
            assert!(
                allowed.contains(&cell.fg),
                "unexpected foreground {:?} for {:?}",
                cell.fg,
                cell.symbol()
            );
            assert_eq!(
                cell.bg,
                Color::Reset,
                "explicit background on {:?}",
                cell.symbol()
            );
        }
    }

    #[test]
    fn active_tunnel_text_uses_the_warm_accent() {
        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Active {
            port: 8080,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });

        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw succeeds");

        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);
        let tunnel_row = buffer
            .content()
            .chunks(width)
            .find(|row| row_text(row).contains("8080"))
            .expect("tunnel row");

        assert!(tunnel_row.iter().any(|cell| cell.fg == DEFAULT_ACCENT));
    }

    #[test]
    fn overlays_keep_the_terminal_background() {
        let mut app = sample_app();
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        assert_transparent_background(&app);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.apply_tunnel(TunnelEvent::CloudflaredMissing { port: 3000 });
        assert_transparent_background(&app);

        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Starting { port: 3000 });
        app.handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_transparent_background(&app);
    }

    fn assert_transparent_background(app: &App) {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app))
            .expect("draw succeeds");

        for cell in terminal.backend().buffer().content() {
            assert_eq!(
                cell.bg,
                Color::Reset,
                "explicit background on {:?}",
                cell.symbol()
            );
        }
    }

    #[test]
    fn selection_uses_text_color_without_a_background_bar() {
        let app = sample_app();
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("draw succeeds");

        let buffer = terminal.backend().buffer();
        let width = usize::from(buffer.area.width);

        let selected_row = buffer
            .content()
            .chunks(width)
            .find(|row| row_text(row).contains("3000"))
            .expect("selected row");
        let unselected_row = buffer
            .content()
            .chunks(width)
            .find(|row| row_text(row).contains("8080"))
            .expect("unselected row");

        assert!(selected_row.iter().any(|cell| cell.fg == DEFAULT_ACCENT));
        assert!(selected_row.iter().all(|cell| cell.bg == Color::Reset));
        assert!(unselected_row.iter().any(|cell| cell.fg == Color::Reset));
        assert!(unselected_row.iter().all(|cell| cell.bg == Color::Reset));
    }

    fn row_text(row: &[ratatui::buffer::Cell]) -> String {
        row.iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn renders_tunnel_starting_state() {
        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Starting { port: 3000 });

        let text = render_text(&app, 100, 24);

        assert!(text.contains("starting..."));
    }

    #[test]
    fn qr_falls_back_gracefully_when_terminal_is_too_short() {
        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        let text = render_text(&app, 76, 13);

        assert!(text.contains("QR needs a"));
        assert!(text.contains("[q] close"));
    }

    #[test]
    fn renders_active_tunnel_and_qr_modal() {
        let mut app = sample_app();
        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

        let text = render_text(&app, 100, 32);

        assert!(text.contains("active"));
        assert!(text.contains("https://calm-river.trycloudflare.com"));
        assert!(text.contains("tunnel qr :3000"));
        assert!(text
            .chars()
            .any(|character| matches!(character, '▀' | '▄' | '█')));
    }

    #[test]
    fn warns_when_terminal_is_too_small() {
        let app = sample_app();
        let text = render_text(&app, 40, 8);

        assert!(text.contains("terminal too small"));
    }
}
