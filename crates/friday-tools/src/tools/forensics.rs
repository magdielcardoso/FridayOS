//! Retroactive macOS investigation: bounded reads of unified logs, power logs
//! and recent file changes. Evidence only; never continuous capture.

use std::time::Duration;

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn run(program: &str, args: &[String], timeout: Duration) -> Result<String, ToolError> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| ToolError::failed(e.to_string()))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(e) => return Err(ToolError::failed(e.to_string())),
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            return Err(ToolError::failed("consulta demorou demais"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read;
        let _ = stdout.read_to_string(&mut out);
    }
    Ok(out)
}

fn cap(text: &str, lines: usize) -> Vec<String> {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|line| line.to_string()).collect()
}

struct ForensicsTool {
    spec: ToolSpec,
}

impl Tool for ForensicsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "forensics_log" => {
                let minutes = args.get("minutes").and_then(|v| v.as_u64()).unwrap_or(30).clamp(1, 1440);
                let process = args.get("process").and_then(|v| v.as_str()).unwrap_or("").trim();
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(200).clamp(20, 2000) as usize;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "minutes": minutes, "process": process }));
                }
                let predicate = if process.is_empty() {
                    "eventMessage CONTAINS[c] \"error\" OR eventMessage CONTAINS[c] \"fail\"".to_string()
                } else {
                    format!("process == \"{process}\"")
                };
                // `head` closes the pipe early: unified-log queries can stream
                // for minutes otherwise.
                let safe_process: String = process
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
                    .collect();
                let command = if safe_process.is_empty() {
                    format!(
                        "/usr/bin/log show --last {minutes}m --style compact --predicate 'eventMessage CONTAINS[c] \"error\" OR eventMessage CONTAINS[c] \"fail\"' 2>/dev/null | head -n {limit}"
                    )
                } else {
                    format!(
                        "/usr/bin/log show --last {minutes}m --style compact --predicate 'process == \"{safe_process}\"' 2>/dev/null | head -n {limit}"
                    )
                };
                let output = run("/bin/zsh", &["-lc".into(), command], Duration::from_secs(45))?;
                let _ = predicate;
                Ok(json!({
                    "ok": true,
                    "source": "unified-log",
                    "window_minutes": minutes,
                    "lines": cap(&output, limit),
                    "note": "logs têm retenção limitada; ausência de linha não prova ausência de evento"
                }))
            }
            "forensics_power" => {
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true }));
                }
                let output = run("/usr/bin/pmset", &["-g".into(), "log".into()], Duration::from_secs(15))?;
                Ok(json!({ "ok": true, "lines": cap(&output, 200) }))
            }
            "forensics_changes" => {
                let minutes = args.get("minutes").and_then(|v| v.as_u64()).unwrap_or(1440).clamp(1, 43200);
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("~/Library/Application Support/FridayOS");
                let expanded = if let Some(rest) = path.strip_prefix("~/") {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{home}/{rest}")
                } else {
                    path.to_string()
                };
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "path": expanded }));
                }
                let output = run(
                    "/usr/bin/find",
                    &[
                        expanded,
                        "-type".into(),
                        "f".into(),
                        "-mmin".into(),
                        format!("-{minutes}"),
                        "-not".into(),
                        "-path".into(),
                        "*/.git/*".into(),
                    ],
                    Duration::from_secs(20),
                )?;
                Ok(json!({ "ok": true, "path": path, "window_minutes": minutes, "files": cap(&output, 500) }))
            }
            "forensics_health" => {
                let software = run("/usr/sbin/system_profiler", &["SPSoftwareDataType".into()], Duration::from_secs(20))?;
                let storage = run("/usr/sbin/system_profiler", &["SPStorageDataType".into()], Duration::from_secs(20))?;
                Ok(json!({ "ok": true, "software": cap(&software, 40), "storage": cap(&storage, 40) }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> ForensicsTool {
    ForensicsTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "forensics_log",
        "Investiga o Unified Log do macOS numa janela de tempo, opcionalmente por processo.",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "minutes": { "type": "integer", "minimum": 1, "maximum": 1440, "default": 30 },
                "process": { "type": "string" },
                "limit": { "type": "integer", "minimum": 20, "maximum": 2000, "default": 200 }
            }
        }),
    ));
    registry.add(tool(
        "forensics_power",
        "Mostra o log de energia/sleep/wake do macOS.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "forensics_changes",
        "Lista arquivos modificados nos últimos N minutos num diretório autorizado.",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "minutes": { "type": "integer", "minimum": 1, "maximum": 43200, "default": 1440 },
                "path": { "type": "string", "default": "~/Library/Application Support/FridayOS" }
            }
        }),
    ));
    registry.add(tool(
        "forensics_health",
        "Resumo de software e armazenamento do sistema para investigações.",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
}
