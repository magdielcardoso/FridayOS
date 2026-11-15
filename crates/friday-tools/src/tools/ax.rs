//! macOS Accessibility (AX) tools.
//!
//! Everything here goes through `/usr/bin/osascript` and `System Events`, which
//! is the only stable, auditable way to drive the Accessibility API from a
//! terminal process without vendoring Objective-C bindings. AppleScript builds
//! a delimited payload (ASCII unit/record separators); Rust parses it into real
//! JSON, so titles containing quotes or commas cannot break the protocol.

use std::process::Command;

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Field separator emitted by the AppleScripts (ASCII US).
const FIELD_SEP: char = '\u{1f}';
/// Record separator emitted by the AppleScripts (ASCII RS).
const RECORD_SEP: char = '\u{1e}';

/// Hard cap on nodes returned by `ax_ui_tree`.
const MAX_TREE_NODES: u64 = 400;
/// Hard cap on nodes walked by the selector engine.
const MAX_SEARCH_NODES: u64 = 2_000;

pub struct AxTool {
    spec: ToolSpec,
}

impl Tool for AxTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        match self.spec.name {
            "ax_windows" => ax_windows(ctx),
            "ax_frontmost" => ax_frontmost(ctx),
            "ax_ui_tree" => ax_ui_tree(args, ctx),
            "ax_focused_value" => ax_focused_value(ctx),
            "ax_click" => ax_click(args, ctx),
            "ax_set_value" => ax_set_value(args, ctx),
            "ax_menu_click" => ax_menu_click(args, ctx),
            other => Err(ToolError::NotFound(other.to_string())),
        }
    }
}

pub fn register(registry: &mut Registry) {
    for spec in all_specs() {
        registry.add(AxTool { spec });
    }
}

fn all_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "ax_windows",
            summary: "Lista as janelas visíveis de todos os aplicativos, com posição, tamanho e qual está em primeiro plano.",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "ax_frontmost",
            summary: "Informa o aplicativo em primeiro plano, o título da janela e o papel do elemento de interface focado.",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "ax_ui_tree",
            summary: "Despeja a árvore de elementos de interface de um aplicativo (papel, título, descrição e valor), limitada por profundidade e número de nós.",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Nome do processo do aplicativo, ex.: \"Safari\"."
                    },
                    "depth": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 8,
                        "default": 2,
                        "description": "Profundidade máxima a percorrer (0 = só o aplicativo)."
                    }
                },
                "required": ["app"]
            }),
        },
        ToolSpec {
            name: "ax_focused_value",
            summary: "Lê o valor ou texto do elemento de interface que está focado (útil para saber o que o usuário está digitando).",
            risk: Risk::ReadOnly,
            args: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        ToolSpec {
            name: "ax_click",
            summary: "Clica em um elemento de interface localizado por aplicativo, papel, título, descrição ou índice.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Nome do processo do aplicativo."
                    },
                    "role": {
                        "type": "string",
                        "description": "Papel AX do elemento, ex.: \"AXButton\"."
                    },
                    "title": {
                        "type": "string",
                        "description": "Título exato do elemento."
                    },
                    "description": {
                        "type": "string",
                        "description": "Descrição AX exata do elemento."
                    },
                    "index": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Qual correspondência usar, começando em 0."
                    }
                },
                "required": ["app"]
            }),
        },
        ToolSpec {
            name: "ax_set_value",
            summary: "Define o valor de um campo de texto localizado por seletor em um aplicativo.",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Nome do processo do aplicativo."
                    },
                    "value": {
                        "type": "string",
                        "description": "Novo valor a gravar no campo."
                    },
                    "role": {
                        "type": "string",
                        "default": "AXTextField",
                        "description": "Papel AX do elemento (padrão: AXTextField)."
                    },
                    "title": {
                        "type": "string",
                        "description": "Título exato do elemento."
                    },
                    "description": {
                        "type": "string",
                        "description": "Descrição AX exata do elemento."
                    },
                    "index": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Qual correspondência usar, começando em 0."
                    }
                },
                "required": ["app", "value"]
            }),
        },
        ToolSpec {
            name: "ax_menu_click",
            summary: "Aciona um item de menu pela barra de menus, por exemplo [\"Arquivo\", \"Salvar\"].",
            risk: Risk::Reversible,
            args: json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "Nome do processo do aplicativo."
                    },
                    "path": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "description": "Caminho da barra de menus, ex.: [\"Arquivo\", \"Salvar\"]."
                    }
                },
                "required": ["app", "path"]
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// Argument and output helpers
// ---------------------------------------------------------------------------

fn require_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ToolError::invalid(format!(
                "campo `{key}` é obrigatório e deve ser texto não vazio"
            ))
        })
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

/// Escapes text for a double-quoted AppleScript literal.
///
/// AppleScript only understands `\"` and `\\`; newlines inside a literal are
/// platform-dependent, so whitespace controls are flattened to spaces instead.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' | '\t' => out.push(' '),
            other if other.is_control() => {}
            other => out.push(other),
        }
    }
    out
}

fn ok(result: Value) -> ToolResult {
    Ok(json!({ "ok": true, "result": result }))
}

fn dry_run(would: Value) -> ToolResult {
    Ok(json!({ "dry_run": true, "would": would }))
}

fn truncate(value: &str, max: usize) -> String {
    let mut out = String::new();
    for (count, ch) in value.chars().enumerate() {
        if count >= max {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

fn parse_int(value: &str) -> i64 {
    value.trim().parse::<i64>().unwrap_or(0)
}

fn split_records(output: &str) -> Vec<&str> {
    output
        .split(RECORD_SEP)
        .filter(|record| !record.is_empty())
        .collect()
}

fn split_fields(record: &str) -> Vec<&str> {
    record.split(FIELD_SEP).collect()
}

/// Parses the five-field summary emitted by the selector engine.
fn parse_found(text: &str) -> Option<Value> {
    let fields = split_fields(text);
    if fields.len() < 5 {
        return None;
    }
    Some(json!({
        "depth": parse_int(fields[0]),
        "role": fields[1],
        "title": fields[2],
        "description": fields[3],
        "value": truncate(fields[4], 500),
    }))
}

// ---------------------------------------------------------------------------
// osascript plumbing
// ---------------------------------------------------------------------------

fn run_script(script: &str) -> Result<String, ToolError> {
    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|error| ToolError::Failed(format!("osascript: {error}")))?;

    if !output.status.success() {
        return Err(map_osascript_error(&String::from_utf8_lossy(
            &output.stderr,
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// Turns AppleScript errors into actionable Portuguese messages.
fn map_osascript_error(stderr: &str) -> ToolError {
    let lower = stderr.to_lowercase();

    if lower.contains("-1719") || lower.contains("not allowed assistive access") {
        return ToolError::Failed(
            "permissão de Acessibilidade ausente: conceda Acessibilidade ao Friday (ou ao \
             osascript) em Ajustes do Sistema → Privacidade e Segurança → Acessibilidade"
                .to_string(),
        );
    }

    if lower.contains("-1743") || lower.contains("not authorized to send apple events") {
        return ToolError::Failed(
            "permissão de Automação ausente: autorize o Friday a controlar o System Events em \
             Ajustes do Sistema → Privacidade e Segurança → Automação"
                .to_string(),
        );
    }

    ToolError::Failed(format!("osascript falhou: {}", stderr.trim()))
}

// ---------------------------------------------------------------------------
// ax_windows
// ---------------------------------------------------------------------------

fn ax_windows(ctx: &ToolCtx) -> ToolResult {
    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_windows",
            "action": "listar janelas visíveis de todos os aplicativos",
        }));
    }

    let script = r#"tell application "System Events"
    set fs to (ASCII character 31)
    set rs to (ASCII character 30)
    set outText to ""
    set frontName to ""
    set frontProc to missing value
    try
        set frontProc to first application process whose frontmost is true
        set frontName to name of frontProc
    end try
    try
        if frontProc is not missing value then
            set probeCount to count of windows of frontProc
        end if
    on error errMsg number errNum
        if errNum is -1719 then error errMsg number errNum
    end try
    set winCount to 0
    repeat with p in (application processes whose visible is true)
        if winCount is greater than or equal to 200 then exit repeat
        set pname to ""
        try
            set pname to name of p
        end try
        set pfront to (pname is frontName)
        try
            repeat with w in windows of p
                if winCount is greater than or equal to 200 then exit repeat
                set wvisible to true
                try
                    set wvisible to visible of w
                end try
                if wvisible then
                    set wname to ""
                    try
                        set wname to title of w
                    end try
                    set wx to 0
                    set wy to 0
                    set ww to 0
                    set wh to 0
                    try
                        set wpos to position of w
                        set wx to item 1 of wpos
                        set wy to item 2 of wpos
                    end try
                    try
                        set wsz to size of w
                        set ww to item 1 of wsz
                        set wh to item 2 of wsz
                    end try
                    set lineText to pname & fs & wname & fs & (wx as text) & fs & (wy as text) & fs & (ww as text) & fs & (wh as text) & fs & (pfront as text)
                    if outText is "" then
                        set outText to lineText
                    else
                        set outText to outText & rs & line
                    end if
                    set winCount to winCount + 1
                end if
            end repeat
        end try
    end repeat
    return outText
end tell"#;

    let output = run_script(script)?;

    let mut windows = Vec::new();
    for record in split_records(&output) {
        let fields = split_fields(record);
        if fields.len() < 7 {
            continue;
        }
        windows.push(json!({
            "app": fields[0],
            "title": fields[1],
            "position": {
                "x": parse_int(fields[2]),
                "y": parse_int(fields[3]),
            },
            "size": {
                "width": parse_int(fields[4]),
                "height": parse_int(fields[5]),
            },
            "frontmost": fields[6].trim() == "true",
        }));
    }

    ok(json!(windows))
}

// ---------------------------------------------------------------------------
// ax_frontmost
// ---------------------------------------------------------------------------

fn ax_frontmost(ctx: &ToolCtx) -> ToolResult {
    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_frontmost",
            "action": "ler aplicativo em primeiro plano, janela e elemento focado",
        }));
    }

    let script = r#"tell application "System Events"
    set fs to (ASCII character 31)
    set frontProc to first application process whose frontmost is true
    set pname to name of frontProc
    set wtitle to ""
    try
        set wtitle to title of front window of frontProc
    end try
    set roleName to ""
    set etitle to ""
    set evalText to ""
    try
        set focused to value of attribute "AXFocusedUIElement" of frontProc
        set roleName to role of focused
        try
            set etitle to title of focused
        end try
        try
            set evalText to value of focused as text
        end try
    on error errMsg number errNum
        if errNum is -1719 then error errMsg number errNum
    end try
    return pname & fs & wtitle & fs & roleName & fs & etitle & fs & evalText
end tell"#;

    let output = run_script(script)?;
    let fields = split_fields(&output);
    if fields.len() < 5 {
        return Err(ToolError::Failed(
            "resposta inesperada do System Events ao ler o aplicativo em primeiro plano"
                .to_string(),
        ));
    }

    ok(json!({
        "app": fields[0],
        "window_title": fields[1],
        "focused_role": fields[2],
        "focused_title": fields[3],
        "focused_value": truncate(fields[4], 500),
    }))
}

// ---------------------------------------------------------------------------
// ax_ui_tree
// ---------------------------------------------------------------------------

fn ax_ui_tree(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let app = require_str(args, "app")?;
    let depth = opt_u64(args, "depth").unwrap_or(2).min(8);

    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_ui_tree",
            "app": app,
            "depth": depth,
            "max_nodes": MAX_TREE_NODES,
        }));
    }

    let script = format!(
        r#"tell application "System Events"
    if not (exists application process "{app}") then return "__NOT_FOUND__"
    set targetProc to application process "{app}"
    try
        set probeCount to count of windows of targetProc
    on error errMsg number errNum
        if errNum is -1719 then error errMsg number errNum
    end try
    set fs to (ASCII character 31)
    set rs to (ASCII character 30)
    set maxNodes to {max_nodes}
    set maxDepth to {depth}
    set queueElems to {{targetProc}}
    set queueDepth to {{0}}
    set outText to ""
    set nodeCount to 0
    repeat while ((count of queueElems) > 0) and (nodeCount < maxNodes)
        set elem to item 1 of queueElems
        set d to item 1 of queueDepth
        set queueElems to rest of queueElems
        set queueDepth to rest of queueDepth
        set nodeCount to nodeCount + 1
        set erole to ""
        set etitle to ""
        set edesc to ""
        set evalText to ""
        try
            set erole to role of elem
        end try
        try
            set etitle to title of elem
        end try
        try
            set edesc to description of elem
        end try
        try
            set evalText to value of elem as text
        end try
        set lineText to (d as text) & fs & erole & fs & etitle & fs & edesc & fs & evalText
        if outText is "" then
            set outText to lineText
        else
            set outText to outText & rs & line
        end if
        if d < maxDepth then
            try
                set kids to UI elements of elem
                repeat with k in kids
                    set end of queueElems to contents of k
                    set end of queueDepth to (d + 1)
                end repeat
            end try
        end if
    end repeat
    return outText
end tell"#,
        app = escape(app),
        max_nodes = MAX_TREE_NODES,
        depth = depth,
    );

    let output = run_script(&script)?;

    if output == "__NOT_FOUND__" {
        return Err(ToolError::Failed(format!(
            "aplicativo `{app}` não encontrado ou não está em execução"
        )));
    }

    let mut nodes = Vec::new();
    for record in split_records(&output) {
        let fields = split_fields(record);
        if fields.len() < 5 {
            continue;
        }
        nodes.push(json!({
            "depth": parse_int(fields[0]),
            "role": fields[1],
            "title": fields[2],
            "description": fields[3],
            "value": truncate(fields[4], 500),
        }));
    }

    let count = nodes.len() as u64;
    ok(json!({
        "app": app,
        "depth": depth,
        "count": count,
        "truncated": count >= MAX_TREE_NODES,
        "nodes": nodes,
    }))
}

// ---------------------------------------------------------------------------
// ax_focused_value
// ---------------------------------------------------------------------------

fn ax_focused_value(ctx: &ToolCtx) -> ToolResult {
    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_focused_value",
            "action": "ler o valor do elemento de interface focado",
        }));
    }

    let script = r#"tell application "System Events"
    set fs to (ASCII character 31)
    set frontProc to first application process whose frontmost is true
    set found to false
    set roleName to ""
    set evalText to ""
    set etitle to ""
    set edesc to ""
    try
        set focused to value of attribute "AXFocusedUIElement" of frontProc
        set roleName to role of focused
        set found to true
        try
            set evalText to value of focused as text
        end try
        try
            set etitle to title of focused
        end try
        try
            set edesc to description of focused
        end try
    on error errMsg number errNum
        if errNum is -1719 then error errMsg number errNum
    end try
    if found is false then return "__NO_FOCUS__"
    return roleName & fs & evalText & fs & etitle & fs & edesc
end tell"#;

    let output = run_script(script)?;
    if output == "__NO_FOCUS__" {
        return Err(ToolError::Failed(
            "nenhum elemento de interface focado no momento".to_string(),
        ));
    }

    let fields = split_fields(&output);
    if fields.len() < 4 {
        return Err(ToolError::Failed(
            "resposta inesperada do System Events ao ler o elemento focado".to_string(),
        ));
    }

    ok(json!({
        "role": fields[0],
        "value": truncate(fields[1], 10_000),
        "title": fields[2],
        "description": fields[3],
    }))
}

// ---------------------------------------------------------------------------
// Selector engine (shared by ax_click and ax_set_value)
// ---------------------------------------------------------------------------

fn finder_script(
    app: &str,
    role: &str,
    title: &str,
    description: &str,
    index: u64,
    setup: &str,
    action: &str,
) -> String {
    format!(
        r#"tell application "System Events"
    if not (exists application process "{app}") then return "__NOT_FOUND__"
    set targetProc to application process "{app}"
    try
        set probeCount to count of windows of targetProc
    on error errMsg number errNum
        if errNum is -1719 then error errMsg number errNum
    end try
    set fs to (ASCII character 31)
    set rs to (ASCII character 30)
    set wantRole to "{role}"
    set wantTitle to "{title}"
    set wantDesc to "{description}"
    set wantIndex to {index}
    {setup}
    set queueElems to {{targetProc}}
    set queueDepth to {{0}}
    set nodeCount to 0
    set matchCount to 0
    repeat while ((count of queueElems) > 0) and (nodeCount < {max_nodes})
        set elem to item 1 of queueElems
        set d to item 1 of queueDepth
        set queueElems to rest of queueElems
        set queueDepth to rest of queueDepth
        set nodeCount to nodeCount + 1
        set erole to ""
        set etitle to ""
        set edesc to ""
        set evalText to ""
        try
            set erole to role of elem
        end try
        try
            set etitle to title of elem
        end try
        try
            set edesc to description of elem
        end try
        try
            set evalText to value of elem as text
        end try
        set matched to true
        if (wantRole is not "") and (erole is not wantRole) then set matched to false
        if (wantTitle is not "") and (etitle is not wantTitle) then set matched to false
        if (wantDesc is not "") and (edesc is not wantDesc) then set matched to false
        if matched then
            if matchCount is wantIndex then
                {action}
                return (d as text) & fs & erole & fs & etitle & fs & edesc & fs & evalText
            end if
            set matchCount to matchCount + 1
        end if
        try
            set kids to UI elements of elem
            repeat with k in kids
                set end of queueElems to contents of k
                set end of queueDepth to (d + 1)
            end repeat
        end try
    end repeat
    return "__NO_MATCH__"
end tell"#,
        app = escape(app),
        role = escape(role),
        title = escape(title),
        description = escape(description),
        index = index,
        setup = setup,
        action = action,
        max_nodes = MAX_SEARCH_NODES,
    )
}

fn selector_guard(app: &str, role: &str, title: &str, description: &str) -> Result<(), ToolError> {
    if role.is_empty() && title.is_empty() && description.is_empty() {
        return Err(ToolError::invalid(
            "informe pelo menos um de `role`, `title` ou `description` para localizar o elemento"
                .to_string(),
        ));
    }
    if app.is_empty() {
        return Err(ToolError::invalid(
            "campo `app` é obrigatório e deve ser texto não vazio".to_string(),
        ));
    }
    Ok(())
}

fn map_missing_app(app: &str) -> ToolError {
    ToolError::Failed(format!(
        "aplicativo `{app}` não encontrado ou não está em execução"
    ))
}

fn map_no_match(app: &str, role: &str, title: &str, description: &str, index: u64) -> ToolError {
    ToolError::Failed(format!(
        "nenhum elemento em `{app}` corresponde ao seletor (role=`{role}`, title=`{title}`, \
         description=`{description}`, index={index})"
    ))
}

// ---------------------------------------------------------------------------
// ax_click
// ---------------------------------------------------------------------------

fn ax_click(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let app = require_str(args, "app")?;
    let role = opt_str(args, "role").unwrap_or_default();
    let title = opt_str(args, "title").unwrap_or_default();
    let description = opt_str(args, "description").unwrap_or_default();
    let index = opt_u64(args, "index").unwrap_or(0);
    selector_guard(app, role, title, description)?;

    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_click",
            "app": app,
            "role": role,
            "title": title,
            "description": description,
            "index": index,
        }));
    }

    let action = r#"try
                    click elem
                on error
                    perform action "AXPress" of elem
                end try"#;
    let script = finder_script(app, role, title, description, index, "", action);
    let output = run_script(&script)?;

    if output == "__NOT_FOUND__" {
        return Err(map_missing_app(app));
    }
    if output == "__NO_MATCH__" {
        return Err(map_no_match(app, role, title, description, index));
    }

    let found = parse_found(&output).ok_or_else(|| {
        ToolError::Failed("resposta inesperada do System Events ao clicar no elemento".to_string())
    })?;

    ok(json!({ "clicked": found }))
}

// ---------------------------------------------------------------------------
// ax_set_value
// ---------------------------------------------------------------------------

fn ax_set_value(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let app = require_str(args, "app")?;
    // An empty string is a valid value (it clears the field), so do not trim or
    // reject it here.
    let value = args
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::invalid("campo `value` é obrigatório e deve ser texto"))?;
    let role = opt_str(args, "role").unwrap_or("AXTextField");
    let title = opt_str(args, "title").unwrap_or_default();
    let description = opt_str(args, "description").unwrap_or_default();
    let index = opt_u64(args, "index").unwrap_or(0);
    selector_guard(app, role, title, description)?;

    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_set_value",
            "app": app,
            "value": value,
            "role": role,
            "title": title,
            "description": description,
            "index": index,
        }));
    }

    let setup = format!("set wantValue to \"{}\"", escape(value));
    let action = r#"try
                    set focused of elem to true
                end try
                set value of elem to wantValue"#;
    let script = finder_script(app, role, title, description, index, &setup, action);
    let output = run_script(&script)?;

    if output == "__NOT_FOUND__" {
        return Err(map_missing_app(app));
    }
    if output == "__NO_MATCH__" {
        return Err(map_no_match(app, role, title, description, index));
    }

    let found = parse_found(&output).ok_or_else(|| {
        ToolError::Failed(
            "resposta inesperada do System Events ao definir o valor do elemento".to_string(),
        )
    })?;

    ok(json!({
        "updated": found,
        "new_value": value,
    }))
}

// ---------------------------------------------------------------------------
// ax_menu_click
// ---------------------------------------------------------------------------

fn ax_menu_click(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let app = require_str(args, "app")?;

    let path: Vec<String> = args
        .get("path")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ToolError::invalid(
                "campo `path` é obrigatório e deve ser uma lista de textos".to_string(),
            )
        })?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(str::to_string)
        })
        .collect::<Option<Vec<String>>>()
        .ok_or_else(|| {
            ToolError::invalid("cada item de `path` deve ser texto não vazio".to_string())
        })?;

    if path.len() < 2 {
        return Err(ToolError::invalid(
            "`path` precisa ter ao menos o menu e o item, ex.: [\"Arquivo\", \"Salvar\"]"
                .to_string(),
        ));
    }

    if ctx.dry_run {
        return dry_run(json!({
            "tool": "ax_menu_click",
            "app": app,
            "path": path,
        }));
    }

    let first = escape(&path[0]);
    let mut container = format!(
        "menu \"{first}\" of menu bar item \"{first}\" of menu bar 1",
        first = first
    );
    for part in &path[1..path.len() - 1] {
        container = format!(
            "menu 1 of menu item \"{part}\" of {container}",
            part = escape(part),
            container = container
        );
    }
    let last = escape(path.last().map(String::as_str).unwrap_or_default());
    let target = format!("menu item \"{last}\" of {container}");

    let script = format!(
        r#"tell application "System Events"
    if not (exists application process "{app}") then return "__NOT_FOUND__"
    tell application process "{app}"
        click {target}
    end tell
    return "ok"
end tell"#,
        app = escape(app),
        target = target,
    );

    let output = run_script(&script)?;
    if output == "__NOT_FOUND__" {
        return Err(map_missing_app(app));
    }

    ok(json!({
        "app": app,
        "path": path,
        "clicked": true,
    }))
}
