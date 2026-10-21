//! zooport entry point and async event loop.
//!
//! Ownership split:
//!
//! * The main task owns `App` and the terminal.
//! * A scanner task owns socket/process discovery and sends owned snapshots.
//! * Each lifecycle action is a short-lived Tokio task that owns a cloned
//!   `PortEntry` and sends back an owned outcome.
//! * The tunnel manager owns every cloudflared child and emits owned tunnel
//!   state changes. Shutdown is awaited before the terminal is restored.
//!
//! No shared mutable state exists between tasks; channels move values instead.
//! That keeps render-time code free of locks and makes cancellation safe.

mod app;
mod clipboard;
mod config;
mod scanner;
mod tunnel;
mod ui;

use std::io;
use std::process::ExitCode;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::app::{run_lifecycle_batch, App, AppCommand, ExportOutcome, LifecycleBatchOutcome};
use crate::clipboard::{Clipboard, ClipboardHandle};
use crate::config::Config;
use crate::scanner::{ScanError, ScanSnapshot};
use crate::tunnel::{TunnelEvent, TunnelManager};

const FRAME_TICK: Duration = Duration::from_millis(33);
const SCAN_INTERVAL: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> ExitCode {
    // `ratatui::init` enters raw mode + the alternate screen and installs a
    // panic hook that restores the terminal before printing a panic.
    let mut terminal = ratatui::init();
    let result = run(&mut terminal).await;
    ratatui::restore();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("zooport: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let config = Config::load();
    let clipboard = Clipboard::start();
    // The manager owns all cloudflared children. This wrapper guarantees its
    // shutdown is awaited even when the event loop exits through an I/O error.
    let (tunnel_manager, mut tunnel_events) = TunnelManager::spawn(clipboard.handle());
    let result = event_loop(
        terminal,
        &tunnel_manager,
        &mut tunnel_events,
        clipboard.handle(),
        config,
    )
    .await;
    tunnel_manager.shutdown().await;
    clipboard.shutdown();
    result
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    tunnel_manager: &TunnelManager,
    tunnel_events: &mut mpsc::Receiver<TunnelEvent>,
    clipboard: ClipboardHandle,
    config: Config,
) -> io::Result<()> {
    let (refresh_tx, refresh_rx) = mpsc::channel::<()>(1);
    let (scan_tx, mut scan_rx) = mpsc::channel::<Result<ScanSnapshot, ScanError>>(2);
    let (action_tx, mut action_rx) = mpsc::channel::<LifecycleBatchOutcome>(4);
    let (export_tx, mut export_rx) = mpsc::channel::<ExportOutcome>(2);

    tokio::spawn(scanner_worker(refresh_rx, scan_tx));

    let mut app = App::with_config(config);

    // Render immediately so the header/table shell is visible while the first
    // scan is still running.
    terminal.draw(|frame| ui::draw(frame, &app))?;
    app.take_dirty();

    let mut ticker = tokio::time::interval(FRAME_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Input polling is intentionally non-blocking. The ticker
                // bounds input latency to one frame; `poll(Duration::ZERO)`
                // drains everything currently buffered without ever awaiting
                // a terminal read inside the render loop.
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            if let Some(command) = app.handle_key(key) {
                                match command {
                                    AppCommand::Refresh => {
                                        // Capacity is one; if a refresh is
                                        // already queued the worker coalesces it.
                                        let _ = refresh_tx.try_send(());
                                    }
                                    AppCommand::Lifecycle(requests) => {
                                        let sender = action_tx.clone();
                                        tokio::spawn(async move {
                                            let outcome = run_lifecycle_batch(requests).await;
                                            let _ = sender.send(outcome).await;
                                        });
                                    }
                                    AppCommand::Tunnel { port } => {
                                        if !tunnel_manager.toggle(port) {
                                            app.apply_tunnel(TunnelEvent::Failed {
                                                port,
                                                message: "tunnel manager is busy; try again"
                                                    .to_owned(),
                                            });
                                        }
                                    }
                                    AppCommand::Export { markdown, count } => {
                                        let sender = export_tx.clone();
                                        let clipboard = clipboard.clone();
                                        tokio::spawn(async move {
                                            let result = clipboard.copy(markdown).await;
                                            let _ = sender
                                                .send(ExportOutcome { count, result })
                                                .await;
                                        });
                                    }
                                }
                            }
                        }
                        Event::Resize(_, _) => app.mark_dirty(),
                        _ => {}
                    }

                    if app.should_quit() {
                        break;
                    }
                }
            }

            scan_result = scan_rx.recv() => {
                match scan_result {
                    Some(Ok(snapshot)) => app.apply_scan(snapshot),
                    Some(Err(error)) => app.apply_scan_error(error),
                    None => {
                        app.apply_scan_error(ScanError::Worker(
                            "scanner task stopped unexpectedly".to_owned(),
                        ));
                        break;
                    }
                }
            }

            outcome = action_rx.recv() => {
                if let Some(outcome) = outcome {
                    app.apply_lifecycle(outcome);
                    // The socket table changed; fetch a fresh snapshot rather
                    // than mutating rows optimistically.
                    app.request_refresh();
                    let _ = refresh_tx.try_send(());
                }
            }

            tunnel_event = tunnel_events.recv() => {
                let Some(event) = tunnel_event else {
                    break;
                };
                app.apply_tunnel(event);
            }

            export_outcome = export_rx.recv() => {
                if let Some(outcome) = export_outcome {
                    app.apply_export(outcome);
                }
            }
        }

        if app.should_quit() {
            break;
        }

        if app.take_dirty() {
            terminal.draw(|frame| ui::draw(frame, &app))?;
        }
    }

    Ok(())
}

/// One long-lived scanner task.
///
/// It scans immediately, then every `SCAN_INTERVAL`, or when the UI sends a
/// refresh request. Refreshes are coalesced: a burst of F5 presses can never
/// start overlapping scans or queue unbounded work.
async fn scanner_worker(
    mut refresh_rx: mpsc::Receiver<()>,
    scan_tx: mpsc::Sender<Result<ScanSnapshot, ScanError>>,
) {
    let mut ticker = tokio::time::interval(SCAN_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            refresh = refresh_rx.recv() => {
                if refresh.is_none() {
                    break;
                }
                while refresh_rx.try_recv().is_ok() {}
            }
        }

        // `scanner::scan` runs the kernel/sysinfo work in `spawn_blocking`
        // internally and sends an owned snapshot, so this task never shares
        // process-table state with the render loop.
        let result = scanner::scan().await;
        if scan_tx.send(result).await.is_err() {
            break;
        }
    }
}
