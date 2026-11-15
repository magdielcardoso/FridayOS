use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::policy::Risk;

/// Append-only JSONL log of every tool call, kept next to the app's data.
/// This is the file the user can read to know exactly what Friday did.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `~/Library/Application Support/FridayOS/audit.jsonl`
    pub fn default_path() -> PathBuf {
        let home = std::env::var_os("HOME").unwrap_or_default();
        PathBuf::from(home)
            .join("Library/Application Support/FridayOS")
            .join("audit.jsonl")
    }

    pub fn discover() -> Self {
        Self::new(Self::default_path())
    }

    pub fn record(&self, tool: &str, risk: Risk, args: &Value, dry_run: bool) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let entry = json!({
            "ts": SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "tool": tool,
            "risk": risk.label(),
            "dry_run": dry_run,
            "args": args,
        });
        // Keep the file bounded; older entries stay in audit.1.jsonl.
        if fs::metadata(&self.path)
            .map(|meta| meta.len() > 5_000_000)
            .unwrap_or(false)
        {
            let rotated = self.path.with_file_name("audit.1.jsonl");
            let _ = fs::rename(&self.path, rotated);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(file, "{entry}");
        }
    }
}
