//! Detection of installed harness tools (opencode, Claude Code) and
//! registration of the local-proxy MCP server into their config files.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Executable name of the opencode CLI.
const OPENCODE_BIN: &str = "opencode";

/// Executable name of the Claude Code CLI.
const CLAUDE_BIN: &str = "claude";

/// Suffixes appended when resolving an executable on PATH (Windows).
#[cfg(target_os = "windows")]
const PATH_EXTS: [&str; 4] = ["", ".exe", ".cmd", ".bat"];

/// No extra suffix on Unix: executables are plain files.
#[cfg(not(target_os = "windows"))]
const PATH_EXTS: [&str; 1] = [""];

/// A supported harness: an external AI CLI tool that can run MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// opencode, configured at `<home>/.config/opencode/opencode.json`.
    OpenCode,
    /// Claude Code, configured at the global `<home>/.claude.json` file.
    Claude,
}

impl Harness {
    /// Human display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Claude => "claude",
        }
    }

    /// The config file path for this harness in the user's home, using the
    /// OS-appropriate paths.
    ///
    /// # Panics
    ///
    /// Panics if the OS cannot determine the user's home directory.
    #[must_use]
    pub fn config_path(self) -> PathBuf {
        let home = directories::BaseDirs::new()
            .expect("base dirs")
            .home_dir()
            .to_path_buf();
        match self {
            Self::OpenCode => home.join(".config").join("opencode").join("opencode.json"),
            Self::Claude => home.join(".claude.json"),
        }
    }

    /// The top-level config key holding MCP server registrations.
    #[must_use]
    const fn mcp_key(self) -> &'static str {
        match self {
            Self::OpenCode => "mcp",
            Self::Claude => "mcpServers",
        }
    }

    /// The MCP server entry that registers local-proxy for this harness.
    #[must_use]
    fn mcp_value(self) -> Value {
        match self {
            Self::OpenCode => json!({
                "type": "local",
                "command": ["local-proxy", "mcp"],
                "enabled": true,
            }),
            Self::Claude => json!({
                "type": "stdio",
                "command": "local-proxy",
                "args": ["mcp"],
                "env": {},
            }),
        }
    }
}

/// Whether `binary` resolves to a file on the system PATH.
#[must_use]
fn is_on_path(binary: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        PATH_EXTS
            .iter()
            .any(|ext| dir.join(format!("{binary}{ext}")).is_file())
    })
}

/// Which harnesses are detected as installed (binary on PATH OR config
/// dir/file exists).
#[must_use]
pub fn detect() -> Vec<Harness> {
    let home = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf());
    let mut detected = Vec::new();
    if is_on_path(OPENCODE_BIN)
        || home
            .as_deref()
            .is_some_and(|h| h.join(".config").join("opencode").is_dir())
    {
        detected.push(Harness::OpenCode);
    }
    if is_on_path(CLAUDE_BIN)
        || home
            .as_deref()
            .is_some_and(|h| h.join(".claude.json").is_file() || h.join(".claude").is_dir())
    {
        detected.push(Harness::Claude);
    }
    detected
}

/// Recursively merge `patch` into `base`, preserving sibling keys: objects are
/// merged field-by-field, everything else is overwritten.
fn deep_merge(base: &mut Value, patch: Value) {
    match (base, patch) {
        (Value::Object(dst), Value::Object(src)) => {
            for (key, value) in src {
                match dst.get_mut(&key) {
                    Some(existing) if existing.is_object() && value.is_object() => {
                        deep_merge(existing, value);
                    }
                    _ => {
                        dst.insert(key, value);
                    }
                }
            }
        }
        (dst, patch) => *dst = patch,
    }
}

/// Merge the local-proxy MCP server entry into an existing JSON config string,
/// preserving all other keys, and return the merged JSON (pretty-printed).
///
/// If the input is empty or blank, starts fresh from `{}`.
///
/// # Errors
///
/// Returns a `serde_json::Error` if the existing content is non-blank and not
/// valid JSON.
pub fn merge_mcp_entry(existing: &str, harness: Harness) -> Result<String, serde_json::Error> {
    let mut root = if existing.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str::<Value>(existing)?
    };
    if !root.is_object() {
        root = Value::Object(Map::new());
    }
    let mut servers = Map::new();
    servers.insert("local-proxy".to_string(), harness.mcp_value());
    let mut patch = Map::new();
    patch.insert(harness.mcp_key().to_string(), Value::Object(servers));
    deep_merge(&mut root, Value::Object(patch));
    serde_json::to_string_pretty(&root)
}

/// Write the harness config, backing up any existing file to `<path>.bak`
/// first. Returns the path written.
///
/// # Errors
///
/// Returns an I/O error if the parent directory cannot be created, the backup
/// cannot be copied, or the config cannot be written.
pub fn write_with_backup(path: &Path, content: &str) -> io::Result<PathBuf> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let backup = PathBuf::from(format!("{}.bak", path.display()));
        std::fs::copy(path, &backup)?;
    }
    std::fs::write(path, content)?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_existing_opencode_keys() {
        let existing = r#"{
            "name": "my-project",
            "mcp": {
                "other": { "type": "local", "command": ["other"], "enabled": false }
            }
        }"#;
        let merged = merge_mcp_entry(existing, Harness::OpenCode).expect("merge");
        let v: Value = serde_json::from_str(&merged).expect("valid json");
        assert_eq!(v["name"], "my-project");
        assert_eq!(
            v["mcp"]["other"],
            json!({ "type": "local", "command": ["other"], "enabled": false })
        );
        assert_eq!(v["mcp"]["local-proxy"]["type"], "local");
        assert_eq!(
            v["mcp"]["local-proxy"]["command"],
            json!(["local-proxy", "mcp"])
        );
        assert_eq!(v["mcp"]["local-proxy"]["enabled"], true);
    }

    #[test]
    fn merge_preserves_claude_projects() {
        let existing = r#"{
            "projects": { "C:\\dev\\x": { "history": [1, 2] } },
            "someOther": 42
        }"#;
        let merged = merge_mcp_entry(existing, Harness::Claude).expect("merge");
        let v: Value = serde_json::from_str(&merged).expect("valid json");
        assert_eq!(v["projects"]["C:\\dev\\x"]["history"], json!([1, 2]));
        assert_eq!(v["someOther"], 42);
        assert_eq!(v["mcpServers"]["local-proxy"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["local-proxy"]["command"], "local-proxy");
        assert_eq!(v["mcpServers"]["local-proxy"]["args"], json!(["mcp"]));
        assert_eq!(v["mcpServers"]["local-proxy"]["env"], json!({}));
    }

    #[test]
    fn merge_starts_fresh_on_empty_input() {
        for blank in ["", "   ", "\n"] {
            let merged = merge_mcp_entry(blank, Harness::OpenCode).expect("merge");
            let v: Value = serde_json::from_str(&merged).expect("valid json");
            assert_eq!(
                v["mcp"]["local-proxy"]["command"],
                json!(["local-proxy", "mcp"])
            );
        }
    }

    #[test]
    fn merge_is_idempotent() {
        let existing = r#"{"mcp": {"local-proxy": {"enabled": false, "extra": 1}}}"#;
        let once = merge_mcp_entry(existing, Harness::OpenCode).expect("merge once");
        let twice = merge_mcp_entry(&once, Harness::OpenCode).expect("merge twice");
        let a: Value = serde_json::from_str(&once).expect("valid json");
        let b: Value = serde_json::from_str(&twice).expect("valid json");
        assert_eq!(a, b);
        assert_eq!(a["mcp"]["local-proxy"]["enabled"], true);
        assert_eq!(a["mcp"]["local-proxy"]["extra"], 1);
    }

    #[test]
    fn write_with_backup_creates_bak_when_file_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode.json");
        std::fs::write(&path, "original").expect("write original");
        let written = write_with_backup(&path, "new").expect("write");
        assert_eq!(written, path);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "new");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("opencode.json.bak")).expect("read bak"),
            "original"
        );
    }

    #[test]
    fn write_with_backup_creates_parent_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("opencode").join("opencode.json");
        write_with_backup(&path, "{}").expect("write");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "{}");
    }
}
