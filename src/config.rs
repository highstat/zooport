//! Optional `zooport.toml` configuration.
//!
//! Configuration is intentionally best-effort: a missing, unreadable, or
//! malformed file falls back to the defaults instead of preventing the TUI
//! from starting.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

use crate::scanner::PortEntry;

pub const DEFAULT_ACCENT: Color = Color::Rgb(255, 179, 71);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    accent: Color,
    ignored_ports: Vec<u16>,
    ignored_processes: Vec<String>,
}

impl Config {
    /// Load the first configuration file found.
    ///
    /// Search order:
    ///
    /// 1. `$ZOOPORT_CONFIG` (explicit path)
    /// 2. `./zooport.toml` (project-local)
    /// 3. `$XDG_CONFIG_HOME/zooport/zooport.toml`
    ///    (`$HOME/.config/zooport/zooport.toml` when XDG is unset)
    /// 4. The legacy `config.toml` name in those directories
    ///
    /// On Windows, `%APPDATA%\zooport\` is checked before the XDG locations.
    pub fn load() -> Self {
        Self::load_first(config_paths())
    }

    fn load_first(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        paths
            .into_iter()
            .find(|path| path.is_file())
            .as_deref()
            .map(Self::load_from)
            .unwrap_or_default()
    }

    fn load_from(path: &Path) -> Self {
        let Ok(contents) = fs::read_to_string(path) else {
            return Self::default();
        };
        Self::from_toml(&contents)
    }

    pub(crate) fn from_toml(contents: &str) -> Self {
        let Ok(raw) = toml::from_str::<RawConfig>(contents) else {
            return Self::default();
        };

        let accent = raw
            .accent
            .as_deref()
            .and_then(parse_accent)
            .unwrap_or(DEFAULT_ACCENT);

        let mut ignored_ports = raw.ignored_ports;
        ignored_ports.sort_unstable();
        ignored_ports.dedup();

        let mut ignored_processes: Vec<String> = raw
            .ignored_processes
            .into_iter()
            .map(|name| name.trim().to_lowercase())
            .filter(|name| !name.is_empty())
            .collect();
        ignored_processes.sort();
        ignored_processes.dedup();

        Self {
            accent,
            ignored_ports,
            ignored_processes,
        }
    }

    pub fn accent(&self) -> Color {
        self.accent
    }

    pub fn ignores(&self, entry: &PortEntry) -> bool {
        if self.ignored_ports.binary_search(&entry.port).is_ok() {
            return true;
        }

        if self.ignored_processes.is_empty() {
            return false;
        }

        let process_name = entry.process_name.trim().to_lowercase();
        self.ignored_processes
            .iter()
            .any(|ignored| ignored == &process_name)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            accent: DEFAULT_ACCENT,
            ignored_ports: Vec::new(),
            ignored_processes: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    accent: Option<String>,
    #[serde(default)]
    ignored_ports: Vec<u16>,
    #[serde(default)]
    ignored_processes: Vec<String>,
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(path) = env::var_os("ZOOPORT_CONFIG").filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(path));
    }

    paths.push(PathBuf::from("zooport.toml"));

    #[cfg(windows)]
    if let Some(root) = env::var_os("APPDATA").filter(|value| !value.is_empty()) {
        push_config_candidates(&mut paths, PathBuf::from(root));
    }

    if let Some(root) = env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        push_config_candidates(&mut paths, PathBuf::from(root));
    } else if let Some(home) = env::var_os("HOME").filter(|value| !value.is_empty()) {
        push_config_candidates(&mut paths, PathBuf::from(home).join(".config"));
    }

    paths
}

fn push_config_candidates(paths: &mut Vec<PathBuf>, root: PathBuf) {
    let directory = root.join("zooport");
    paths.push(directory.join("zooport.toml"));
    paths.push(directory.join("config.toml"));
}

fn parse_accent(value: &str) -> Option<Color> {
    let value = value.trim();
    let hex = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .or_else(|| value.strip_prefix("0X"));

    if let Some(hex) = hex {
        if hex.len() != 6 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
            return None;
        }

        let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(red, green, blue));
    }

    match value.to_ascii_lowercase().as_str() {
        "amber" => Some(DEFAULT_ACCENT),
        "orange" => Some(Color::Rgb(255, 165, 0)),
        "black" => Some(Color::Black),
        "red" | "lightred" => Some(Color::Red),
        "green" | "lightgreen" => Some(Color::Green),
        "yellow" | "lightyellow" => Some(Color::Yellow),
        "blue" | "lightblue" => Some(Color::Blue),
        "magenta" | "lightmagenta" => Some(Color::Magenta),
        "cyan" | "lightcyan" => Some(Color::Cyan),
        "gray" | "grey" | "lightgray" | "lightgrey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "white" => Some(Color::White),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::scanner::{RuntimeContext, RuntimeKind, Transport};

    static NEXT_CONFIG_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_config(contents: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let unique = NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "zooport-config-{}-{unique}-{nanos}.toml",
            std::process::id()
        ));
        fs::write(&path, contents).expect("write config");
        path
    }

    fn entry(port: u16, name: &str) -> PortEntry {
        PortEntry {
            port,
            pid: 10,
            process_name: name.to_owned(),
            context: RuntimeContext {
                kind: RuntimeKind::Generic,
                detail: None,
            },
            memory_bytes: 0,
            cmdline: vec![name.to_owned()],
            cwd: None,
            exe: None,
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            transport: Transport::Tcp,
        }
    }

    #[test]
    fn parses_hex_and_named_accents() {
        assert_eq!(parse_accent("#3366FF"), Some(Color::Rgb(51, 102, 255)));
        assert_eq!(parse_accent("0x10b981"), Some(Color::Rgb(16, 185, 129)));
        assert_eq!(parse_accent("cyan"), Some(Color::Cyan));
        assert_eq!(parse_accent("not-a-color"), None);
        assert_eq!(parse_accent("#12345"), None);
    }

    #[test]
    fn loads_ignored_ports_and_processes_case_insensitively() {
        let path = temp_config(
            "accent = \"#112233\"\n\
             ignored_ports = [3000, 3000]\n\
             ignored_processes = [\"Spotlight\", \" \"]\n",
        );

        let config = Config::load_from(&path);

        assert_eq!(config.accent(), Color::Rgb(17, 34, 51));
        assert!(config.ignores(&entry(3000, "node")));
        assert!(config.ignores(&entry(8000, "spotlight")));
        assert!(!config.ignores(&entry(8000, "node")));

        fs::remove_file(path).ok();
    }

    #[test]
    fn missing_or_invalid_config_uses_defaults() {
        assert_eq!(
            Config::load_from(Path::new("/definitely/missing/zooport.toml")),
            Config::default()
        );

        let invalid = temp_config("accent = [not valid toml");
        assert_eq!(Config::load_from(&invalid), Config::default());
        fs::remove_file(invalid).ok();

        let invalid_accent = temp_config("accent = \"nope\"\nignored_ports = [3000]\n");
        let config = Config::load_from(&invalid_accent);
        assert_eq!(config.accent(), DEFAULT_ACCENT);
        assert!(config.ignores(&entry(3000, "node")));
        fs::remove_file(invalid_accent).ok();
    }

    #[test]
    fn loads_the_first_existing_config_path() {
        let first = temp_config("accent = \"red\"\n");
        let second = temp_config("accent = \"blue\"\n");
        let missing = first.with_extension("missing");

        let config = Config::load_first(vec![missing, first.clone(), second.clone()]);

        assert_eq!(config.accent(), Color::Red);

        fs::remove_file(first).ok();
        fs::remove_file(second).ok();
    }
}
