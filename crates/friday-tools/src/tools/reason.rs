//! Reason-adjacent, read-only tools built on top of Friday's own state.
//!
//! - `mission_sweep` turns the active missions into one compact spoken line
//!   ("status geral").
//! - `timeline_query` merges the action journal with mission findings into a
//!   single chronological timeline.
//! - `watcher_triage` is a local, deterministic pre-filter for watcher events:
//!   it scores relevance/urgency/repetition and suggests a delivery channel
//!   without calling a model or the network.
//!
//! Every read is bounded and tolerant of missing or malformed files; nothing
//! here mutates user state (so `ctx.dry_run` is inherently respected).

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::delivery::{self, PresenceContext};
use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

const MAX_MISSIONS: usize = 256;
const MAX_JOURNAL_LINES: usize = 1_000;
const MAX_WATCHERS: usize = 128;
const MAX_FINDINGS_SCAN: usize = 50;
const SWEEP_CHARS: usize = 600;
const DEFAULT_SINCE_SECS: u64 = 3600;
const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;
const NOTIFY_THRESHOLD: f64 = 0.5;

// ---------------------------------------------------------------------------
// Paths / time / small helpers
// ---------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn missions_dir() -> PathBuf {
    data_dir().join("missions")
}

fn journal_paths() -> [PathBuf; 2] {
    [
        data_dir().join("actions.jsonl"),
        data_dir().join("actions.1.jsonl"),
    ]
}

fn watchers_path() -> PathBuf {
    data_dir().join("watchers.json")
}

fn delivery_path() -> PathBuf {
    data_dir().join("delivery.json")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn field_str<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn field_i64(value: &Value, key: &str) -> Option<i64> {
    let number = value.get(key)?;
    number.as_i64().or_else(|| number.as_u64().map(|u| u as i64))
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= max {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn compact_json(value: &Value, max: usize) -> String {
    truncate_chars(&serde_json::to_string(value).unwrap_or_default(), max)
}

fn normalize_source(source: &str) -> String {
    match source.trim().to_lowercase().as_str() {
        "url" | "http" | "https" => "http".into(),
        "sms" => "imessage".into(),
        "mail" => "email".into(),
        other => other.into(),
    }
}

/// Drops the most common diacritics so `crítico` matches `critico` text and
/// mission keywords survive being written without accents.
fn strip_accents(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            'á' | 'à' | 'â' | 'ã' | 'ä' | 'Á' | 'À' | 'Â' | 'Ã' | 'Ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
            'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
            'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
            'ç' | 'Ç' => 'c',
            'ñ' | 'Ñ' => 'n',
            other => other,
        })
        .collect()
}

fn human_age(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        "agora".into()
    } else if secs < 3600 {
        format!("há {} min", secs / 60)
    } else if secs < 86_400 {
        format!("há {} h", secs / 3600)
    } else {
        format!("há {} d", secs / 86_400)
    }
}

// ---------------------------------------------------------------------------
// Bounded readers
// ---------------------------------------------------------------------------

/// Missions, newest first; both missing dirs and malformed files yield an
/// empty/partial list rather than an error.
fn load_missions() -> Vec<Value> {
    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(missions_dir()) {
        for entry in entries.flatten().take(MAX_MISSIONS * 2) {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if value.is_object() {
                list.push(value);
                if list.len() >= MAX_MISSIONS {
                    break;
                }
            }
        }
    }
    list.sort_by(|a, b| {
        let a_time = field_i64(a, "updated_at")
            .or_else(|| field_i64(a, "created_at"))
            .unwrap_or(0);
        let b_time = field_i64(b, "updated_at")
            .or_else(|| field_i64(b, "created_at"))
            .unwrap_or(0);
        b_time.cmp(&a_time)
    });
    list
}

fn find_mission_by_id(id: &str) -> Option<Value> {
    if id.trim().is_empty() {
        return None;
    }
    let direct = missions_dir().join(format!("{id}.json"));
    if let Ok(raw) = fs::read_to_string(&direct) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if value.is_object() {
                return Some(value);
            }
        }
    }
    load_missions().into_iter().find(|mission| {
        ["id", "slug", "name"]
            .iter()
            .filter_map(|key| field_str(mission, key))
            .any(|candidate| candidate.eq_ignore_ascii_case(id))
    })
}

/// Both journal generations, oldest file first; capped so a huge journal can
/// never blow up a read-only query.
fn read_journal() -> Vec<Value> {
    let mut records = Vec::new();
    for path in journal_paths() {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if let Ok(record) = serde_json::from_str::<Value>(line) {
                records.push(record);
                if records.len() >= MAX_JOURNAL_LINES {
                    return records;
                }
            }
        }
    }
    records
}

fn read_watchers() -> Vec<Value> {
    let mut list = fs::read_to_string(watchers_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    list.truncate(MAX_WATCHERS);
    list
}

// ---------------------------------------------------------------------------
// Tool plumbing
// ---------------------------------------------------------------------------

struct ReasonTool {
    spec: ToolSpec,
}

impl Tool for ReasonTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "mission_sweep" => mission_sweep(),
            "timeline_query" => timeline_query(args),
            "watcher_triage" => watcher_triage(args, ctx),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, args: Value) -> ReasonTool {
    ReasonTool {
        spec: ToolSpec {
            name,
            summary,
            risk: Risk::ReadOnly,
            args,
        },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "mission_sweep",
        "Resumo falado e compacto de todas as missões ativas ou aguardando aprovação: nome, status, último achado, idade e quantas estão bloqueadas. Use para `status geral`.",
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "timeline_query",
        "Linha do tempo cronológica e unificada do diário de ações e dos achados das missões; filtra por ferramenta, missão e janela de tempo, sem apagar nada.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "tool": { "type": "string", "description": "Filtra ações pelo nome da ferramenta (ex.: \"fs_write\")." },
                "since_secs": { "type": "integer", "minimum": 0, "default": 3600, "description": "Janela retroativa em segundos; 0 = sem limite." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 50, "description": "Número máximo de entradas (mais recentes, em ordem cronológica)." },
                "mission": { "type": "string", "description": "Id de uma missão: restringe a seus achados." }
            }
        }),
    ));
    registry.add(tool(
        "watcher_triage",
        "Pré-filtro local e determinístico para eventos de watchers: pontua relevância, urgência e repetição e sugere overlay, push, hold ou log — sem chamar modelo ou rede.",
        json!({
            "type": "object",
            "required": ["source", "text"],
            "additionalProperties": false,
            "properties": {
                "source": { "type": "string", "description": "Fonte do evento (shell, file, url, clipboard, imessage, email...)." },
                "text": { "type": "string", "description": "Conteúdo/detalhe do evento a avaliar." },
                "mission": { "type": "string", "description": "Id opcional da missão cujo objetivo/condição devem ser usados na pontuação." }
            }
        }),
    ));
}

// ---------------------------------------------------------------------------
// mission_sweep
// ---------------------------------------------------------------------------

fn mission_sweep() -> ToolResult {
    let missions = load_missions();
    let now = now_secs();
    let mut active = 0usize;
    let mut waiting = 0usize;
    let mut blocked = 0usize;
    let mut lines: Vec<String> = Vec::new();

    for mission in &missions {
        let status = field_str(mission, "status").unwrap_or("active").to_lowercase();
        if status == "blocked" {
            blocked += 1;
            continue;
        }
        if status != "active" && status != "waiting_approval" {
            continue;
        }
        if status == "waiting_approval" {
            waiting += 1;
        } else {
            active += 1;
        }

        let name = truncate_chars(field_str(mission, "name").unwrap_or("?"), 24);
        let updated = field_i64(mission, "updated_at")
            .or_else(|| field_i64(mission, "created_at"))
            .unwrap_or(0);
        let age = if updated > 0 { now - updated } else { 0 };
        let last = mission
            .get("findings")
            .and_then(Value::as_array)
            .and_then(|findings| findings.last())
            .and_then(|finding| field_str(finding, "text"))
            .unwrap_or("")
            .trim();
        let last = if last.is_empty() {
            "sem achados".to_string()
        } else {
            truncate_chars(last, 48)
        };
        let status_label = if status == "waiting_approval" {
            "aguardando"
        } else {
            "ativa"
        };
        lines.push(format!(
            "{}: {}, {}, último: '{}'",
            name,
            status_label,
            human_age(age),
            last
        ));
    }

    let listed = active + waiting;
    let mut summary = if listed == 0 {
        "Nenhuma missão em andamento.".to_string()
    } else {
        format!(
            "Em andamento: {} {}.",
            listed,
            if listed == 1 { "missão" } else { "missões" }
        )
    };
    for line in lines {
        let addition = format!(" {}", line);
        if summary.chars().count() + addition.chars().count() > SWEEP_CHARS {
            break;
        }
        summary.push_str(&addition);
    }
    if blocked > 0 {
        let tail = format!(
            " {} bloqueada{}.",
            blocked,
            if blocked == 1 { "" } else { "s" }
        );
        if summary.chars().count() + tail.chars().count() <= SWEEP_CHARS {
            summary.push_str(&tail);
        }
    }
    let summary = truncate_chars(&summary, SWEEP_CHARS);

    Ok(json!({
        "ok": true,
        "summary": summary,
        "active": active,
        "waiting_approval": waiting,
        "blocked": blocked,
        "missions_loaded": missions.len(),
    }))
}

// ---------------------------------------------------------------------------
// timeline_query
// ---------------------------------------------------------------------------

fn timeline_query(args: &Value) -> ToolResult {
    let since_secs = match args.get("since_secs") {
        None | Some(Value::Null) => DEFAULT_SINCE_SECS,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| ToolError::invalid("`since_secs` deve ser um inteiro positivo"))?,
        Some(_) => return Err(ToolError::invalid("`since_secs` deve ser um inteiro positivo")),
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| ToolError::invalid("`limit` deve ser um inteiro positivo"))?
            .clamp(1, MAX_LIMIT as u64) as usize,
        Some(_) => return Err(ToolError::invalid("`limit` deve ser um inteiro positivo")),
    };
    let tool_filter = arg(args, "tool");
    let mission_filter = arg(args, "mission");

    let cutoff = if since_secs == 0 {
        i64::MIN
    } else {
        now_secs().saturating_sub(since_secs as i64)
    };
    let mut entries: Vec<Value> = Vec::new();

    // Action-journal entries.
    if mission_filter.is_none() {
        for record in read_journal() {
            let ts = field_i64(&record, "ts").unwrap_or(0);
            if ts < cutoff {
                continue;
            }
            let tool_name = field_str(&record, "tool").unwrap_or("?");
            if tool_filter.is_some_and(|wanted| tool_name != wanted) {
                continue;
            }
            let args_value = record.get("args").cloned().unwrap_or(Value::Null);
            let compact = compact_json(&args_value, 120);
            let text = if compact.is_empty() || compact == "{}" {
                tool_name.to_string()
            } else {
                format!("{tool_name} {compact}")
            };
            entries.push(json!({
                "ts": ts,
                "kind": "action",
                "text": text,
                "tool": tool_name,
            }));
        }
    }

    // Mission findings.
    if tool_filter.is_none() {
        for mission in load_missions() {
            let mission_id = field_str(&mission, "id")
                .or_else(|| field_str(&mission, "slug"))
                .unwrap_or("?");
            if mission_filter.is_some_and(|wanted| mission_id != wanted) {
                continue;
            }
            let Some(findings) = mission.get("findings").and_then(Value::as_array) else {
                continue;
            };
            for finding in findings.iter().rev().take(MAX_FINDINGS_SCAN) {
                let ts = field_i64(finding, "ts").unwrap_or(0);
                if ts < cutoff {
                    continue;
                }
                entries.push(json!({
                    "ts": ts,
                    "kind": "finding",
                    "text": truncate_chars(field_str(finding, "text").unwrap_or(""), 200),
                    "mission": mission_id,
                }));
            }
        }
    }

    entries.sort_by(|a, b| {
        field_i64(a, "ts")
            .unwrap_or(0)
            .cmp(&field_i64(b, "ts").unwrap_or(0))
    });
    let total = entries.len();
    if total > limit {
        let start = total - limit;
        entries = entries.split_off(start);
    }

    Ok(json!({
        "ok": true,
        "count": entries.len(),
        "total": total,
        "since_secs": since_secs,
        "entries": entries,
    }))
}

// ---------------------------------------------------------------------------
// watcher_triage
// ---------------------------------------------------------------------------

const STOPWORDS: &[&str] = &[
    "de", "da", "do", "das", "dos", "que", "para", "com", "por", "uma", "uns", "umas", "nos",
    "nas", "em", "foi", "se", "ao", "aos", "pelo", "pela", "pelos", "pelas", "the", "and",
    "for", "with", "this", "that", "from", "when", "status", "check", "verificar", "verifique",
];

/// All entries are already lowercased and accent-free.
const URGENCY_KEYWORDS: &[&str] = &[
    "erro",
    "falhou",
    "down",
    "outage",
    "critico",
    "urgent",
    "failed",
    "breaking",
    "caiu",
    "indisponivel",
    "critical",
    "error",
    "failure",
];

const CRITICAL_KEYWORDS: &[&str] = &["critico", "critical", "outage", "breaking", "down", "caiu"];

fn extract_keywords(text: &str) -> Vec<String> {
    let mut keywords: Vec<String> = Vec::new();
    for raw in text.split(|ch: char| !ch.is_alphanumeric()) {
        let token = strip_accents(&raw.to_lowercase());
        if token.len() < 4 || STOPWORDS.contains(&token.as_str()) || keywords.contains(&token) {
            continue;
        }
        keywords.push(token);
        if keywords.len() >= 24 {
            break;
        }
    }
    keywords
}

fn urgency_level(text_norm: &str) -> (f64, usize) {
    let mut hits = 0usize;
    let mut critical = false;
    for keyword in URGENCY_KEYWORDS {
        if text_norm.contains(*keyword) {
            hits += 1;
            if CRITICAL_KEYWORDS.contains(keyword) {
                critical = true;
            }
        }
    }
    let level = if critical {
        1.0
    } else if hits > 0 {
        0.7
    } else {
        0.0
    };
    (level, hits)
}

fn is_repeated(source_norm: &str, text_norm: &str, mission: Option<&Value>) -> bool {
    if text_norm.is_empty() {
        return false;
    }
    for watcher in read_watchers() {
        let watcher_source = normalize_source(field_str(&watcher, "source").unwrap_or(""));
        let matches_source = watcher_source == source_norm && !source_norm.is_empty();
        let matches_mission = mission
            .and_then(|m| field_str(m, "id"))
            .map(|id| field_str(&watcher, "mission") == Some(id))
            .unwrap_or(false);
        if !(matches_source || matches_mission) {
            continue;
        }
        if let Some(last) = field_str(&watcher, "last_value") {
            let last_norm = strip_accents(&last.to_lowercase());
            if !last_norm.is_empty()
                && (last_norm == text_norm
                    || (last_norm.len() >= 8 && text_norm.contains(&last_norm))
                    || (text_norm.len() >= 8 && last_norm.contains(text_norm)))
            {
                return true;
            }
        }
    }
    if let Some(mission) = mission {
        if let Some(findings) = mission.get("findings").and_then(Value::as_array) {
            for finding in findings.iter().rev().take(10) {
                if let Some(previous) = field_str(finding, "text") {
                    let previous_norm = strip_accents(&previous.to_lowercase());
                    if !previous_norm.is_empty() && previous_norm == text_norm {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn triage_reason(
    channel: &str,
    relevant: bool,
    critical: bool,
    repeated: bool,
    quiet: bool,
    partial: bool,
    keyword: Option<&str>,
    mission: Option<&str>,
) -> String {
    let base = if quiet {
        "janela de silêncio".to_string()
    } else if repeated && !critical {
        "evento repetido".to_string()
    } else if critical {
        "urgência crítica detectada".to_string()
    } else if relevant {
        match (keyword, mission) {
            (Some(k), Some(m)) => format!("relevante para `{}` ({})", m, k),
            (Some(k), None) => format!("palavra-chave: {}", k),
            (None, Some(m)) => format!("relevante para `{}`", m),
            (None, None) => "evento relevante".to_string(),
        }
    } else if partial {
        "sinal fraco".to_string()
    } else {
        "sem relevância ou urgência".to_string()
    };
    let suffix = match channel {
        "overlay" => " → overlay no Mac",
        "push" => " → push no iPhone",
        "hold" => " → segurando",
        _ => " → só registro",
    };
    format!("{base}{suffix}")
}

fn watcher_triage(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let source = arg(args, "source").unwrap_or("").to_string();
    let text = arg(args, "text").unwrap_or("").to_string();
    if source.is_empty() && text.is_empty() {
        return Err(ToolError::invalid("informe `source` e `text`"));
    }
    let mission_id = arg(args, "mission").map(str::to_string);

    let text_norm = strip_accents(&text.to_lowercase());
    let source_norm = normalize_source(&source);
    let mission = mission_id.as_deref().and_then(find_mission_by_id);
    let found_mission = mission
        .as_ref()
        .and_then(|m| field_str(m, "id").or_else(|| field_str(m, "slug")));

    let keywords = mission.as_ref().map(|m| {
        extract_keywords(&format!(
            "{} {}",
            field_str(m, "goal").unwrap_or(""),
            field_str(m, "condition").unwrap_or("")
        ))
    }).unwrap_or_default();
    let matched_keywords: Vec<String> = keywords
        .iter()
        .filter(|keyword| text_norm.contains(keyword.as_str()))
        .cloned()
        .collect();
    let relevance = if keywords.is_empty() {
        0.0
    } else {
        matched_keywords.len() as f64 / keywords.len() as f64
    };

    let (urgency, _urgency_hits) = urgency_level(&text_norm);
    let critical = urgency >= 0.99;
    let repeated = is_repeated(&source_norm, &text_norm, mission.as_ref());
    let novelty = if repeated { 0.0 } else { 1.0 };

    let score = (0.35 * relevance + 0.45 * urgency + 0.20 * novelty).clamp(0.0, 1.0);

    let policy = delivery::load(&delivery_path());
    let context = policy.context();
    let quiet = now_ms() < policy.quiet_until_ms() && !critical;
    let relevant = score >= NOTIFY_THRESHOLD;
    let partial = relevance > 0.0 || urgency > 0.0;

    let channel = if quiet || (repeated && !critical) {
        "hold"
    } else if !relevant && !critical {
        if partial {
            "hold"
        } else {
            "log"
        }
    } else {
        match context {
            PresenceContext::AtMac => "overlay",
            PresenceContext::Recording | PresenceContext::Meeting | PresenceContext::Focus => {
                if critical {
                    "push"
                } else {
                    "hold"
                }
            }
            _ => "push",
        }
    };
    let should_notify = channel == "overlay" || channel == "push";
    let reason = triage_reason(
        channel,
        relevant,
        critical,
        repeated,
        quiet,
        partial,
        matched_keywords.first().map(String::as_str),
        found_mission,
    );

    Ok(json!({
        "ok": true,
        "source": source,
        "score": (score * 100.0).round() / 100.0,
        "should_notify": should_notify,
        "channel": channel,
        "reason": reason,
        "urgency": if critical { "critical" } else if urgency > 0.0 { "action_needed" } else { "none" },
        "repeated": repeated,
        "mission": found_mission.unwrap_or(""),
        "matched_keywords": matched_keywords,
        "dry_run": ctx.dry_run,
    }))
}
