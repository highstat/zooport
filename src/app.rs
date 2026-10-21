//! Application state, key handling, and process lifecycle control.
//!
//! # Async ownership model
//!
//! `App` is owned by the event loop thread and is never shared. Background work
//! communicates through owned messages on `tokio::sync::mpsc` channels:
//!
//! * The scanner task owns its `System`, builds an owned `ScanSnapshot`, and
//!   sends it to the UI. No lock is held while rendering.
//! * Lifecycle tasks receive an owned `PortEntry` clone and return an owned
//!   `LifecycleOutcome`. They never borrow `App`, so a slow `SIGTERM` wait can
//!   never block navigation or rendering.
//!
//! This is the Rust equivalent of the Elm "message" half of Model-View-Update:
//! the model is `App`, messages are channel values, and `handle_key` /
//! `apply_*` are the only places that mutate state.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use listeners::SocketState;
use ratatui::style::Color;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::config::Config;
use crate::scanner::{PortEntry, RuntimeKind, ScanError, ScanSnapshot};
use crate::tunnel::TunnelEvent;

/// How long a process gets to exit after SIGTERM before SIGKILL is sent.
const GRACEFUL_TIMEOUT: Duration = Duration::from_millis(1_500);
/// How long SIGKILL gets to release the socket.
const FORCE_TIMEOUT: Duration = Duration::from_millis(1_500);
/// Socket/probe polling interval.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_HISTORY_ENTRIES: usize = 100;

pub const CLOUDFLARED_MISSING_NOTICE: &str =
    "cloudflared not found in PATH. Please install it (e.g. brew install cloudflared).";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub level: StatusLevel,
}

impl Status {
    fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: StatusLevel::Info,
        }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: StatusLevel::Warning,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            level: StatusLevel::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleAction {
    Kill,
    Restart,
}

impl LifecycleAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Kill => "kill",
            Self::Restart => "restart",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyAction {
    pub action: LifecycleAction,
    pub ports: Vec<u16>,
}

impl BusyAction {
    pub fn description(&self) -> String {
        match self.ports.as_slice() {
            [port] => format!("{} :{port}", self.action.label()),
            ports => format!("{} {} ports", self.action.label(), ports.len()),
        }
    }
}

/// UI-side mirror of the tunnel manager's state for one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Starting,
    Active { url: String },
    Stopping,
}

impl TunnelState {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting...",
            Self::Active { .. } => "active",
            Self::Stopping => "stopping...",
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Active { url } => Some(url),
            Self::Starting | Self::Stopping => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelHistoryStatus {
    Starting,
    Active,
    Stopping,
    Stopped,
    Failed,
}

impl TunnelHistoryStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Active => "active",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelHistoryEntry {
    pub port: u16,
    pub url: Option<String>,
    pub status: TunnelHistoryStatus,
    pub started_at: SystemTime,
    pub ended_at: Option<SystemTime>,
}

/// UI-originated work that must run outside the event loop.
///
/// The `PortEntry` is cloned into the request. That clone is the ownership
/// boundary: once the command leaves `App`, no `&App` reference exists, so the
/// async worker can run for seconds without aliasing UI state.
#[derive(Debug)]
pub enum AppCommand {
    Refresh,
    Lifecycle(Vec<LifecycleRequest>),
    Tunnel { port: u16 },
    Export { markdown: String, count: usize },
}

#[derive(Debug)]
pub struct LifecycleRequest {
    pub action: LifecycleAction,
    pub entry: PortEntry,
}

#[derive(Debug)]
pub struct LifecycleOutcome {
    pub action: LifecycleAction,
    pub port: u16,
    pub pid: u32,
    pub result: Result<String, LifecycleError>,
}

#[derive(Debug)]
pub struct LifecycleBatchOutcome {
    pub action: LifecycleAction,
    pub outcomes: Vec<LifecycleOutcome>,
    pub worker_errors: Vec<String>,
}

#[derive(Debug)]
pub struct ExportOutcome {
    pub count: usize,
    pub result: Result<(), String>,
}

#[derive(Debug)]
pub struct App {
    entries: Vec<PortEntry>,
    /// Indices into `entries` that pass the current filter. Keeping this
    /// separate avoids cloning entries just to render a filtered table.
    visible: Vec<usize>,
    selected: usize,
    marks: HashSet<(u16, u32)>,
    filter: String,
    input_mode: InputMode,
    status: Option<Status>,
    busy: Option<BusyAction>,
    refreshing: bool,
    exporting: bool,
    docker_available: bool,
    last_scan: Option<Instant>,
    scan_error: Option<String>,
    tunnels: HashMap<u16, TunnelState>,
    history: Vec<TunnelHistoryEntry>,
    history_visible: bool,
    history_selected: usize,
    qr_port: Option<u16>,
    help_visible: bool,
    notice: Option<String>,
    config: Config,
    dirty: bool,
    quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        Self {
            entries: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            marks: HashSet::new(),
            filter: String::new(),
            input_mode: InputMode::Normal,
            status: None,
            busy: None,
            refreshing: true,
            exporting: false,
            docker_available: false,
            last_scan: None,
            scan_error: None,
            tunnels: HashMap::new(),
            history: Vec::new(),
            history_visible: false,
            history_selected: 0,
            qr_port: None,
            help_visible: false,
            notice: None,
            config,
            dirty: true,
            quit: false,
        }
    }

    // ---------------------------------------------------------------------
    // Model updates
    // ---------------------------------------------------------------------

    pub fn apply_scan(&mut self, snapshot: ScanSnapshot) {
        // Remember the selected process identity, not its row index: rows move
        // when ports open/close or when the filter changes.
        let selected = self.selected_key();

        let mut entries = snapshot.entries;
        entries.retain(|entry| !self.config.ignores(entry));
        let present: HashSet<(u16, u32)> = entries
            .iter()
            .map(|entry| (entry.port, entry.pid))
            .collect();
        self.marks.retain(|key| present.contains(key));

        self.entries = entries;
        self.docker_available = snapshot.docker_available;
        self.last_scan = Some(snapshot.generated_at);
        self.scan_error = None;
        self.refreshing = false;
        self.rebuild_view(selected);
        self.dirty = true;
    }

    pub fn apply_scan_error(&mut self, error: ScanError) {
        self.scan_error = Some(error.to_string());
        self.refreshing = false;
        self.dirty = true;
    }

    pub fn apply_tunnel(&mut self, event: TunnelEvent) {
        match event {
            TunnelEvent::Starting { port } => {
                self.tunnels.insert(port, TunnelState::Starting);
                self.start_tunnel_history(port);
                self.status = Some(Status::info(format!("tunnel :{port} starting...")));
            }
            TunnelEvent::Stopping { port } => {
                self.tunnels.insert(port, TunnelState::Stopping);
                self.update_tunnel_history(port, TunnelHistoryStatus::Stopping);
                self.status = Some(Status::info(format!("tunnel :{port} stopping...")));
            }
            TunnelEvent::Active {
                port,
                url,
                clipboard_error,
            } => {
                self.set_tunnel_active(port, &url);
                self.status = Some(match clipboard_error {
                    Some(error) => Status::warning(format!(
                        "tunnel :{port} active; clipboard unavailable: {error}"
                    )),
                    None => Status::info(format!("tunnel :{port} active; URL copied to clipboard")),
                });
            }
            TunnelEvent::Stopped { port, message } => {
                self.tunnels.remove(&port);
                if self.qr_port == Some(port) {
                    self.qr_port = None;
                }
                self.finish_tunnel_history(port, TunnelHistoryStatus::Stopped);
                self.status = Some(match message {
                    Some(message) => Status::warning(format!("tunnel :{port} stopped: {message}")),
                    None => Status::info(format!("tunnel :{port} stopped")),
                });
            }
            TunnelEvent::CloudflaredMissing { port } => {
                self.tunnels.remove(&port);
                self.qr_port = None;
                self.help_visible = false;
                self.finish_tunnel_history(port, TunnelHistoryStatus::Failed);
                self.notice = Some(CLOUDFLARED_MISSING_NOTICE.to_owned());
                self.status = Some(Status::error(format!(
                    "tunnel :{port} unavailable: cloudflared not found"
                )));
            }
            TunnelEvent::Failed { port, message } => {
                self.tunnels.remove(&port);
                if self.qr_port == Some(port) {
                    self.qr_port = None;
                }
                self.finish_tunnel_history(port, TunnelHistoryStatus::Failed);
                self.status = Some(Status::error(format!("tunnel :{port} failed: {message}")));
            }
        }

        self.clamp_history_selection();
        self.dirty = true;
    }

    pub fn apply_lifecycle(&mut self, batch: LifecycleBatchOutcome) {
        self.busy = None;
        self.status = Some(summarize_lifecycle(&batch));
        self.dirty = true;
    }

    pub fn apply_export(&mut self, outcome: ExportOutcome) {
        self.exporting = false;
        self.status = Some(match outcome.result {
            Ok(()) => Status::info(format!(
                "copied {} port{} to clipboard as Markdown",
                outcome.count,
                if outcome.count == 1 { "" } else { "s" }
            )),
            Err(error) => Status::error(format!("clipboard export failed: {error}")),
        });
        self.dirty = true;
    }

    fn start_tunnel_history(&mut self, port: u16) {
        if self
            .history
            .iter()
            .any(|entry| entry.port == port && entry.ended_at.is_none())
        {
            self.update_tunnel_history(port, TunnelHistoryStatus::Starting);
            return;
        }

        self.history.insert(
            0,
            TunnelHistoryEntry {
                port,
                url: None,
                status: TunnelHistoryStatus::Starting,
                started_at: SystemTime::now(),
                ended_at: None,
            },
        );
        self.history.truncate(MAX_HISTORY_ENTRIES);
        self.history_selected = 0;
    }

    fn set_tunnel_active(&mut self, port: u16, url: &str) {
        self.tunnels.insert(
            port,
            TunnelState::Active {
                url: url.to_owned(),
            },
        );

        if !self
            .history
            .iter()
            .any(|entry| entry.port == port && entry.ended_at.is_none())
        {
            self.start_tunnel_history(port);
        }

        if let Some(entry) = self.open_tunnel_history(port) {
            entry.url = Some(url.to_owned());
            entry.status = TunnelHistoryStatus::Active;
            entry.ended_at = None;
        }
    }

    fn update_tunnel_history(&mut self, port: u16, status: TunnelHistoryStatus) {
        if let Some(entry) = self.open_tunnel_history(port) {
            entry.status = status;
        }
    }

    fn finish_tunnel_history(&mut self, port: u16, status: TunnelHistoryStatus) {
        if let Some(entry) = self.open_tunnel_history(port) {
            entry.status = status;
            entry.ended_at = Some(SystemTime::now());
        }
    }

    fn open_tunnel_history(&mut self, port: u16) -> Option<&mut TunnelHistoryEntry> {
        self.history
            .iter_mut()
            .find(|entry| entry.port == port && entry.ended_at.is_none())
    }

    fn clamp_history_selection(&mut self) {
        self.history_selected = self
            .history_selected
            .min(self.history.len().saturating_sub(1));
    }

    pub fn request_refresh(&mut self) {
        self.refreshing = true;
        self.dirty = true;
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    // ---------------------------------------------------------------------
    // Key handling
    // ---------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl-C is the one binding that works in every mode, including while
        // a modal is open. Cyrillic `с` is included because some terminals
        // report the layout character instead of the physical C key.
        if is_ctrl_c(&key) {
            self.quit = true;
            return None;
        }

        if self.notice.is_some() {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                self.notice = None;
                self.dirty = true;
            }
            return None;
        }

        if self.help_visible {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                self.help_visible = false;
                self.dirty = true;
            }
            return None;
        }

        // The QR modal is a focused view. It deliberately swallows navigation
        // so the displayed code always matches the port named in its title.
        if self.qr_port.is_some() {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                self.qr_port = None;
                self.dirty = true;
            }
            return None;
        }

        if self.history_visible {
            return self.handle_history_key(key);
        }

        if self.input_mode == InputMode::Filter {
            return self.handle_filter_key(key, ctrl);
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::Home | KeyCode::Char('g') => self.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.select_last(),
            KeyCode::Char(' ') => self.toggle_mark(),
            KeyCode::Char('x') => return self.begin_lifecycle(LifecycleAction::Kill),
            KeyCode::Char('r') => return self.begin_lifecycle(LifecycleAction::Restart),
            KeyCode::Char('t') => return self.begin_tunnel_toggle(),
            KeyCode::Char('e') => return self.begin_export(),
            KeyCode::Char('h') => self.open_history(),
            KeyCode::Char('q') => self.open_qr(),
            KeyCode::Char('R') | KeyCode::F(5) => {
                self.request_refresh();
                return Some(AppCommand::Refresh);
            }
            KeyCode::Char('/') => self.begin_filter(),
            KeyCode::Char('?') => {
                self.help_visible = true;
                self.dirty = true;
            }
            KeyCode::Esc => {
                // Esc clears an applied filter first; a second Esc quits.
                if self.filter.is_empty() {
                    self.quit = true;
                } else {
                    self.set_filter(String::new());
                }
            }
            _ => {}
        }

        None
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> Option<AppCommand> {
        match key.code {
            KeyCode::Char('h') | KeyCode::Esc => {
                self.history_visible = false;
                self.dirty = true;
            }
            KeyCode::Char('j') | KeyCode::Down => self.select_history_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_history_previous(),
            KeyCode::Home | KeyCode::Char('g') => self.select_history_first(),
            KeyCode::End | KeyCode::Char('G') => self.select_history_last(),
            KeyCode::Char('c') => self.clear_history(),
            _ => {}
        }

        None
    }

    fn open_history(&mut self) {
        self.history_visible = true;
        self.clamp_history_selection();
        self.dirty = true;
    }

    fn select_history_next(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.history_selected = (self.history_selected + 1) % self.history.len();
        self.dirty = true;
    }

    fn select_history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.history_selected = if self.history_selected == 0 {
            self.history.len() - 1
        } else {
            self.history_selected - 1
        };
        self.dirty = true;
    }

    fn select_history_first(&mut self) {
        self.history_selected = 0;
        self.dirty = true;
    }

    fn select_history_last(&mut self) {
        self.history_selected = self.history.len().saturating_sub(1);
        self.dirty = true;
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.history_selected = 0;
        self.status = Some(Status::info("tunnel history cleared"));
        self.dirty = true;
    }

    fn toggle_mark(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        let key = (entry.port, entry.pid);

        if !self.marks.insert(key) {
            self.marks.remove(&key);
        }
        self.dirty = true;
    }

    fn handle_filter_key(&mut self, key: KeyEvent, ctrl: bool) -> Option<AppCommand> {
        match key.code {
            KeyCode::Esc => self.clear_filter_and_exit(),
            KeyCode::Enter => self.finish_filter(),
            KeyCode::Backspace => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.set_filter(filter);
            }
            KeyCode::Char('u') if ctrl => self.set_filter(String::new()),
            KeyCode::Char(c) if !ctrl => {
                let mut filter = self.filter.clone();
                filter.push(c);
                self.set_filter(filter);
            }
            _ => {}
        }

        None
    }

    fn begin_filter(&mut self) {
        self.input_mode = InputMode::Filter;
        self.dirty = true;
    }

    fn finish_filter(&mut self) {
        self.input_mode = InputMode::Normal;
        self.dirty = true;
    }

    fn clear_filter_and_exit(&mut self) {
        self.input_mode = InputMode::Normal;
        self.set_filter(String::new());
        self.dirty = true;
    }

    fn set_filter(&mut self, filter: String) {
        if self.filter == filter {
            return;
        }

        let selected = self.selected_key();
        self.filter = filter;
        self.rebuild_view(selected);
        self.dirty = true;
    }

    fn begin_lifecycle(&mut self, action: LifecycleAction) -> Option<AppCommand> {
        if self.busy.is_some() {
            self.status = Some(Status::warning("another lifecycle action is running"));
            self.dirty = true;
            return None;
        }

        let targets: Vec<PortEntry> = if self.marks.is_empty() {
            vec![self.selected_entry()?.clone()]
        } else {
            self.entries
                .iter()
                .filter(|entry| self.marks.contains(&(entry.port, entry.pid)))
                .cloned()
                .collect()
        };

        if targets.is_empty() {
            return None;
        }

        let ports = targets.iter().map(|entry| entry.port).collect();
        let requests = targets
            .into_iter()
            .map(|entry| LifecycleRequest { action, entry })
            .collect();

        self.busy = Some(BusyAction { action, ports });
        self.marks.clear();
        self.status = None;
        self.dirty = true;

        Some(AppCommand::Lifecycle(requests))
    }

    fn begin_export(&mut self) -> Option<AppCommand> {
        if self.exporting {
            self.status = Some(Status::warning("an export is already in progress"));
            self.dirty = true;
            return None;
        }

        let count = self.visible.len();
        if count == 0 {
            self.status = Some(Status::warning("nothing to export"));
            self.dirty = true;
            return None;
        }

        self.exporting = true;
        self.status = Some(Status::info(format!(
            "copying {count} port{} to clipboard...",
            if count == 1 { "" } else { "s" }
        )));
        self.dirty = true;

        Some(AppCommand::Export {
            markdown: self.export_markdown(),
            count,
        })
    }

    fn export_markdown(&self) -> String {
        let mut lines = vec![
            "| PORT | PID | PROCESS | CONTEXT | MEMORY | TUNNEL |".to_owned(),
            "| ---: | ---: | --- | --- | ---: | --- |".to_owned(),
        ];

        for entry in self.visible_entries() {
            let tunnel = match self.tunnels.get(&entry.port) {
                Some(TunnelState::Active { url }) => url.clone(),
                Some(state) => state.label().to_owned(),
                None => "-".to_owned(),
            };

            lines.push(format!(
                "| {} | {} | {} | {} | {:.1} MB | {} |",
                entry.port,
                entry.pid,
                markdown_cell(&entry.process_name),
                markdown_cell(&entry.context.text()),
                entry.memory_mb(),
                markdown_cell(&tunnel)
            ));
        }

        lines.join("\n")
    }

    fn begin_tunnel_toggle(&mut self) -> Option<AppCommand> {
        let port = self.selected_entry()?.port;

        match self.tunnels.get(&port) {
            Some(TunnelState::Starting | TunnelState::Active { .. }) => {
                self.tunnels.insert(port, TunnelState::Stopping);
                self.status = Some(Status::info(format!("tunnel :{port} stopping...")));
            }
            Some(TunnelState::Stopping) => {
                self.status = Some(Status::warning(format!(
                    "tunnel :{port} is already stopping"
                )));
                self.dirty = true;
                return None;
            }
            None => {
                self.tunnels.insert(port, TunnelState::Starting);
                self.status = Some(Status::info(format!("tunnel :{port} starting...")));
            }
        }

        self.dirty = true;
        Some(AppCommand::Tunnel { port })
    }

    fn open_qr(&mut self) {
        let Some(port) = self.selected_entry().map(|entry| entry.port) else {
            return;
        };

        match self.tunnels.get(&port) {
            Some(TunnelState::Active { .. }) => {
                self.qr_port = Some(port);
                self.dirty = true;
            }
            Some(TunnelState::Starting) => {
                self.status = Some(Status::warning(format!("tunnel :{port} is still starting")));
                self.dirty = true;
            }
            Some(TunnelState::Stopping) => {
                self.status = Some(Status::warning(format!("tunnel :{port} is stopping")));
                self.dirty = true;
            }
            None => {
                self.status = Some(Status::warning(
                    "no active tunnel for this port; press t to start one",
                ));
                self.dirty = true;
            }
        }
    }

    fn select_next(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.visible.len();
        self.dirty = true;
    }

    fn select_previous(&mut self) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.visible.len() - 1
        } else {
            self.selected - 1
        };
        self.dirty = true;
    }

    fn select_first(&mut self) {
        self.selected = 0;
        self.dirty = true;
    }

    fn select_last(&mut self) {
        self.selected = self.visible.len().saturating_sub(1);
        self.dirty = true;
    }

    fn rebuild_view(&mut self, preserve: Option<(u16, u32)>) {
        let needle = self.filter.trim().to_lowercase();

        self.visible = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || entry_matches(entry, &needle))
            .map(|(index, _)| index)
            .collect();

        if let Some((port, pid)) = preserve {
            if let Some(position) = self.visible.iter().position(|&index| {
                let entry = &self.entries[index];
                entry.port == port && entry.pid == pid
            }) {
                self.selected = position;
                return;
            }
        }

        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
    }

    fn selected_key(&self) -> Option<(u16, u32)> {
        self.selected_entry().map(|entry| (entry.port, entry.pid))
    }

    // ---------------------------------------------------------------------
    // Read-only view for ui/
    // ---------------------------------------------------------------------

    pub fn visible_entries(&self) -> impl Iterator<Item = &PortEntry> + '_ {
        self.visible
            .iter()
            .filter_map(|&index| self.entries.get(index))
    }

    pub fn tunnel_state(&self, port: u16) -> Option<&TunnelState> {
        self.tunnels.get(&port)
    }

    pub fn selected_tunnel(&self) -> Option<&TunnelState> {
        let port = self.selected_entry()?.port;
        self.tunnels.get(&port)
    }

    /// The frozen (port, URL) pair shown by the QR modal. `None` means the
    /// modal is closed or that tunnel is no longer active.
    pub fn qr_tunnel(&self) -> Option<(u16, &str)> {
        let port = self.qr_port?;
        let url = self.tunnels.get(&port)?.url()?;
        Some((port, url))
    }

    pub fn selected_entry(&self) -> Option<&PortEntry> {
        self.visible
            .get(self.selected)
            .and_then(|&index| self.entries.get(index))
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub fn total_len(&self) -> usize {
        self.entries.len()
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn is_filtering(&self) -> bool {
        self.input_mode == InputMode::Filter
    }

    pub fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    pub fn busy(&self) -> Option<&BusyAction> {
        self.busy.as_ref()
    }

    pub fn accent(&self) -> Color {
        self.config.accent()
    }

    pub fn is_marked(&self, entry: &PortEntry) -> bool {
        self.marks.contains(&(entry.port, entry.pid))
    }

    pub fn marked_count(&self) -> usize {
        self.marks.len()
    }

    pub fn tunnel_history(&self) -> &[TunnelHistoryEntry] {
        &self.history
    }

    pub fn history_visible(&self) -> bool {
        self.history_visible
    }

    pub fn history_selected(&self) -> usize {
        self.history_selected
    }

    pub fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    pub fn docker_available(&self) -> bool {
        self.docker_available
    }

    pub fn last_scan(&self) -> Option<Instant> {
        self.last_scan
    }

    pub fn scan_error(&self) -> Option<&str> {
        self.scan_error.as_deref()
    }

    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn is_ctrl_c(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C' | 'с' | 'С'))
}

fn entry_matches(entry: &PortEntry, needle: &str) -> bool {
    if let Some((kind, term)) = runtime_filter(needle) {
        return entry.context.kind == kind && (term.is_empty() || entry_fields_match(entry, term));
    }

    entry_fields_match(entry, needle)
}

fn entry_fields_match(entry: &PortEntry, needle: &str) -> bool {
    // All strings are allocated only for the duration of this comparison.
    // With a few dozen listening sockets this is cheaper than maintaining a
    // secondary lowercase index across every scan.
    contains_ignore_case(&entry.port.to_string(), needle)
        || contains_ignore_case(&entry.pid.to_string(), needle)
        || contains_ignore_case(&entry.process_name, needle)
        || contains_ignore_case(&entry.context.text(), needle)
        || contains_ignore_case(&entry.cmdline_string(), needle)
        || contains_ignore_case(&entry.cwd_display(), needle)
}

fn runtime_filter(needle: &str) -> Option<(RuntimeKind, &str)> {
    let (prefix, term) = needle.split_once(':')?;
    let kind = match prefix {
        "docker" => RuntimeKind::Docker,
        "podman" => RuntimeKind::Podman,
        "rust" => RuntimeKind::Rust,
        "go" => RuntimeKind::Go,
        "python" => RuntimeKind::Python,
        "node" | "nodejs" => RuntimeKind::Node,
        "generic" => RuntimeKind::Generic,
        _ => return None,
    };

    Some((kind, term.trim()))
}

fn contains_ignore_case(haystack: &str, lowercase_needle: &str) -> bool {
    haystack.to_lowercase().contains(lowercase_needle)
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

fn summarize_lifecycle(batch: &LifecycleBatchOutcome) -> Status {
    let total = batch.outcomes.len() + batch.worker_errors.len();
    let succeeded = batch
        .outcomes
        .iter()
        .filter(|outcome| outcome.result.is_ok())
        .count();
    let failed = total.saturating_sub(succeeded);
    let first_error = batch
        .outcomes
        .iter()
        .find_map(|outcome| match &outcome.result {
            Ok(_) => None,
            Err(error) => Some(format!(
                "{} :{} (pid {}) failed: {error}",
                outcome.action.label(),
                outcome.port,
                outcome.pid
            )),
        })
        .or_else(|| batch.worker_errors.first().cloned());

    if failed == 0 {
        if let [outcome] = batch.outcomes.as_slice() {
            if let Ok(message) = &outcome.result {
                return Status::info(message.clone());
            }
        }

        return Status::info(format!(
            "{} completed for {total} port{}",
            batch.action.label(),
            if total == 1 { "" } else { "s" }
        ));
    }

    let message = if total == 1 {
        first_error.unwrap_or_else(|| "lifecycle action failed".to_owned())
    } else {
        format!(
            "{}: {succeeded} succeeded, {failed} failed; {}",
            batch.action.label(),
            first_error.unwrap_or_else(|| "unknown error".to_owned())
        )
    };

    Status::error(message)
}

// -------------------------------------------------------------------------
// Lifecycle worker functions
// -------------------------------------------------------------------------

pub async fn run_lifecycle_batch(requests: Vec<LifecycleRequest>) -> LifecycleBatchOutcome {
    let action = requests
        .first()
        .map(|request| request.action)
        .unwrap_or(LifecycleAction::Kill);
    let mut tasks = tokio::task::JoinSet::new();

    for request in requests {
        tasks.spawn(run_lifecycle(request));
    }

    let mut outcomes = Vec::new();
    let mut worker_errors = Vec::new();
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => worker_errors.push(format!("lifecycle worker failed: {error}")),
        }
    }
    outcomes.sort_by_key(|outcome| (outcome.port, outcome.pid));

    LifecycleBatchOutcome {
        action,
        outcomes,
        worker_errors,
    }
}

/// Run a kill or restart off the UI thread and return an owned result.
pub async fn run_lifecycle(request: LifecycleRequest) -> LifecycleOutcome {
    let LifecycleRequest { action, entry } = request;
    let port = entry.bind_addr.port();
    let pid = entry.pid;

    let result = match action {
        LifecycleAction::Kill => terminate_port(&entry)
            .await
            .map(|termination| format!("port {port} stopped ({termination})")),
        LifecycleAction::Restart => restart_port(&entry).await.map(|report| {
            format!(
                "port {port} restarted (pid {pid} -> {}; {})",
                report.new_pid, report.termination
            )
        }),
    };

    LifecycleOutcome {
        action,
        port,
        pid,
        result,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Termination {
    Graceful,
    Forced,
}

impl std::fmt::Display for Termination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Graceful => f.write_str("graceful"),
            Self::Forced => f.write_str("forced"),
        }
    }
}

#[derive(Debug)]
struct RestartReport {
    new_pid: u32,
    termination: Termination,
}

async fn terminate_port(entry: &PortEntry) -> Result<Termination, LifecycleError> {
    let port = entry.bind_addr.port();
    let pid = entry.pid;

    // Never signal a PID unless it still owns the socket from the snapshot.
    // This closes the "process exited, PID was reused" race as far as the OS
    // socket table allows us to observe it.
    match socket_is_listening(port, pid).await {
        Some(true) => {}
        Some(false) => return Err(LifecycleError::StaleTarget { port, pid }),
        None => {
            return Err(LifecycleError::Probe(format!(
                "could not verify the owner of port {port}"
            )));
        }
    }

    send_graceful(pid)?;
    if wait_for_release(port, pid, GRACEFUL_TIMEOUT).await {
        return Ok(Termination::Graceful);
    }

    // The socket is still held after SIGTERM. Escalate.
    send_force(pid)?;
    if wait_for_release(port, pid, FORCE_TIMEOUT).await {
        Ok(Termination::Forced)
    } else {
        Err(LifecycleError::SocketBusy { port, pid })
    }
}

async fn restart_port(entry: &PortEntry) -> Result<RestartReport, LifecycleError> {
    // Capture the recipe *before* killing: once the process is gone, its
    // cmdline, cwd, and environment may no longer be readable.
    let capture_target = entry.clone();
    let recipe = tokio::task::spawn_blocking(move || capture_recipe(&capture_target))
        .await
        .map_err(|error| LifecycleError::Worker(error.to_string()))??;

    let termination = terminate_port(entry).await?;
    let child = spawn_detached(&recipe)?;
    let new_pid = child.id();

    // The child is detached into its own process group, but it is still our
    // child until it exits. Reap it on a plain OS thread so zooport can keep
    // running without accumulating zombies. The thread blocks; it never touches
    // the async runtime or UI state.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });

    Ok(RestartReport {
        new_pid,
        termination,
    })
}

async fn wait_for_release(port: u16, pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;

    loop {
        // `Some(false)` means another scan successfully observed that this PID
        // no longer owns a LISTEN socket. A probe error is treated as "keep
        // waiting" rather than declaring success.
        if matches!(socket_is_listening(port, pid).await, Some(false)) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Probe the kernel socket table without blocking a Tokio worker.
///
/// `None` means the probe itself failed; callers decide whether that is fatal
/// (before signalling) or transient (while polling).
async fn socket_is_listening(port: u16, pid: u32) -> Option<bool> {
    tokio::task::spawn_blocking(move || {
        listeners::get_all().ok().map(|listeners| {
            listeners.iter().any(|listener| {
                listener.socket.port() == port
                    && listener.process.pid == pid
                    && listener.state == SocketState::Listen
            })
        })
    })
    .await
    .ok()
    .flatten()
}

// -------------------------------------------------------------------------
// Process recipe + detached spawn
// -------------------------------------------------------------------------

/// Everything needed to reproduce the process outside the TUI.
///
/// Owned deliberately: the recipe is moved into `spawn_blocking`/the action
/// task and has no borrows into `PortEntry` or `sysinfo`.
#[derive(Debug)]
struct ProcessRecipe {
    program: PathBuf,
    fallback_program: Option<PathBuf>,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    environment: Vec<(OsString, OsString)>,
}

fn capture_recipe(entry: &PortEntry) -> Result<ProcessRecipe, LifecycleError> {
    let pid = Pid::from_u32(entry.pid);
    let mut system = System::new();

    // Refresh only this PID and only the fields we need. `environ` is not part
    // of the default refresh set, so request it explicitly.
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_environ(UpdateKind::Always)
            .with_cwd(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );

    // Best effort: on platforms where the environment is restricted, this may
    // be empty. In that case the child inherits zooport's environment, which is
    // still better than spawning with an empty environment.
    let environment = system
        .process(pid)
        .map(|process| parse_environment(process.environ()))
        .unwrap_or_default();

    let cwd = entry.cwd.clone().filter(|cwd| cwd.is_dir());

    // argv[0] is the command name, never an argument. Keeping every following
    // element preserves flags with spaces and arguments that look like paths.
    let args: Vec<OsString> = entry
        .cmdline
        .iter()
        .skip(1)
        .cloned()
        .map(OsString::from)
        .collect();

    let argv0 = entry
        .cmdline
        .first()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| resolve_argv0(&path, cwd.as_deref()));
    let exe = resolve_exe(entry.exe.as_deref(), cwd.as_deref());

    // The executable reported by the kernel is authoritative and already an
    // absolute path on macOS/Linux. Fall back to argv[0] only when it is gone.
    let (program, fallback_program) = match (exe, argv0) {
        (Some(exe), Some(argv0)) => {
            let fallback = (exe != argv0).then_some(argv0);
            (exe, fallback)
        }
        (Some(exe), None) => (exe, None),
        (None, Some(argv0)) => (argv0, None),
        (None, None) => return Err(LifecycleError::NoCommand { pid: entry.pid }),
    };

    Ok(ProcessRecipe {
        program,
        fallback_program,
        args,
        cwd,
        environment,
    })
}

fn resolve_exe(exe: Option<&Path>, cwd: Option<&Path>) -> Option<PathBuf> {
    let exe = exe?;
    let resolved = if exe.is_absolute() {
        exe.to_path_buf()
    } else {
        cwd?.join(exe)
    };

    resolved.is_file().then_some(resolved)
}

fn resolve_argv0(argv0: &Path, cwd: Option<&Path>) -> PathBuf {
    if argv0.is_absolute() {
        return argv0.to_path_buf();
    }

    // Anchor relative programs to the process's saved working directory first,
    // so `./server` and bare local scripts restart in the directory they were
    // launched from. If no local file exists, keep the original name and let
    // `Command` search the restored PATH.
    if let Some(cwd) = cwd {
        let candidate = cwd.join(argv0);
        if candidate.is_file() {
            return candidate;
        }
    }

    argv0.to_path_buf()
}

fn parse_environment(environment: &[OsString]) -> Vec<(OsString, OsString)> {
    environment
        .iter()
        .filter_map(|entry| {
            let text = entry.to_string_lossy();
            let (key, value) = text.split_once('=')?;
            if key.is_empty() {
                return None;
            }
            Some((OsString::from(key), OsString::from(value)))
        })
        .collect()
}

fn spawn_detached(recipe: &ProcessRecipe) -> Result<Child, LifecycleError> {
    match spawn_command(recipe, &recipe.program) {
        Ok(child) => Ok(child),
        Err(first_error) => {
            // A vanished kernel executable path can still be recoverable from
            // argv[0] (or vice versa). Retry once with the other candidate.
            if let Some(fallback) = recipe.fallback_program.as_deref() {
                spawn_command(recipe, fallback).map_err(|source| LifecycleError::Spawn {
                    program: fallback.display().to_string(),
                    source,
                })
            } else {
                Err(LifecycleError::Spawn {
                    program: recipe.program.display().to_string(),
                    source: first_error,
                })
            }
        }
    }
}

fn spawn_command(recipe: &ProcessRecipe, program: &Path) -> io::Result<Child> {
    let mut command = Command::new(program);

    command
        .args(&recipe.args)
        // Null stdio is important in a TUI: a restarted server must not write
        // into the alternate screen and corrupt the frame buffer.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Some(cwd) = &recipe.cwd {
        command.current_dir(cwd);
    }

    if !recipe.environment.is_empty() {
        command.env_clear();
        command.envs(recipe.environment.iter().map(|(key, value)| (key, value)));
    }

    configure_detached(&mut command);
    command.spawn()
}

fn configure_detached(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // New process group: terminal-generated signals (Ctrl-C) target the
        // foreground group and will not reach the restarted service.
        command.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
}

// -------------------------------------------------------------------------
// Signals
// -------------------------------------------------------------------------

#[cfg(unix)]
fn send_graceful(pid: u32) -> Result<(), LifecycleError> {
    send_signal(pid, libc::SIGTERM)
}

#[cfg(unix)]
fn send_force(pid: u32) -> Result<(), LifecycleError> {
    send_signal(pid, libc::SIGKILL)
}

#[cfg(unix)]
fn send_signal(pid: u32, signal: i32) -> Result<(), LifecycleError> {
    let signal_pid = i32::try_from(pid).map_err(|_| LifecycleError::Signal {
        pid,
        source: io::Error::new(io::ErrorKind::InvalidInput, "PID does not fit in pid_t"),
    })?;

    // SAFETY: `kill(2)` only reads two integer arguments and writes no Rust
    // memory. The PID conversion was checked above, and the signal is one of
    // libc's constants.
    let result = unsafe { libc::kill(signal_pid, signal) };
    if result == 0 {
        return Ok(());
    }

    let source = io::Error::last_os_error();
    // ESRCH means the process already exited. Treat that as success so the
    // caller can proceed to the socket-release check.
    if source.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(LifecycleError::Signal { pid, source })
    }
}

#[cfg(windows)]
fn send_graceful(pid: u32) -> Result<(), LifecycleError> {
    taskkill(pid, false)
}

#[cfg(windows)]
fn send_force(pid: u32) -> Result<(), LifecycleError> {
    taskkill(pid, true)
}

#[cfg(windows)]
fn taskkill(pid: u32, force: bool) -> Result<(), LifecycleError> {
    let mut command = Command::new("taskkill");
    command.args(["/PID", &pid.to_string(), "/T"]);
    if force {
        command.arg("/F");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let status = command
        .status()
        .map_err(|source| LifecycleError::Signal { pid, source })?;

    if status.success() {
        Ok(())
    } else {
        Err(LifecycleError::Signal {
            pid,
            source: io::Error::other(format!("taskkill exited with {status}")),
        })
    }
}

// -------------------------------------------------------------------------
// Errors
// -------------------------------------------------------------------------

#[derive(Debug)]
pub enum LifecycleError {
    StaleTarget { port: u16, pid: u32 },
    NoCommand { pid: u32 },
    Signal { pid: u32, source: io::Error },
    SocketBusy { port: u16, pid: u32 },
    Spawn { program: String, source: io::Error },
    Probe(String),
    Worker(String),
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleTarget { port, pid } => write!(
                f,
                "port {port} is no longer owned by PID {pid}; refresh and retry"
            ),
            Self::NoCommand { pid } => write!(f, "PID {pid} has no recoverable command line"),
            Self::Signal { pid, source } => write!(f, "could not signal PID {pid}: {source}"),
            Self::SocketBusy { port, pid } => {
                write!(f, "PID {pid} was killed but port {port} is still listening")
            }
            Self::Spawn { program, source } => {
                write!(f, "could not spawn {program:?}: {source}")
            }
            Self::Probe(message) => f.write_str(message),
            Self::Worker(message) => write!(f, "lifecycle worker failed: {message}"),
        }
    }
}

impl std::error::Error for LifecycleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Signal { source, .. } | Self::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{RuntimeContext, RuntimeKind, Transport};

    fn entry(port: u16, pid: u32, name: &str, kind: RuntimeKind) -> PortEntry {
        PortEntry {
            port,
            pid,
            process_name: name.to_owned(),
            context: RuntimeContext { kind, detail: None },
            memory_bytes: 2 * 1024 * 1024,
            cmdline: vec![name.to_owned(), "--port".to_owned(), port.to_string()],
            cwd: Some(PathBuf::from("/tmp")),
            exe: None,
            bind_addr: format!("127.0.0.1:{port}").parse().unwrap(),
            transport: Transport::Tcp,
        }
    }

    fn app_with(entries: Vec<PortEntry>) -> App {
        app_with_config(entries, Config::default())
    }

    fn app_with_config(entries: Vec<PortEntry>, config: Config) -> App {
        let mut app = App::with_config(config);
        app.apply_scan(ScanSnapshot {
            entries,
            generated_at: Instant::now(),
            docker_available: false,
        });
        app
    }

    fn press(app: &mut App, character: char) -> Option<AppCommand> {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }

    #[test]
    fn navigation_wraps_in_both_directions() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "python", RuntimeKind::Python),
        ]);

        press(&mut app, 'j');
        assert_eq!(app.selected(), 1);
        press(&mut app, 'j');
        assert_eq!(app.selected(), 0);
        press(&mut app, 'k');
        assert_eq!(app.selected(), 1);
    }

    #[test]
    fn filter_applies_live_and_escape_clears_and_exits() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "python", RuntimeKind::Python),
        ]);

        press(&mut app, '/');
        for character in "py".chars() {
            press(&mut app, character);
        }

        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.selected_entry().unwrap().port, 8000);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.is_filtering());
        assert!(!app.should_quit());
        assert_eq!(app.filter(), "");
        assert_eq!(app.visible_len(), 2);
    }

    #[test]
    fn escape_with_an_empty_filter_does_not_quit_or_trap_input() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        press(&mut app, '/');
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!app.is_filtering());
        assert!(!app.should_quit());

        press(&mut app, 'j');
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn filter_matches_unicode_case_insensitively() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "Приложение", RuntimeKind::Python),
        ]);

        press(&mut app, '/');
        for character in "ПРИЛ".chars() {
            press(&mut app, character);
        }

        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.selected_entry().unwrap().port, 8000);
    }

    #[test]
    fn ctrl_c_works_with_a_cyrillic_layout_key() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        app.handle_key(KeyEvent::new(KeyCode::Char('с'), KeyModifiers::CONTROL));

        assert!(app.should_quit());
    }

    #[test]
    fn filter_enter_keeps_the_filter() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "python", RuntimeKind::Python),
        ]);
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        for character in "8000".chars() {
            press(&mut app, character);
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(!app.is_filtering());
        assert_eq!(app.filter(), "8000");
        assert_eq!(app.visible_len(), 1);
    }

    #[test]
    fn scan_keeps_selection_on_the_same_process() {
        let first = entry(3000, 10, "node", RuntimeKind::Node);
        let second = entry(8000, 20, "python", RuntimeKind::Python);
        let mut app = app_with(vec![first, second]);
        press(&mut app, 'j');
        assert_eq!(app.selected_entry().unwrap().port, 8000);

        app.apply_scan(ScanSnapshot {
            entries: vec![
                entry(8000, 20, "python", RuntimeKind::Python),
                entry(9000, 30, "go", RuntimeKind::Go),
            ],
            generated_at: Instant::now(),
            docker_available: true,
        });

        assert_eq!(app.selected_entry().unwrap().port, 8000);
    }

    #[test]
    fn kill_requires_idle_state_and_returns_an_owned_request() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        let command = press(&mut app, 'x');
        match command {
            Some(AppCommand::Lifecycle(requests)) => {
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0].action, LifecycleAction::Kill);
                assert_eq!(requests[0].entry.port, 3000);
            }
            other => panic!("expected lifecycle command, got {other:?}"),
        }
        assert!(app.busy().is_some());

        // A second action must be rejected while the first is in flight.
        assert!(press(&mut app, 'r').is_none());
    }

    #[test]
    fn tunnel_toggle_starts_and_stops_the_selected_port() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        let command = press(&mut app, 't');
        assert!(matches!(command, Some(AppCommand::Tunnel { port: 3000 })));
        assert_eq!(app.tunnel_state(3000), Some(&TunnelState::Starting));

        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });
        assert_eq!(
            app.tunnel_state(3000),
            Some(&TunnelState::Active {
                url: "https://calm-river.trycloudflare.com".to_owned()
            })
        );

        assert!(matches!(
            press(&mut app, 't'),
            Some(AppCommand::Tunnel { port: 3000 })
        ));
        assert_eq!(app.tunnel_state(3000), Some(&TunnelState::Stopping));

        app.apply_tunnel(TunnelEvent::Stopped {
            port: 3000,
            message: None,
        });
        assert!(app.tunnel_state(3000).is_none());
    }

    #[test]
    fn missing_cloudflared_opens_a_dismissible_notice() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        app.apply_tunnel(TunnelEvent::CloudflaredMissing { port: 3000 });

        assert_eq!(app.notice(), Some(CLOUDFLARED_MISSING_NOTICE));
        assert!(app.tunnel_state(3000).is_none());

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.notice().is_none());
        assert!(!app.should_quit());
    }

    #[test]
    fn qr_requires_an_active_tunnel_and_closes_with_q() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        press(&mut app, 'q');
        assert!(app.qr_tunnel().is_none());
        assert_eq!(
            app.status().map(|status| status.level),
            Some(StatusLevel::Warning)
        );

        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });
        press(&mut app, 'q');
        assert_eq!(
            app.qr_tunnel(),
            Some((3000, "https://calm-river.trycloudflare.com"))
        );

        press(&mut app, 'q');
        assert!(app.qr_tunnel().is_none());
    }

    #[test]
    fn runtime_filter_presets_combine_with_free_text() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "api", RuntimeKind::Python),
            entry(8001, 21, "worker", RuntimeKind::Python),
            entry(9000, 30, "docker-proxy", RuntimeKind::Docker),
        ]);

        app.set_filter("docker:".to_owned());
        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.selected_entry().unwrap().port, 9000);

        app.set_filter("python:".to_owned());
        assert_eq!(app.visible_len(), 2);

        app.set_filter("python: api".to_owned());
        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.selected_entry().unwrap().port, 8000);

        app.set_filter("node: 3000".to_owned());
        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.selected_entry().unwrap().port, 3000);
    }

    #[test]
    fn space_marks_visible_rows_and_bulk_actions_use_all_marks() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "python", RuntimeKind::Python),
            entry(9000, 30, "go", RuntimeKind::Go),
        ]);

        press(&mut app, ' ');
        press(&mut app, 'j');
        press(&mut app, ' ');

        assert_eq!(app.marked_count(), 2);
        assert!(app.is_marked(&entry(3000, 10, "node", RuntimeKind::Node)));

        let command = press(&mut app, 'x');
        match command {
            Some(AppCommand::Lifecycle(requests)) => {
                let ports: Vec<u16> = requests.iter().map(|request| request.entry.port).collect();
                assert_eq!(ports, vec![3000, 8000]);
            }
            other => panic!("expected bulk lifecycle command, got {other:?}"),
        }

        assert_eq!(app.marked_count(), 0);
        assert_eq!(app.busy().unwrap().ports, vec![3000, 8000]);
    }

    #[test]
    fn scan_prunes_marks_for_ports_that_disappeared() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);
        press(&mut app, ' ');
        assert_eq!(app.marked_count(), 1);

        app.apply_scan(ScanSnapshot {
            entries: vec![entry(8000, 20, "python", RuntimeKind::Python)],
            generated_at: Instant::now(),
            docker_available: false,
        });

        assert_eq!(app.marked_count(), 0);
    }

    #[test]
    fn config_filters_ignored_ports_and_processes() {
        let config = Config::from_toml(
            "accent = \"#3366ff\"\n\
             ignored_ports = [3000]\n\
             ignored_processes = [\"Spotlight\"]\n",
        );
        let app = app_with_config(
            vec![
                entry(3000, 10, "node", RuntimeKind::Node),
                entry(8000, 20, "Spotlight", RuntimeKind::Generic),
                entry(9000, 30, "python", RuntimeKind::Python),
            ],
            config,
        );

        assert_eq!(app.total_len(), 1);
        assert_eq!(app.visible_len(), 1);
        assert_eq!(app.selected_entry().unwrap().port, 9000);
        assert_eq!(app.accent(), Color::Rgb(51, 102, 255));
    }

    #[test]
    fn export_builds_markdown_from_filtered_rows_and_reports_completion() {
        let mut app = app_with(vec![
            entry(3000, 10, "node", RuntimeKind::Node),
            entry(8000, 20, "python", RuntimeKind::Python),
        ]);
        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });

        let command = press(&mut app, 'e');
        match command {
            Some(AppCommand::Export { markdown, count }) => {
                assert_eq!(count, 2);
                assert!(markdown.starts_with("| PORT | PID | PROCESS |"));
                assert!(markdown.contains("| 3000 | 10 | node |"));
                assert!(markdown.contains("https://calm-river.trycloudflare.com"));
                assert!(markdown.contains("| 8000 | 20 | python |"));
            }
            other => panic!("expected export command, got {other:?}"),
        }

        app.apply_export(ExportOutcome {
            count: 2,
            result: Ok(()),
        });

        assert!(app
            .status()
            .is_some_and(|status| status.text.contains("copied 2 ports")));
    }

    #[test]
    fn export_escapes_markdown_table_delimiters() {
        assert_eq!(markdown_cell("weird|name\nnext"), "weird\\|name next");
    }

    #[test]
    fn export_of_an_empty_filter_is_rejected() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);
        app.set_filter("nothing-matches".to_owned());

        assert!(press(&mut app, 'e').is_none());
        assert_eq!(
            app.status().map(|status| status.text.as_str()),
            Some("nothing to export")
        );
    }

    #[test]
    fn tunnel_history_tracks_start_activity_and_stop() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);

        app.apply_tunnel(TunnelEvent::Starting { port: 3000 });
        assert_eq!(app.tunnel_history().len(), 1);
        assert_eq!(
            app.tunnel_history()[0].status,
            TunnelHistoryStatus::Starting
        );

        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });
        assert_eq!(app.tunnel_history()[0].status, TunnelHistoryStatus::Active);
        assert_eq!(
            app.tunnel_history()[0].url.as_deref(),
            Some("https://calm-river.trycloudflare.com")
        );

        app.apply_tunnel(TunnelEvent::Stopping { port: 3000 });
        assert_eq!(
            app.tunnel_history()[0].status,
            TunnelHistoryStatus::Stopping
        );

        app.apply_tunnel(TunnelEvent::Stopped {
            port: 3000,
            message: None,
        });
        assert_eq!(app.tunnel_history()[0].status, TunnelHistoryStatus::Stopped);
        assert!(app.tunnel_history()[0].ended_at.is_some());
    }

    #[test]
    fn history_modal_browses_and_clears_without_touching_live_tunnels() {
        let mut app = app_with(vec![entry(3000, 10, "node", RuntimeKind::Node)]);
        app.apply_tunnel(TunnelEvent::Starting { port: 3000 });
        app.apply_tunnel(TunnelEvent::Active {
            port: 3000,
            url: "https://calm-river.trycloudflare.com".to_owned(),
            clipboard_error: None,
        });

        press(&mut app, 'h');
        assert!(app.history_visible());

        press(&mut app, 'j');
        assert_eq!(app.history_selected(), 0);
        press(&mut app, 'c');
        assert!(app.tunnel_history().is_empty());
        assert!(app.tunnel_state(3000).is_some());

        press(&mut app, 'h');
        assert!(!app.history_visible());
    }

    #[test]
    fn lifecycle_summary_reports_partial_failures() {
        let batch = LifecycleBatchOutcome {
            action: LifecycleAction::Kill,
            outcomes: vec![
                LifecycleOutcome {
                    action: LifecycleAction::Kill,
                    port: 3000,
                    pid: 10,
                    result: Ok("port 3000 stopped (graceful)".to_owned()),
                },
                LifecycleOutcome {
                    action: LifecycleAction::Kill,
                    port: 8000,
                    pid: 20,
                    result: Err(LifecycleError::Probe("probe failed".to_owned())),
                },
            ],
            worker_errors: Vec::new(),
        };

        let status = summarize_lifecycle(&batch);

        assert_eq!(status.level, StatusLevel::Error);
        assert!(status.text.contains("1 succeeded, 1 failed"));
        assert!(status.text.contains("probe failed"));
    }

    fn temp_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("zooport-restart-{}-{label}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).expect("create temp dir");
        root
    }

    #[test]
    fn restart_recipe_prefers_absolute_exe_and_preserves_all_arguments() {
        let root = temp_root("absolute");
        let exe = root.join("server");
        std::fs::write(&exe, "").expect("create executable placeholder");

        let mut target = entry(3000, 10, "python", RuntimeKind::Python);
        target.cwd = Some(root.clone());
        target.exe = Some(exe.clone());
        target.cmdline = vec![
            "python3".to_owned(),
            "server.py".to_owned(),
            "--port".to_owned(),
            "3000".to_owned(),
        ];

        let recipe = capture_recipe(&target).expect("capture restart recipe");

        assert_eq!(recipe.program, exe);
        assert_eq!(
            recipe.args,
            vec![
                OsString::from("server.py"),
                OsString::from("--port"),
                OsString::from("3000")
            ]
        );
        assert_eq!(recipe.cwd, Some(root.clone()));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn restart_recipe_resolves_relative_program_against_saved_cwd() {
        let root = temp_root("relative");
        std::fs::write(root.join("run.sh"), "").expect("create script placeholder");

        let mut target = entry(3000, 10, "run.sh", RuntimeKind::Generic);
        target.cwd = Some(root.clone());
        target.exe = None;
        target.cmdline = vec!["run.sh".to_owned(), "--serve".to_owned()];

        let recipe = capture_recipe(&target).expect("capture restart recipe");

        assert_eq!(recipe.program, root.join("run.sh"));
        assert_eq!(recipe.args, vec![OsString::from("--serve")]);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn parses_environment_only_for_valid_assignments() {
        let parsed = parse_environment(&[
            OsString::from("PATH=/usr/bin"),
            OsString::from("NO_EQUALS"),
            OsString::from("EMPTY="),
        ]);

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, OsString::from("PATH"));
        assert_eq!(parsed[0].1, OsString::from("/usr/bin"));
        assert_eq!(parsed[1].0, OsString::from("EMPTY"));
        assert_eq!(parsed[1].1, OsString::from(""));
    }
}
