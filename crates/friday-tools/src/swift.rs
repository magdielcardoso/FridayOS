//! Lazily compiled Swift helpers.
//!
//! Some macOS APIs (synthetic events, Vision OCR, Speech) have no ergonomic
//! Rust bindings. Instead of vendoring `objc2` sprawl, we ship tiny Swift
//! sources and compile them on first use into the user's cache directory.
//! Build once, then the tool just execs the cached binary.

use std::path::PathBuf;
use std::time::SystemTime;

use crate::registry::ToolError;

/// Returns the path to a compiled helper, building it if needed.
pub fn helper(name: &str) -> Result<PathBuf, ToolError> {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("swift")
        .join(format!("{name}.swift"));
    if !source.is_file() {
        return Err(ToolError::Failed(format!(
            "helper Swift ausente: {}",
            source.display()
        )));
    }

    let cache = cache_dir();
    std::fs::create_dir_all(&cache).map_err(|e| ToolError::failed(e.to_string()))?;
    let binary = cache.join(name);

    let stale = match (binary.metadata(), source.metadata()) {
        (Ok(bin), Ok(src)) => match (bin.modified(), src.modified()) {
            (Ok(bin_at), Ok(src_at)) => bin_at < src_at,
            _ => true,
        },
        _ => true,
    };

    if stale {
        let status = std::process::Command::new("/usr/bin/xcrun")
            .args(["swiftc", "-O", "-o"])
            .arg(&binary)
            .arg(&source)
            .status()
            .map_err(|e| ToolError::Failed(format!("xcrun swiftc: {e}")))?;
        if !status.success() {
            return Err(ToolError::Failed(format!(
                "falha compilando o helper Swift `{name}`"
            )));
        }
    }

    Ok(binary)
}

/// Runs a cached Swift helper with arguments and returns stdout.
pub fn run(name: &str, args: &[&str]) -> Result<String, ToolError> {
    let binary = helper(name)?;
    let output = std::process::Command::new(binary)
        .args(args)
        .output()
        .map_err(|e| ToolError::Failed(format!("helper {name}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Failed(format!(
            "helper {name} falhou: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn cache_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Caches/FridayOS/bin")
}

/// Unix seconds, used to timestamp audit and helper builds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
