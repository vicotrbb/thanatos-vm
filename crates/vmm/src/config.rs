//! One TOML file per VM. Section 11 has no TOML crate, so the subset the file format uses is parsed
//! here into a JSON tree and handed to serde.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{io, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    #[serde(default = "default_memory")]
    pub memory_mib: u64,
    pub kernel: PathBuf,
    #[serde(default)]
    pub cmdline_extra: String,
    pub disk: Vec<Disk>,
    #[serde(default)]
    pub network: Network,
    #[serde(default)]
    pub display: Display,
    #[serde(default)]
    pub api: Api,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub sandbox: Sandbox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sandbox {
    pub enabled: bool,
}

impl Default for Sandbox {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disk {
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    /// Created sparse at this size if the file is missing.
    pub create_mib: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Network {
    /// "user" or, on Linux, "tap".
    pub backend: String,
    /// The tap device name for the "tap" backend.
    pub tap: String,
    /// "allow" or "deny".
    pub default: String,
    pub deny: Vec<String>,
    pub allow: Vec<String>,
    pub forward: Vec<String>,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            backend: "user".into(),
            tap: String::new(),
            default: "allow".into(),
            deny: Vec::new(),
            allow: Vec::new(),
            forward: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Display {
    /// "headless", "vnc" or "window".
    pub mode: String,
    pub width: u32,
    pub height: u32,
    pub vnc_bind: String,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            mode: "window".into(),
            width: 1280,
            height: 800,
            vnc_bind: "127.0.0.1:5901".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Api {
    /// Defaults to `~/.thanatos/vms/<name>/api.sock`.
    pub socket: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Agent {
    pub enabled: bool,
}

impl Default for Agent {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn default_name() -> String {
    "vm".into()
}
fn default_vcpus() -> u32 {
    2
}
fn default_memory() -> u64 {
    2048
}

/// `~/` in a configured path means the invoking user's home.
pub fn expand(path: &std::path::Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

impl Config {
    pub fn parse(text: &str) -> io::Result<Self> {
        let tree = toml(text).map_err(invalid)?;
        serde_json::from_value(tree).map_err(|e| invalid(e.to_string()))
    }

    pub fn load(path: &std::path::Path) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }

    pub fn socket(&self) -> PathBuf {
        match &self.api.socket {
            Some(path) => expand(path),
            None => expand(std::path::Path::new("~/.thanatos/vms"))
                .join(&self.name)
                .join("api.sock"),
        }
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Keys, `[table]`, `[[array of tables]]`, and string, integer, boolean and string-array values.
fn toml(text: &str) -> Result<Value, String> {
    let mut root = Map::new();
    let mut section: Option<(String, bool)> = None;
    for (index, line) in text.lines().enumerate() {
        let at = |message: &str| format!("config line {}: {message}", index + 1);
        let line = strip_comment(line).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            let tables = root.entry(name.trim()).or_insert_with(|| json!([]));
            tables
                .as_array_mut()
                .ok_or_else(|| at("not an array of tables"))?
                .push(json!({}));
            section = Some((name.trim().to_string(), true));
        } else if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            root.entry(name.trim()).or_insert_with(|| json!({}));
            section = Some((name.trim().to_string(), false));
        } else {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| at("expected key = value"))?;
            let value = scalar(value.trim()).map_err(|e| at(&e))?;
            let table = match &section {
                None => &mut root,
                Some((name, true)) => root[name.as_str()]
                    .as_array_mut()
                    .and_then(|tables| tables.last_mut())
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| at("bad table"))?,
                Some((name, false)) => root[name.as_str()]
                    .as_object_mut()
                    .ok_or_else(|| at("bad table"))?,
            };
            table.insert(key.trim().to_string(), value);
        }
    }
    Ok(Value::Object(root))
}

/// A `#` outside a string starts a comment.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (index, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..index],
            _ => (),
        }
    }
    line
}

fn scalar(value: &str) -> Result<Value, String> {
    if let Some(inner) = value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let items = inner
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(scalar)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Array(items));
    }
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        return Ok(json!(inner));
    }
    match value {
        "true" => Ok(json!(true)),
        "false" => Ok(json!(false)),
        _ => value
            .parse::<i64>()
            .map(|n| json!(n))
            .map_err(|_| format!("unsupported value {value}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_file_and_applies_defaults() {
        let config = Config::parse(
            r#"
name = "dev"   # comment
memory_mib = 512
kernel = "~/k#ernel"
[[disk]]
path = "a.ext4"
[[disk]]
path = "b.ext4"
create_mib = 8192
[network]
deny = ["10.0.0.0/8", "192.168.0.0/16"]
[display]
mode = "vnc"
"#,
        )
        .unwrap();
        assert_eq!(config.memory_mib, 512);
        assert_eq!(config.vcpus, 2);
        assert_eq!(config.kernel, PathBuf::from("~/k#ernel"));
        assert_eq!(config.disk[1].create_mib, Some(8192));
        assert_eq!(config.network.deny.len(), 2);
        assert_eq!(config.network.backend, "user");
        assert_eq!(config.display.mode, "vnc");
        assert_eq!(config.display.width, 1280);
        assert!(config.agent.enabled);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_lines() {
        assert!(Config::parse("kernel = \"k\"\nbogus = 1\n[[disk]]\npath=\"d\"").is_err());
        assert!(Config::parse("kernel").is_err());
    }
}
