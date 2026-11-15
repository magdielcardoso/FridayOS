//! Painel estilo Jarvis: um único overlay composto com dados reais do Mac
//! (sistema, armazenamento, energia, rede e as próprias tarefas da Friday).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

const PANELS: [&str; 5] = ["system", "storage", "power", "network", "tasks"];
const ANCHORS: [&str; 9] = [
    "center",
    "top",
    "bottom",
    "left",
    "right",
    "top-left",
    "top-right",
    "bottom-left",
    "bottom-right",
];
const MAX_BODY: usize = 1500;
const TIMEOUT: Duration = Duration::from_secs(5);

fn socket_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS/overlay.sock")
}

fn data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn send(payload: Value) -> ToolResult {
    let path = socket_path();
    if !path.exists() {
        return Err(ToolError::failed(
            "a janela da Friday não está aberta (socket de overlay ausente)",
        ));
    }
    let mut stream = UnixStream::connect(&path)
        .map_err(|error| ToolError::failed(format!("overlay: {error}")))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = payload.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|error| ToolError::failed(format!("overlay: {error}")))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|error| ToolError::failed(format!("overlay: {error}")))?;
    let value: Value = serde_json::from_str(reply.trim())
        .map_err(|error| ToolError::failed(format!("overlay: resposta inválida: {error}")))?;
    if value.get("ok").and_then(|ok| ok.as_bool()).unwrap_or(false) {
        Ok(value)
    } else {
        Err(ToolError::failed(
            value
                .get("error")
                .and_then(|error| error.as_str())
                .unwrap_or("a Friday não conseguiu abrir a janela"),
        ))
    }
}

/// Runs a macOS CLI with a hard 5 s budget and returns stdout only on success.
/// Arguments are passed as an argv list, so nothing is shell-interpreted.
fn capture(program: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut output = String::new();
                child.stdout.take()?.read_to_string(&mut output).ok()?;
                return Some(output);
            }
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn trim_chars(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix(key)
                .map(|value| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

fn vm_gb(text: &str) -> (f64, f64) {
    let page_size = text
        .split("page size of ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(16384.0);
    let mut free = 0u64;
    let mut active = 0u64;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(pages) = value.trim().trim_end_matches('.').parse::<u64>() else {
            continue;
        };
        match key.trim() {
            "Pages free" | "Pages speculative" => free += pages,
            "Pages active" => active += pages,
            _ => {}
        }
    }
    let to_gb = |pages: u64| pages as f64 * page_size / 1_073_741_824.0;
    (to_gb(free), to_gb(active))
}

fn system_panel() -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(uptime) = capture("/usr/bin/uptime", &[]) {
        let flat = uptime.trim();
        let head = flat.split("load ave").next().unwrap_or(flat).trim();
        let up = head
            .split_once("up ")
            .map(|(_, rest)| {
                rest.split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty() && !part.contains("user"))
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|up| !up.is_empty());
        let load = flat
            .split("load average")
            .nth(1)
            .map(|rest| {
                rest.trim_start_matches(|c: char| c == 's' || c == ':' || c == ' ')
                    .trim()
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|load| !load.is_empty());
        let mut line = String::from("uptime  ");
        if let Some(up) = up {
            line.push_str(&up);
        }
        if let Some(load) = load {
            line.push_str(&format!(" · carga {load}"));
        }
        lines.push(line);
    }
    if let Some(bytes) = capture("/usr/sbin/sysctl", &["-n", "hw.memsize"])
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        let total = bytes as f64 / 1_073_741_824.0;
        let mut line = format!("memória  {total:.1} GB");
        if let Some(vm) = capture("/usr/bin/vm_stat", &[]) {
            let (free, active) = vm_gb(&vm);
            line.push_str(&format!(" · livre {free:.1} · ativa {active:.1}"));
        }
        lines.push(line);
    }
    if let Some(brand) = capture("/usr/sbin/sysctl", &["-n", "machdep.cpu.brand_string"]) {
        let brand = brand.trim();
        if !brand.is_empty() {
            lines.push(trim_chars(&format!("cpu  {brand}"), 60));
        }
    }
    lines
}

fn storage_panel() -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(df) = capture("/bin/df", &["-h", "/"]) {
        if let Some(row) = df.lines().last() {
            let fields: Vec<&str> = row.split_whitespace().collect();
            if fields.len() >= 6 {
                lines.push(format!(
                    "/  {} total · {} usados · {} livres ({})",
                    fields[1], fields[2], fields[3], fields[4]
                ));
            }
        }
    }
    if let Some(info) = capture("/usr/sbin/diskutil", &["info", "/"]) {
        let wanted = [
            "Volume Name",
            "Container Free Space",
            "Volume Free Space",
            "Total Size",
            "Container Total Space",
        ];
        let mut kept = 0;
        for line in info.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            if wanted.iter().any(|name| key.eq_ignore_ascii_case(name)) {
                lines.push(trim_chars(&format!("{key}: {value}"), 52));
                kept += 1;
                if kept >= 3 {
                    break;
                }
            }
        }
    }
    lines
}

fn power_panel() -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(battery) = capture("/usr/bin/pmset", &["-g", "batt"]) {
        let source = battery
            .lines()
            .find(|line| line.contains("drawing from"))
            .and_then(|line| line.split('\'').nth(1))
            .unwrap_or("");
        if let Some(line) = battery.lines().find(|line| line.contains('%')) {
            let percent = line
                .split_whitespace()
                .find(|word| word.contains('%'))
                .unwrap_or("?")
                .trim_end_matches(';');
            let mut text = String::new();
            if !source.is_empty() {
                text.push_str(source);
                text.push_str(" · ");
            }
            text.push_str(percent);
            for part in line.split(';').map(str::trim).skip(1).take(2) {
                if part.is_empty() {
                    continue;
                }
                text.push_str(" · ");
                text.push_str(part);
            }
            lines.push(trim_chars(&text, 64));
        }
    }
    if let Some(settings) = capture("/usr/bin/pmset", &["-g"]) {
        let wanted = [
            "sleep",
            "displaysleep",
            "disksleep",
            "womp",
            "powernap",
            "standby",
            "hibernatemode",
            "lowpowermode",
        ];
        let mut values = Vec::new();
        for line in settings.lines() {
            let mut words = line.split_whitespace();
            let Some(key) = words.next() else {
                continue;
            };
            let key = key.trim_end_matches(':');
            if !wanted.contains(&key) {
                continue;
            }
            values.push(format!("{key} {}", words.next().unwrap_or("?")));
        }
        if !values.is_empty() {
            lines.push(trim_chars(&values.join(" · "), 72));
        }
    }
    lines
}

fn network_panel() -> Vec<String> {
    let mut lines = Vec::new();
    let address = capture("/usr/sbin/ipconfig", &["getifaddr", "en0"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| ("en0", value))
        .or_else(|| {
            capture("/usr/sbin/ipconfig", &["getifaddr", "en1"])
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| ("en1", value))
        });
    let default_route = capture("/sbin/route", &["-n", "get", "default"]);
    let gateway = default_route
        .as_deref()
        .and_then(|text| field(text, "gateway:"));
    let interface = default_route
        .as_deref()
        .and_then(|text| field(text, "interface:"));

    match address {
        Some((device, ip)) => {
            let mut line = format!("{device} · {ip}");
            if let Some(gateway) = gateway {
                line.push_str(&format!(" · gw {gateway}"));
            }
            if let Some(interface) = interface {
                line.push_str(&format!(" · rota {interface}"));
            }
            lines.push(trim_chars(&line, 64));
        }
        None => {
            let mut line = String::from("sem IPv4 local");
            if let Some(gateway) = gateway {
                line.push_str(&format!(" · gw {gateway}"));
            }
            if let Some(interface) = interface {
                line.push_str(&format!(" · rota {interface}"));
            }
            lines.push(trim_chars(&line, 64));
        }
    }
    let vpn = capture("/usr/sbin/scutil", &["--nc", "list"])
        .map(|text| {
            text.lines()
                .filter(|line| line.contains("(Connected)"))
                .count()
        })
        .unwrap_or(0);
    lines.push(if vpn > 0 {
        format!("vpn  {vpn} conectada(s)")
    } else {
        "vpn  nenhuma ativa".into()
    });
    lines
}

fn task_lines(path: PathBuf, label: &str) -> Vec<String> {
    let items = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    if items.is_empty() {
        return vec![format!("{label}  nenhuma")];
    }
    let enabled = items
        .iter()
        .filter(|item| {
            item.get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true)
        })
        .count();
    let mut lines = vec![format!("{label}  {} · {} ativa(s)", items.len(), enabled)];
    for item in items.iter().take(4) {
        let name = item
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("sem nome");
        let on = item
            .get("enabled")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        lines.push(format!(
            "  {} {}",
            if on { "✓" } else { "✗" },
            trim_chars(name, 30)
        ));
    }
    lines
}

fn tasks_panel() -> Vec<String> {
    let dir = data_dir();
    let mut lines = task_lines(dir.join("routines.json"), "rotinas");
    lines.extend(task_lines(dir.join("watchers.json"), "watchers"));
    lines
}

fn clamp_body(body: String) -> String {
    if body.chars().count() <= MAX_BODY {
        return body;
    }
    let mut out = String::new();
    for line in body.lines() {
        let extra = if out.is_empty() {
            line.chars().count()
        } else {
            line.chars().count() + 1
        };
        if out.chars().count() + extra + 1 > MAX_BODY {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out.push('…');
    out
}

fn build_body(panels: &[String]) -> String {
    let mut sections = Vec::new();
    for panel in panels {
        let (title, lines) = match panel.as_str() {
            "system" => ("SISTEMA", system_panel()),
            "storage" => ("ARMAZENAMENTO", storage_panel()),
            "power" => ("ENERGIA", power_panel()),
            "network" => ("REDE", network_panel()),
            "tasks" => ("TAREFAS", tasks_panel()),
            _ => continue,
        };
        let mut section = format!("▌ {title}");
        for line in lines {
            section.push('\n');
            section.push_str(&trim_chars(&line, 64));
        }
        sections.push(section);
    }
    clamp_body(sections.join("\n\n"))
}

fn panels_arg(args: &Value) -> Result<Vec<String>, ToolError> {
    let requested: Vec<String> = args
        .get("panels")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|panel| !panel.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if requested.is_empty() {
        return Ok(PANELS.iter().map(|panel| panel.to_string()).collect());
    }
    let mut chosen: Vec<String> = Vec::new();
    for panel in requested {
        if !PANELS.contains(&panel.as_str()) {
            return Err(ToolError::invalid(format!(
                "painel desconhecido `{panel}`: use system, storage, power, network ou tasks"
            )));
        }
        if !chosen.contains(&panel) {
            chosen.push(panel);
        }
    }
    Ok(chosen)
}

struct DashboardTool {
    spec: ToolSpec,
}

impl Tool for DashboardTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        let panels = panels_arg(args)?;
        let body = build_body(&panels);
        if body.trim().is_empty() {
            return Err(ToolError::failed(
                "não consegui coletar dados do sistema agora",
            ));
        }
        let anchor = match args.get("anchor").and_then(|value| value.as_str()) {
            Some(value) if ANCHORS.contains(&value) => value,
            _ => "top-right",
        };
        let width = args
            .get("width")
            .and_then(|value| value.as_f64())
            .unwrap_or(440.0)
            .clamp(240.0, 1200.0);
        let seconds = args
            .get("seconds")
            .and_then(|value| value.as_u64())
            .unwrap_or(0);
        let chars = body.chars().count();
        let payload = json!({
            "action": "show",
            "title": "Friday · painel",
            "body": body.clone(),
            "anchor": anchor,
            "width": width,
            "seconds": seconds,
        });
        if ctx.dry_run {
            return Ok(json!({
                "dry_run": true,
                "body": body,
                "panels": panels,
                "chars": chars,
                "would_show": payload,
            }));
        }
        send(payload)?;
        Ok(json!({
            "ok": true,
            "shown": true,
            "panels": panels,
            "chars": chars,
            "anchor": anchor,
        }))
    }
}

pub fn register(registry: &mut Registry) {
    registry.add(DashboardTool {
        spec: ToolSpec {
            name: "dashboard_open",
            summary: "Abre um painel flutuante estilo Jarvis com dados reais do Mac: sistema (uptime, memória, CPU), armazenamento, energia, rede e as rotinas/watchers da Friday. Monta o texto localmente e envia pelo overlay.",
            risk: crate::policy::Risk::Reversible,
            args: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "panels": {
                        "type": "array",
                        "description": "Quais painéis incluir. Padrão: todos.",
                        "items": {
                            "type": "string",
                            "enum": ["system", "storage", "power", "network", "tasks"]
                        },
                        "uniqueItems": true
                    },
                    "anchor": {
                        "type": "string",
                        "enum": ["center", "top", "bottom", "left", "right", "top-left", "top-right", "bottom-left", "bottom-right"],
                        "default": "top-right"
                    },
                    "width": { "type": "number", "minimum": 240, "maximum": 1200, "default": 440 },
                    "seconds": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "0 mantém aberto até substituir/fechar."
                    }
                }
            }),
        },
    });
}
