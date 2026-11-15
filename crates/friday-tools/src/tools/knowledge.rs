//! Knowledge graph tools backed by friday-brain (SQLite + FTS5).

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

fn active_app() -> Value {
    let front = std::process::Command::new("/usr/bin/lsappinfo")
        .args(["front"])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    if front.is_empty() {
        return json!({ "ok": false, "error": "sem aplicativo ativo" });
    }
    let info = std::process::Command::new("/usr/bin/lsappinfo")
        .args(["info", "-only", "bundleID,name", &front])
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .unwrap_or_default();
    let bundle = info
        .split("\"CFBundleIdentifier\"")
        .nth(1)
        .or_else(|| info.split("bundleID=").nth(1))
        .map(|part| {
            part.trim()
                .trim_start_matches('=')
                .trim()
                .trim_matches('"')
                .split('"')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    let name = info
        .split("\"LSDisplayName\"")
        .nth(1)
        .or_else(|| info.split("name=").nth(1))
        .map(|part| {
            part.trim()
                .trim_start_matches('=')
                .trim()
                .trim_matches('"')
                .split('"')
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        })
        .unwrap_or_default();

    let mut category = String::new();
    let mut path = String::new();
    if !bundle.is_empty() {
        let found = std::process::Command::new("/usr/bin/mdfind")
            .arg(format!("kMDItemCFBundleIdentifier == '{bundle}'"))
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).lines().next().unwrap_or("").to_string())
            .unwrap_or_default();
        if !found.is_empty() {
            path = found.clone();
            let plist = format!("{found}/Contents/Info.plist");
            if let Ok(output) = std::process::Command::new("/usr/bin/plutil")
                .args(["-extract", "LSApplicationCategoryType", "raw", &plist])
                .output()
            {
                if output.status.success() {
                    category = String::from_utf8_lossy(&output.stdout).trim().to_string();
                }
            }
        }
    }

    json!({
        "ok": true,
        "app": name,
        "bundle_id": bundle,
        "category": category,
        "path": path,
        "observed": true,
        "note": "contexto observado; não é intenção do usuário",
    })
}

struct KnowledgeTool {
    spec: ToolSpec,
}

impl Tool for KnowledgeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "knowledge_index" => {
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true }));
                }
                let report = friday_brain::index_all()
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                Ok(json!({ "ok": true, "report": report }))
            }
            "knowledge_search" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| ToolError::invalid("informe `query`"))?;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10).min(50) as usize;
                let hits = friday_brain::search(query, limit)
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                Ok(json!({ "ok": true, "hits": hits }))
            }
            "knowledge_neighbors" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .or_else(|| args.get("name").and_then(|v| v.as_str()))
                    .ok_or_else(|| ToolError::invalid("informe `id` ou `name`"))?;
                let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1).min(3) as usize;
                let ids = friday_brain::resolve(id)
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                let mut nodes = Vec::new();
                let mut edges = Vec::new();
                for id in ids.iter().take(3) {
                    if let Ok(result) = friday_brain::neighbors(id, depth) {
                        nodes.extend(result.nodes);
                        edges.extend(result.edges);
                    }
                }
                Ok(json!({ "ok": true, "nodes": nodes, "edges": edges }))
            }
            "knowledge_graph" => {
                let mut highlight = Vec::new();
                if let Some(query) = args.get("query").and_then(|v| v.as_str()) {
                    if let Ok(ids) = friday_brain::resolve(query) {
                        highlight.extend(ids);
                    }
                }
                if let Some(names) = args.get("highlight").and_then(|v| v.as_array()) {
                    for name in names.iter().filter_map(|v| v.as_str()) {
                        if let Ok(ids) = friday_brain::resolve(name) {
                            highlight.extend(ids);
                        }
                    }
                }
                if ctx.dry_run {
                    return Ok(json!({ "dry_run": true, "highlight": highlight }));
                }
                let _ = friday_brain::index_all();
                // Prefer the native GPUI panel; fall back to the browser page.
                let socket = std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_default()
                    .join("Library/Application Support/FridayOS/overlay.sock");
                if socket.exists() {
                    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) {
                        use std::io::{BufRead, BufReader, Write};
                        let payload = json!({ "action": "graph", "highlight": highlight });
                        let mut line = payload.to_string();
                        line.push('\n');
                        if stream.write_all(line.as_bytes()).is_ok() {
                            let mut reply = String::new();
                            let _ = BufReader::new(stream).read_line(&mut reply);
                            if reply.contains("\"ok\":true") {
                                return Ok(json!({
                                    "ok": true,
                                    "native": true,
                                    "highlighted": highlight,
                                    "note": "nós destacados no grafo da interface principal"
                                }));
                            }
                        }
                    }
                }
                let path = friday_brain::open_panel(&highlight)
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                Ok(json!({
                    "ok": true,
                    "native": false,
                    "panel": path.display().to_string(),
                    "highlighted": highlight,
                    "note": "painel aberto no navegador com o grafo interativo"
                }))
            }
            "context_now" => Ok(active_app()),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

fn tool(name: &'static str, summary: &'static str, risk: crate::policy::Risk, args: Value) -> KnowledgeTool {
    KnowledgeTool {
        spec: ToolSpec { name, summary, risk, args },
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "knowledge_index",
        "Reindexa a memória Markdown e as memórias persistentes no grafo SQLite/FTS5.",
        crate::policy::Risk::Reversible,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
    registry.add(tool(
        "knowledge_search",
        "Busca textual (FTS5) no grafo de conhecimento e retorna trechos.",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "required": ["query"],
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 10 }
            }
        }),
    ));
    registry.add(tool(
        "knowledge_neighbors",
        "Traz nós e relações conectados a uma entidade, com profundidade limitada.",
        crate::policy::Risk::ReadOnly,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "id": { "type": "string" },
                "name": { "type": "string" },
                "depth": { "type": "integer", "minimum": 1, "maximum": 3, "default": 1 }
            }
        }),
    ));
    registry.add(tool(
        "knowledge_graph",
        "Abre o painel interativo do grafo, destacando entidades/caminhos quando informados.",
        crate::policy::Risk::Reversible,
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string" },
                "highlight": { "type": "array", "items": { "type": "string" } }
            }
        }),
    ));
    registry.add(tool(
        "context_now",
        "Diz qual aplicativo está ativo, bundle id e categoria declarada (contexto observado, não intenção).",
        crate::policy::Risk::ReadOnly,
        json!({ "type": "object", "properties": {}, "additionalProperties": false }),
    ));
}
