//! Persistent memories: facts Friday chooses to remember across sessions.

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
                // Repeating a fact refreshes the existing memory instead of
                // filling the graph with near-duplicates.
                let existing = load();
                if let Some(found) = existing.iter().find(|memory| {
                    memory
                        .get("content")
                        .and_then(|value| value.as_str())
                        .map(|old| normalize(old) == normalize(content))
                        .unwrap_or(false)
                }) {
                    let id = found.get("id").cloned().unwrap_or(Value::Null);
                    let mut updated = found.clone();
                    updated["ts"] = json!(now_secs());
                    if !tags.is_empty() {
                        updated["tags"] = json!(tags);
                    }
                    let mut memories = existing.clone();
                    for memory in memories.iter_mut() {
                        if memory.get("id") == Some(&id) {
                            *memory = updated.clone();
                        }
                    }
                    write_all(&memories)?;
                    let _ = friday_brain::index_all();
                    return Ok(json!({
                        "ok": true,
                        "deduped": true,
                        "memory": updated,
                    }));
                }
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
}
