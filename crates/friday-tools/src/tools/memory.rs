//! Persistent memories: facts Friday chooses to remember across sessions.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/Application Support/FridayOS")
        .join("memory.jsonl")
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

fn normalize(content: &str) -> String {
    content
        .to_lowercase()
        .chars()
        .map(fold)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|ch: char| ch.is_ascii_punctuation())
        .to_string()
}

/// Remove partículas de negação de um texto já normalizado e informa se havia
/// alguma. Usado para detectar contradições ("prefiro X" vs "não prefiro X").
fn strip_negation(normalized: &str) -> (String, bool) {
    let mut negated = false;
    let mut words = Vec::new();
    for word in normalized.split_whitespace() {
        if matches!(word, "nao" | "nunca" | "never" | "not") {
            negated = true;
        } else {
            words.push(word);
        }
    }
    (words.join(" "), negated)
}

fn write_all(memories: &[Value]) -> Result<(), ToolError> {
    let body = memories
        .iter()
        .map(|memory| memory.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(path(), if body.is_empty() { String::new() } else { format!("{body}\n") })
        .map_err(|error| ToolError::failed(error.to_string()))
}

fn load() -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Leitura limitada usada pela busca relevante: no máximo `max_lines` linhas.
fn load_limited(max_lines: usize) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path()) else {
        return Vec::new();
    };
    text.lines()
        .take(max_lines)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// Grava (ou reconcilia) uma memória. Reaproveitado por `memory_save` e pelo
/// helper público `remember`. Não reindexa: quem chama decide quando indexar.
fn save_memory(content: &str, tags: Vec<String>) -> Result<Value, ToolError> {
    let existing = load();
    let key = normalize(content);

    // 1) Fato idêntico: refreshes the existing entry instead of duplicating it.
    if let Some(index) = existing.iter().position(|memory| {
        memory
            .get("content")
            .and_then(|value| value.as_str())
            .map(|old| normalize(old) == key)
            .unwrap_or(false)
    }) {
        let mut updated = existing[index].clone();
        updated["ts"] = json!(now_secs());
        if !tags.is_empty() {
            updated["tags"] = json!(tags);
        }
        let mut memories = existing.clone();
        memories[index] = updated.clone();
        write_all(&memories)?;
        return Ok(updated);
    }

    // 2) Contradição: mesmo texto com/sem negação. O registro antigo é mantido
    //    e marcado como superado; o novo é acrescentado. Nenhuma linha é
    //    removida do diário (append-only).
    let (base, negated) = strip_negation(&key);
    if !base.is_empty() {
        if let Some(index) = existing.iter().position(|memory| {
            memory
                .get("content")
                .and_then(|value| value.as_str())
                .map(|old| {
                    let (old_base, old_neg) = strip_negation(&normalize(old));
                    old_base == base && old_neg != negated
                })
                .unwrap_or(false)
        }) {
            let new_id = format!("mem-{}", now_ms());
            let old_id = existing[index].get("id").cloned().unwrap_or(Value::Null);
            let entry = json!({
                "id": new_id.clone(),
                "ts": now_secs(),
                "content": content,
                "tags": tags,
                "supersedes": old_id,
            });
            let mut memories = existing.clone();
            memories[index]["superseded"] = json!(true);
            memories[index]["superseded_by"] = json!(new_id);
            memories.push(entry.clone());
            write_all(&memories)?;
            return Ok(entry);
        }
    }

    // 3) Fato novo: acrescenta ao final do diário.
    let entry = json!({
        "id": format!("mem-{}", now_ms()),
        "ts": now_secs(),
        "content": content,
        "tags": tags,
    });
    if let Some(parent) = path().parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::failed(e.to_string()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path())
        .map_err(|e| ToolError::failed(e.to_string()))?;
    writeln!(file, "{entry}").map_err(|e| ToolError::failed(e.to_string()))?;
    Ok(entry)
}

/// Grava uma memória persistente de forma programática (usada pelo parser de
/// aprendizado). Reusa a dedupe/normalização de `memory_save` e reindexa o
/// grafo de conhecimento para que relações sejam inferidas imediatamente.
pub fn remember(text: &str) -> Result<Value, String> {
    let content = text.trim();
    if content.is_empty() {
        return Err("conteúdo vazio".to_string());
    }
    let entry = save_memory(content, Vec::new()).map_err(|error| error.to_string())?;
    let _ = friday_brain::index_all();
    Ok(entry)
}

/// Conjunto de palavras normalizado (minúsculas, sem acentos).
fn word_set(text: &str) -> HashSet<String> {
    normalize(text)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Fração das palavras da consulta presentes no conteúdo (0.0 .. 1.0).
fn relevance_score(query_words: &HashSet<String>, content: &str) -> f64 {
    if query_words.is_empty() {
        return 0.0;
    }
    let content_words = word_set(content);
    let hits = query_words
        .iter()
        .filter(|word| content_words.contains(*word))
        .count();
    hits as f64 / query_words.len() as f64
}

fn relevance_item(memory: &Value, score: f64) -> Value {
    json!({
        "id": memory.get("id").cloned().unwrap_or(Value::Null),
        "content": memory.get("content").cloned().unwrap_or(Value::Null),
        "score": score,
    })
}

struct MemoryTool {
    spec: ToolSpec,
}

impl Tool for MemoryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "memory_save" => {
                let content = args
                    .get("content")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `content`"))?;
                let tags: Vec<String> = args
                    .get("tags")
                    .and_then(|value| value.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_save": content }));
                }
                // Repeating a fact refreshes it; a negation supersedes it while
                // keeping the old line. Both live in `save_memory`.
                let entry = save_memory(content, tags)?;
                // Keep the knowledge graph in sync: new memories get inferred
                // relations immediately.
                let _ = friday_brain::index_all();
                Ok(entry)
            }
            "memory_list" => {
                let query = args
                    .get("query")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_lowercase());
                let limit = args
                    .get("limit")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(20)
                    .clamp(1, 200) as usize;
                let mut memories = load();
                memories.reverse();
                let filtered: Vec<Value> = memories
                    .into_iter()
                    .filter(|memory| match &query {
                        Some(query) => memory
                            .get("content")
                            .and_then(|value| value.as_str())
                            .map(|text| text.to_lowercase().contains(query.as_str()))
                            .unwrap_or(false),
                        None => true,
                    })
                    .take(limit)
                    .collect();
                Ok(json!({ "ok": true, "count": filtered.len(), "memories": filtered }))
            }
            "memory_relevant" => {
                const MAX_SCAN_LINES: usize = 500;
                let limit = args
                    .get("limit")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(6)
                    .clamp(1, 200) as usize;
                let query = args
                    .get("query")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .trim();
                let query_words = word_set(query);

                // Sem consulta, devolve as mais recentes (o arquivo é append-only).
                if query_words.is_empty() {
                    let selected: Vec<Value> = load_limited(MAX_SCAN_LINES)
                        .into_iter()
                        .rev()
                        .take(limit)
                        .map(|memory| relevance_item(&memory, 0.0))
                        .collect();
                    return Ok(json!({
                        "ok": true,
                        "count": selected.len(),
                        "memories": selected,
                    }));
                }

                let mut scored: Vec<(f64, usize, Value)> = load_limited(MAX_SCAN_LINES)
                    .into_iter()
                    .enumerate()
                    .filter_map(|(index, memory)| {
                        let content = memory
                            .get("content")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let score = relevance_score(&query_words, content);
                        (score > 0.0).then_some((score, index, memory))
                    })
                    .collect();
                // Maior overlap primeiro; empate desempata pela mais recente.
                scored.sort_by(|a, b| {
                    b.0.partial_cmp(&a.0)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.1.cmp(&a.1))
                });
                let selected: Vec<Value> = scored
                    .into_iter()
                    .take(limit)
                    .map(|(score, _, memory)| relevance_item(&memory, score))
                    .collect();
                Ok(json!({
                    "ok": true,
                    "count": selected.len(),
                    "memories": selected,
                }))
            }
            "memory_update" => {
                let id = args
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| ToolError::invalid("informe `id`"))?;
                let content = args
                    .get("content")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let tags = args.get("tags").and_then(|value| value.as_array()).map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                });
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_update": id }));
                }
                let mut memories = load();
                let Some(memory) = memories
                    .iter_mut()
                    .find(|memory| memory.get("id").and_then(|value| value.as_str()) == Some(id))
                else {
                    return Err(ToolError::failed(format!("memória `{id}` não existe")));
                };
                if let Some(content) = content {
                    memory["content"] = json!(content);
                    memory["ts"] = json!(now_secs());
                }
                if let Some(tags) = tags {
                    memory["tags"] = json!(tags);
                }
                let updated = memory.clone();
                write_all(&memories)?;
                let _ = friday_brain::index_all();
                Ok(json!({ "ok": true, "memory": updated }))
            }
            "memory_delete" => {
                let id = args
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| ToolError::invalid("informe `id`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_delete": id }));
                }
                let memories = load();
                let kept: Vec<Value> = memories
                    .iter()
                    .filter(|memory| memory.get("id").and_then(|value| value.as_str()) != Some(id))
                    .cloned()
                    .collect();
                let removed = memories.len() - kept.len();
                let body = kept
                    .iter()
                    .map(|memory| memory.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(path(), if body.is_empty() { String::new() } else { format!("{body}\n") })
                    .map_err(|e| ToolError::failed(e.to_string()))?;
                Ok(json!({ "ok": true, "removed": removed }))
            }
            "memory_forget" => {
                let id = args
                    .get("id")
                    .and_then(|value| value.as_str())
                    .ok_or_else(|| ToolError::invalid("informe `id`"))?;
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "would_forget": id }));
                }
                let mut memories = load();
                let Some(memory) = memories
                    .iter_mut()
                    .find(|memory| memory.get("id").and_then(|value| value.as_str()) == Some(id))
                else {
                    return Err(ToolError::failed(format!("memória `{id}` não existe")));
                };
                // Exclusão lógica: a linha permanece no diário, apenas marcada.
                memory["forgotten"] = json!(true);
                memory["ts"] = json!(now_secs());
                let updated = memory.clone();
                write_all(&memories)?;
                let _ = friday_brain::index_all();
                Ok(json!({ "ok": true, "memory": updated }))
            }
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> MemoryTool {
    MemoryTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "memory_save",
        "Guarda uma memória persistente (fato, preferência, decisão) que sobrevive a reinícios.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["content"],
            "additionalProperties": false,
            "properties": {
                "content": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        }),
    ));
    registry.add(tool(
        "memory_list",
        "Lista as memórias salvas, opcionalmente filtrando por texto.",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 20 }
            }
        }),
    ));
    registry.add(tool(
        "memory_relevant",
        "Busca as memórias mais relevantes por sobreposição de palavras (ignora acentos). Sem `query`, devolve as mais recentes.",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "default": 6 }
            }
        }),
    ));
    registry.add(tool(
        "memory_update",
        "Atualiza o conteúdo ou as tags de uma memória existente (e reindexa o grafo).",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "required": ["id"],
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string" },
                "content": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } }
            }
        }),
    ));
    registry.add(tool(
        "memory_delete",
        "Apaga uma memória pelo id.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["id"],
            "additionalProperties": false,
            "properties": { "id": { "type": "string" } }
        }),
    ));
    registry.add(tool(
        "memory_forget",
        "Marca uma memória como esquecida (exclusão lógica; a linha permanece no diário) e reindexa o grafo.",
        crate::policy::Risk::Sensitive,
        json!({
            "type": "object",
            "required": ["id"],
            "additionalProperties": false,
            "properties": { "id": { "type": "string" } }
        }),
    ));
}
