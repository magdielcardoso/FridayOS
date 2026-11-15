//! Rede, calendário, lembretes, contatos e cenas da casa.
//!
//! Tudo aqui é feito com CLIs do próprio macOS (`route`, `ifconfig`,
//! `networksetup`, `ping`, `dns-sd`, `scutil`, `curl`, `shortcuts`) ou com JXA
//! via `osascript -l JavaScript` para Calendar/Reminders/Contacts.
//!
//! Regras do módulo:
//! - todo comando externo tem timeout fixo, implementado com spawn + poll +
//!   kill (o macOS não traz o binário `timeout`);
//! - toda interpolação em JXA passa por [`js_str`], que escapa aspas, barras e
//!   caracteres de controle;
//! - nenhum caminho dá panic: argumentos ausentes viram `InvalidArgs`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

// ---------------------------------------------------------------------------
// Infra: execução com timeout
// ---------------------------------------------------------------------------

struct CmdOut {
    success: bool,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

impl CmdOut {
    fn text(&self) -> String {
        if self.success {
            self.stdout.clone()
        } else if self.stderr.trim().is_empty() {
            self.stdout.clone()
        } else {
            self.stderr.clone()
        }
    }
}

/// Spawn + poll + kill. `try_wait` é chamado em laço curto e o filho é morto
/// quando o prazo estoura, então nenhuma ferramenta trava a sessão.
///
/// A saída vai para arquivos temporários em vez de pipes: assim um comando
/// verboso (500 lembretes, dezenas de redes) nunca enche o buffer de 64 KB e
/// trava o processo antes de ser lido.
fn run_timeout(
    cmd: &str,
    args: &[&str],
    timeout: Duration,
    clean_locale: bool,
) -> Result<CmdOut, ToolError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = format!("friday-net-{}-{stamp}-{seq}", std::process::id());
    let out_path = std::env::temp_dir().join(format!("{base}.out"));
    let err_path = std::env::temp_dir().join(format!("{base}.err"));

    let out_file = std::fs::File::create(&out_path)
        .map_err(|e| ToolError::Failed(format!("{cmd}: não foi possível criar saída temporária: {e}")))?;
    let err_file = std::fs::File::create(&err_path)
        .map_err(|e| ToolError::Failed(format!("{cmd}: não foi possível criar erro temporário: {e}")))?;

    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file));
    if clean_locale {
        command.env("LC_ALL", "C").env("LANG", "C");
    }

    let spawn = command.spawn();
    let mut child = match spawn {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            let _ = std::fs::remove_file(&err_path);
            return Err(ToolError::Failed(format!("{cmd}: {e}")));
        }
    };

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&out_path);
                let _ = std::fs::remove_file(&err_path);
                return Err(ToolError::Failed(format!("{cmd}: {e}")));
            }
        }
    };

    let stdout = std::fs::read(&out_path).unwrap_or_default();
    let stderr = std::fs::read(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);

    Ok(CmdOut {
        success: status.map(|s| s.success()).unwrap_or(false),
        timed_out,
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    })
}

fn run_cli(cmd: &str, args: &[&str], timeout: Duration) -> Result<CmdOut, ToolError> {
    run_timeout(cmd, args, timeout, true)
}

const QUICK: Duration = Duration::from_secs(10);
const NETWORK: Duration = Duration::from_secs(12);

// ---------------------------------------------------------------------------
// Infra: JXA
// ---------------------------------------------------------------------------

/// Escapa uma string como literal JavaScript seguro.
fn js_str(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Trecho JXA comum a Calendar e Reminders: serializa datas em ISO local
/// (formato que o modelo entende) e em ISO UTC (inequívoco).
const JXA_DATE_HELPERS: &str = r#"
function _pad(n) { return (n < 10 ? '0' : '') + n; }
function localIso(d) {
  if (!d) return null;
  try {
    return d.getFullYear() + '-' + _pad(d.getMonth() + 1) + '-' + _pad(d.getDate())
      + 'T' + _pad(d.getHours()) + ':' + _pad(d.getMinutes()) + ':' + _pad(d.getSeconds());
  } catch (e) { return null; }
}
function utcIso(d) {
  if (!d) return null;
  try { return d.toISOString(); } catch (e) { return null; }
}
"#;

fn jxa_error(stderr: &str) -> ToolError {
    let detail = stderr.trim();
    let lower = detail.to_lowercase();
    if lower.contains("-1743")
        || lower.contains("not authorized")
        || lower.contains("not permitted")
        || lower.contains("não tem permissão")
        || lower.contains("operation not permitted")
    {
        return ToolError::Failed(format!(
            "o macOS bloqueou o acesso (Automação/TCC). Autorize o FridayOS em \
             Ajustes do Sistema > Privacidade e Segurança > Automação. Detalhe: {detail}"
        ));
    }
    if detail.is_empty() {
        ToolError::Failed("osascript falhou sem mensagem".to_string())
    } else {
        ToolError::Failed(detail.to_string())
    }
}

fn run_jxa(script: &str, timeout: Duration) -> Result<String, ToolError> {
    let out = run_timeout(
        "/usr/bin/osascript",
        &["-l", "JavaScript", "-e", script],
        timeout,
        false,
    )?;
    if out.timed_out {
        return Err(ToolError::Failed(
            "osascript excedeu o tempo limite (o app pode estar pedindo permissão ou travado)".to_string(),
        ));
    }
    if !out.success {
        return Err(jxa_error(&out.stderr));
    }
    Ok(out.stdout.trim().to_string())
}

/// Roda JXA que termina em `JSON.stringify(...)` e devolve o valor já parseado.
fn run_jxa_json(script: &str, timeout: Duration) -> ToolResult {
    let raw = run_jxa(script, timeout)?;
    if raw.is_empty() {
        return Ok(Value::Array(Vec::new()));
    }
    serde_json::from_str::<Value>(&raw).map_err(|e| {
        let snippet: String = raw.chars().take(200).collect();
        ToolError::Failed(format!("resposta inesperada do osascript ({e}): {snippet}"))
    })
}

// ---------------------------------------------------------------------------
// Infra: argumentos
// ---------------------------------------------------------------------------

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::invalid(format!("`{key}` é obrigatório e não pode ser vazio")))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn opt_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

/// Valida um host sem deixá-lo virar flag de CLI.
fn valid_host(host: &str) -> Result<(), ToolError> {
    if host.len() > 255 {
        return Err(ToolError::invalid("host longo demais"));
    }
    if host.starts_with('-') {
        return Err(ToolError::invalid("host não pode começar com '-'"));
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
    {
        return Err(ToolError::invalid(format!("host inválido: {host}")));
    }
    Ok(())
}

fn valid_interface(name: &str) -> Result<String, ToolError> {
    let name = name.trim();
    if name.is_empty() || name.len() > 16 || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ToolError::invalid(format!("interface inválida: {name}")));
    }
    Ok(name.to_string())
}

fn opt_interface(args: &Value) -> Result<String, ToolError> {
    match opt_str(args, "interface") {
        Some(name) => valid_interface(name),
        None => Ok(wifi_interface()),
    }
}

/// Descobre a interface Wi-Fi real (`en1`, `en0`, ...) em vez de assumir en0.
fn wifi_interface() -> String {
    if let Ok(out) = run_cli(
        "/usr/sbin/networksetup",
        &["-listallhardwareports"],
        QUICK,
    ) {
        let mut port = String::new();
        for line in out.stdout.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("Hardware Port:") {
                port = rest.trim().to_string();
            } else if let Some(rest) = trimmed.strip_prefix("Device:") {
                if port.eq_ignore_ascii_case("Wi-Fi") || port.eq_ignore_ascii_case("AirPort") {
                    let device = rest.trim();
                    if !device.is_empty() {
                        return device.to_string();
                    }
                }
            }
        }
    }
    "en0".to_string()
}

fn valid_ssid(ssid: &str) -> Result<(), ToolError> {
    if ssid.is_empty() || ssid.chars().count() > 64 {
        return Err(ToolError::invalid("ssid deve ter entre 1 e 64 caracteres"));
    }
    if ssid.contains('\n') || ssid.contains('\r') {
        return Err(ToolError::invalid("ssid não pode conter quebras de linha"));
    }
    if ssid.starts_with('-') {
        return Err(ToolError::invalid("ssid não pode começar com '-'"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Infra: parsing
// ---------------------------------------------------------------------------

fn parse_number_before(text: &str, marker: &str) -> Option<f64> {
    let idx = text.find(marker)?;
    let before = text[..idx].trim_end();
    let start = before
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map(|i| i + 1)
        .unwrap_or(0);
    before[start..].parse::<f64>().ok()
}

fn is_mac(token: &str) -> bool {
    let parts: Vec<&str> = token.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_uuid(token: &str) -> bool {
    let parts: Vec<&str> = token.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts
            .iter()
            .zip(lens.iter())
            .all(|(p, want)| p.len() == *want && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn which(binary: &str) -> Option<String> {
    let path = Path::new(binary);
    if path.is_absolute() {
        return path.is_file().then(|| binary.to_string());
    }
    let dirs = ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/local/bin", "/opt/homebrew/bin"];
    dirs.iter()
        .map(|dir| format!("{dir}/{binary}"))
        .find(|candidate| Path::new(candidate).is_file())
}

// ---------------------------------------------------------------------------
// Tool wrapper
// ---------------------------------------------------------------------------

struct FnTool {
    spec: ToolSpec,
    handler: fn(&Value, &ToolCtx) -> ToolResult,
}

impl FnTool {
    fn new(spec: ToolSpec, handler: fn(&Value, &ToolCtx) -> ToolResult) -> Self {
        Self { spec, handler }
    }
}

impl Tool for FnTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
        (self.handler)(args, ctx)
    }
}

fn tool(
    name: &'static str,
    summary: &'static str,
    risk: Risk,
    args: Value,
    handler: fn(&Value, &ToolCtx) -> ToolResult,
) -> FnTool {
    FnTool::new(
        ToolSpec {
            name,
            summary,
            risk,
            args,
        },
        handler,
    )
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

// ---------------------------------------------------------------------------
// net_status
// ---------------------------------------------------------------------------

fn wifi_power_of(iface: &str) -> Option<String> {
    let out = run_cli(
        "/usr/sbin/networksetup",
        &["-getairportpower", iface],
        QUICK,
    )
    .ok()?;
    if !out.success {
        return None;
    }
    let value = out.stdout.rsplit(':').next()?.trim().to_string();
    matches!(value.as_str(), "On" | "Off").then_some(value)
}

fn net_status(args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let requested = match opt_str(args, "interface") {
        Some(name) => Some(valid_interface(name)?),
        None => None,
    };

    let route = run_cli("/sbin/route", &["-n", "get", "default"], QUICK).ok();
    let route_text = route.as_ref().map(|r| r.stdout.clone()).unwrap_or_default();

    let route_interface = route_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let interface = requested.or(route_interface);

    let gateway = route_text
        .lines()
        .find_map(|line| line.trim().strip_prefix("gateway:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut ipv4: Vec<String> = Vec::new();
    let mut ipv6: Vec<String> = Vec::new();
    let mut ssid: Option<String> = None;
    let mut wifi_power: Option<String> = None;
    let mut wifi_interface_used: Option<String> = None;
    let mut notes: Vec<String> = Vec::new();

    if let Some(iface) = interface.as_deref() {
        if let Ok(out) = run_cli("/sbin/ifconfig", &[iface], QUICK) {
            for line in out.stdout.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("inet ") {
                    if let Some(addr) = rest.split_whitespace().next() {
                        ipv4.push(addr.to_string());
                    }
                } else if let Some(rest) = trimmed.strip_prefix("inet6 ") {
                    if let Some(addr) = rest.split_whitespace().next() {
                        if !addr.starts_with("fe80") && addr != "::1" {
                            ipv6.push(addr.to_string());
                        }
                    }
                }
            }
        }

        // A rota default pode estar numa interface Ethernet; para energia e
        // SSID usamos a interface Wi-Fi detectada quando a atual não responde.
        let mut wifi_iface = iface.to_string();
        let mut power = wifi_power_of(iface);
        if power.is_none() {
            let detected = wifi_interface();
            if detected != iface {
                power = wifi_power_of(&detected);
                if power.is_some() {
                    wifi_iface = detected;
                }
            }
        }
        wifi_power = power;
        wifi_interface_used = Some(wifi_iface.clone());

        // SSID: networksetup primeiro; em versões novas do macOS ele pode
        // responder "not associated" mesmo estando conectado, então cai para
        // `ipconfig getsummary`.
        if let Ok(out) = run_cli(
            "/usr/sbin/networksetup",
            &["-getairportnetwork", &wifi_iface],
            QUICK,
        ) {
            if let Some((_, rest)) = out.stdout.split_once(':') {
                let candidate = rest.trim();
                if !candidate.is_empty()
                    && !candidate.to_lowercase().contains("not associated")
                    && !candidate.to_lowercase().contains("não está associado")
                {
                    ssid = Some(candidate.to_string());
                }
            }
        }

        if ssid.is_none() {
            if let Ok(out) = run_cli("/usr/sbin/ipconfig", &["getsummary", &wifi_iface], QUICK) {
                ssid = out.stdout.lines().find_map(|line| {
                    let trimmed = line.trim();
                    let value = trimmed.strip_prefix("SSID :")?;
                    let value = value.trim();
                    (!value.is_empty()).then(|| value.to_string())
                });
                if ssid.is_none() {
                    notes.push(
                        "SSID indisponível sem permissão de Localização ou em redes ocultas"
                            .to_string(),
                    );
                }
            }
        }
    } else {
        notes.push("nenhuma rota padrão ativa".to_string());
    }

    Ok(json!({
        "interface": interface,
        "gateway": gateway,
        "ipv4": ipv4,
        "ipv6": ipv6,
        "ssid": ssid,
        "wifi_power": wifi_power,
        "wifi_interface": wifi_interface_used,
        "notes": notes,
    }))
}

// ---------------------------------------------------------------------------
// wifi_scan
// ---------------------------------------------------------------------------

const AIRPORT: &str =
    "/System/Library/PrivateFrameworks/Apple80211.framework/Versions/Current/Resources/airport";

fn parse_airport_scan(text: &str) -> Vec<Value> {
    let mut networks = Vec::new();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(mac_idx) = tokens.iter().position(|t| is_mac(t)) else {
            continue;
        };
        if mac_idx == 0 {
            continue;
        }
        let ssid = tokens[..mac_idx].join(" ");
        // Depois do BSSID: RSSI, CHANNEL, HT, CC, SECURITY
        let tail = &tokens[mac_idx + 1..];
        let rssi = tail.first().and_then(|t| t.parse::<i64>().ok());
        let channel = tail.get(1).map(|t| t.to_string());
        let cc = tail.get(3).map(|t| t.to_string());
        let security = (tail.len() > 4).then(|| tail[4..].join(" "));
        networks.push(json!({
            "ssid": ssid,
            "bssid": tokens[mac_idx],
            "rssi": rssi,
            "channel": channel,
            "country_code": cc,
            "security": security,
        }));
    }
    networks
}

fn wifi_scan(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let mut notes: Vec<&str> = Vec::new();
    if Path::new(AIRPORT).is_file() {
        let out = run_cli(AIRPORT, &["-s"], NETWORK)?;
        let networks = parse_airport_scan(&out.stdout);
        // Em macOS recentes o binário existe mas foi desativado: sai com
        // sucesso e sem linhas, ou exige permissão de Localização.
        if out.success && !networks.is_empty() {
            return Ok(json!({
                "source": "airport",
                "count": networks.len(),
                "networks": networks,
            }));
        }
        notes.push("`airport -s` não devolveu redes (desativado ou sem permissão de Localização)");
    } else {
        notes.push("`airport` não existe nesta versão do macOS");
    }

    // Fallback: só as redes já conhecidas, sem RSSI nem canal.
    let iface = wifi_interface();
    let out = run_cli(
        "/usr/sbin/networksetup",
        &["-listpreferredwirelessnetworks", &iface],
        QUICK,
    )?;
    if !out.success {
        return Err(ToolError::Failed(format!(
            "nenhum método de varredura disponível: {}",
            out.text().trim()
        )));
    }
    let mut networks = Vec::new();
    for line in out.stdout.lines().skip(1) {
        let ssid = line.trim();
        if ssid.is_empty() {
            continue;
        }
        networks.push(json!({
            "ssid": ssid,
            "bssid": Value::Null,
            "rssi": Value::Null,
            "channel": Value::Null,
            "security": Value::Null,
        }));
    }
    notes.push("listadas apenas redes preferidas, sem RSSI nem canal");
    Ok(json!({
        "source": "preferred",
        "count": networks.len(),
        "networks": networks,
        "notes": notes,
    }))
}

// ---------------------------------------------------------------------------
// wifi_join
// ---------------------------------------------------------------------------

fn wifi_join(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let ssid = req_str(args, "ssid")?;
    valid_ssid(ssid)?;
    let iface = opt_interface(args)?;
    let password = opt_str(args, "password");
    if let Some(pass) = password {
        if pass.chars().count() > 128 || pass.contains('\n') {
            return Err(ToolError::invalid("senha inválida"));
        }
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_join": {
                "ssid": ssid,
                "interface": iface,
                "password": if password.is_some() { "(fornecida)" } else { "(nenhuma)" },
            },
        }));
    }

    let mut argv: Vec<&str> = vec!["-setairportnetwork", &iface, ssid];
    if let Some(pass) = password {
        argv.push(pass);
    }
    let out = run_cli("/usr/sbin/networksetup", &argv, NETWORK)?;
    let text = out.text();
    let lowered = text.to_lowercase();
    if !out.success
        || lowered.contains("could not find")
        || lowered.contains("failed")
        || lowered.contains("error")
    {
        return Err(ToolError::Failed(format!(
            "não foi possível entrar em `{ssid}`: {}",
            text.trim()
        )));
    }

    Ok(json!({
        "joined": true,
        "ssid": ssid,
        "interface": iface,
        "note": "a senha não é registrada na resposta; a auditoria grava apenas os argumentos da chamada",
    }))
}

// ---------------------------------------------------------------------------
// net_ping
// ---------------------------------------------------------------------------

fn net_ping(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let host = req_str(args, "host")?;
    valid_host(host)?;
    let count = opt_usize(args, "count").unwrap_or(4).clamp(1, 10);
    let count_arg = count.to_string();
    let timeout = Duration::from_millis(4_000 + (count as u64) * 1_500);

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_ping": { "host": host, "count": count },
        }));
    }

    let out = run_cli(
        "/sbin/ping",
        &["-c", &count_arg, "-W", "1000", host],
        timeout,
    )?;

    let text = if out.stdout.trim().is_empty() {
        out.stderr.clone()
    } else {
        out.stdout.clone()
    };

    if out.timed_out {
        return Err(ToolError::Failed(format!(
            "ping para `{host}` excedeu o tempo limite"
        )));
    }
    if !out.success && !text.contains("packet loss") && !text.contains("round-trip") {
        return Err(ToolError::Failed(format!(
            "ping falhou: {}",
            out.text().trim()
        )));
    }

    let loss = parse_number_before(&text, "% packet loss");
    let rtt_avg = text.lines().find_map(|line| {
        if !(line.contains("round-trip") || line.contains("min/avg/max")) {
            return None;
        }
        let after = line.split('=').nth(1)?.trim();
        let nums = after.trim_end_matches("ms").trim();
        nums.split('/').nth(1)?.trim().parse::<f64>().ok()
    });

    let transmitted = text
        .lines()
        .find(|line| line.contains("packets transmitted"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok());
    let received = text
        .lines()
        .find(|line| line.contains("packets received"))
        .and_then(|line| {
            line.split("packets received")
                .next()
                .and_then(|head| head.split_whitespace().last())
                .and_then(|n| n.parse::<u64>().ok())
        });

    Ok(json!({
        "host": host,
        "count": count,
        "packet_loss_percent": loss,
        "avg_rtt_ms": rtt_avg,
        "packets_transmitted": transmitted,
        "packets_received": received,
        "reachable": loss.map(|l| l < 100.0),
    }))
}

// ---------------------------------------------------------------------------
// net_local_hosts (Bonjour)
// ---------------------------------------------------------------------------

fn net_local_hosts(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let Some(dns_sd) = which("dns-sd") else {
        return Err(ToolError::Failed(
            "`dns-sd` não encontrado nesta instalação do macOS".to_string(),
        ));
    };

    // dns-sd -B fica rodando para sempre; 2s de escuta e kill é o contrato.
    let out = run_timeout(
        &dns_sd,
        &["-B", "_services._dns-sd._udp", "local."],
        Duration::from_millis(2_300),
        true,
    )?;

    let mut types: Vec<String> = Vec::new();
    for line in out.stdout.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 6 {
            continue;
        }
        if tokens[1] != "Add" {
            continue;
        }
        // Saída real da busca meta:
        //   Timestamp A/R Flags if Domain Service Type Instance Name
        //   21:18:47 Add 3 6 . _udp.local. _asquic
        // O tipo descoberto é Instance Name + transporte da coluna 5.
        let Some(transport) = tokens[5].split('.').next() else {
            continue;
        };
        if transport != "_tcp" && transport != "_udp" {
            continue;
        }
        let instance = tokens[6..].join(" ");
        let instance = instance.trim().trim_end_matches('.');
        if !instance.starts_with('_') {
            continue;
        }
        let service_type = format!("{instance}.{transport}");
        if !types.iter().any(|t| t == &service_type) {
            types.push(service_type);
        }
        if types.len() >= 40 {
            break;
        }
    }

    let mut notes = vec!["varredura limitada a ~2s; só tipos de serviço, não instâncias".to_string()];
    if out.timed_out {
        notes.push("tempo esgotado encerrado pelo Friday (comportamento esperado)".to_string());
    }
    if types.is_empty() {
        notes.push("nenhum serviço anunciado respondeu; verifique se Bonjour está ativo na rede".to_string());
    }

    Ok(json!({
        "count": types.len(),
        "service_types": types,
        "timed_out": out.timed_out,
        "notes": notes,
    }))
}

// ---------------------------------------------------------------------------
// net_vpn_status
// ---------------------------------------------------------------------------

fn net_vpn_status(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let out = run_cli("/usr/sbin/scutil", &["--nc", "list"], QUICK)?;
    if !out.success {
        return Err(ToolError::Failed(format!(
            "scutil --nc list falhou: {}",
            out.text().trim()
        )));
    }

    let mut connections = Vec::new();
    for line in out.stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.contains('(') || !trimmed.contains('"') {
            continue;
        }
        let enabled = trimmed.starts_with('*');
        let status = trimmed
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(s, _)| s.trim().to_string());
        let name = trimmed
            .split('"')
            .nth(1)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let kind = trimmed
            .split('[')
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .map(|s| s.trim().to_string());
        let uuid = trimmed
            .split_whitespace()
            .find(|t| is_uuid(t))
            .map(str::to_string);

        connections.push(json!({
            "name": name,
            "status": status,
            "enabled": enabled,
            "uuid": uuid,
            "kind": kind,
        }));
    }

    let connected = connections
        .iter()
        .filter(|c| c.get("status").and_then(Value::as_str) == Some("Connected"))
        .count();

    Ok(json!({
        "count": connections.len(),
        "connected": connected,
        "connections": connections,
        "notes": if connections.is_empty() {
            vec!["nenhuma VPN configurada".to_string()]
        } else {
            Vec::<String>::new()
        },
    }))
}

// ---------------------------------------------------------------------------
// net_public_ip
// ---------------------------------------------------------------------------

fn net_public_ip(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let out = run_cli(
        "/usr/bin/curl",
        &["-s", "--max-time", "5", "https://api.ipify.org"],
        Duration::from_secs(8),
    )?;
    if out.timed_out {
        return Err(ToolError::Failed(
            "consulta de IP público excedeu o tempo limite".to_string(),
        ));
    }
    let ip = out.stdout.trim();
    if !out.success || ip.is_empty() {
        return Err(ToolError::Failed(format!(
            "não foi possível consultar o IP público: {}",
            out.text().trim()
        )));
    }
    if ip.len() > 64
        || !ip
            .chars()
            .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':'))
    {
        return Err(ToolError::Failed(format!(
            "resposta inesperada do serviço de IP: {ip}"
        )));
    }

    Ok(json!({
        "ip": ip,
        "source": "api.ipify.org",
        "note": "consulta externa: o endereço de rede é enviado a um serviço de terceiros",
    }))
}

// ---------------------------------------------------------------------------
// calendar_today
// ---------------------------------------------------------------------------

fn calendar_names() -> Result<Vec<String>, ToolError> {
    let script = r#"
var app = Application('Calendar');
var out = [];
var calendars = app.calendars();
for (var i = 0; i < calendars.length; i++) {
  try { out.push(String(calendars[i].name())); } catch (e) {}
}
JSON.stringify(out);
"#;
    let value = run_jxa_json(script, Duration::from_secs(45))?;
    Ok(value
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

/// Eventos de um único calendário. Roda em processo próprio porque alguns
/// calendários (feriados assinados, contas Google) travam o Calendar por tempo
/// indeterminado; o timeout por processo isola o problema.
fn calendar_events_script(calendar: &str) -> String {
    format!(
        r#"
{JXA_DATE_HELPERS}
var app = Application('Calendar');
var wanted = {calendar};
var now = new Date();
var start = new Date(now.getFullYear(), now.getMonth(), now.getDate());
var end = new Date(start.getTime() + 86400000);
var out = [];
var cal = null;
try {{ cal = app.calendars.byName(wanted); }} catch (e) {{ cal = null; }}
if (cal) {{
  var events = [];
  try {{
    // Um único objeto com dois operadores é muito mais rápido que
    // `_and: [...]`, que trava o Calendar por dezenas de segundos.
    events = cal.events.whose({{
      startDate: {{ _greaterThanEquals: start, _lessThan: end }}
    }})();
  }} catch (e) {{ events = []; }}
  for (var j = 0; j < events.length && out.length < 50; j++) {{
    var ev = events[j];
    var title = '';
    try {{ title = String(ev.summary()); }} catch (e) {{ continue; }}
    var s = null, e2 = null, allDay = false, location = null;
    try {{ s = ev.startDate(); }} catch (e) {{}}
    try {{ e2 = ev.endDate(); }} catch (e) {{}}
    try {{ allDay = !!ev.alldayEvent(); }} catch (e) {{}}
    try {{ var loc = ev.location(); if (loc) location = String(loc); }} catch (e) {{}}
    out.push({{
      title: title,
      start: localIso(s),
      end: localIso(e2),
      start_utc: utcIso(s),
      end_utc: utcIso(e2),
      all_day: allDay,
      calendar: wanted,
      location: location
    }});
  }}
}}
JSON.stringify(out);
"#,
        calendar = js_str(calendar),
    )
}

fn calendar_today(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let names = calendar_names()?;
    let mut events: Vec<Value> = Vec::new();
    let mut notes: Vec<String> = vec![
        "horários em ISO local; `_utc` traz o mesmo instante em UTC".to_string(),
    ];
    let mut skipped: Vec<String> = Vec::new();

    // Consultas em série com orçamento global: o Calendar serializa Apple
    // Events, então paralelizar só aumenta a latência. Calendários de feriados
    // e aniversários (quase sempre assinados/remotos) vão para o fim e são os
    // primeiros a serem cortados pelo orçamento.
    let mut ordered: Vec<String> = names;
    ordered.sort_by_key(|name| {
        let lower = name.to_lowercase();
        u8::from(
            lower.contains("feriado")
                || lower.contains("holiday")
                || lower.contains("aniversar")
                || lower.contains("birthday"),
        )
    });

    let budget = Duration::from_secs(15);
    let started = Instant::now();
    for name in ordered.into_iter().take(24) {
        if started.elapsed() >= budget {
            skipped.push(name);
            continue;
        }
        let remaining = budget.saturating_sub(started.elapsed());
        let timeout = remaining.min(Duration::from_secs(5));
        let script = calendar_events_script(&name);
        match run_jxa_json(&script, timeout)
            .ok()
            .and_then(|value| value.as_array().cloned())
        {
            Some(list) => events.extend(list),
            None => skipped.push(name),
        }
    }

    events.sort_by(|a, b| {
        let left = a.get("start").and_then(Value::as_str).unwrap_or("");
        let right = b.get("start").and_then(Value::as_str).unwrap_or("");
        left.cmp(right)
    });
    events.truncate(200);

    if !skipped.is_empty() {
        notes.push(format!(
            "calendários ignorados por lentidão/erro: {}",
            skipped.join(", ")
        ));
    }

    Ok(json!({
        "date": "hoje",
        "count": events.len(),
        "events": events,
        "notes": notes,
    }))
}

// ---------------------------------------------------------------------------
// calendar_create
// ---------------------------------------------------------------------------

fn calendar_create(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let title = req_str(args, "title")?;
    let start = req_str(args, "start")?;
    let end = opt_str(args, "end");
    let calendar = opt_str(args, "calendar");
    let notes = opt_str(args, "notes");

    if title.chars().count() > 500 {
        return Err(ToolError::invalid("título longo demais"));
    }
    if !start.contains('T') {
        return Err(ToolError::invalid(
            "`start` deve ser ISO 8601, por exemplo 2026-11-14T09:00:00",
        ));
    }

    let end_expr = match end {
        Some(value) => {
            if !value.contains('T') {
                return Err(ToolError::invalid(
                    "`end` deve ser ISO 8601, por exemplo 2026-11-14T10:00:00",
                ));
            }
            format!("new Date({})", js_str(value))
        }
        None => "new Date(start.getTime() + 3600000)".to_string(),
    };

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_create": {
                "title": title,
                "start": start,
                "end": end,
                "calendar": calendar,
                "notes": notes,
            },
        }));
    }

    let script = format!(
        r#"
{JXA_DATE_HELPERS}
var app = Application('Calendar');
var title = {title};
var start = new Date({start});
var end = {end_expr};
if (isNaN(start.getTime())) throw new Error('data de início inválida: ' + {start_lit});
if (isNaN(end.getTime())) throw new Error('data de fim inválida');
if (end.getTime() <= start.getTime()) throw new Error('o fim precisa ser depois do início');
var wanted = {calendar};
var target = null;
if (wanted) {{
  try {{ target = app.calendars.byName(wanted); }} catch (e) {{ target = null; }}
  if (target) {{ try {{ String(target.name()); }} catch (e) {{ target = null; }} }}
  if (!target) throw new Error('calendário não encontrado: ' + wanted);
}} else {{
  target = app.defaultCalendar();
}}
var props = {{ summary: title, startDate: start, endDate: end }};
var notes = {notes};
if (notes) props.description = notes;
var ev = app.Event(props);
target.events.push(ev);
var result = {{
  created: true,
  title: title,
  start: localIso(start),
  end: localIso(end),
  start_utc: utcIso(start),
  end_utc: utcIso(end),
  calendar: ''
}};
try {{ result.calendar = String(target.name()); }} catch (e) {{}}
try {{ result.uid = String(ev.uid()); }} catch (e) {{}}
JSON.stringify(result);
"#,
        title = js_str(title),
        start_lit = js_str(start),
        start = js_str(start),
        end_expr = end_expr,
        calendar = calendar.map(js_str).unwrap_or_else(|| "null".to_string()),
        notes = notes.map(js_str).unwrap_or_else(|| "null".to_string()),
    );

    run_jxa_json(&script, Duration::from_secs(35))
}

// ---------------------------------------------------------------------------
// reminders_list
// ---------------------------------------------------------------------------

fn reminders_list(args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let list = opt_str(args, "list");
    let limit = opt_usize(args, "limit").unwrap_or(100).clamp(1, 500);

    let script = format!(
        r#"
{JXA_DATE_HELPERS}
var app = Application('Reminders');
var wanted = {list};
var limit = {limit};
var out = [];
var lists = app.lists();
for (var i = 0; i < lists.length; i++) {{
  var list = lists[i];
  var listName = '';
  try {{ listName = String(list.name()); }} catch (e) {{ continue; }}
  if (wanted && listName !== wanted) continue;
  var items = [];
  try {{ items = list.reminders.whose({{ completed: false }})(); }} catch (e) {{ continue; }}
  for (var j = 0; j < items.length && out.length < limit; j++) {{
    var r = items[j];
    var title = '';
    try {{ title = String(r.name()); }} catch (e) {{ continue; }}
    var due = null, notes = null, flagged = false;
    try {{ due = r.dueDate(); }} catch (e) {{}}
    try {{ var body = r.body(); if (body) notes = String(body); }} catch (e) {{}}
    try {{ flagged = !!r.flagged(); }} catch (e) {{}}
    out.push({{
      title: title,
      list: listName,
      due: localIso(due),
      due_utc: utcIso(due),
      flagged: flagged,
      notes: notes
    }});
  }}
}}
JSON.stringify({{ count: out.length, reminders: out }});
"#,
        list = list.map(js_str).unwrap_or_else(|| "null".to_string()),
    );

    // O Reminders pode levar ~30s no primeiro acesso frio.
    run_jxa_json(&script, Duration::from_secs(45))
}

// ---------------------------------------------------------------------------
// reminder_add
// ---------------------------------------------------------------------------

fn reminder_add(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let title = req_str(args, "title")?;
    let list = opt_str(args, "list");
    let due = opt_str(args, "due");

    if title.chars().count() > 500 {
        return Err(ToolError::invalid("título longo demais"));
    }
    if let Some(value) = due {
        if !value.contains('T') && !value.contains('-') {
            return Err(ToolError::invalid(
                "`due` deve ser ISO 8601, por exemplo 2026-11-14T18:00:00",
            ));
        }
    }

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_add": { "title": title, "list": list, "due": due },
        }));
    }

    let script = format!(
        r#"
{JXA_DATE_HELPERS}
var app = Application('Reminders');
var title = {title};
var wanted = {list};
var dueText = {due};
var target = null;
if (wanted) {{
  try {{ target = app.lists.byName(wanted); }} catch (e) {{ target = null; }}
  if (target) {{ try {{ String(target.name()); }} catch (e) {{ target = null; }} }}
  if (!target) throw new Error('lista não encontrada: ' + wanted);
}} else {{
  try {{ target = app.defaultList(); }} catch (e) {{ target = null; }}
  if (!target) {{
    var all = app.lists();
    if (all.length > 0) target = all[0];
  }}
  if (!target) throw new Error('nenhuma lista de lembretes disponível');
}}
var props = {{ name: title }};
if (dueText) {{
  var due = new Date(dueText);
  if (isNaN(due.getTime())) throw new Error('data de vencimento inválida: ' + dueText);
  props.dueDate = due;
}}
var reminder = app.Reminder(props);
target.reminders.push(reminder);
var result = {{ created: true, title: title, list: '', due: dueText || null }};
try {{ result.list = String(target.name()); }} catch (e) {{}}
JSON.stringify(result);
"#,
        title = js_str(title),
        list = list.map(js_str).unwrap_or_else(|| "null".to_string()),
        due = due.map(js_str).unwrap_or_else(|| "null".to_string()),
    );

    run_jxa_json(&script, Duration::from_secs(25))
}

// ---------------------------------------------------------------------------
// contacts_search
// ---------------------------------------------------------------------------

fn contacts_search(args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let query = req_str(args, "query")?;
    let limit = opt_usize(args, "limit").unwrap_or(20).clamp(1, 50);
    if query.chars().count() > 200 {
        return Err(ToolError::invalid("consulta longa demais"));
    }

    // Iterar todos os contatos custa dezenas de segundos (milhares de Apple
    // Events). `whose` filtra do lado do Contacts; pedimos os detalhes
    // completos apenas dos poucos que casaram.
    let script = format!(
        r#"
var app = Application('Contacts');
var query = {query}.toLowerCase();
var limit = {limit};
var out = [];
var seen = {{}};
function collect(list) {{
  for (var i = 0; i < list.length && out.length < limit; i++) {{
    var p = list[i];
    var name = '', org = '';
    try {{ name = String(p.name() || ''); }} catch (e) {{}}
    try {{ org = String(p.organization() || ''); }} catch (e) {{}}
    var key = name + '|' + org;
    if (seen[key]) continue;
    seen[key] = true;
    var emails = [], phones = [];
    try {{
      var es = p.emails();
      for (var a = 0; a < es.length; a++) {{
        try {{ var v = es[a].value(); if (v) emails.push(String(v)); }} catch (e) {{}}
      }}
    }} catch (e) {{}}
    try {{
      var ps = p.phones();
      for (var b = 0; b < ps.length; b++) {{
        try {{ var w = ps[b].value(); if (w) phones.push(String(w)); }} catch (e) {{}}
      }}
    }} catch (e) {{}}
    if (name || org || emails.length || phones.length) {{
      out.push({{ name: name, organization: org || null, emails: emails, phones: phones }});
    }}
  }}
}}
try {{ collect(app.people.whose({{ name: {{ _contains: query }} }})()); }} catch (e) {{}}
if (out.length < limit) {{
  try {{ collect(app.people.whose({{ organization: {{ _contains: query }} }})()); }} catch (e) {{}}
}}
JSON.stringify({{ count: out.length, contacts: out }});
"#,
        query = js_str(query),
    );

    run_jxa_json(&script, Duration::from_secs(25))
}

// ---------------------------------------------------------------------------
// HomeKit via Shortcuts
// ---------------------------------------------------------------------------

const SCENE_PREFIX: &str = "Casa:";

fn home_list_scenes(_args: &Value, _ctx: &ToolCtx) -> ToolResult {
    let out = run_cli("/usr/bin/shortcuts", &["list"], Duration::from_secs(20))?;
    if !out.success {
        return Err(ToolError::Failed(format!(
            "`shortcuts list` falhou: {}",
            out.text().trim()
        )));
    }

    let mut scenes = Vec::new();
    for line in out.stdout.lines() {
        let shortcut = line.trim();
        if shortcut.is_empty() {
            continue;
        }
        // Comparação ASCII sem fatiar por offset de `to_lowercase()`, que pode
        // mudar o número de bytes e quebrar no meio de um caractere.
        let prefix_len = SCENE_PREFIX.len();
        if shortcut.len() < prefix_len
            || !shortcut.is_char_boundary(prefix_len)
            || !shortcut[..prefix_len].eq_ignore_ascii_case(SCENE_PREFIX)
        {
            continue;
        }
        let name = shortcut[prefix_len..].trim();
        if name.is_empty() {
            continue;
        }
        scenes.push(json!({ "name": name, "shortcut": shortcut }));
    }

    Ok(json!({
        "count": scenes.len(),
        "scenes": scenes,
        "notes": [
            "cenas do HomeKit são expostas como atalhos nomeados `Casa: <cena>`",
            "crie um atalho por cena em Atalhos para que Friday consiga executá-la"
        ],
    }))
}

fn home_run_scene(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let name = req_str(args, "name")?.trim();
    if name.chars().count() > 200 || name.contains('\n') || name.starts_with('-') {
        return Err(ToolError::invalid("nome de cena inválido"));
    }
    let shortcut = if name.to_lowercase().starts_with(&SCENE_PREFIX.to_lowercase()) {
        name.to_string()
    } else {
        format!("{SCENE_PREFIX} {name}")
    };

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_run": shortcut,
        }));
    }

    let out = run_cli("/usr/bin/shortcuts", &["run", &shortcut], Duration::from_secs(60))?;
    if out.timed_out {
        return Err(ToolError::Failed(format!(
            "o atalho `{shortcut}` excedeu o tempo limite (pode estar pedindo confirmação)"
        )));
    }
    if !out.success {
        let text = out.text();
        let lowered = text.to_lowercase();
        if lowered.contains("couldn't find") || lowered.contains("not found") {
            return Err(ToolError::Failed(format!(
                "atalho `{shortcut}` não existe. Rode home_list_scenes para ver as cenas disponíveis"
            )));
        }
        return Err(ToolError::Failed(format!(
            "`shortcuts run` falhou para `{shortcut}`: {}",
            text.trim()
        )));
    }

    Ok(json!({
        "ran": true,
        "shortcut": shortcut,
        "output": out.stdout.trim(),
    }))
}

// ---------------------------------------------------------------------------
// register
// ---------------------------------------------------------------------------

pub fn register(registry: &mut Registry) {
    registry.add(tool(
        "net_status",
        "Mostra a interface de rede ativa, IPv4/IPv6, gateway, SSID e se o Wi-Fi está ligado.",
        Risk::ReadOnly,
        object_schema(
            json!({
                "interface": {
                    "type": "string",
                    "description": "Interface a inspecionar (padrão: a da rota default)."
                }
            }),
            &[],
        ),
        net_status,
    ));

    registry.add(tool(
        "wifi_scan",
        "Varre redes Wi-Fi próximas (SSID, sinal, canal, segurança). Usa `airport` quando disponível e cai para as redes preferidas.",
        Risk::ReadOnly,
        object_schema(json!({}), &[]),
        wifi_scan,
    ));

    registry.add(tool(
        "wifi_join",
        "Conecta o Mac a uma rede Wi-Fi, opcionalmente com senha.",
        Risk::Sensitive,
        object_schema(
            json!({
                "ssid": { "type": "string", "description": "Nome da rede Wi-Fi." },
                "password": { "type": "string", "description": "Senha da rede, se exigida." },
                "interface": { "type": "string", "description": "Interface Wi-Fi (padrão en0)." }
            }),
            &["ssid"],
        ),
        wifi_join,
    ));

    registry.add(tool(
        "net_ping",
        "Testa conectividade com um host e devolve perda de pacotes e RTT médio.",
        Risk::ReadOnly,
        object_schema(
            json!({
                "host": { "type": "string", "description": "Host ou IP de destino." },
                "count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "description": "Número de pacotes (padrão 4)."
                }
            }),
            &["host"],
        ),
        net_ping,
    ));

    registry.add(tool(
        "net_local_hosts",
        "Escuta o Bonjour por ~2s e lista os tipos de serviço anunciados na rede local.",
        Risk::ReadOnly,
        object_schema(json!({}), &[]),
        net_local_hosts,
    ));

    registry.add(tool(
        "net_vpn_status",
        "Lista as conexões VPN configuradas e quais estão conectadas (scutil --nc).",
        Risk::ReadOnly,
        object_schema(json!({}), &[]),
        net_vpn_status,
    ));

    registry.add(tool(
        "net_public_ip",
        "Consulta o IP público da conexão via api.ipify.org (acesso externo à rede).",
        Risk::ReadOnly,
        object_schema(json!({}), &[]),
        net_public_ip,
    ));

    registry.add(tool(
        "calendar_today",
        "Lista os eventos de hoje no Calendário, com título, horário e calendário.",
        Risk::ReadOnly,
        object_schema(json!({}), &[]),
        calendar_today,
    ));

    registry.add(tool(
        "calendar_create",
        "Cria um evento no Calendário a partir de datas ISO 8601.",
        Risk::Reversible,
        object_schema(
            json!({
                "title": { "type": "string", "description": "Título do evento." },
                "start": { "type": "string", "description": "Início em ISO 8601, ex.: 2026-11-14T09:00:00." },
                "end": { "type": "string", "description": "Fim em ISO 8601 (padrão: início + 1h)." },
                "calendar": { "type": "string", "description": "Nome do calendário (padrão: o calendário padrão)." },
                "notes": { "type": "string", "description": "Notas do evento." }
            }),
            &["title", "start"],
        ),
        calendar_create,
    ));

    registry.add(tool(
        "reminders_list",
        "Lista lembretes não concluídos, opcionalmente de uma lista específica.",
        Risk::ReadOnly,
        object_schema(
            json!({
                "list": { "type": "string", "description": "Nome da lista de lembretes." },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500,
                    "description": "Máximo de lembretes (padrão 100)."
                }
            }),
            &[],
        ),
        reminders_list,
    ));

    registry.add(tool(
        "reminder_add",
        "Cria um lembrete, opcionalmente com data de vencimento e lista.",
        Risk::Reversible,
        object_schema(
            json!({
                "title": { "type": "string", "description": "Texto do lembrete." },
                "list": { "type": "string", "description": "Lista onde criar (padrão: lista padrão)." },
                "due": { "type": "string", "description": "Vencimento em ISO 8601." }
            }),
            &["title"],
        ),
        reminder_add,
    ));

    registry.add(tool(
        "contacts_search",
        "Busca contatos por nome, organização, e-mail ou telefone.",
        Risk::ReadOnly,
        object_schema(
            json!({
                "query": { "type": "string", "description": "Texto a procurar." },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Máximo de contatos (padrão 20)."
                }
            }),
            &["query"],
        ),
        contacts_search,
    ));

    registry.add(tool(
        "home_list_scenes",
        "Lista as cenas da casa disponíveis como atalhos chamados `Casa: <nome>`.",
        Risk::ReadOnly,
        object_schema(json!({}), &[]),
        home_list_scenes,
    ));

    registry.add(tool(
        "home_run_scene",
        "Executa uma cena da casa rodando o atalho `Casa: <nome>`.",
        Risk::Reversible,
        object_schema(
            json!({
                "name": { "type": "string", "description": "Nome da cena, ex.: \"Sala cinema\"." }
            }),
            &["name"],
        ),
        home_run_scene,
    ));
}
