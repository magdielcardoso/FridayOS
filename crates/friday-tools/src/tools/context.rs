//! Situational context and operating modes (JARVIS-style autonomy primitives).
//!
//! `friday_mode` persists the current behaviour mode so the speech/notification
//! layer can decide whether to talk, transcribe silently or stay quiet.
//! `context_snapshot` gathers a compact, best-effort picture of the user's
//! current situation. Every subprocess is time-boxed and the whole snapshot
//! shares a ~2 s budget: if a probe is slow or lacks a macOS permission, that
//! field degrades to an empty value plus a note instead of blocking Friday.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Behaviour modes Friday can be in.
const MODES: [&str; 5] = ["normal", "reuniao", "foco", "dirigindo", "jogo"];

/// Whole-snapshot deadline and per-probe caps. The sum of caps can exceed the
/// budget, so `run_capped` always uses the *remaining* budget as a second cap.
const SNAPSHOT_BUDGET: Duration = Duration::from_secs(2);
const AX_CAP: Duration = Duration::from_millis(700);
const CLIP_CAP: Duration = Duration::from_millis(500);
const CAL_CAP: Duration = Duration::from_millis(700);
const DATE_CAP: Duration = Duration::from_millis(300);

const CLIPBOARD_CHARS: usize = 300;
const EVENT_SUMMARY_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// Mode persistence
// ---------------------------------------------------------------------------

fn mode_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/FridayOS")
        .join("mode.json")
}

/// Current mode, validated against [`MODES`]; anything else means `normal`.
pub(crate) fn current_mode() -> String {
    std::fs::read_to_string(mode_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| {
            value
                .get("mode")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_lowercase)
        })
        .filter(|mode| MODES.contains(&mode.as_str()))
        .unwrap_or_else(|| "normal".to_string())
}

fn mode_descriptions() -> Value {
    json!({
        "normal": "responde e age normalmente",
        "reuniao": "não fala: transcreve em silêncio e notifica ao terminar",
        "foco": "sem gatilhos proativos, exceto críticos",
        "dirigindo": "apenas respostas faladas curtas",
        "jogo": "silencioso, exceto palavra de despertar",
    })
}

fn set_mode(mode: &str) -> std::io::Result<()> {
    let path = mode_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = json!({
        "mode": mode,
        "updated": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    std::fs::write(path, serde_json::to_vec_pretty(&payload)?)
}

// ---------------------------------------------------------------------------
// Tool plumbing
// ---------------------------------------------------------------------------

struct ContextTool {
    spec: ToolSpec,
}

impl Tool for ContextTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "friday_mode" => friday_mode(args, ctx),
            "context_snapshot" => context_snapshot(ctx),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(ContextTool {
        spec: ToolSpec {
            name: "friday_mode",
            summary: "Lê ou muda o modo da Friday (normal, reuniao, foco, dirigindo, jogo). Sem `mode`, apenas informa o modo ativo.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["normal", "reuniao", "foco", "dirigindo", "jogo"],
                        "description": "Modo a ativar. Omita para apenas ler o modo atual."
                    }
                },
                "additionalProperties": false
            }),
        },
    });
    registry.add(ContextTool {
        spec: ToolSpec {
            name: "context_snapshot",
            summary: "Snapshot compacto e tolerante a falhas da situação atual: app ativo, área de transferência, próximos eventos de hoje, data/hora e modo da Friday. Sem rede.",
            risk: Risk::ReadOnly,
            args: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
    });
}

// ---------------------------------------------------------------------------
// friday_mode
// ---------------------------------------------------------------------------

fn friday_mode(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let previous = current_mode();
    let Some(requested) = args
        .get("mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(json!({
            "ok": true,
            "mode": previous,
            "modes": MODES,
            "descriptions": mode_descriptions(),
        }));
    };

    let mode = requested.to_lowercase();
    if !MODES.contains(&mode.as_str()) {
        return Err(ToolError::invalid(format!(
            "modo inválido `{requested}`; use: {}",
            MODES.join(", ")
        )));
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "mode": previous,
            "would_set": mode,
        }));
    }

    set_mode(&mode)
        .map_err(|error| ToolError::failed(format!("não consegui salvar o modo: {error}")))?;
    Ok(json!({
        "ok": true,
        "mode": mode,
        "previous": previous,
        "descriptions": mode_descriptions(),
    }))
}

// ---------------------------------------------------------------------------
// context_snapshot
// ---------------------------------------------------------------------------

fn context_snapshot(ctx: &ToolCtx) -> ToolResult {
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would": "coletar app ativo, clipboard, eventos de hoje, data/hora e modo",
        }));
    }

    let deadline = Instant::now() + SNAPSHOT_BUDGET;
    let mut notes: Vec<String> = Vec::new();

    let (active_app, window_title) = frontmost_app(&deadline, &mut notes);
    let (clipboard_text, clipboard_truncated) = clipboard_text(&deadline, &mut notes);
    let (events, calendar_note) = today_events(&deadline, &mut notes);
    let (captured_at, epoch) = local_now(&deadline, &mut notes);

    Ok(json!({
        "ok": true,
        "captured_at": captured_at,
        "epoch": epoch,
        "active_app": active_app,
        "window_title": window_title,
        "clipboard": {
            "text": clipboard_text,
            "truncated": clipboard_truncated,
        },
        "calendar": {
            "events": events,
            "note": calendar_note,
        },
        "friday_mode": current_mode(),
        "notes": notes,
    }))
}

/// Runs a short subprocess. Never waits longer than `cap` or the shared
/// `deadline`, whichever comes first, and never inherits stdin.
fn run_capped(
    program: &str,
    args: &[&str],
    cap: Duration,
    deadline: Instant,
) -> Result<String, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("orçamento de tempo do snapshot esgotado".to_string());
    }
    let timeout = cap.min(remaining);

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;

    // Drain both pipes on threads so a chatty child cannot deadlock on a full
    // pipe while we poll `try_wait`.
    let stdout = child.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{program} excedeu {}ms e foi encerrado",
                        timeout.as_millis()
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program}: {error}"));
            }
        }
    };

    let output = stdout
        .map(|handle| handle.join().unwrap_or_default())
        .unwrap_or_default();
    if !status.success() {
        let detail = stderr
            .map(|handle| handle.join().unwrap_or_default())
            .unwrap_or_default();
        return Err(format!("{program} falhou: {}", detail.trim()));
    }
    Ok(output)
}

fn truncate_chars(value: &str, max: usize) -> (String, bool) {
    let mut out = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max {
            out.push('…');
            return (out, true);
        }
        out.push(ch);
    }
    (out, false)
}

fn frontmost_app(deadline: &Instant, notes: &mut Vec<String>) -> (String, String) {
    // Reuses the same System Events surface as `ax_frontmost`, but inlined here
    // because ax.rs exposes no public helper. Any failure is non-fatal.
    let script = r#"tell application "System Events"
    set appName to ""
    set winTitle to ""
    try
        set frontProc to first application process whose frontmost is true
        set appName to name of frontProc
        try
            set winTitle to title of front window of frontProc
        end try
    end try
    return appName & (ASCII character 31) & winTitle
end tell"#;

    match run_capped("/usr/bin/osascript", &["-e", script], AX_CAP, *deadline) {
        Ok(output) => {
            let fields: Vec<&str> = output.trim_end().split('\u{1f}').collect();
            let app = fields.first().copied().unwrap_or("").trim().to_string();
            let title = fields.get(1).copied().unwrap_or("").trim().to_string();
            (app, title)
        }
        Err(error) => {
            notes.push(format!("app ativo indisponível: {error}"));
            (String::new(), String::new())
        }
    }
}

fn clipboard_text(deadline: &Instant, notes: &mut Vec<String>) -> (String, bool) {
    match run_capped("/usr/bin/pbpaste", &[], CLIP_CAP, *deadline) {
        Ok(text) => {
            let text = text.trim_end_matches(|ch| ch == '\n' || ch == '\r');
            truncate_chars(text, CLIPBOARD_CHARS)
        }
        Err(error) => {
            notes.push(format!("clipboard indisponível: {error}"));
            (String::new(), false)
        }
    }
}

fn today_events(
    deadline: &Instant,
    notes: &mut Vec<String>,
) -> (Vec<Value>, Option<String>) {
    // No EventKit Swift helper ships with this crate, so fall back to
    // Calendar.app over osascript. `with timeout` plus our own cap keeps it
    // bounded; a TCC denial simply yields an empty list and a note.
    let script = r#"with timeout of 1 second
    tell application "Calendar"
        set fs to (ASCII character 31)
        set rs to (ASCII character 30)
        set outText to ""
        set nowDate to current date
        set startOfDay to nowDate - (time of nowDate)
        set endOfDay to startOfDay + 1 * days
        set n to 0
        repeat with cal in calendars
            if n is greater than or equal to 5 then exit repeat
            set evs to {}
            try
                set evs to (every event of cal whose start date >= nowDate and start date < endOfDay)
            end try
            repeat with ev in evs
                if n is greater than or equal to 5 then exit repeat
                set summ to ""
                set startDate to ""
                try
                    set summ to summary of ev
                end try
                try
                    set startDate to start date of ev as text
                end try
                if outText is "" then
                    set outText to summ & fs & startDate
                else
                    set outText to outText & rs & summ & fs & startDate
                end if
                set n to n + 1
            end repeat
        end repeat
        return outText
    end tell
end timeout"#;

    match run_capped("/usr/bin/osascript", &["-e", script], CAL_CAP, *deadline) {
        Ok(output) => {
            let mut events = Vec::new();
            for row in output.trim_end().split('\u{1e}') {
                if row.trim().is_empty() {
                    continue;
                }
                let fields: Vec<&str> = row.split('\u{1f}').collect();
                let summary = fields.first().copied().unwrap_or("").trim();
                if summary.is_empty() {
                    continue;
                }
                let start = fields.get(1).copied().unwrap_or("").trim();
                let (summary, _) = truncate_chars(summary, EVENT_SUMMARY_CHARS);
                events.push(json!({ "title": summary, "start": start }));
            }
            (events, None)
        }
        Err(error) => {
            let note = format!("calendário indisponível: {error}");
            notes.push(note.clone());
            (Vec::new(), Some(note))
        }
    }
}

fn local_now(deadline: &Instant, notes: &mut Vec<String>) -> (Option<String>, i64) {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    match run_capped(
        "/bin/date",
        &["+%Y-%m-%dT%H:%M:%S%z"],
        DATE_CAP,
        *deadline,
    ) {
        Ok(text) => (Some(text.trim().to_string()), epoch),
        Err(error) => {
            notes.push(format!("data/hora local indisponível: {error}"));
            (None, epoch)
        }
    }
}
