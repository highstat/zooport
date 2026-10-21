//! Socket and process discovery.
//!
//! # Ownership model
//!
//! The scanner runs off the UI thread and returns a *self-contained snapshot*
//! (`ScanSnapshot`) of owned `String`s, `PathBuf`s and integers. It never
//! returns references into `sysinfo::System` or into the kernel tables. That
//! is deliberate:
//!
//! * `sysinfo::System` owns its process map, and every `&Process` borrows from
//!   that map. Sending such a borrow to the UI thread would require a
//!   `'static` lifetime and a lock held for the duration of rendering.
//! * Kernel sockets disappear between scans. A snapshot gives the UI a stable
//!   view even when a port closes while a frame is being drawn.
//! * The UI keeps the last good snapshot as an owned value in `App` and reads
//!   it without taking a lock. A new snapshot is built entirely on the blocking
//!   worker and moved to the event loop when it is ready.
//!
//! Borrowing is limited to short-lived helper calls (`&ProcMeta`,
//! `&DockerIndex`, `&Path`). No reference escapes the function that creates
//! it.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use listeners::{Protocol, SocketState};
use regex::Regex;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// How long `docker ps` is allowed to take before the lookup is abandoned.
const DOCKER_LOOKUP_TIMEOUT: Duration = Duration::from_millis(800);

/// Docker prints published ports as `0.0.0.0:8080->80/tcp` (possibly with a
/// range such as `8000-8010->8000-8010/tcp`). Only the host side is relevant.
static DOCKER_PORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r":(\d+)(?:-(\d+))?->").expect("docker port regex is valid"));

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Transport {
    Tcp,
    Udp,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp => f.write_str("tcp"),
            Self::Udp => f.write_str("udp"),
        }
    }
}

impl From<Protocol> for Transport {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::TCP => Self::Tcp,
            Protocol::UDP => Self::Udp,
        }
    }
}

/// The inferred runtime family. Only recognised runtimes carry a badge;
/// generic processes stay plain text so the UI never invents an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Docker,
    Podman,
    Rust,
    Go,
    Python,
    Node,
    Generic,
}

impl RuntimeKind {
    pub const fn badge(self) -> Option<&'static str> {
        match self {
            Self::Docker => Some("🐳"),
            Self::Podman => Some("🦭"),
            Self::Rust => Some("🦀"),
            Self::Go => Some("🦫"),
            Self::Python => Some("🐍"),
            Self::Node => Some("🐱"),
            Self::Generic => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Docker => "Docker",
            Self::Podman => "Podman",
            Self::Rust => "Rust",
            Self::Go => "Go",
            Self::Python => "Python",
            Self::Node => "Node",
            Self::Generic => "Generic",
        }
    }
}

/// An owned badge plus optional detail (currently a Docker container name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContext {
    pub kind: RuntimeKind,
    pub detail: Option<String>,
}

impl RuntimeContext {
    fn new(kind: RuntimeKind, detail: Option<String>) -> Self {
        Self { kind, detail }
    }

    /// Text for the `CONTEXT` column, e.g. `🐳 my-api`, `🦀 Rust`, or `Generic`.
    ///
    /// A detail replaces the runtime label inside the badge instead of being
    /// appended to it. Empty details and details that merely repeat the label
    /// are ignored, so the column can never render `🦀 Rust Rust` or
    /// `Generic Generic`.
    pub fn text(&self) -> String {
        let detail = self
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .filter(|detail| !detail.eq_ignore_ascii_case(self.kind.label()));

        match (self.kind.badge(), detail) {
            (Some(badge), Some(detail)) => format!("{badge} {detail}"),
            (Some(badge), None) => format!("{badge} {}", self.kind.label()),
            (None, Some(detail)) => detail.to_owned(),
            (None, None) => self.kind.label().to_owned(),
        }
    }
}

/// One listening socket, fully detached from the kernel and `sysinfo`.
#[derive(Debug, Clone)]
pub struct PortEntry {
    pub port: u16,
    pub pid: u32,
    pub process_name: String,
    pub context: RuntimeContext,
    pub memory_bytes: u64,
    pub cmdline: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub exe: Option<PathBuf>,
    pub bind_addr: std::net::SocketAddr,
    pub transport: Transport,
}

impl PortEntry {
    pub fn memory_mb(&self) -> f64 {
        self.memory_bytes as f64 / (1024.0 * 1024.0)
    }

    pub fn cmdline_string(&self) -> String {
        self.cmdline.join(" ")
    }

    pub fn cwd_display(&self) -> String {
        self.cwd
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "-".to_owned())
    }
}

/// Result of one complete scan. `Instant` is cheap to copy and lets the UI
/// show a "last refreshed" age without storing a wall-clock timestamp.
#[derive(Debug, Clone)]
pub struct ScanSnapshot {
    pub entries: Vec<PortEntry>,
    pub generated_at: Instant,
    pub docker_available: bool,
}

#[derive(Debug)]
pub enum ScanError {
    /// The kernel socket table could not be read.
    Sockets(String),
    /// The blocking worker running the scan panicked or was cancelled.
    Worker(String),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sockets(message) => write!(f, "could not enumerate listening sockets: {message}"),
            Self::Worker(message) => write!(f, "scanner worker failed: {message}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// Docker's published-port index. It is built once per scan and then borrowed
/// immutably while badges are inferred. `HashMap` is the right shape here:
/// lookup is `O(1)` by host port, and the map is tiny (one entry per published
/// container port).
#[derive(Debug, Clone, Default)]
pub struct DockerIndex {
    by_port: HashMap<u16, String>,
}

impl DockerIndex {
    pub fn is_empty(&self) -> bool {
        self.by_port.is_empty()
    }

    pub fn container_for_port(&self, port: u16) -> Option<&str> {
        // The returned `&str` borrows from `self`; callers consume it
        // immediately (`to_owned`) before the index is dropped.
        self.by_port.get(&port).map(String::as_str)
    }
}

/// Metadata copied out of a borrowed `sysinfo::Process`.
///
/// Every field is owned. The `System` can be dropped as soon as this struct is
/// built, which is what allows the scanner to run in `spawn_blocking` and send
/// the result across threads.
struct ProcMeta {
    name: String,
    exe: Option<PathBuf>,
    cmdline: Vec<String>,
    cwd: Option<PathBuf>,
    memory_bytes: u64,
}

impl ProcMeta {
    fn load(system: &System, pid: u32, fallback_name: &str) -> Self {
        let Some(process) = system.process(Pid::from_u32(pid)) else {
            return Self {
                name: fallback_name.to_owned(),
                exe: None,
                cmdline: vec![fallback_name.to_owned()],
                cwd: None,
                memory_bytes: 0,
            };
        };

        let cmdline: Vec<String> = process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        Self {
            name: {
                let name = process.name().to_string_lossy().into_owned();
                if name.is_empty() {
                    fallback_name.to_owned()
                } else {
                    name
                }
            },
            exe: process.exe().map(Path::to_path_buf),
            cmdline: if cmdline.is_empty() {
                vec![fallback_name.to_owned()]
            } else {
                cmdline
            },
            // `canonicalize` resolves symlinks. On macOS the cwd reported by
            // the kernel can be a symlinked path (e.g. /tmp -> /private/tmp).
            // If it fails (permissions, process exited), keep the raw path
            // instead of discarding useful information.
            cwd: process
                .cwd()
                .map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf())),
            // sysinfo reports RSS in bytes; the UI converts to MiB.
            memory_bytes: process.memory(),
        }
    }
}

/// Run one complete scan on the current thread.
///
/// This function is synchronous because `listeners` and `sysinfo` are
/// synchronous. The async entry point below moves it to a blocking worker.
pub fn scan_blocking(docker: &DockerIndex) -> Result<Vec<PortEntry>, ScanError> {
    let listeners = listeners::get_all().map_err(|err| ScanError::Sockets(err.to_string()))?;

    // Refresh only the processes that own a listening socket, and only the
    // fields the UI and restart flow need. This avoids the system-wide work of
    // `System::new_all()` on every scan. Name and PID come from the kernel's
    // process table and are always available.
    let mut pids: Vec<Pid> = listeners
        .iter()
        .filter(|listener| listener.state == SocketState::Listen)
        .map(|listener| Pid::from_u32(listener.process.pid))
        .collect();
    pids.sort_unstable();
    pids.dedup();

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&pids),
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet),
    );

    let mut meta_cache: HashMap<u32, ProcMeta> = HashMap::new();
    let mut by_socket: HashMap<(u16, u32, Transport), PortEntry> = HashMap::new();

    for listener in listeners {
        // The kernel reports many transient connections. Only LISTEN sockets
        // are long-lived enough to be useful in the port manager. (UDP has no
        // LISTEN state, so this intentionally excludes ephemeral UDP sockets.)
        if listener.state != SocketState::Listen {
            continue;
        }

        let transport = Transport::from(listener.protocol);
        let port = listener.socket.port();
        let pid = listener.process.pid;
        let key = (port, pid, transport);

        // The same process can listen on both 0.0.0.0:8080 and [::]:8080.
        // Collapse those into one row while keeping the first endpoint.
        if by_socket.contains_key(&key) {
            continue;
        }

        let meta = meta_cache
            .entry(pid)
            .or_insert_with(|| ProcMeta::load(&system, pid, &listener.process.name));

        let context = infer_context(meta, port, docker);

        by_socket.insert(
            key,
            PortEntry {
                port,
                pid,
                process_name: meta.name.clone(),
                context,
                memory_bytes: meta.memory_bytes,
                cmdline: meta.cmdline.clone(),
                cwd: meta.cwd.clone(),
                exe: meta.exe.clone(),
                bind_addr: listener.socket,
                transport,
            },
        );
    }

    let mut entries: Vec<PortEntry> = by_socket.into_values().collect();
    entries.sort_by_key(|entry| (entry.port, entry.pid, entry.transport));
    Ok(entries)
}

/// Async scan entry point used by the background worker.
///
/// The Docker lookup is async and time-bounded. The kernel/process scan is
/// moved to Tokio's blocking pool so it can never stall the render loop.
pub async fn scan() -> Result<ScanSnapshot, ScanError> {
    let docker = docker_index().await;
    let docker_available = !docker.is_empty();

    let entries = tokio::task::spawn_blocking(move || scan_blocking(&docker))
        .await
        .map_err(|err| ScanError::Worker(err.to_string()))??;

    Ok(ScanSnapshot {
        entries,
        generated_at: Instant::now(),
        docker_available,
    })
}

/// Query Docker without blocking the async runtime.
///
/// Any failure (Docker not installed, daemon down, timeout) degrades silently
/// to an empty index: badges still fall back to process-name heuristics.
pub async fn docker_index() -> DockerIndex {
    let output = tokio::process::Command::new("docker")
        .args(["ps", "--format", "{{.Names}}\t{{.Ports}}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();

    match tokio::time::timeout(DOCKER_LOOKUP_TIMEOUT, output).await {
        Ok(Ok(output)) if output.status.success() => {
            parse_docker_ps(&String::from_utf8_lossy(&output.stdout))
        }
        _ => DockerIndex::default(),
    }
}

fn parse_docker_ps(output: &str) -> DockerIndex {
    let mut index = DockerIndex::default();

    for line in output.lines() {
        // `{{.Names}}\t{{.Ports}}`; names cannot contain tabs.
        let Some((name, ports)) = line.split_once('\t') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        for captures in DOCKER_PORT_RE.captures_iter(ports) {
            let Some(start) = captures.get(1) else {
                continue;
            };
            let Ok(start) = start.as_str().parse::<u16>() else {
                continue;
            };
            let end = captures
                .get(2)
                .and_then(|port| port.as_str().parse::<u16>().ok())
                .unwrap_or(start);

            // If two containers somehow publish the same host port, keep the
            // first one. Docker would normally reject the conflict.
            for port in start..=end {
                index.by_port.entry(port).or_insert_with(|| name.to_owned());
            }
        }
    }

    index
}

/// Prioritized runtime heuristics.
///
/// Order matters: container proxies often live inside a Rust or Go process
/// tree, and a Python project may also contain a `package.json`. We stop at the
/// first confident match.
fn infer_context(meta: &ProcMeta, port: u16, docker: &DockerIndex) -> RuntimeContext {
    let process_name = normalize_binary_name(&meta.name);
    let exe_name = meta
        .exe
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| normalize_binary_name(&name.to_string_lossy()))
        .unwrap_or_default();
    let cmdline = meta.cmdline.join(" ").to_ascii_lowercase();
    let exe_path = meta
        .exe
        .as_deref()
        .map(|path| path.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    // 1. Docker / OrbStack. The container lookup is an enhancement, not a
    // requirement: the badge is inferred even when Docker cannot be queried.
    if process_name.contains("docker-proxy")
        || process_name.starts_with("com.docker")
        || process_name.contains("orbstack-helper")
        || process_name.contains("vpnkit")
        || cmdline.contains("com.docker.")
        || exe_path.contains("/docker/")
    {
        let detail = docker.container_for_port(port).map(str::to_owned);
        return RuntimeContext::new(RuntimeKind::Docker, detail);
    }

    // 2. Podman (rootless port forwarder / VM proxy / container monitor).
    if process_name.contains("rootlessport")
        || process_name.contains("gvproxy")
        || process_name.contains("conmon")
        || process_name.contains("podman")
    {
        return RuntimeContext::new(RuntimeKind::Podman, None);
    }

    // 3. Explicit runtime executables and launch patterns are stronger evidence
    // than project files. A Python server started from a Rust checkout is still
    // Python, not Rust.
    let python_runtime = matches!(
        exe_name.as_str(),
        "python" | "python3" | "pypy" | "pypy3" | "uvicorn" | "gunicorn" | "hypercorn"
    ) || exe_name.starts_with("python")
        || process_name.starts_with("python")
        || meta.cmdline.iter().any(|arg| arg.ends_with(".py"));
    if python_runtime {
        return RuntimeContext::new(RuntimeKind::Python, None);
    }

    let node_runtime = matches!(
        exe_name.as_str(),
        "node" | "nodejs" | "bun" | "deno" | "tsx" | "ts-node"
    ) || process_name == "node"
        || process_name.starts_with("node");
    if node_runtime {
        return RuntimeContext::new(RuntimeKind::Node, None);
    }

    let rust_target = is_rust_target_artifact(&exe_path, &cmdline);
    if rust_target {
        return RuntimeContext::new(RuntimeKind::Rust, None);
    }

    let go_build = exe_path.contains("/go-build/")
        || cmdline.contains("go-build")
        || cmdline.contains("go run ");
    if go_build {
        return RuntimeContext::new(RuntimeKind::Go, None);
    }

    // 4. Project-file fallbacks. These run last so an interpreter or a compiled
    // artifact always wins over an unrelated manifest in the working directory.
    let has_cargo_manifest = meta
        .cwd
        .as_deref()
        .is_some_and(|cwd| cwd.join("Cargo.toml").is_file());
    if has_cargo_manifest {
        return RuntimeContext::new(RuntimeKind::Rust, None);
    }

    let has_go_mod = meta
        .cwd
        .as_deref()
        .is_some_and(|cwd| cwd.join("go.mod").is_file());
    if has_go_mod {
        return RuntimeContext::new(RuntimeKind::Go, None);
    }

    let has_package_json = meta
        .cwd
        .as_deref()
        .is_some_and(|cwd| cwd.join("package.json").is_file());
    if has_package_json {
        return RuntimeContext::new(RuntimeKind::Node, None);
    }

    // 5. Everything else: native binaries, databases, daemons.
    RuntimeContext::new(RuntimeKind::Generic, None)
}

fn is_rust_target_artifact(exe_path: &str, cmdline: &str) -> bool {
    const TARGET_MARKERS: [&str; 4] = [
        "/target/debug/",
        "/target/release/",
        "\\target\\debug\\",
        "\\target\\release\\",
    ];

    TARGET_MARKERS
        .iter()
        .any(|marker| exe_path.contains(marker) || cmdline.contains(marker))
}

fn normalize_binary_name(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_reports_the_current_process_for_a_listening_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
        let port = listener.local_addr().expect("listener address").port();

        let entries = scan_blocking(&DockerIndex::default()).expect("scan listening sockets");
        let entry = entries
            .iter()
            .find(|entry| entry.port == port && entry.pid == std::process::id())
            .expect("current process should own the test listener");

        assert!(!entry.process_name.is_empty());
        assert!(!entry.cmdline.is_empty());
        assert!(entry.cwd.is_some());
    }

    #[test]
    fn generic_context_has_no_badge_and_no_duplicate_label() {
        let context = RuntimeContext::new(RuntimeKind::Generic, None);

        assert_eq!(context.kind.badge(), None);
        assert_eq!(context.text(), "Generic");
    }

    #[test]
    fn runtime_detail_replaces_the_label_without_duplication() {
        let docker = RuntimeContext::new(RuntimeKind::Docker, Some("api".to_owned()));
        let repeated = RuntimeContext::new(RuntimeKind::Rust, Some("rust".to_owned()));

        assert_eq!(docker.text(), "🐳 api");
        assert_eq!(repeated.text(), "🦀 Rust");
    }

    #[test]
    fn parses_docker_published_ports() {
        let output = "api\t0.0.0.0:8080->80/tcp, [::]:8080->80/tcp\n\
                      redis\t127.0.0.1:6379->6379/tcp\n\
                      private\t80/tcp\n";
        let index = parse_docker_ps(output);

        assert_eq!(index.container_for_port(8080), Some("api"));
        assert_eq!(index.container_for_port(6379), Some("redis"));
        assert_eq!(index.container_for_port(80), None);
    }

    #[test]
    fn parses_port_ranges() {
        let index = parse_docker_ps("worker\t0.0.0.0:8000-8002->8000-8002/tcp\n");

        assert_eq!(index.container_for_port(8000), Some("worker"));
        assert_eq!(index.container_for_port(8001), Some("worker"));
        assert_eq!(index.container_for_port(8002), Some("worker"));
        assert_eq!(index.container_for_port(8003), None);
    }

    #[test]
    fn explicit_runtime_beats_a_project_manifest() {
        let root = std::env::temp_dir().join("zooport-runtime-context-test");
        std::fs::create_dir_all(&root).expect("create temp dir");
        std::fs::write(root.join("Cargo.toml"), "").expect("write Cargo.toml");

        let python = ProcMeta {
            name: "Python".to_owned(),
            exe: Some(PathBuf::from("/usr/bin/python3")),
            cmdline: vec!["python3".to_owned(), "server.py".to_owned()],
            cwd: Some(root.clone()),
            memory_bytes: 1024 * 1024,
        };
        let node = ProcMeta {
            name: "node".to_owned(),
            exe: Some(PathBuf::from("/usr/bin/node")),
            cmdline: vec!["node".to_owned()],
            cwd: Some(root.clone()),
            memory_bytes: 1024 * 1024,
        };

        let index = DockerIndex::default();
        assert_eq!(
            infer_context(&python, 8000, &index).kind,
            RuntimeKind::Python
        );
        assert_eq!(infer_context(&node, 3000, &index).kind, RuntimeKind::Node);

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn docker_wins_over_project_files() {
        let root = std::env::temp_dir().join("zooport-docker-context-test");
        std::fs::create_dir_all(&root).expect("create temp dir");
        std::fs::write(root.join("Cargo.toml"), "").expect("write Cargo.toml");

        let meta = ProcMeta {
            name: "docker-proxy".to_owned(),
            exe: Some(PathBuf::from("/usr/local/bin/docker-proxy")),
            cmdline: vec!["docker-proxy".to_owned()],
            cwd: Some(root.clone()),
            memory_bytes: 1024 * 1024,
        };
        let mut index = DockerIndex::default();
        index.by_port.insert(8080, "api".to_owned());

        let context = infer_context(&meta, 8080, &index);
        assert_eq!(context.kind, RuntimeKind::Docker);
        assert_eq!(context.detail.as_deref(), Some("api"));

        std::fs::remove_dir_all(root).ok();
    }
}
