//! Persistent, long-running "missions" (JARVIS-style objectives).
//!
//! A mission is created with [`mission_start`] with a goal and a completion
//! condition, or proactively from natural language with `mission_from_request`
//! (deterministic PT/EN parser; `reason::mission_auto` is its dry preview).
//! If a watcher source is supplied, a matching entry is installed in
//! `watchers.json`; when that watcher fires, the app injects a prompt that tells
//! the agent to record evidence with [`mission_update`] and to close the
//! objective with [`mission_complete`] once the condition is satisfied.
//! `mission_query` / `mission_list` let a follow-up question ("e a build?")
//! retrieve the full history, and everything lives as plain JSON under the
//! FridayOS data dir so a mission survives an app restart. Missions that need a
//! human decision flip to `waiting_approval` so Friday can ask with the spoken
//! sim/não flow; `mission_cancel` abandons one. Removal of a mission's watcher
//! is always a surgical merge (re-read, drop only the matching entry, atomic
//! rename) so concurrent watcher edits are never clobbered.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Upper bounds: watchers.json is shared with `watcher_*`, so don't grow it
/// without limit; queries return only the most recent findings.
const MAX_WATCHERS: usize = 64;
const MAX_FINDINGS: usize = 30;
const VALID_STATUSES: [&str; 3] = ["active", "waiting_approval", "blocked"];

// ---------------------------------------------------------------------------
// Paths / low-level IO
// ---------------------------------------------------------------------------

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn dir() -> PathBuf {
    data_dir().join("missions")
}

fn watchers_path() -> PathBuf {
    data_dir().join("watchers.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Monotonic-ish nanoseconds, used only to make temp file names unique.
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Atomic JSON write (unique temp file + rename). The caller passes the full
/// value it previously loaded, so unknown fields are preserved.
fn write_json_atomic(path: &Path, value: &Value) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| ToolError::failed(error.to_string()))?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| ToolError::failed(error.to_string()))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("record");
    let tmp = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        name,
        std::process::id(),
        now_nanos() % 1_000_000_000
    ));
    fs::write(&tmp, bytes).map_err(|error| ToolError::failed(error.to_string()))?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(ToolError::failed(error.to_string()));
    }
    Ok(())
}

/// Contract embedded in every auto-created mission. The watcher prompt and the
/// deterministic loop in `friday-app` both honor it.
const HANDOFF_CONTRACT: &str = "A cada achado chame `mission_update` (text + state mesclados); \
 se precisar do usuário use `needs_approval`: true; quando a condição for satisfeita \
 chame `mission_complete` com um achado final — nunca deixe a missão ativa após a \
 condição.";

// ---------------------------------------------------------------------------
// Deterministic intent parser (no network, no model call)
// ---------------------------------------------------------------------------

/// Accent folding, one input char to one output char, so byte indices in the
/// folded string can be mapped back onto the original request.
fn fold_char(ch: char) -> char {
    match ch {
        'á' | 'à' | 'â' | 'ã' | 'ä' | 'Á' | 'À' | 'Â' | 'Ã' | 'Ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' | 'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => 'u',
        'ç' | 'Ç' => 'c',
        'ñ' | 'Ñ' => 'n',
        other => other.to_ascii_lowercase(),
    }
}

fn fold_request(raw: &str) -> (String, Vec<char>) {
    let chars: Vec<char> = raw.chars().collect();
    let folded: String = chars.iter().map(|ch| fold_char(*ch)).collect();
    (folded, chars)
}

fn slice_chars(chars: &[char], from: usize, to: usize) -> String {
    if from >= chars.len() {
        return String::new();
    }
    chars[from..to.min(chars.len())].iter().collect()
}

fn cap_chars(text: String, max: usize) -> String {
    text.chars().take(max).collect()
}

fn quoted_goal(raw: &str) -> Option<String> {
    for (open, close) in [('"', '"'), ('\'', '\''), ('“', '”'), ('«', '»')] {
        if let Some(start) = raw.find(open) {
            let after = &raw[start + open.len_utf8()..];
            if let Some(end) = after.find(close) {
                let inside = after[..end].trim();
                if !inside.is_empty() {
                    return Some(inside.to_string());
                }
            }
        }
    }
    None
}

/// Subject/goal: quoted text wins; otherwise the text after a watch trigger up
/// to the first stop word; otherwise the whole request (bounded).
fn extract_goal(raw: &str) -> String {
    if let Some(goal) = quoted_goal(raw) {
        return cap_chars(goal, 200);
    }
    let (folded, chars) = fold_request(raw);
    let triggers = [
        "acompanha", "acompanhe", "monitora", "monitore", "vigia", "vigie", "watch", "track",
    ];
    let mut from_char = 0usize;
    for trigger in triggers {
        if let Some(byte_end) = folded.find(trigger) {
            from_char = folded[..byte_end].chars().count() + trigger.chars().count();
            break;
        }
    }
    let byte_from = folded
        .char_indices()
        .nth(from_char)
        .map(|(index, _)| index)
        .unwrap_or(folded.len());
    let rest = &folded[byte_from..];
    let mut end_bytes = rest.len();
    for stop in [",", ".", ";", " se ", " quando ", " só "] {
        if let Some(pos) = rest.find(stop) {
            end_bytes = end_bytes.min(pos);
        }
    }
    let to_char = from_char + rest[..end_bytes].chars().count();
    let mut goal = slice_chars(&chars, from_char, to_char).trim().to_string();
    for prefix in ["a ", "o ", "as ", "os ", "the ", "de ", "do ", "da "] {
        if goal.to_lowercase().starts_with(prefix) && goal.len() > prefix.len() {
            goal = goal[prefix.len()..].trim().to_string();
            break;
        }
    }
    if goal.is_empty() {
        goal = raw.trim().to_string();
    }
    cap_chars(goal, 200)
}

fn extract_command(raw: &str) -> Option<String> {
    if let Some(start) = raw.find('`') {
        let after = &raw[start + 1..];
        if let Some(end) = after.find('`') {
            let command = after[..end].trim();
            if !command.is_empty() {
                return Some(command.to_string());
            }
        }
    }
    let lower = raw.to_lowercase();
    for marker in ["comando:", "comando ", "rode ", "execute ", "run "] {
        if let Some(pos) = lower.find(marker) {
            let rest = &raw[pos + marker.len()..];
            let command = rest
                .split(|ch| ch == ',' || ch == '.' || ch == ';')
                .next()
                .unwrap_or("")
                .trim();
            if !command.is_empty() {
                return Some(command.to_string());
            }
        }
    }
    None
}

fn extract_url(raw: &str) -> Option<String> {
    for scheme in ["https://", "http://"] {
        if let Some(pos) = raw.find(scheme) {
            let rest = &raw[pos..];
            let end = rest
                .find(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '"' | '\'' | ')' | ';'))
                .unwrap_or(rest.len());
            let url = rest[..end].trim_end_matches(['.', ',', ';']).to_string();
            if !url.is_empty() {
                return Some(url);
            }
        }
    }
    None
}

/// Placeholder lives under the FridayOS data dir: it exists (missions create
/// the dir) and is never a guessed destructive command.
fn placeholder_path() -> String {
    data_dir().to_string_lossy().into_owned()
}

/// Source + params. Unknown/uncertain requests fall back to a `file` watcher
/// pointing at the data dir rather than guessing a shell command.
fn detect_source(raw: &str, folded: &str) -> (String, Value) {
    let has = |words: &[&str]| words.iter().any(|word| folded.contains(*word));

    if has(&["build", "ci", "compilacao", "pipeline"]) {
        let command = extract_command(raw)
            .unwrap_or_else(|| "echo \"Monitorando build — configure o comando real\"".to_string());
        return ("shell".into(), json!({ "command": command }));
    }
    if has(&["http://", "https://", "url", "site", "pagina", "web"]) {
        let url = extract_url(raw).unwrap_or_else(|| "https://example.com/status".into());
        return ("url".into(), json!({ "url": url }));
    }
    if has(&["arquivo", "log", "pasta", "file", "diretorio"]) {
        return ("file".into(), json!({ "path": placeholder_path() }));
    }
    if has(&["clipboard", "area de transferencia", "transferencia"]) {
        return ("clipboard".into(), json!({}));
    }
    if has(&["imessage", "mensagem", "sms"]) {
        return ("imessage".into(), json!({}));
    }
    if has(&["mail", "e-mail", "email", "correio"]) {
        return ("mail".into(), json!({}));
    }
    ("file".into(), json!({ "path": placeholder_path() }))
}

/// Condition kind plus a human string that carries the same keywords so the
/// app-side loop can classify it deterministically from the mission JSON.
fn detect_condition(folded: &str) -> (&'static str, String) {
    let failure = [
        "se quebrar", "se falhar", "se der erro", "if fail", "if broken", "quebrar", "falhar",
        "erro", "failure", "broken", "failed",
    ];
    if failure.iter().any(|key| folded.contains(*key)) {
        return (
            "failure",
            "Failure / falha: apurar a causa quando a build quebrar ou falhar e pedir a decisão \
             do usuário se necessário (failure, quebrar, falhar, erro)."
                .into(),
        );
    }
    let success = [
        "se passar", "se ficar verde", "if green", "if success", "passou", "sucesso", "green",
        "passed", "succeeded", "success",
    ];
    if success.iter().any(|key| folded.contains(*key)) {
        return (
            "success",
            "Success / sucesso: concluir quando passar, ficar verde ou sinalizar sucesso \
             (success, passou, green, passed)."
                .into(),
        );
    }
    (
        "change",
        "Change / mudança: acompanhar e registrar alterações relevantes até atingir o objetivo \
         (change, mudar, alterar)."
            .into(),
    )
}

fn detect_silence(folded: &str) -> bool {
    [
        "so me chama se precisar de mim",
        "so me chame se precisar de mim",
        "so me chamar se precisar",
        "me chama so se precisar",
        "me chame so se precisar",
        "so me chama se precisar",
        "so me chama quando precisar",
        "only call me if needed",
        "only call me if you need me",
        "call me only if needed",
    ]
    .iter()
    .any(|phrase| folded.contains(*phrase))
}

fn derive_name(goal: &str, source: &str) -> String {
    let name = goal.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
    let name = if name.is_empty() {
        format!("missao {source}")
    } else {
        name
    };
    cap_chars(name, 40)
}

/// Public deterministic parser shared by `mission_from_request` (creates) and
/// `mission_auto` in `reason.rs` (dry preview). Pure: touches no files.
pub fn parse_mission_request(request: &str) -> Value {
    let raw = request.trim();
    let (folded, _) = fold_request(raw);
    let goal = extract_goal(raw);
    let (source, params) = detect_source(raw, &folded);
    let (condition_kind, condition) = detect_condition(&folded);
    let silent_until_approval = detect_silence(&folded);
    let name = derive_name(&goal, &source);
    json!({
        "request": raw,
        "name": name,
        "goal": goal,
        "condition_kind": condition_kind,
        "condition": condition,
        "source": source,
        "params": params,
        "silent_until_approval": silent_until_approval,
        "interval_secs": 60,
        "budget_minutes": 30,
        "handoff_contract": HANDOFF_CONTRACT,
    })
}

// ---------------------------------------------------------------------------
// Mission storage / lookup
// ---------------------------------------------------------------------------

fn base_slug(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "missao".into()
    } else {
        trimmed
    }
}

/// Deterministic from the human name, with a short timestamp/nonce suffix when
/// a file with that slug already exists (so restarting a mission keeps history).
fn unique_slug(name: &str) -> String {
    let base = base_slug(name);
    if !dir().join(format!("{base}.json")).exists() {
        return base;
    }
    let suffix = format!("{:x}", now_secs() % 0x00ff_ffff);
    let mut candidate = format!("{base}-{suffix}");
    let mut counter = 0u32;
    while dir().join(format!("{candidate}.json")).exists() {
        counter += 1;
        candidate = format!("{base}-{suffix}-{counter}");
    }
    candidate
}

/// All missions, newest first, so a name lookup resolves to the current one.
fn load_missions() -> Vec<(PathBuf, Value)> {
    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir()) {
        for entry in entries.flatten() {
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
                list.push((path, value));
            }
        }
    }
    list.sort_by(|a, b| {
        let a_time = a.1.get("created_at").and_then(Value::as_u64).unwrap_or(0);
        let b_time = b.1.get("created_at").and_then(Value::as_u64).unwrap_or(0);
        b_time.cmp(&a_time)
    });
    list
}

/// Resolves `id` (exact slug/file) or `name` (case-insensitive) to a mission.
fn find_mission(args: &Value) -> Result<(PathBuf, Value), ToolError> {
    let id = arg(args, "id");
    let name = arg(args, "name");
    if id.is_none() && name.is_none() {
        return Err(ToolError::invalid("informe `name` ou `id` da missão"));
    }
    let missions = load_missions();

    if let Some(id) = id {
        let direct = dir().join(format!("{id}.json"));
        if let Ok(raw) = fs::read_to_string(&direct) {
            if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                if value.is_object() {
                    return Ok((direct, value));
                }
            }
        }
        if let Some((path, value)) = missions.iter().find(|(_, value)| {
            field(value, "id") == Some(id) || field(value, "slug") == Some(id)
        }) {
            return Ok((path.clone(), value.clone()));
        }
    }

    if let Some(key) = name.or(id) {
        let needle = key.to_lowercase();
        if let Some((path, value)) = missions.iter().find(|(_, value)| {
            ["id", "slug", "name"]
                .iter()
                .filter_map(|key| field(value, key))
                .any(|candidate| candidate.to_lowercase() == needle)
        }) {
            return Ok((path.clone(), value.clone()));
        }
    }

    Err(ToolError::failed(format!(
        "missão `{}` não existe",
        name.or(id).unwrap_or("?")
    )))
}

fn append_finding(mission: &mut Value, text: &str) -> Value {
    let finding = json!({ "ts": now_secs(), "text": text });
    match mission.get_mut("findings").and_then(Value::as_array_mut) {
        Some(findings) => findings.push(finding.clone()),
        None => {
            mission["findings"] = json!([finding.clone()]);
        }
    }
    finding
}

/// Mirror a mission finding + verification into the linked run artifact.
/// Best-effort: a missing/corrupt run never breaks the mission itself.
fn mirror_to_run(run_id: &str, text: &str, verification: &str) {
    if let Some(run) = crate::artifact::load_run(run_id) {
        let _ = run.append_finding(text);
        let _ = run.set_verification(verification, text);
    }
}

// ---------------------------------------------------------------------------
// Watcher handoff (shared watchers.json, merged safely)
// ---------------------------------------------------------------------------

/// The app's watcher sources are `shell|file|http|clipboard|imessage|email`.
/// Accept the friendlier names from the tool docs as aliases.
fn normalize_source(source: &str) -> Result<&'static str, ToolError> {
    match source.to_lowercase().as_str() {
        "shell" => Ok("shell"),
        "file" => Ok("file"),
        "url" | "http" | "https" => Ok("http"),
        "clipboard" => Ok("clipboard"),
        "imessage" | "sms" => Ok("imessage"),
        "mail" | "email" => Ok("email"),
        other => Err(ToolError::invalid(format!(
            "fonte inválida `{other}`: use file, shell, url, clipboard, imessage ou mail"
        ))),
    }
}

fn watcher_name(id: &str) -> String {
    format!("mission:{id}")
}

fn read_watchers() -> Vec<Value> {
    fs::read_to_string(watchers_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn handoff_prompt(id: &str, goal: &str, condition: &str, assistant: Option<&str>) -> String {
    let mut prompt = format!(
        "Você está cuidando da missão `{id}`. Objetivo: {goal}\n\
         Critério de conclusão: {condition}\n\n\
         A cada mudança relevante, registre o que descobriu chamando `mission_update` \
         com `id`: \"{id}\", `text`: o achado (use `state` para notas de progresso).\n\
         Quando o critério de conclusão for satisfeito, chame `mission_complete` com \
         `id`: \"{id}\" e um `text` final explicando o desfecho.\n\
         Se precisar de uma decisão ou aprovação do usuário, chame `mission_update` com \
         `id`: \"{id}\", `needs_approval`: true e um `text` claro: a Friday irá perguntar \
         ao usuário (sim/não). Trabalhe de forma autônoma e só chame o usuário quando \
         realmente precisar.\n\
         REGRA DE CONCLUSÃO (execute sempre): Call `mission_update` with what you found \
         (text + state merged). If you need the user, set needs_approval=true. When the \
         completion condition is met, call `mission_complete` with a final finding. Never \
         leave the mission active after the condition is satisfied."
    );
    if let Some(assistant) = assistant {
        prompt.push_str(&format!(
            "\nAssistente designado: `{assistant}` — use `assistant_task` para delegar investigações."
        ));
    }
    prompt
}

/// Installs/updates the mission's watcher. Re-reads watchers.json immediately
/// before writing, merges with concurrent entries, and only ever replaces the
/// entry tagged with this mission id. Returns `Ok(false)` when the shared list
/// is at its cap (the mission itself still exists).
fn install_watcher(
    id: &str,
    source: &str,
    params: Value,
    interval_secs: u64,
    prompt: String,
) -> Result<bool, ToolError> {
    let name = watcher_name(id);
    let mut list = read_watchers();
    let index = list.iter().position(|watcher| {
        field(watcher, "mission") == Some(id) || field(watcher, "name") == Some(name.as_str())
    });

    let mut record = index
        .and_then(|position| list.get(position).cloned())
        .unwrap_or_else(|| json!({}));
    let map = record
        .as_object_mut()
        .ok_or_else(|| ToolError::failed("watcher existente inválido"))?;
    map.insert("name".into(), json!(name));
    map.insert("mission".into(), json!(id));
    map.insert("source".into(), json!(source));
    map.insert("params".into(), params);
    map.insert("prompt".into(), json!(prompt));
    map.insert("interval_secs".into(), json!(interval_secs.max(15)));
    map.insert("enabled".into(), json!(true));
    map.insert("fire_on_start".into(), json!(true));
    if index.is_none() {
        map.insert("last_check".into(), json!(0));
        map.insert("last_value".into(), json!(""));
        map.insert("created".into(), json!(now_secs()));
    }
    let record = Value::Object(map.clone());

    if let Some(position) = index {
        list[position] = record;
    } else {
        if list.len() >= MAX_WATCHERS {
            return Ok(false);
        }
        list.push(record);
    }
    write_json_atomic(&watchers_path(), &Value::Array(list))?;
    Ok(true)
}

/// Removes only the watcher belonging to this mission. Re-reads right before
/// writing and keeps every other (possibly concurrently added) entry.
fn remove_watcher(id: &str, recorded_name: Option<&str>) -> Result<bool, ToolError> {
    let name = watcher_name(id);
    let mut list = read_watchers();
    let before = list.len();
    list.retain(|watcher| {
        let by_mission = field(watcher, "mission") == Some(id);
        let by_name = recorded_name
            .map(|recorded| field(watcher, "name") == Some(recorded))
            .unwrap_or(false)
            || field(watcher, "name") == Some(name.as_str());
        !(by_mission || by_name)
    });
    if list.len() == before {
        return Ok(false);
    }
    write_json_atomic(&watchers_path(), &Value::Array(list))?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Tool plumbing
// ---------------------------------------------------------------------------

struct MissionTool {
    spec: ToolSpec,
}

impl Tool for MissionTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "mission_start" => mission_start(args, ctx),
            "mission_from_request" => mission_from_request(args, ctx),
            "mission_update" => mission_update(args, ctx),
            "mission_query" => mission_query(args),
            "mission_complete" => mission_complete(args, ctx),
            "mission_cancel" => mission_cancel(args, ctx),
            "mission_list" => mission_list(),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: Risk, args: Value) -> MissionTool {
    MissionTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "mission_start",
        "Cria uma missão persistente com objetivo e condição de conclusão; opcionalmente instala um watcher que acorda a Friday com achados.",
        Risk::Reversible,
        json!({
            "type": "object",
            "required": ["name", "goal", "condition"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Nome curto e humano da missão." },
                "goal": { "type": "string", "description": "O que a Friday deve acompanhar/resolver." },
                "condition": { "type": "string", "description": "Critério para considerar a missão concluída." },
                "source": { "type": "string", "enum": ["file", "shell", "url", "clipboard", "imessage", "mail"], "description": "Fonte do watcher a instalar (opcional)." },
                "params": { "type": "object", "description": "Parâmetros do watcher (path, command, url, contains...)." },
                "interval_secs": { "type": "integer", "minimum": 15, "default": 60 },
                "assistant": { "type": "string", "description": "Nome de um assistente permanente para delegar investigações." },
                "run_id": { "type": "string", "description": "Id de uma execução (run) cujos artefatos recebem achados e verificação desta missão." }
            }
        }),
    ));
    registry.add(tool(
        "mission_from_request",
        "Cria uma missão de forma proativa a partir de um pedido em linguagem natural (PT/EN): interpreta objetivo, fonte, condição, silêncio e orçamento sem chamar modelo ou rede, e instala o watcher.",
        Risk::Reversible,
        json!({
            "type": "object",
            "required": ["request"],
            "additionalProperties": false,
            "properties": {
                "request": { "type": "string", "description": "Pedido do usuário, ex.: 'acompanha a build do StackWoot. Se quebrar, descobre por quê. Só me chama se precisar de mim.'" },
                "silent_until_approval": { "type": "boolean", "default": true, "description": "Não incomodar até que a missão precise de uma decisão do usuário." },
                "interval_secs": { "type": "integer", "minimum": 15, "description": "Intervalo do watcher em segundos (padrão 60)." },
                "budget_minutes": { "type": "integer", "minimum": 0, "description": "Orçamento de tempo antes de pedir aprovação (padrão 30)." }
            }
        }),
    ));
    registry.add(tool(
        "mission_update",
        "Registra um achado numa missão, funde notas de estado e sinaliza quando precisa de decisão do usuário.",
        Risk::Reversible,
        json!({
            "type": "object",
            "required": ["text"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Nome da missão (ou use `id`)." },
                "id": { "type": "string", "description": "Id/slug da missão (ou use `name`)." },
                "text": { "type": "string", "description": "Achado / progresso a registrar." },
                "state": { "type": "object", "description": "Notas livres a fundir no estado da missão." },
                "needs_approval": { "type": "boolean", "description": "True para pedir uma decisão (sim/não) ao usuário." },
                "status": { "type": "string", "enum": ["active", "waiting_approval", "blocked"] }
            }
        }),
    ));
    registry.add(tool(
        "mission_query",
        "Recupera uma missão com status, objetivo, condição, achados recentes e estado; sem nome/id, lista todas.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Nome da missão (ou use `id`)." },
                "id": { "type": "string", "description": "Id/slug da missão (ou use `name`)." },
                "include_findings": { "type": "boolean", "default": true, "description": "Inclui os últimos 30 achados." }
            }
        }),
    ));
    registry.add(tool(
        "mission_complete",
        "Conclui uma missão, registra o desfecho e remove o watcher associado.",
        Risk::Reversible,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Nome da missão (ou use `id`)." },
                "id": { "type": "string", "description": "Id/slug da missão (ou use `name`)." },
                "text": { "type": "string", "description": "Achado final (opcional)." }
            }
        }),
    ));
    registry.add(tool(
        "mission_cancel",
        "Cancela uma missão e remove o watcher associado; precisa de confirmação.",
        Risk::Sensitive,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "description": "Nome da missão (ou use `id`)." },
                "id": { "type": "string", "description": "Id/slug da missão (ou use `name`)." },
                "text": { "type": "string", "description": "Motivo do cancelamento (opcional)." }
            }
        }),
    ));
    registry.add(tool(
        "mission_list",
        "Lista as missões de forma compacta (id, nome, status, última atualização).",
        Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
}

// ---------------------------------------------------------------------------
// mission_start
// ---------------------------------------------------------------------------

fn mission_start(args: &Value, ctx: &ToolCtx) -> ToolResult {
    create_mission(args, ctx)
}

/// Shared creation path used by `mission_start` and `mission_from_request`.
fn create_mission(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let name = arg(args, "name").ok_or_else(|| ToolError::invalid("informe `name`"))?;
    let goal = arg(args, "goal").ok_or_else(|| ToolError::invalid("informe `goal`"))?;
    let condition = arg(args, "condition").ok_or_else(|| ToolError::invalid("informe `condition`"))?;

    let source = match arg(args, "source") {
        Some(raw) => Some(normalize_source(raw)?.to_string()),
        None => None,
    };
    let params = match args.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err(ToolError::invalid("`params` deve ser um objeto")),
    };
    if let Some(source) = source.as_deref() {
        let required = match source {
            "shell" => Some("command"),
            "file" => Some("path"),
            "http" => Some("url"),
            _ => None,
        };
        if let Some(key) = required {
            if field(&params, key).map(str::trim).unwrap_or("").is_empty() {
                return Err(ToolError::invalid(format!(
                    "fonte `{source}` exige `params.{key}`"
                )));
            }
        }
    }
    let assistant = arg(args, "assistant").map(str::to_string);
    let run_id = arg(args, "run_id").map(str::to_string);
    let interval_secs = args.get("interval_secs").and_then(Value::as_u64).unwrap_or(60).max(15);

    let id = unique_slug(name);
    let watcher = source.as_ref().map(|_| watcher_name(&id));
    let prompt = source
        .as_ref()
        .map(|_| handoff_prompt(&id, goal, condition, assistant.as_deref()));
    let timestamp = now_secs();
    let mission = json!({
        "id": id,
        "slug": id,
        "name": name,
        "goal": goal,
        "condition": condition,
        "status": "active",
        "findings": [],
        "state": {},
        "assistant": assistant,
        "watcher": watcher,
        "run_id": run_id,
        "created_at": timestamp,
        "updated_at": timestamp,
        "last_check": 0,
        "completed_at": Value::Null,
        "needs_approval": false,
    });

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_create": id,
            "name": name,
            "watcher": watcher,
            "source": source,
            "interval_secs": interval_secs,
            "handoff_contract": "A cada achado chame `mission_update` (text + state mesclados); \
             se precisar do usuário use `needs_approval`: true; quando a condição for satisfeita \
             chame `mission_complete` com um achado final — nunca deixe a missão ativa após a \
             condição.",
        }));
    }

    fs::create_dir_all(dir()).map_err(|error| ToolError::failed(error.to_string()))?;
    write_json_atomic(&dir().join(format!("{id}.json")), &mission)?;

    let mut installed = false;
    let mut warning: Option<String> = None;
    if let (Some(source), Some(prompt)) = (source.as_deref(), prompt) {
        match install_watcher(&id, source, params, interval_secs, prompt) {
            Ok(true) => installed = true,
            Ok(false) => warning = Some(format!("watchers.json atingiu o limite de {MAX_WATCHERS} entradas")),
            Err(error) => warning = Some(format!("falha ao instalar watcher: {error}")),
        }
    }

    let mut result = json!({
        "ok": true,
        "id": id,
        "mission": mission,
        "watcher": watcher,
        "watcher_installed": installed,
        "handoff_contract": "A cada achado chame `mission_update` (text + state mesclados); \
         se precisar do usuário use `needs_approval`: true; quando a condição for satisfeita \
         chame `mission_complete` com um achado final — nunca deixe a missão ativa após a \
         condição.",
    });
    if let Some(warning) = warning {
        result["warning"] = json!(warning);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// mission_from_request
// ---------------------------------------------------------------------------

fn mission_from_request(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let request = arg(args, "request").ok_or_else(|| ToolError::invalid("informe `request`"))?;
    let mut parsed = parse_mission_request(request);

    // Explicit args win; documented default for silence is true.
    parsed["silent_until_approval"] = json!(
        args.get("silent_until_approval")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    );
    if let Some(value) = args.get("interval_secs").and_then(Value::as_u64) {
        parsed["interval_secs"] = json!(value.max(15));
    }
    if let Some(value) = args.get("budget_minutes").and_then(Value::as_u64) {
        parsed["budget_minutes"] = json!(value);
    }

    let goal = field(&parsed, "goal").unwrap_or("").to_string();
    let condition = field(&parsed, "condition").unwrap_or("change").to_string();
    let condition_kind = field(&parsed, "condition_kind").unwrap_or("change").to_string();
    let source = field(&parsed, "source").unwrap_or("file").to_string();
    let name = field(&parsed, "name").unwrap_or("missao").to_string();
    let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
    let interval_secs = parsed.get("interval_secs").and_then(Value::as_u64).unwrap_or(60);
    let silent_until_approval = parsed
        .get("silent_until_approval")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let budget_minutes = parsed.get("budget_minutes").and_then(Value::as_u64).unwrap_or(30);

    let create_args = json!({
        "name": name,
        "goal": goal,
        "condition": condition,
        "source": source,
        "params": params,
        "interval_secs": interval_secs,
    });
    let created = create_mission(&create_args, ctx)?;
    let mission_id = field(&created, "id")
        .or_else(|| field(&created, "would_create"))
        .unwrap_or("?")
        .to_string();

    // Embed the parsed contract so the app-side watcher loop can close the
    // mission deterministically without a model round-trip.
    if !ctx.dry_run {
        let path = dir().join(format!("{mission_id}.json"));
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(mut mission) = serde_json::from_str::<Value>(&raw) {
                mission["condition_kind"] = json!(condition_kind.clone());
                mission["silent_until_approval"] = json!(silent_until_approval);
                mission["budget_minutes"] = json!(budget_minutes);
                mission["handoff_contract"] = json!(HANDOFF_CONTRACT);
                mission["request"] = json!(request);
                mission["updated_at"] = json!(now_secs());
                let _ = write_json_atomic(&path, &mission);
            }
        }
    }

    Ok(json!({
        "ok": true,
        "mission_id": mission_id,
        "goal": goal,
        "condition": condition,
        "condition_kind": condition_kind,
        "source": source,
        "watcher": format!("mission:{mission_id}"),
        "silent_until_approval": silent_until_approval,
        "budget_minutes": budget_minutes,
        "interval_secs": interval_secs,
        "params": params,
        "handoff_contract": HANDOFF_CONTRACT,
        "dry_run": ctx.dry_run,
    }))
}

// ---------------------------------------------------------------------------
// mission_update
// ---------------------------------------------------------------------------

fn mission_update(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let (path, mut mission) = find_mission(args)?;
    let text = arg(args, "text").ok_or_else(|| ToolError::invalid("informe `text`"))?;

    let needs_approval = args.get("needs_approval").and_then(Value::as_bool).unwrap_or(false);
    let status = if let Some(status) = arg(args, "status") {
        if !VALID_STATUSES.contains(&status.to_lowercase().as_str()) {
            return Err(ToolError::invalid(format!(
                "status inválido `{status}`: use {}",
                VALID_STATUSES.join(", ")
            )));
        }
        Some(status.to_lowercase())
    } else {
        None
    };

    let finding = append_finding(&mut mission, text);
    let mission_id = field(&mission, "id").unwrap_or("?").to_string();
    mission["updated_at"] = json!(now_secs());
    mission["last_check"] = json!(now_secs());
    if args.get("needs_approval").is_some() {
        mission["needs_approval"] = json!(needs_approval);
    }
    let final_status = if needs_approval {
        "waiting_approval".to_string()
    } else {
        status.clone().unwrap_or_else(|| {
            field(&mission, "status").unwrap_or("active").to_string()
        })
    };
    mission["status"] = json!(final_status);

    if let Some(state) = args.get("state").filter(|value| value.is_object()) {
        match mission.get_mut("state").and_then(Value::as_object_mut) {
            Some(current) => {
                for (key, value) in state.as_object().expect("checked above") {
                    current.insert(key.clone(), value.clone());
                }
            }
            None => {
                mission["state"] = state.clone();
            }
        }
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_update": mission_id,
            "finding": finding,
            "status": final_status,
            "needs_approval": needs_approval,
        }));
    }

    write_json_atomic(&path, &mission)?;

    if let Some(run_id) = field(&mission, "run_id") {
        mirror_to_run(run_id, text, "unknown");
    }

    let mut result = json!({
        "ok": true,
        "id": mission_id,
        "status": final_status,
        "finding": finding,
        "needs_approval": needs_approval,
    });
    if needs_approval {
        result["message"] = json!(format!(
            "A missão `{mission_id}` precisa de uma decisão: {text}. A Friday deve perguntar ao usuário em voz alta (sim/não). Após a resposta, registre o desfecho com `mission_update` (use `needs_approval`: false e, se fizer sentido, `status`), ou encerre com `mission_complete` / `mission_cancel`."
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// mission_query
// ---------------------------------------------------------------------------

fn mission_query(args: &Value) -> ToolResult {
    let include_findings = args
        .get("include_findings")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if arg(args, "name").is_none() && arg(args, "id").is_none() {
        let missions = load_missions();
        let brief: Vec<Value> = missions
            .iter()
            .map(|(_, mission)| {
                json!({
                    "id": field(mission, "id").unwrap_or("?"),
                    "name": field(mission, "name").unwrap_or("?"),
                    "status": field(mission, "status").unwrap_or("active"),
                    "goal": field(mission, "goal").unwrap_or(""),
                    "updated_at": mission.get("updated_at").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        return Ok(json!({ "ok": true, "count": brief.len(), "missions": brief }));
    }

    let (_, mission) = find_mission(args)?;
    let findings_total = mission
        .get("findings")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let mut mission = mission;
    if !include_findings {
        mission["findings"] = json!([]);
    } else if let Some(findings) = mission.get_mut("findings").and_then(Value::as_array_mut) {
        if findings.len() > MAX_FINDINGS {
            let start = findings.len() - MAX_FINDINGS;
            *findings = findings[start..].to_vec();
        }
    }
    Ok(json!({
        "ok": true,
        "mission": mission,
        "findings_total": findings_total,
        "findings_returned": mission.get("findings").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
    }))
}

// ---------------------------------------------------------------------------
// mission_complete / mission_cancel
// ---------------------------------------------------------------------------

fn close_mission(args: &Value, ctx: &ToolCtx, cancelled: bool) -> ToolResult {
    let (path, mut mission) = find_mission(args)?;
    let mission_id = field(&mission, "id").unwrap_or("?").to_string();
    let fallback = if cancelled {
        format!("Missão `{mission_id}` cancelada")
    } else {
        format!(
            "Missão `{mission_id}` concluída: {}",
            field(&mission, "condition").unwrap_or("critério atendido")
        )
    };
    let text = arg(args, "text").unwrap_or(&fallback);

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_close": mission_id,
            "status": if cancelled { "cancelled" } else { "completed" },
            "remove_watcher": true,
        }));
    }

    // Cancelling is Sensitive: even in full autonomy, insist on an explicit
    // confirmation from the voice/UI layer before changing user state.
    if cancelled && !ctx.confirmed {
        return Ok(json!({
            "ok": false,
            "needs_confirmation": true,
            "id": mission_id,
            "message": "cancelar uma missão remove o watcher associado; confirme para executar",
        }));
    }

    let status = if cancelled { "cancelled" } else { "completed" };
    let finding = append_finding(&mut mission, text);
    let timestamp = now_secs();
    mission["status"] = json!(status);
    mission["updated_at"] = json!(timestamp);
    mission["needs_approval"] = json!(false);
    if cancelled {
        mission["cancelled_at"] = json!(timestamp);
    } else {
        mission["completed_at"] = json!(timestamp);
    }
    write_json_atomic(&path, &mission)?;

    if let Some(run_id) = field(&mission, "run_id") {
        mirror_to_run(run_id, text, if cancelled { "fail" } else { "pass" });
    }

    let recorded_name = field(&mission, "watcher");
    let watcher_removed = remove_watcher(&mission_id, recorded_name).unwrap_or(false);

    Ok(json!({
        "ok": true,
        "id": mission_id,
        "status": status,
        "finding": finding,
        "watcher": recorded_name,
        "watcher_removed": watcher_removed,
    }))
}

fn mission_complete(args: &Value, ctx: &ToolCtx) -> ToolResult {
    close_mission(args, ctx, false)
}

fn mission_cancel(args: &Value, ctx: &ToolCtx) -> ToolResult {
    close_mission(args, ctx, true)
}

// ---------------------------------------------------------------------------
// mission_list
// ---------------------------------------------------------------------------

fn mission_list() -> ToolResult {
    let missions = load_missions();
    let list: Vec<Value> = missions
        .iter()
        .map(|(_, mission)| {
            json!({
                "id": field(mission, "id").unwrap_or("?"),
                "name": field(mission, "name").unwrap_or("?"),
                "status": field(mission, "status").unwrap_or("active"),
                "updated_at": mission.get("updated_at").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(json!({ "ok": true, "count": list.len(), "missions": list }))
}
