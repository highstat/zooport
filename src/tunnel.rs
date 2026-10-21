//! Cloudflare quick-tunnel lifecycle management.
//!
//! # Ownership model
//!
//! One manager task owns the `HashMap` of active tunnels. Each tunnel has a
//! small worker task that exclusively owns its `tokio::process::Child` and the
//! piped stderr reader. The manager never renders and the UI never owns a
//! child process, so no lock is needed.
//!
//! Workers send owned `WorkerEvent`s back to the manager. The manager updates
//! its map and forwards an owned `TunnelEvent` to the UI. This indirection is
//! deliberate: it lets the manager reject stale events from a tunnel that was
//! already stopped and replaced, without any shared state.
//!
//! `kill_on_drop(true)` is a last-resort safety net. Normal exits go through
//! `TunnelManager::shutdown`, which asks every worker to stop and waits for
//! those workers to reap their children.

use std::collections::HashMap;
use std::io;
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::clipboard::ClipboardHandle;

const CLOUDFLARED_BIN: &str = "cloudflared";
const COMMAND_BUFFER: usize = 16;
const EVENT_BUFFER: usize = 32;
const WORKER_EVENT_BUFFER: usize = 64;
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(2);
const MANAGER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Matches only the public quick-tunnel hostname. The boundary is checked
/// separately below because Rust's regex engine has no look-around.
static QUICK_TUNNEL_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://[A-Za-z0-9-]+\.trycloudflare\.com")
        .expect("quick tunnel URL regex is valid")
});

#[derive(Debug, Clone)]
pub enum TunnelEvent {
    Starting {
        port: u16,
    },
    Stopping {
        port: u16,
    },
    Active {
        port: u16,
        url: String,
        /// Clipboard failures are non-fatal: the tunnel is still active and the
        /// URL remains visible in the inspection panel.
        clipboard_error: Option<String>,
    },
    Stopped {
        port: u16,
        /// Present when cloudflared exited on its own after being active.
        message: Option<String>,
    },
    CloudflaredMissing {
        port: u16,
    },
    Failed {
        port: u16,
        message: String,
    },
}

/// Handle used by the event loop to control the manager task.
pub struct TunnelManager {
    commands: mpsc::Sender<TunnelCommand>,
    task: Option<JoinHandle<()>>,
}

impl TunnelManager {
    /// Spawn the manager and return its control handle plus the event stream.
    ///
    /// The receiver is returned separately so the UI can `select!` on it while
    /// still calling `toggle` through this handle.
    pub fn spawn(clipboard: ClipboardHandle) -> (Self, mpsc::Receiver<TunnelEvent>) {
        let (commands, command_rx) = mpsc::channel(COMMAND_BUFFER);
        let (events, event_rx) = mpsc::channel(EVENT_BUFFER);
        let task = tokio::spawn(manager_loop(command_rx, events, clipboard));

        (
            Self {
                commands,
                task: Some(task),
            },
            event_rx,
        )
    }

    /// Toggle one port's tunnel. Returns `false` if the manager is gone or its
    /// bounded command queue is temporarily full.
    pub fn toggle(&self, port: u16) -> bool {
        self.commands
            .try_send(TunnelCommand::Toggle { port })
            .is_ok()
    }

    /// Stop every tunnel and wait for each worker to reap its child.
    ///
    /// The timeout is a safety valve: if a worker wedges, aborting the manager
    /// drops every stop sender, which closes each worker's stop channel and
    /// still triggers the child's `kill_on_drop` cleanup.
    pub async fn shutdown(mut self) {
        let _ = self.commands.send(TunnelCommand::Shutdown).await;

        if let Some(mut task) = self.task.take() {
            if timeout(MANAGER_SHUTDOWN_TIMEOUT, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

enum TunnelCommand {
    Toggle { port: u16 },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Active,
    Stopping,
}

struct Worker {
    id: u64,
    phase: Phase,
    stop: mpsc::Sender<()>,
    task: JoinHandle<()>,
}

enum WorkerEvent {
    Active {
        id: u64,
        port: u16,
        url: String,
        clipboard_error: Option<String>,
    },
    Stopped {
        id: u64,
        port: u16,
        message: Option<String>,
    },
    CloudflaredMissing {
        id: u64,
        port: u16,
    },
    Failed {
        id: u64,
        port: u16,
        message: String,
    },
}

async fn manager_loop(
    mut commands: mpsc::Receiver<TunnelCommand>,
    events: mpsc::Sender<TunnelEvent>,
    clipboard: ClipboardHandle,
) {
    let (worker_events, mut worker_rx) = mpsc::channel(WORKER_EVENT_BUFFER);
    let mut workers: HashMap<u16, Worker> = HashMap::new();
    let mut next_id: u64 = 1;

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(TunnelCommand::Toggle { port }) => {
                        // Scope the map borrow so it cannot be held across the
                        // event send below (the manager future must stay Send).
                        let already_stopping = match workers.get_mut(&port) {
                            Some(worker) if worker.phase == Phase::Stopping => true,
                            Some(worker) => {
                                worker.phase = Phase::Stopping;
                                // Capacity is one. If a stop is already queued,
                                // or the worker just ended, the queued/closed
                                // channel still communicates the intent.
                                let _ = worker.stop.try_send(());
                                false
                            }
                            None => false,
                        };

                        if already_stopping {
                            continue;
                        }

                        if let Some(worker) = workers.get(&port) {
                            debug_assert_eq!(worker.phase, Phase::Stopping);
                            if events.send(TunnelEvent::Stopping { port }).await.is_err() {
                                break;
                            }
                            continue;
                        }

                        let id = next_id;
                        next_id = next_id.wrapping_add(1);
                        let (stop, stop_rx) = mpsc::channel(1);
                        let task = tokio::spawn(tunnel_worker(
                            port,
                            id,
                            stop_rx,
                            worker_events.clone(),
                            clipboard.clone(),
                        ));

                        workers.insert(
                            port,
                            Worker {
                                id,
                                phase: Phase::Starting,
                                stop,
                                task,
                            },
                        );

                        if events.send(TunnelEvent::Starting { port }).await.is_err() {
                            break;
                        }
                    }
                    Some(TunnelCommand::Shutdown) | None => break,
                }
            }

            event = worker_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                if !handle_worker_event(&mut workers, event, &events).await {
                    break;
                }
            }
        }
    }

    // Close the worker-event channel before cleanup. A worker that is blocked
    // sending a final event during shutdown then gets an immediate error
    // instead of waiting on a queue the manager no longer drains.
    drop(worker_rx);
    drop(worker_events);

    stop_workers(workers).await;
}

/// Apply one worker event, ignoring events from a tunnel generation that has
/// already been removed or replaced.
async fn handle_worker_event(
    workers: &mut HashMap<u16, Worker>,
    event: WorkerEvent,
    events: &mpsc::Sender<TunnelEvent>,
) -> bool {
    match event {
        WorkerEvent::Active {
            id,
            port,
            url,
            clipboard_error,
        } => {
            let is_current = match workers.get_mut(&port) {
                Some(worker) if worker.id == id && worker.phase != Phase::Stopping => {
                    worker.phase = Phase::Active;
                    true
                }
                _ => false,
            };

            if is_current {
                events
                    .send(TunnelEvent::Active {
                        port,
                        url,
                        clipboard_error,
                    })
                    .await
                    .is_ok()
            } else {
                true
            }
        }

        WorkerEvent::Stopped { id, port, message } => {
            let is_current = matches!(workers.get(&port), Some(worker) if worker.id == id);
            if !is_current {
                return true;
            }

            workers.remove(&port);
            events
                .send(TunnelEvent::Stopped { port, message })
                .await
                .is_ok()
        }

        WorkerEvent::CloudflaredMissing { id, port } => {
            let is_current = matches!(workers.get(&port), Some(worker) if worker.id == id);
            if !is_current {
                return true;
            }

            workers.remove(&port);
            events
                .send(TunnelEvent::CloudflaredMissing { port })
                .await
                .is_ok()
        }

        WorkerEvent::Failed { id, port, message } => {
            let is_current = matches!(workers.get(&port), Some(worker) if worker.id == id);
            if !is_current {
                return true;
            }

            workers.remove(&port);
            events
                .send(TunnelEvent::Failed { port, message })
                .await
                .is_ok()
        }
    }
}

async fn stop_workers(mut workers: HashMap<u16, Worker>) {
    // Signal everything first so shutdowns overlap, then reap each task.
    for worker in workers.values_mut() {
        let _ = worker.stop.try_send(());
    }

    for (_, worker) in workers {
        let mut task = worker.task;
        if timeout(WORKER_STOP_TIMEOUT, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

/// Owns exactly one cloudflared process and its stderr stream.
async fn tunnel_worker(
    port: u16,
    id: u64,
    mut stop: mpsc::Receiver<()>,
    events: mpsc::Sender<WorkerEvent>,
    clipboard: ClipboardHandle,
) {
    let mut child = match spawn_cloudflared(port) {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let _ = events
                .send(WorkerEvent::CloudflaredMissing { id, port })
                .await;
            return;
        }
        Err(error) => {
            let _ = events
                .send(WorkerEvent::Failed {
                    id,
                    port,
                    message: format!("could not start {CLOUDFLARED_BIN}: {error}"),
                })
                .await;
            return;
        }
    };

    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = events
            .send(WorkerEvent::Failed {
                id,
                port,
                message: "cloudflared stderr was not captured".to_owned(),
            })
            .await;
        return;
    };

    let mut lines = BufReader::new(stderr).lines();
    let mut stderr_open = true;
    let mut url: Option<String> = None;

    loop {
        tokio::select! {
            _ = stop.recv() => {
                // A closed stop channel means the manager dropped this worker;
                // both cases are a request to terminate the child.
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = events
                    .send(WorkerEvent::Stopped {
                        id,
                        port,
                        message: None,
                    })
                    .await;
                return;
            }

            status = child.wait() => {
                let message = match status {
                    Ok(status) => format!("cloudflared exited ({status})"),
                    Err(error) => format!("could not wait for cloudflared: {error}"),
                };
                let event = if url.is_some() {
                    WorkerEvent::Stopped {
                        id,
                        port,
                        message: Some(message),
                    }
                } else {
                    WorkerEvent::Failed {
                        id,
                        port,
                        message,
                    }
                };
                let _ = events.send(event).await;
                return;
            }

            line = lines.next_line(), if stderr_open => {
                match line {
                    Ok(Some(line)) => {
                        // Only the first URL is meaningful. Matching is skipped
                        // after activation, but the stream is still drained so a
                        // full stderr pipe can never block cloudflared.
                        if let Some(found) = extract_url(&line).filter(|_| url.is_none()) {
                            // The clipboard owner thread performs the
                            // platform call; this worker only awaits a reply.
                            let clipboard_error = clipboard.copy(found.clone()).await.err();
                            url = Some(found.clone());
                            let _ = events
                                .send(WorkerEvent::Active {
                                    id,
                                    port,
                                    url: found,
                                    clipboard_error,
                                })
                                .await;
                        }
                    }
                    Ok(None) => {
                        stderr_open = false;
                        if url.is_none() {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            let _ = events
                                .send(WorkerEvent::Failed {
                                    id,
                                    port,
                                    message: "cloudflared closed stderr before publishing a URL"
                                        .to_owned(),
                                })
                                .await;
                            return;
                        }
                    }
                    Err(error) => {
                        stderr_open = false;
                        if url.is_none() {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            let _ = events
                                .send(WorkerEvent::Failed {
                                    id,
                                    port,
                                    message: format!("could not read cloudflared stderr: {error}"),
                                })
                                .await;
                            return;
                        }
                    }
                }
            }
        }
    }
}

fn spawn_cloudflared(port: u16) -> io::Result<Child> {
    let mut command = Command::new(CLOUDFLARED_BIN);
    command
        .arg("tunnel")
        .arg("--url")
        .arg(format!("http://127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // If the worker is cancelled or panics, dropping the child still sends
        // SIGKILL. The normal shutdown path performs an explicit kill/wait.
        .kill_on_drop(true);

    command.spawn()
}

fn extract_url(line: &str) -> Option<String> {
    let found = QUICK_TUNNEL_URL_RE.find(line)?;
    let rest = &line[found.end()..];
    let next = rest.chars().next();

    // Reject lookalikes such as `...trycloudflare.com.evil.example`. A real
    // log line ends the URL with whitespace, punctuation, or end-of-line.
    if next.is_some_and(|character| {
        character.is_ascii_alphanumeric() || character == '.' || character == '-'
    }) {
        return None;
    }

    Some(found.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_quick_tunnel_url_from_log_line() {
        let line = "2026-10-21T12:00:00Z INF |  https://calm-river-dawn.trycloudflare.com  |";

        assert_eq!(
            extract_url(line).as_deref(),
            Some("https://calm-river-dawn.trycloudflare.com")
        );
    }

    #[test]
    fn rejects_lookalike_and_wrong_scheme_urls() {
        assert!(extract_url("https://calm-river.trycloudflare.com.evil.test").is_none());
        assert!(extract_url("http://calm-river.trycloudflare.com").is_none());
        assert!(extract_url("https://calm-river.example.com").is_none());
    }

    #[test]
    fn accepts_url_followed_by_log_punctuation() {
        assert_eq!(
            extract_url("visit https://calm-river.trycloudflare.com).").as_deref(),
            Some("https://calm-river.trycloudflare.com")
        );
    }
}
