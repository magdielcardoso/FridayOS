//! Metacognition: who Friday is and who the user is.
//!
//! Two small, local, persistent models — `self.json` and `user.json` under the
//! FridayOS data dir. They are injected into the system prompt so she starts
//! every conversation already knowing her own identity, limits and the person
//! she serves. No network, no clipboard, no telemetry, nothing leaves the Mac.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn self_path() -> PathBuf {
    data_dir().join("self.json")
}

fn user_path() -> PathBuf {
    data_dir().join("user.json")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fold(ch: char) -> char {
    match ch {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        other => other,
    }
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(fold)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Um caractere normalizado por vez (minúsculo, sem acento). Mantém o mesmo
/// índice de caractere do texto original para que possamos capturar o valor
/// preservando caixa e acentos.
fn norm_char(ch: char) -> char {
    fold(ch.to_lowercase().next().unwrap_or(ch))
}

/// Encontra um padrão já normalizado no texto original, respeitando fronteiras
/// de palavra. Devolve os índices de bytes onde o padrão começa e termina.
fn find_pattern(text: &str, pattern: &str) -> Option<(usize, usize)> {
    let original: Vec<char> = text.chars().collect();
    let normalized: Vec<char> = original.iter().copied().map(norm_char).collect();
    let pat: Vec<char> = pattern.chars().collect();
    if pat.is_empty() || pat.len() > normalized.len() {
        return None;
    }
    for start in 0..=normalized.len() - pat.len() {
        if &normalized[start..start + pat.len()] != &pat[..] {
            continue;
        }
        let end = start + pat.len();
        let before_ok = start == 0 || !original[start - 1].is_alphanumeric();
        let after_ok = end == original.len() || !original[end].is_alphanumeric();
        if before_ok && after_ok {
            let byte_start: usize = original[..start].iter().map(|c| c.len_utf8()).sum();
            let byte_end: usize = original[..end].iter().map(|c| c.len_utf8()).sum();
            return Some((byte_start, byte_end));
        }
    }
    None
}

/// Captura o valor que vem depois do primeiro padrão encontrado.
fn capture(text: &str, patterns: &[&str]) -> Option<String> {
    for pattern in patterns {
        if let Some((_start, end)) = find_pattern(text, pattern) {
            let value = text[end..]
                .trim()
                .trim_end_matches(|ch: char| matches!(ch, '.' | ',' | '!' | '?' | ';' | ':'))
                .trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn default_self() -> Value {
    json!({
        "name": "Friday",
        "role": "assistante privada desta máquina macOS, construída em FridayOS",
        "facts": [
            { "text": "Eu sou a Friday: composta, precisa, um pouco seca, nunca teatral.", "ts": 0, "source": "system" },
            { "text": "Eu falo português brasileiro por padrão e trato o usuário por 'senhor' quando cabe.", "ts": 0, "source": "system" },
            { "text": "Eu sou o próprio sistema: vejo a tela, controlo apps, arquivos, shell, rede, calendário e voz.", "ts": 0, "source": "system" },
            { "text": "Minha voz é Piper Dii, neural e local; a transcrição é on-device; o raciocínio vem da assinatura Codex.", "ts": 0, "source": "system" }
        ],
        "updated_at": 0
    })
}

fn default_user() -> Value {
    json!({
        "name": "senhor",
        "role": "",
        "facts": [],
        "updated_at": 0
    })
}

fn load(path: &Path, fallback: fn() -> Value) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return fallback();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return fallback();
    };
    if !value.is_object() {
        return fallback();
    }
    value
}

fn write(path: &Path, value: &Value) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| ToolError::failed(error.to_string()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|error| ToolError::failed(error.to_string()))?;
    file.write_all(value.to_string().as_bytes())
        .map_err(|error| ToolError::failed(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    drop(file);
    std::fs::rename(&tmp, path).map_err(|error| ToolError::failed(error.to_string()))
}

fn upsert_fact(model: &mut Value, text: &str, source: &str) -> bool {
    let key = normalize(text);
    if key.is_empty() {
        return false;
    }
    let facts = model
        .get_mut("facts")
        .and_then(Value::as_array_mut)
        .map(|facts| facts);
    let Some(facts) = facts else {
        return false;
    };
    if let Some(existing) = facts.iter_mut().find(|fact| {
        fact.get("text")
            .and_then(Value::as_str)
            .map(normalize)
            .map(|old| old == key)
            .unwrap_or(false)
    }) {
        existing["ts"] = json!(now());
        existing["source"] = json!(source);
        return false;
    }
    facts.push(json!({
        "text": text,
        "ts": now(),
        "source": source,
    }));
    const MAX: usize = 100;
    if facts.len() > MAX {
        let extra = facts.len() - MAX;
        facts.drain(0..extra);
    }
    true
}

/// Atualiza um dos modelos (self/user) com um fato durável.
fn update_model(
    is_self: bool,
    text: &str,
    name: Option<&str>,
    role: Option<&str>,
    source: &str,
) -> Result<bool, String> {
    let path = if is_self { self_path() } else { user_path() };
    let mut model = load(&path, if is_self { default_self } else { default_user });
    if let Some(name) = name {
        let clean = name.trim();
        if (1..=48).contains(&clean.chars().count()) {
            model["name"] = json!(clean);
        }
    }
    if let Some(role) = role {
        let clean = role.trim();
        if (1..=160).contains(&clean.chars().count()) {
            model["role"] = json!(clean);
        }
    }
    let added = upsert_fact(&mut model, text, source);
    model["updated_at"] = json!(now());
    write(&path, &model).map_err(|error| error.to_string())?;
    Ok(added)
}

fn update_from_capture(
    text: &str,
    patterns: &[&str],
    is_self: bool,
    fact_template: &str,
    confirm_template: &str,
    set_name: bool,
    out: &mut Vec<String>,
) -> Result<(), String> {
    if let Some(value) = capture(text, patterns) {
        let fact = fact_template.replace("{v}", &value);
        let name = if set_name { Some(value.as_str()) } else { None };
        update_model(is_self, &fact, name, None, "user")?;
        out.push(confirm_template.replace("{v}", &value));
    }
    Ok(())
}

/// Aprende com um texto em português ou inglês, de forma determinística e
/// local (sem rede). Reconhece declarações sobre o usuário, sobre a Friday e
/// pedidos explícitos de memória, atualizando os modelos automaticamente.
///
/// Devolve confirmações curtas em português. O conteúdo bruto nunca é logado e
/// o texto é limitado a 300 caracteres.
pub fn learn_from_text(text: &str) -> Result<Vec<String>, String> {
    let capped: String = text.trim().chars().take(300).collect();
    if capped.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();

    // Usuário.
    update_from_capture(
        &capped, &["me chame de", "meu nome e"], false,
        "O nome do usuário é {v}.", "Registrei que seu nome é {v}.", true, &mut out,
    )?;
    update_from_capture(
        &capped, &["trabalho com", "trabalho em"], false,
        "O usuário trabalha com {v}.", "Registrei que você trabalha com {v}.", false, &mut out,
    )?;
    update_from_capture(
        &capped, &["nao gosto de"], false,
        "O usuário não gosta de {v}.", "Registrei que você não gosta de {v}.", false, &mut out,
    )?;
    update_from_capture(
        &capped, &["prefiro"], false,
        "O usuário prefere {v}.", "Registrei que você prefere {v}.", false, &mut out,
    )?;
    update_from_capture(
        &capped, &["eu sou", "sou"], false,
        "O usuário é {v}.", "Registrei que você é {v}.", false, &mut out,
    )?;

    // Friday.
    update_from_capture(
        &capped, &["seu nome e"], true,
        "Meu nome é {v}.", "Registrei que meu nome é {v}.", true, &mut out,
    )?;
    update_from_capture(
        &capped, &["voce deve"], true,
        "A Friday deve {v}.", "Registrei que devo {v}.", false, &mut out,
    )?;
    update_from_capture(
        &capped, &["nao faca"], true,
        "A Friday não deve fazer {v}.", "Registrei que não devo fazer {v}.", false, &mut out,
    )?;
    update_from_capture(
        &capped, &["pare de"], true,
        "A Friday deve parar de {v}.", "Registrei que devo parar de {v}.", false, &mut out,
    )?;
    update_from_capture(
        &capped, &["voce e"], true,
        "A Friday é {v}.", "Registrei que eu sou {v}.", false, &mut out,
    )?;

    // Memória explícita. A confirmação é genérica para não repetir conteúdo
    // possivelmente sensível.
    if let Some(value) = capture(&capped, &["lembre-se de", "guarda isso:", "guarda isto:"]) {
        let _ = crate::tools::memory::remember(&value);
        out.push("Guardei isso na memória.".to_string());
    }

    Ok(out)
}

/// Aplica uma correção do usuário ("não, eu disse X"): o fato antigo é mantido
/// e marcado como superado; o novo é inserido. Nada é apagado.
pub fn apply_correction(kind: &str, old: &str, new: &str) -> Result<(), String> {
    let is_self = matches!(kind, "self" | "self_update" | "friday" | "assistant");
    let path = if is_self { self_path() } else { user_path() };
    let mut model = load(&path, if is_self { default_self } else { default_user });

    let old_key = normalize(old);
    if !old_key.is_empty() {
        if model
            .get("name")
            .and_then(Value::as_str)
            .map(normalize)
            .map(|value| value == old_key)
            .unwrap_or(false)
        {
            model["name"] = json!(new);
        }
        if model
            .get("role")
            .and_then(Value::as_str)
            .map(normalize)
            .map(|value| value == old_key)
            .unwrap_or(false)
        {
            model["role"] = json!(new);
        }
        if let Some(facts) = model.get_mut("facts").and_then(Value::as_array_mut) {
            for fact in facts.iter_mut() {
                let matches = fact
                    .get("text")
                    .and_then(Value::as_str)
                    .map(normalize)
                    .map(|value| value == old_key)
                    .unwrap_or(false);
                if matches {
                    fact["superseded"] = json!(true);
                    fact["superseded_by"] = json!(new);
                    fact["ts"] = json!(now());
                }
            }
        }
    }

    upsert_fact(&mut model, new, "user");
    model["updated_at"] = json!(now());
    write(&path, &model).map_err(|error| error.to_string())
}

fn summary(model: &Value) -> String {
    let name = model.get("name").and_then(Value::as_str).unwrap_or("");
    let role = model.get("role").and_then(Value::as_str).unwrap_or("");
    let mut out = String::new();
    if !name.is_empty() {
        out.push_str(&format!("Nome: {name}. "));
    }
    if !role.is_empty() {
        out.push_str(&format!("Papel: {role}. "));
    }
    if let Some(facts) = model.get("facts").and_then(Value::as_array) {
        for fact in facts.iter().rev().take(20) {
            if let Some(text) = fact.get("text").and_then(Value::as_str) {
                out.push_str(&format!("- {text}\n"));
            }
        }
    }
    out
}

struct ToolSpecified {
    spec: ToolSpec,
}

impl Tool for ToolSpecified {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "self_describe" => Ok(json!({
                "ok": true,
                "self": load(&self_path(), default_self),
                "summary": summary(&load(&self_path(), default_self)),
            })),
            "user_profile" => Ok(json!({
                "ok": true,
                "user": load(&user_path(), default_user),
                "summary": summary(&load(&user_path(), default_user)),
            })),
            "metacognition" => {
                let self_model = load(&self_path(), default_self);
                let user_model = load(&user_path(), default_user);
                let mode = std::fs::read_to_string(data_dir().join("mode.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| value.get("mode").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "normal".into());
                let missions = std::fs::read_dir(data_dir().join("missions"))
                    .map(|entries| entries.filter_map(Result::ok).count())
                    .unwrap_or(0);
                let autonomy = std::fs::read_to_string(data_dir().join("autonomy.json"))
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .and_then(|value| value.get("level").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_else(|| "standard".into());
                Ok(json!({
                    "ok": true,
                    "self": summary(&self_model),
                    "user": summary(&user_model),
                    "context": {
                        "mode": mode,
                        "autonomy": autonomy,
                        "missions": missions,
                        "voice": "Piper Dii (local)",
                        "reasoning": "Codex (gpt-6.1-terra, low)",
                    },
                }))
            }
            "self_update" | "user_update" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `text`"))?;
                let is_self = self.spec.name == "self_update";
                let path = if is_self { self_path() } else { user_path() };
                let mut model = load(&path, if is_self { default_self } else { default_user });
                if let Some(name) = args.get("name").and_then(Value::as_str) {
                    let clean = name.trim();
                    if (1..=48).contains(&clean.chars().count()) {
                        model["name"] = json!(clean);
                    }
                }
                if let Some(role) = args.get("role").and_then(Value::as_str) {
                    let clean = role.trim();
                    if (1..=160).contains(&clean.chars().count()) {
                        model["role"] = json!(clean);
                    }
                }
                let source = args
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                let added = upsert_fact(&mut model, text, source);
                model["updated_at"] = json!(now());
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_update": text, "added": added }));
                }
                write(&path, &model)?;
                Ok(json!({ "ok": true, "added": added, "model": model }))
            }
            "learn_fact" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `text`"))?;
                let kind = args
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("auto");
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_learn": text, "kind": kind }));
                }
                match kind {
                    "auto" => {
                        let confirmations = learn_from_text(text).map_err(ToolError::failed)?;
                        Ok(json!({ "ok": true, "confirmations": confirmations }))
                    }
                    "user" | "self" => {
                        let added = update_model(kind == "self", text, None, None, "user")
                            .map_err(ToolError::failed)?;
                        Ok(json!({ "ok": true, "added": added, "kind": kind }))
                    }
                    "memory" => {
                        let entry = crate::tools::memory::remember(text).map_err(ToolError::failed)?;
                        Ok(json!({ "ok": true, "kind": "memory", "memory": entry }))
                    }
                    other => Err(ToolError::invalid(format!("kind inválido: {other}"))),
                }
            }
            "correct_fact" => {
                let old = args
                    .get("old")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `old`"))?;
                let new = args
                    .get("new")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `new`"))?;
                let kind = args
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("user");
                if ctx.dry_run {
                    return Ok(json!({
                        "dry_run": true,
                        "would_correct": { "old": old, "new": new, "kind": kind },
                    }));
                }
                apply_correction(kind, old, new).map_err(ToolError::failed)?;
                Ok(json!({ "ok": true, "corrected": true, "kind": kind }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn register_tool(
    registry: &mut Registry,
    name: &'static str,
    summary: &'static str,
    risk: crate::policy::Risk,
) {
    registry.add(ToolSpecified {
        spec: ToolSpec {
            name,
            summary,
            risk,
            args: json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string" },
                    "name": { "type": "string" },
                    "role": { "type": "string" },
                    "source": { "type": "string" }
                },
            }),
        },
    });
}

fn add_tool(
    registry: &mut Registry,
    name: &'static str,
    summary: &'static str,
    risk: crate::policy::Risk,
    args: Value,
) {
    registry.add(ToolSpecified {
        spec: ToolSpec { name, summary, risk, args },
    });
}

pub fn register(registry: &mut Registry) {
    register_tool(
        registry,
        "self_describe",
        "Descreve quem a Friday é: identidade, papéis, limites e fatos estáveis.",
        crate::policy::Risk::ReadOnly,
    );
    register_tool(
        registry,
        "self_update",
        "Atualiza a identidade da Friday quando o usuário corrige ou define quem ela é.",
        crate::policy::Risk::Reversible,
    );
    register_tool(
        registry,
        "user_profile",
        "Descreve quem é o usuário: nome, papel, preferências e fatos duráveis.",
        crate::policy::Risk::ReadOnly,
    );
    register_tool(
        registry,
        "user_update",
        "Registra quem o usuário é ou uma preferência durável que ele declarou.",
        crate::policy::Risk::Reversible,
    );
    register_tool(
        registry,
        "metacognition",
        "Visão condensada de quem a Friday é, quem é o usuário e do estado atual do sistema.",
        crate::policy::Risk::ReadOnly,
    );
    add_tool(
        registry,
        "learn_fact",
        "Aprende um fato durável de um texto: atualiza self/user automaticamente ou guarda na memória (kind: auto|user|self|memory).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["text"],
            "additionalProperties": false,
            "properties": {
                "text": { "type": "string" },
                "kind": {
                    "type": "string",
                    "enum": ["auto", "user", "self", "memory"],
                    "default": "auto"
                }
            }
        }),
    );
    add_tool(
        registry,
        "correct_fact",
        "Corrige um fato: marca o antigo como superado (sem apagá-lo) e insere o novo.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["old", "new"],
            "additionalProperties": false,
            "properties": {
                "kind": { "type": "string", "enum": ["auto", "user", "self"], "default": "user" },
                "old": { "type": "string" },
                "new": { "type": "string" }
            }
        }),
    );
}
