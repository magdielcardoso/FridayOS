//! Filesystem, Spotlight, clipboard and Keychain tools.
//!
//! Everything here shells out to the macOS CLIs (`stat`, `mdfind`, `pbpaste`,
//! `pbcopy`, `osascript`, `security`) so the behaviour matches what the user
//! sees in Finder and Terminal. Reads are cheap; anything that mutates state is
//! at least `Reversible`, and Keychain access is `Critical`.
//!
//! Caveats worth surfacing to the model/user:
//! - Some locations (`~/Library`, `~/Desktop` for TCC-protected data, external
//!   volumes) need Full Disk Access for the Friday binary.
//! - `osascript` trashing needs Automation permission for Finder.
//! - `security` may show a Keychain prompt unless the item is already trusted.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{run, Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Absolute ceiling for `fs_read`, even if `max_bytes` is larger.
const READ_HARD_LIMIT: u64 = 2 * 1024 * 1024;
const READ_DEFAULT: u64 = 64 * 1024;
const SEARCH_HARD_LIMIT: u64 = 50;

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

struct FnTool {
    spec: ToolSpec,
    run: fn(&Value, &ToolCtx) -> ToolResult,
}

impl Tool for FnTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        (self.run)(args, ctx)
    }
}

fn spec(name: &'static str, summary: &'static str, risk: Risk, args: Value) -> ToolSpec {
    ToolSpec {
        name,
        summary,
        risk,
        args,
    }
}

fn register_tool(
    registry: &mut Registry,
    name: &'static str,
    summary: &'static str,
    risk: Risk,
    args: Value,
    run: fn(&Value, &ToolCtx) -> ToolResult,
) {
    registry.add(FnTool {
        spec: spec(name, summary, risk, args),
        run,
    });
}

fn arg_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolError::invalid(format!("campo `{key}` (texto) é obrigatório")))
}

fn arg_opt_bool(args: &Value, key: &str) -> Result<bool, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ToolError::invalid(format!(
            "campo `{key}` deve ser booleano"
        ))),
    }
}

fn arg_opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| ToolError::invalid(format!("campo `{key}` deve ser inteiro positivo"))),
        Some(_) => Err(ToolError::invalid(format!(
            "campo `{key}` deve ser inteiro positivo"
        ))),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Expands `~`, rejects NUL bytes and `..` traversal, and anchors relative
/// paths at the session working directory.
fn resolve(raw: &str, ctx: &ToolCtx) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::invalid("caminho não pode ser vazio"));
    }
    if raw.contains('\0') {
        return Err(ToolError::invalid("caminho não pode conter byte NUL"));
    }

    let expanded = if raw == "~" {
        home_dir().ok_or_else(|| ToolError::failed("não foi possível resolver `~`: HOME ausente"))?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir()
            .ok_or_else(|| ToolError::failed("não foi possível resolver `~`: HOME ausente"))?
            .join(rest)
    } else {
        PathBuf::from(raw)
    };

    if expanded
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ToolError::invalid(
            "caminho não pode conter componentes `..`; use um caminho absoluto",
        ));
    }

    Ok(if expanded.is_absolute() {
        expanded
    } else {
        ctx.cwd.join(expanded)
    })
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> ToolError {
    let hint = if error.kind() == std::io::ErrorKind::PermissionDenied {
        " (pode ser necessário conceder Acesso Total ao Disco ao Friday)"
    } else {
        ""
    };
    ToolError::failed(format!("{action} {}: {error}{hint}", path.display()))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn epoch_secs(time: Result<SystemTime, std::io::Error>) -> Option<i64> {
    time.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
}

fn kind_of(metadata: &Metadata) -> &'static str {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

fn run_text(command: &str, args: &[&str]) -> Result<String, ToolError> {
    match run(command, args)? {
        Value::String(text) => Ok(text),
        other => Ok(other.to_string()),
    }
}

fn dry_run(action: &str, details: Value) -> ToolResult {
    Ok(json!({
        "dry_run": true,
        "action": action,
        "details": details,
    }))
}

/// Escapes a string for an AppleScript string literal.
fn applescript_literal(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

/// Escapes a value embedded in a Spotlight query string.
fn spotlight_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' | '\r' => escaped.push(' '),
            _ => escaped.push(character),
        }
    }
    escaped
}

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

fn fs_list(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let raw = arg_str(args, "path")?;
    let show_hidden = arg_opt_bool(args, "show_hidden")?;
    let path = resolve(&raw, ctx)?;

    let metadata = fs::metadata(&path).map_err(|error| io_error("ler", &path, &error))?;
    if !metadata.is_dir() {
        return Err(ToolError::invalid(format!(
            "{} não é um diretório",
            path.display()
        )));
    }

    let entries = fs::read_dir(&path).map_err(|error| io_error("listar", &path, &error))?;
    let mut collected: Vec<(bool, String, Value)> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let entry_path = entry.path();
        let entry_meta = match fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let kind = kind_of(&entry_meta);
        let value = json!({
            "name": name,
            "kind": kind,
            "size": entry_meta.len(),
            "modified": epoch_secs(entry_meta.modified()),
        });
        collected.push((kind == "dir", name.to_lowercase(), value));
    }

    collected.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
    });

    let entries: Vec<Value> = collected.into_iter().map(|(_, _, value)| value).collect();
    Ok(json!({
        "path": path_text(&path),
        "count": entries.len(),
        "entries": entries,
    }))
}

fn fs_read(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let raw = arg_str(args, "path")?;
    let requested = arg_opt_u64(args, "max_bytes")?.unwrap_or(READ_DEFAULT);
    if requested == 0 {
        return Err(ToolError::invalid("`max_bytes` deve ser maior que zero"));
    }
    let max_bytes = requested.min(READ_HARD_LIMIT);
    let path = resolve(&raw, ctx)?;

    let metadata = fs::metadata(&path).map_err(|error| io_error("abrir", &path, &error))?;
    if metadata.is_dir() {
        return Err(ToolError::invalid(format!(
            "{} é um diretório, não um arquivo",
            path.display()
        )));
    }
    if metadata.len() > READ_HARD_LIMIT {
        return Err(ToolError::failed(format!(
            "{} tem {} bytes; o limite de leitura é 2 MB",
            path.display(),
            metadata.len()
        )));
    }

    let file = File::open(&path).map_err(|error| io_error("abrir", &path, &error))?;
    let mut buffer = Vec::new();
    file.take(max_bytes)
        .read_to_end(&mut buffer)
        .map_err(|error| io_error("ler", &path, &error))?;

    if buffer.contains(&0) {
        return Err(ToolError::failed(format!(
            "{} parece ser binário (contém byte NUL); use outra ferramenta",
            path.display()
        )));
    }

    let content = String::from_utf8_lossy(&buffer).into_owned();
    Ok(json!({
        "path": path_text(&path),
        "bytes": buffer.len(),
        "truncated": metadata.len() > buffer.len() as u64,
        "content": content,
    }))
}

fn fs_write(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let raw = arg_str(args, "path")?;
    let content = arg_str(args, "content")?;
    let append = arg_opt_bool(args, "append")?;
    let path = resolve(&raw, ctx)?;

    if ctx.dry_run {
        return dry_run(
            "escrever arquivo",
            json!({
                "path": path_text(&path),
                "append": append,
                "bytes": content.len(),
            }),
        );
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| io_error("criar diretório", parent, &error))?;
        }
    }

    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }

    let mut file = options
        .open(&path)
        .map_err(|error| io_error("abrir para escrita", &path, &error))?;
    file.write_all(content.as_bytes())
        .map_err(|error| io_error("escrever", &path, &error))?;
    file.flush()
        .map_err(|error| io_error("finalizar escrita", &path, &error))?;

    Ok(json!({
        "path": path_text(&path),
        "bytes_written": content.len(),
        "appended": append,
    }))
}

fn fs_move(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let from_raw = arg_str(args, "from")?;
    let to_raw = arg_str(args, "to")?;
    let overwrite = arg_opt_bool(args, "overwrite")?;
    let from = resolve(&from_raw, ctx)?;
    let to = resolve(&to_raw, ctx)?;

    if ctx.dry_run {
        return dry_run(
            "mover arquivo",
            json!({
                "from": path_text(&from),
                "to": path_text(&to),
                "overwrite": overwrite,
            }),
        );
    }

    if fs::symlink_metadata(&from).is_err() {
        return Err(ToolError::failed(format!(
            "origem não existe: {}",
            from.display()
        )));
    }
    if fs::symlink_metadata(&to).is_ok() && !overwrite {
        return Err(ToolError::failed(format!(
            "destino já existe: {} (use overwrite=true para substituir)",
            to.display()
        )));
    }
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(ToolError::failed(format!(
                "diretório de destino não existe: {}",
                parent.display()
            )));
        }
    }

    fs::rename(&from, &to).map_err(|error| {
        ToolError::failed(format!(
            "mover {} para {}: {error}",
            from.display(),
            to.display()
        ))
    })?;

    Ok(json!({
        "from": path_text(&from),
        "to": path_text(&to),
        "moved": true,
    }))
}

fn fs_trash(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let raw = arg_str(args, "path")?;
    let path = resolve(&raw, ctx)?;

    if ctx.dry_run {
        return dry_run("mover para o Lixo", json!({ "path": path_text(&path) }));
    }

    if fs::symlink_metadata(&path).is_err() {
        return Err(ToolError::failed(format!(
            "caminho não existe: {}",
            path.display()
        )));
    }
    if path == Path::new("/") {
        return Err(ToolError::invalid("recusando mover a raiz do sistema para o Lixo"));
    }

    let absolute = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let script = format!(
        "tell application \"Finder\" to delete POSIX file {}",
        applescript_literal(&path_text(&absolute))
    );

    run("osascript", &["-e", &script]).map_err(|error| {
        ToolError::failed(format!(
            "Finder não conseguiu mover {} para o Lixo: {error} \
             (verifique a permissão de Automação para o Finder)",
            absolute.display()
        ))
    })?;

    Ok(json!({
        "path": path_text(&absolute),
        "trashed": true,
    }))
}

fn fs_mkdir(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let raw = arg_str(args, "path")?;
    let path = resolve(&raw, ctx)?;

    if ctx.dry_run {
        return dry_run("criar diretório", json!({ "path": path_text(&path) }));
    }

    let existed = path.exists();
    fs::create_dir_all(&path).map_err(|error| io_error("criar diretório", &path, &error))?;

    Ok(json!({
        "path": path_text(&path),
        "created": !existed,
    }))
}

fn fs_info(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let raw = arg_str(args, "path")?;
    let path = resolve(&raw, ctx)?;

    let metadata =
        fs::symlink_metadata(&path).map_err(|error| io_error("inspecionar", &path, &error))?;

    let target = path_text(&path);
    let permissions = run("stat", &["-f", "%Sp", &target])
        .ok()
        .and_then(|value| value.as_str().map(|text| text.trim().to_string()))
        .filter(|text| !text.is_empty());

    Ok(json!({
        "path": target,
        "kind": kind_of(&metadata),
        "size": metadata.len(),
        "created": epoch_secs(metadata.created()),
        "modified": epoch_secs(metadata.modified()),
        "accessed": epoch_secs(metadata.accessed()),
        "permissions": permissions,
    }))
}

// ---------------------------------------------------------------------------
// Spotlight
// ---------------------------------------------------------------------------

fn kind_predicate(kind: &str) -> Option<&'static str> {
    match kind {
        "pdf" => Some("kMDItemContentTypeTree == \"com.adobe.pdf\""),
        "image" => Some("kMDItemContentTypeTree == \"public.image\""),
        "audio" => Some("kMDItemContentTypeTree == \"public.audio\""),
        "video" => Some("kMDItemContentTypeTree == \"public.movie\""),
        "code" => Some("kMDItemContentTypeTree == \"public.source-code\""),
        _ => None,
    }
}

fn spotlight_search(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let query = arg_str(args, "query")?;
    if query.trim().is_empty() {
        return Err(ToolError::invalid("`query` não pode ser vazio"));
    }
    let kind = args
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .map(str::to_string);
    let limit = arg_opt_u64(args, "limit")?
        .unwrap_or(20)
        .clamp(1, SEARCH_HARD_LIMIT) as usize;

    let _ = ctx;
    let escaped = spotlight_escape(&query);
    let term = format!(
        "(kMDItemDisplayName == \"*{escaped}*\"cd || kMDItemFSName == \"*{escaped}*\"cd \
         || kMDItemTextContent == \"*{escaped}*\"cd)"
    );

    let full_query = match kind.as_deref() {
        None => term,
        Some(value) => {
            let predicate = kind_predicate(value).ok_or_else(|| {
                ToolError::invalid(format!(
                    "kind inválido: `{value}` (use pdf, image, audio, video ou code)"
                ))
            })?;
            format!("{predicate} && {term}")
        }
    };

    let output = run_text("mdfind", &[&full_query])?;
    let results: Vec<Value> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(limit)
        .map(|line| json!(line))
        .collect();

    Ok(json!({
        "query": query,
        "kind": kind,
        "count": results.len(),
        "results": results,
    }))
}

fn mdls_last_used(path: &str) -> Option<String> {
    let text = run_text("mdls", &["-name", "kMDItemLastUsedDate", "-raw", path]).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "(null)" || trimmed == "null" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn recent_files(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let limit = arg_opt_u64(args, "limit")?
        .unwrap_or(20)
        .clamp(1, SEARCH_HARD_LIMIT) as usize;
    let _ = ctx;

    let output = run_text(
        "mdfind",
        &["kMDItemLastUsedDate >= $time.today(-7)"],
    )?;

    let candidates: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(300)
        .map(str::to_string)
        .collect();

    // `mdls` is one process per file, so probe a bounded window only.
    let probe = candidates.len().min(limit.saturating_mul(4).max(60));
    let mut rows: Vec<(Option<String>, String)> = candidates
        .into_iter()
        .take(probe)
        .map(|path| (mdls_last_used(&path), path))
        .collect();

    rows.sort_by(|left, right| right.0.cmp(&left.0));
    let results: Vec<Value> = rows
        .into_iter()
        .take(limit)
        .map(|(last_used, path)| {
            json!({
                "path": path,
                "last_used": last_used,
            })
        })
        .collect();

    Ok(json!({
        "count": results.len(),
        "results": results,
    }))
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

fn clipboard_get(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let text = run_text("pbpaste", &[])?;
    Ok(json!({
        "text": text,
        "chars": text.chars().count(),
    }))
}

fn clipboard_set(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let text = arg_str(args, "text")?;
    if ctx.dry_run {
        return dry_run(
            "copiar para a área de transferência",
            json!({ "chars": text.chars().count() }),
        );
    }

    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| ToolError::failed(format!("pbcopy: {error}")))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ToolError::failed("pbcopy: stdin indisponível"))?;
    stdin
        .write_all(text.as_bytes())
        .map_err(|error| ToolError::failed(format!("pbcopy: {error}")))?;
    drop(stdin);

    let status = child
        .wait()
        .map_err(|error| ToolError::failed(format!("pbcopy: {error}")))?;
    if !status.success() {
        return Err(ToolError::failed(format!(
            "pbcopy terminou com {status}"
        )));
    }

    Ok(json!({
        "copied": true,
        "chars": text.chars().count(),
    }))
}

// ---------------------------------------------------------------------------
// Keychain
// ---------------------------------------------------------------------------

fn keychain_get(args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let service = arg_str(args, "service")?;
    let account = arg_str(args, "account")?;
    let reveal = arg_opt_bool(args, "reveal")?;

    if service.is_empty() || account.is_empty() {
        return Err(ToolError::invalid(
            "`service` e `account` não podem ser vazios",
        ));
    }

    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            &service,
            "-a",
            &account,
            "-w",
        ])
        .output()
        .map_err(|error| ToolError::failed(format!("security: {error}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(44) || stderr.contains("could not be found") {
            return Ok(json!({
                "found": false,
                "service": service,
                "account": account,
            }));
        }
        // Never echo stderr here: defensive against tools that leak values.
        return Err(ToolError::failed(format!(
            "security find-generic-password falhou (código {:?})",
            output.status.code()
        )));
    }

    let mut secret = String::from_utf8_lossy(&output.stdout).into_owned();
    if secret.ends_with('\n') {
        secret.pop();
        if secret.ends_with('\r') {
            secret.pop();
        }
    }
    let length = secret.chars().count();

    // The secret must never reach the audit log or the model transcript.
    let value = if reveal {
        secret
    } else {
        "<redacted>".to_string()
    };

    Ok(json!({
        "found": true,
        "service": service,
        "account": account,
        "value": value,
        "length": length,
        "revealed": reveal,
    }))
}

fn keychain_set(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let service = arg_str(args, "service")?;
    let account = arg_str(args, "account")?;
    let secret = arg_str(args, "secret")?;

    if service.is_empty() || account.is_empty() {
        return Err(ToolError::invalid(
            "`service` e `account` não podem ser vazios",
        ));
    }
    if secret.contains('\0') {
        return Err(ToolError::invalid("`secret` não pode conter byte NUL"));
    }

    if ctx.dry_run {
        return dry_run(
            "gravar item no Keychain",
            json!({
                "service": service,
                "account": account,
                "length": secret.chars().count(),
            }),
        );
    }

    let output = Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            &service,
            "-a",
            &account,
            "-w",
            &secret,
        ])
        .output()
        .map_err(|error| ToolError::failed(format!("security: {error}")))?;

    if !output.status.success() {
        // Do not surface stderr or argv contents: both may hold the secret.
        return Err(ToolError::failed(format!(
            "security add-generic-password falhou (código {:?})",
            output.status.code()
        )));
    }

    Ok(json!({
        "service": service,
        "account": account,
        "stored": true,
        "length": secret.chars().count(),
    }))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(registry: &mut Registry) {
    register_tool(
        registry,
        "fs_list",
        "Lista um diretório (pastas primeiro) com nome, tipo, tamanho e data de modificação.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Caminho do diretório; aceita ~." },
                "show_hidden": {
                    "type": "boolean",
                    "description": "Inclui entradas ocultas (iniciadas por ponto).",
                    "default": false
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        fs_list,
    );

    register_tool(
        registry,
        "fs_read",
        "Lê um arquivo de texto (até 2 MB; padrão 64 KB). Recusa binários.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Caminho do arquivo; aceita ~." },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 2097152,
                    "default": 65536,
                    "description": "Máximo de bytes a ler."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        fs_read,
    );

    register_tool(
        registry,
        "fs_write",
        "Cria ou sobrescreve um arquivo de texto, criando diretórios pai se necessário.",
        Risk::Sensitive,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Caminho do arquivo; aceita ~." },
                "content": { "type": "string", "description": "Conteúdo a escrever." },
                "append": {
                    "type": "boolean",
                    "description": "Anexa ao final em vez de sobrescrever.",
                    "default": false
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
        fs_write,
    );

    register_tool(
        registry,
        "fs_move",
        "Move ou renomeia um arquivo ou pasta.",
        Risk::Sensitive,
        json!({
            "type": "object",
            "properties": {
                "from": { "type": "string", "description": "Origem; aceita ~." },
                "to": { "type": "string", "description": "Destino; aceita ~." },
                "overwrite": {
                    "type": "boolean",
                    "description": "Permite substituir um destino existente.",
                    "default": false
                }
            },
            "required": ["from", "to"],
            "additionalProperties": false
        }),
        fs_move,
    );

    register_tool(
        registry,
        "fs_trash",
        "Move um arquivo ou pasta para o Lixo do Finder (não apaga permanentemente).",
        Risk::Sensitive,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Caminho a mover para o Lixo; aceita ~." }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        fs_trash,
    );

    register_tool(
        registry,
        "fs_mkdir",
        "Cria um diretório, incluindo diretórios pai ausentes.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Caminho do diretório; aceita ~." }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        fs_mkdir,
    );

    register_tool(
        registry,
        "fs_info",
        "Mostra tamanho, tipo, datas e permissões de um caminho.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Caminho a inspecionar; aceita ~." }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        fs_info,
    );

    register_tool(
        registry,
        "spotlight_search",
        "Busca arquivos no Spotlight por nome ou conteúdo, com filtro opcional de tipo.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Termo de busca." },
                "kind": {
                    "type": "string",
                    "enum": ["pdf", "image", "audio", "video", "code"],
                    "description": "Restringe o tipo de conteúdo."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 20,
                    "description": "Número máximo de resultados."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        spotlight_search,
    );

    register_tool(
        registry,
        "clipboard_get",
        "Lê o texto atual da área de transferência.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        clipboard_get,
    );

    register_tool(
        registry,
        "clipboard_set",
        "Copia um texto para a área de transferência.",
        Risk::Reversible,
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Texto a copiar." }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
        clipboard_set,
    );

    register_tool(
        registry,
        "keychain_get",
        "Lê um item genérico do Keychain; por padrão retorna segredo redigido.",
        Risk::Critical,
        json!({
            "type": "object",
            "properties": {
                "service": { "type": "string", "description": "Serviço do item." },
                "account": { "type": "string", "description": "Conta/usuário do item." },
                "reveal": {
                    "type": "boolean",
                    "description": "Retorna o segredo em texto puro. Use com extremo cuidado.",
                    "default": false
                }
            },
            "required": ["service", "account"],
            "additionalProperties": false
        }),
        keychain_get,
    );

    register_tool(
        registry,
        "keychain_set",
        "Cria ou atualiza um item genérico no Keychain.",
        Risk::Critical,
        json!({
            "type": "object",
            "properties": {
                "service": { "type": "string", "description": "Serviço do item." },
                "account": { "type": "string", "description": "Conta/usuário do item." },
                "secret": { "type": "string", "description": "Segredo a armazenar." }
            },
            "required": ["service", "account", "secret"],
            "additionalProperties": false
        }),
        keychain_set,
    );

    register_tool(
        registry,
        "recent_files",
        "Lista arquivos usados nos últimos 7 dias, ordenados pelo último uso quando disponível.",
        Risk::ReadOnly,
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "default": 20,
                    "description": "Número máximo de resultados."
                }
            },
            "additionalProperties": false
        }),
        recent_files,
    );
}
