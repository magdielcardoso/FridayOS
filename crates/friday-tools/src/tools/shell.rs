//! Ferramentas de shell, processos, energia e volume.
//!
//! Tudo aqui delega aos binários nativos do macOS (`zsh`, `ps`, `kill`,
//! `pmset`, `caffeinate`, `osascript`, `launchctl`), então a superfície de
//! ataque é pequena e fácil de auditar. Nenhuma ferramenta entra em pânico:
//! erros de argumento viram [`ToolError::InvalidArgs`] e falhas de execução
//! viram [`ToolError::Failed`].

use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::policy::Risk;
use crate::registry::{Registry, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};

/// Teto de stdout e stderr, por stream, devolvido ao modelo.
const MAX_OUTPUT_BYTES: usize = 100 * 1024;
/// Timeout padrão de `shell_run`.
const DEFAULT_TIMEOUT_SECS: u64 = 20;
/// Timeout máximo de `shell_run`.
const MAX_TIMEOUT_SECS: u64 = 120;
/// Número máximo de linhas devolvidas por `launchctl_list`.
const MAX_LAUNCH_ROWS: usize = 200;
/// Número máximo de processos devolvidos por `process_list`.
const MAX_PROCESS_ROWS: usize = 100;

// ---------------------------------------------------------------------------
// Especificações
// ---------------------------------------------------------------------------

static SPEC_SHELL_RUN: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "shell_run",
    summary: "Executa um comando no shell de login do macOS (/bin/zsh -lc) e devolve stdout, stderr e status. Ação sensível: exige confirmação.",
    risk: Risk::Sensitive,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["command"],
        "properties": {
            "command": {
                "type": "string",
                "description": "Comando a executar, interpretado pelo zsh de login (ex.: \"df -h /\")."
            },
            "cwd": {
                "type": "string",
                "description": "Diretório de trabalho. Se omitido, usa o diretório atual do processo."
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 120,
                "default": 20,
                "description": "Tempo limite em segundos (padrão 20, máximo 120)."
            }
        }
    }),
});

static SPEC_SHELL_WHICH: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "shell_which",
    summary: "Localiza o caminho de um executável usando `which`.",
    risk: Risk::ReadOnly,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["program"],
        "properties": {
            "program": {
                "type": "string",
                "description": "Nome ou caminho do programa (ex.: \"git\")."
            }
        }
    }),
});

static SPEC_PROCESS_LIST: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "process_list",
    summary: "Lista os processos mais ativos do sistema (pid, ppid, %cpu, %mem, comando), opcionalmente filtrando por nome.",
    risk: Risk::ReadOnly,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "filter": {
                "type": "string",
                "description": "Trecho do nome/caminho do processo; a busca ignora maiúsculas e minúsculas."
            }
        }
    }),
});

static SPEC_PROCESS_KILL: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "process_kill",
    summary: "Encerra um processo por pid (SIGTERM, ou SIGKILL com force). Ação sensível: exige confirmação.",
    risk: Risk::Sensitive,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["pid"],
        "properties": {
            "pid": {
                "type": "integer",
                "minimum": 2,
                "description": "PID do processo. O pid 1 é recusado."
            },
            "force": {
                "type": "boolean",
                "default": false,
                "description": "Usa SIGKILL em vez de SIGTERM."
            }
        }
    }),
});

static SPEC_POWER_INFO: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "power_info",
    summary: "Resume energia e bateria (`pmset -g batt` + `pmset -g`).",
    risk: Risk::ReadOnly,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    }),
});

static SPEC_POWER_KEEP_AWAKE: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "power_keep_awake",
    summary: "Mantém o Mac acordado por N minutos, destacando um `caffeinate -dimsu -t`.",
    risk: Risk::Reversible,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "minutes": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1440,
                "default": 30,
                "description": "Duração em minutos (padrão 30, máximo 1440)."
            }
        }
    }),
});

static SPEC_POWER_SLEEP: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "power_sleep",
    summary: "Coloca o Mac em repouso imediatamente (`pmset sleepnow`). Ação sensível: exige confirmação.",
    risk: Risk::Sensitive,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "confirm": {
                "type": "boolean",
                "description": "Confirmação explícita do usuário para dormir agora."
            }
        }
    }),
});

static SPEC_VOLUME_GET: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "volume_get",
    summary: "Lê o volume de saída, entrada e alerta, e o estado de mudo.",
    risk: Risk::ReadOnly,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {}
    }),
});

static SPEC_VOLUME_SET: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "volume_set",
    summary: "Ajusta o volume de saída entre 0 e 100.",
    risk: Risk::Reversible,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["volume"],
        "properties": {
            "volume": {
                "type": "integer",
                "minimum": 0,
                "maximum": 100,
                "description": "Volume de saída desejado (0–100); valores fora da faixa são limitados."
            }
        }
    }),
});

static SPEC_LAUNCHCTL_LIST: LazyLock<ToolSpec> = LazyLock::new(|| ToolSpec {
    name: "launchctl_list",
    summary: "Lista os serviços do launchd (pid, status, label), opcionalmente filtrando por label.",
    risk: Risk::ReadOnly,
    args: json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "filter": {
                "type": "string",
                "description": "Trecho da label; a busca ignora maiúsculas e minúsculas."
            }
        }
    }),
});

// ---------------------------------------------------------------------------
// Registro
// ---------------------------------------------------------------------------

/// Registra todas as ferramentas deste módulo.
pub fn register(registry: &mut Registry) {
    registry.add(ShellRun);
    registry.add(ShellWhich);
    registry.add(ProcessList);
    registry.add(ProcessKill);
    registry.add(PowerInfo);
    registry.add(PowerKeepAwake);
    registry.add(PowerSleep);
    registry.add(VolumeGet);
    registry.add(VolumeSet);
    registry.add(LaunchctlList);
}

macro_rules! tool {
    ($ty:ident, $spec:ident, $call:ident) => {
        struct $ty;

        impl Tool for $ty {
            fn spec(&self) -> &ToolSpec {
                &$spec
            }

            fn call(&self, args: &Value, ctx: &ToolCtx) -> ToolResult {
                $call(args, ctx)
            }
        }
    };
}

tool!(ShellRun, SPEC_SHELL_RUN, shell_run);
tool!(ShellWhich, SPEC_SHELL_WHICH, shell_which);
tool!(ProcessList, SPEC_PROCESS_LIST, process_list);
tool!(ProcessKill, SPEC_PROCESS_KILL, process_kill);
tool!(PowerInfo, SPEC_POWER_INFO, power_info);
tool!(PowerKeepAwake, SPEC_POWER_KEEP_AWAKE, power_keep_awake);
tool!(PowerSleep, SPEC_POWER_SLEEP, power_sleep);
tool!(VolumeGet, SPEC_VOLUME_GET, volume_get);
tool!(VolumeSet, SPEC_VOLUME_SET, volume_set);
tool!(LaunchctlList, SPEC_LAUNCHCTL_LIST, launchctl_list);

// ---------------------------------------------------------------------------
// Execução de comandos
// ---------------------------------------------------------------------------

struct ShellOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
}

/// Executa `zsh -lc <command>` com timeout, redirecionando stdout/stderr para
/// arquivos temporários (evita deadlock de pipe quando a saída é grande).
fn run_shell(command: &str, cwd: Option<&Path>, timeout: Duration) -> Result<ShellOutput, ToolError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = std::env::temp_dir().join(format!(
        "friday-shell-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base).map_err(|e| {
        ToolError::failed(format!("não foi possível criar diretório temporário: {e}"))
    })?;

    let stdout_path = base.join("stdout");
    let stderr_path = base.join("stderr");
    let stdout_file = File::create(&stdout_path).map_err(|e| {
        let _ = std::fs::remove_dir_all(&base);
        ToolError::failed(format!("não foi possível criar stdout temporário: {e}"))
    })?;
    let stderr_file = File::create(&stderr_path).map_err(|e| {
        let _ = std::fs::remove_dir_all(&base);
        ToolError::failed(format!("não foi possível criar stderr temporário: {e}"))
    })?;

    let mut cmd = Command::new("/bin/zsh");
    cmd.arg("-lc")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let mut child = cmd.spawn().map_err(|e| {
        let _ = std::fs::remove_dir_all(&base);
        ToolError::failed(format!("não foi possível executar /bin/zsh: {e}"))
    })?;

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&base);
                return Err(ToolError::failed(format!(
                    "falha ao aguardar o processo: {e}"
                )));
            }
        }
    };

    let stdout = read_capped(&stdout_path);
    let stderr = read_capped(&stderr_path);
    let _ = std::fs::remove_dir_all(&base);

    Ok(ShellOutput {
        stdout,
        stderr,
        status,
        timed_out,
    })
}

/// Lê no máximo `MAX_OUTPUT_BYTES + 1` bytes de um arquivo.
fn read_capped(path: &Path) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Ok(file) = File::open(path) {
        let mut limited = file.take((MAX_OUTPUT_BYTES + 1) as u64);
        let _ = limited.read_to_end(&mut buf);
    }
    buf
}

/// Converte bytes em texto, truncando em fronteira de caractere.
fn cap_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).into_owned();
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    out.push_str("\n… [saída truncada em 100 KB]");
    out
}

/// Executa um CLI curto (sem timeout próprio) devolvendo (sucesso, stdout, stderr).
fn capture_quick(program: &str, args: &[&str]) -> Result<(bool, String, String), ToolError> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| ToolError::failed(format!("não foi possível executar {program}: {e}")))?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn dry_run_result(would_run: impl Into<String>) -> ToolResult {
    Ok(json!({
        "dry_run": true,
        "would_run": would_run.into(),
        "note": "nada foi executado",
    }))
}

/// Escapa um comando apenas para exibição legível em dry-run.
fn display_command(command: &str) -> String {
    format!("/bin/zsh -lc '{}'", command.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Validação de argumentos
// ---------------------------------------------------------------------------

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.as_str()),
        Some(Value::String(_)) => Err(ToolError::invalid(format!("`{key}` não pode ser vazio"))),
        Some(_) => Err(ToolError::invalid(format!("`{key}` precisa ser uma string"))),
        None => Err(ToolError::invalid(format!("`{key}` é obrigatório"))),
    }
}

fn optional_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolError::invalid(format!("`{key}` precisa ser uma string"))),
    }
}

fn optional_filter(args: &Value, key: &str) -> Result<Option<String>, ToolError> {
    Ok(optional_str(args, key)?
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty()))
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            ToolError::invalid(format!("`{key}` precisa ser um inteiro positivo"))
        }),
        Some(Value::String(s)) => s
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| ToolError::invalid(format!("`{key}` precisa ser um inteiro positivo"))),
        Some(_) => Err(ToolError::invalid(format!(
            "`{key}` precisa ser um inteiro positivo"
        ))),
    }
}

fn required_i64(args: &Value, key: &str) -> Result<i64, ToolError> {
    match args.get(key) {
        Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
            ToolError::invalid(format!("`{key}` precisa ser um inteiro"))
        }),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| ToolError::invalid(format!("`{key}` precisa ser um inteiro"))),
        Some(_) => Err(ToolError::invalid(format!("`{key}` precisa ser um inteiro"))),
        None => Err(ToolError::invalid(format!("`{key}` é obrigatório"))),
    }
}

fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(ToolError::invalid(format!("`{key}` precisa ser booleano"))),
    }
}

fn validate_command(command: &str) -> Result<(), ToolError> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(ToolError::invalid("`command` não pode ser vazio"));
    }
    if command.len() > 20_000 {
        return Err(ToolError::invalid(
            "`command` excede o limite de 20.000 caracteres",
        ));
    }
    if command.contains('\0') {
        return Err(ToolError::invalid("`command` contém um byte nulo"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// shell_run
// ---------------------------------------------------------------------------

fn shell_run(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let command = required_str(args, "command")?;
    validate_command(command)?;

    let cwd = optional_str(args, "cwd")?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    if let Some(dir) = &cwd {
        if !dir.is_dir() {
            return Err(ToolError::invalid(format!(
                "`cwd` não é um diretório válido: {}",
                dir.display()
            )));
        }
    }

    let timeout_secs = optional_u64(args, "timeout_secs")?
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_run": display_command(command),
            "cwd": cwd.as_ref().map(|p| p.display().to_string()),
            "timeout_secs": timeout_secs,
            "note": "nada foi executado",
        }));
    }

    let output = run_shell(command, cwd.as_deref(), Duration::from_secs(timeout_secs))?;

    let code = output.status.and_then(|s| s.code());
    let signal = output.status.and_then(|s| s.signal());
    let success = output.status.map(|s| s.success()).unwrap_or(false);

    let mut result = json!({
        "stdout": cap_text(&output.stdout),
        "stderr": cap_text(&output.stderr),
        "status": {
            "code": code,
            "signal": signal,
            "success": success,
            "timed_out": output.timed_out,
        },
        "cwd": cwd.as_ref().map(|p| p.display().to_string()),
        "timeout_secs": timeout_secs,
    });

    if output.timed_out {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "note".to_string(),
                Value::String(format!(
                    "tempo limite de {timeout_secs}s excedido; o shell foi encerrado (processos filhos podem sobreviver)"
                )),
            );
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// shell_which
// ---------------------------------------------------------------------------

fn shell_which(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let program = required_str(args, "program")?.trim().to_string();
    if program.contains('\0') {
        return Err(ToolError::invalid("`program` contém um byte nulo"));
    }
    if program.len() > 1024 {
        return Err(ToolError::invalid("`program` é longo demais"));
    }

    if ctx.dry_run {
        return dry_run_result(format!("/usr/bin/which {program}"));
    }

    let (success, stdout, stderr) = capture_quick("/usr/bin/which", &[program.as_str()])?;
    // `which` sai com 1 e stdout vazio quando o programa não existe; qualquer
    // stderr indica um problema real (permissão, PATH inválido, etc.).
    if !success && !stderr.trim().is_empty() {
        return Err(ToolError::failed(format!(
            "which falhou: {}",
            stderr.trim()
        )));
    }

    let path = stdout.lines().map(str::trim).find(|l| !l.is_empty());
    match path {
        Some(path) => Ok(json!({
            "program": program,
            "found": true,
            "path": path,
        })),
        None => Ok(json!({
            "program": program,
            "found": false,
            "path": Value::Null,
        })),
    }
}

// ---------------------------------------------------------------------------
// process_list
// ---------------------------------------------------------------------------

fn process_list(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let filter = optional_filter(args, "filter")?;

    if ctx.dry_run {
        return dry_run_result("/bin/ps -axo pid,ppid,pcpu,pmem,comm");
    }

    let (success, stdout, stderr) =
        capture_quick("/bin/ps", &["-axo", "pid,ppid,pcpu,pmem,comm"])?;
    if !success {
        return Err(ToolError::failed(format!(
            "ps falhou: {}",
            stderr.trim()
        )));
    }

    #[derive(Debug)]
    struct Proc {
        pid: i64,
        ppid: i64,
        cpu: f64,
        mem: f64,
        command: String,
    }

    let mut processes = Vec::new();
    let mut total = 0usize;

    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let (Ok(pid), Ok(ppid)) = (fields[0].parse::<i64>(), fields[1].parse::<i64>()) else {
            // Cabeçalho ("PID PPID ...") cai aqui.
            continue;
        };
        let cpu = fields[2].parse::<f64>().unwrap_or(0.0);
        let mem = fields[3].parse::<f64>().unwrap_or(0.0);
        let command = fields[4..].join(" ");

        if let Some(needle) = &filter {
            if !command.to_lowercase().contains(needle.as_str()) {
                continue;
            }
        }

        total += 1;
        processes.push(Proc {
            pid,
            ppid,
            cpu: if cpu.is_finite() { cpu } else { 0.0 },
            mem: if mem.is_finite() { mem } else { 0.0 },
            command,
        });
    }

    processes.sort_by(|a, b| {
        b.cpu
            .partial_cmp(&a.cpu)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.mem.partial_cmp(&a.mem).unwrap_or(Ordering::Equal))
    });
    processes.truncate(MAX_PROCESS_ROWS);

    let rows: Vec<Value> = processes
        .iter()
        .map(|p| {
            json!({
                "pid": p.pid,
                "ppid": p.ppid,
                "cpu_percent": p.cpu,
                "mem_percent": p.mem,
                "command": p.command,
            })
        })
        .collect();

    Ok(json!({
        "filter": filter,
        "matched": total,
        "returned": rows.len(),
        "truncated": total > rows.len(),
        "processes": rows,
    }))
}

// ---------------------------------------------------------------------------
// process_kill
// ---------------------------------------------------------------------------

fn process_kill(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let pid = required_i64(args, "pid")?;
    if pid <= 1 {
        return Err(ToolError::invalid(
            "`pid` precisa ser maior que 1 (o pid 1 é o kernel/launchd)",
        ));
    }
    if pid > i32::MAX as i64 {
        return Err(ToolError::invalid("`pid` fora da faixa válida"));
    }
    if pid == std::process::id() as i64 {
        return Err(ToolError::invalid(
            "recusado: este é o processo da própria Friday",
        ));
    }

    let force = optional_bool(args, "force")?.unwrap_or(false);
    let signal = if force { "KILL" } else { "TERM" };

    if ctx.dry_run {
        return dry_run_result(format!("/bin/kill -{signal} {pid}"));
    }

    let pid_arg = pid.to_string();
    let signal_arg = format!("-{signal}");
    let (success, _stdout, stderr) = capture_quick("/bin/kill", &[&signal_arg, &pid_arg])?;
    if !success {
        let detail = stderr.trim();
        return Err(ToolError::failed(if detail.is_empty() {
            format!("não foi possível encerrar o pid {pid}")
        } else {
            format!("não foi possível encerrar o pid {pid}: {detail}")
        }));
    }

    Ok(json!({
        "pid": pid,
        "signal": signal,
        "killed": true,
    }))
}

// ---------------------------------------------------------------------------
// power_info
// ---------------------------------------------------------------------------

struct BatteryInfo {
    source: Option<String>,
    percent: Option<i64>,
    state: Option<String>,
    time_remaining: Option<String>,
    present: bool,
}

fn parse_battery(raw: &str) -> BatteryInfo {
    let mut info = BatteryInfo {
        source: None,
        percent: None,
        state: None,
        time_remaining: None,
        present: false,
    };

    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with("Now drawing from") {
            if let Some(start) = line.find('\'') {
                if let Some(len) = line[start + 1..].find('\'') {
                    info.source = Some(line[start + 1..start + 1 + len].to_string());
                }
            }
            continue;
        }
        if !line.contains('%') {
            continue;
        }

        info.present = true;
        if let Some(pct) = parse_percent(line) {
            info.percent = Some(pct);
        }

        let parts: Vec<&str> = line.split(';').map(str::trim).collect();
        if parts.len() >= 2 && !parts[1].is_empty() {
            info.state = Some(parts[1].to_string());
        }
        for part in &parts {
            if let Some(idx) = part.find("remaining") {
                let time = part[..idx].trim();
                if !time.is_empty() {
                    info.time_remaining = Some(time.to_string());
                }
            }
        }
    }

    info
}

fn parse_percent(line: &str) -> Option<i64> {
    let idx = line.find('%')?;
    let digits: String = line[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    digits.parse::<i64>().ok()
}

/// Converte a saída de `pmset -g` em um objeto chave/valor best-effort.
fn parse_pmset_settings(raw: &str) -> Value {
    let mut map = Map::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.ends_with(':') {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 {
            continue;
        }
        let value = tokens[tokens.len() - 1];
        let key = tokens[..tokens.len() - 1].join(" ");
        map.insert(key, Value::String(value.to_string()));
    }
    Value::Object(map)
}

fn power_info(_args: &Value, ctx: &ToolCtx) -> ToolResult {
    if ctx.dry_run {
        return dry_run_result("/usr/bin/pmset -g batt && /usr/bin/pmset -g");
    }

    let (batt_ok, batt_out, batt_err) = capture_quick("/usr/bin/pmset", &["-g", "batt"])?;
    if !batt_ok {
        return Err(ToolError::failed(format!(
            "pmset -g batt falhou: {}",
            batt_err.trim()
        )));
    }

    let (settings_ok, settings_out, settings_err) =
        capture_quick("/usr/bin/pmset", &["-g"])?;
    if !settings_ok {
        return Err(ToolError::failed(format!(
            "pmset -g falhou: {}",
            settings_err.trim()
        )));
    }

    let battery = parse_battery(&batt_out);
    let on_battery = battery
        .source
        .as_deref()
        .map(|s| s.to_lowercase().contains("battery"))
        .unwrap_or(false);

    Ok(json!({
        "source": battery.source,
        "on_battery": on_battery,
        "battery_present": battery.present,
        "percent": battery.percent,
        "state": battery.state,
        "time_remaining": battery.time_remaining,
        "settings": parse_pmset_settings(&settings_out),
        "raw_batt": cap_text(batt_out.as_bytes()),
        "raw_settings": cap_text(settings_out.as_bytes()),
    }))
}

// ---------------------------------------------------------------------------
// power_keep_awake
// ---------------------------------------------------------------------------

fn power_keep_awake(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let minutes = optional_u64(args, "minutes")?.unwrap_or(30).clamp(1, 1440);
    let seconds = minutes * 60;
    let command = format!("/usr/bin/caffeinate -dimsu -t {seconds}");

    if ctx.dry_run {
        return dry_run_result(command);
    }

    let seconds_arg = seconds.to_string();
    Command::new("/usr/bin/caffeinate")
        .args(["-dimsu", "-t", seconds_arg.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            ToolError::failed(format!("não foi possível iniciar o caffeinate: {e}"))
        })?;

    Ok(json!({
        "awake_for_minutes": minutes,
        "seconds": seconds,
        "command": command,
        "spawned": true,
        "note": "processo caffeinate destacado; expira sozinho ao fim do tempo",
    }))
}

// ---------------------------------------------------------------------------
// power_sleep
// ---------------------------------------------------------------------------

fn power_sleep(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let confirm = optional_bool(args, "confirm")?.unwrap_or(false);

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "would_run": "/usr/bin/pmset sleepnow",
            "confirm": confirm,
            "note": "nada foi executado",
        }));
    }

    let (success, _stdout, stderr) = capture_quick("/usr/bin/pmset", &["sleepnow"])?;
    if !success {
        return Err(ToolError::failed(format!(
            "pmset sleepnow falhou: {}",
            stderr.trim()
        )));
    }

    Ok(json!({
        "sleeping": true,
        "confirm": confirm,
    }))
}

// ---------------------------------------------------------------------------
// volume_get / volume_set
// ---------------------------------------------------------------------------

fn volume_get(_args: &Value, ctx: &ToolCtx) -> ToolResult {
    if ctx.dry_run {
        return dry_run_result("/usr/bin/osascript -e 'get volume settings'");
    }

    let (success, stdout, stderr) =
        capture_quick("/usr/bin/osascript", &["-e", "get volume settings"])?;
    if !success {
        return Err(ToolError::failed(format!(
            "osascript falhou ao ler o volume: {}",
            stderr.trim()
        )));
    }

    let raw = stdout.trim().to_string();
    let mut output_volume = None;
    let mut input_volume = None;
    let mut alert_volume = None;
    let mut muted = None;

    for part in raw.split(',') {
        let Some((key, value)) = part.split_once(':') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value.trim();
        match key.as_str() {
            "output volume" => output_volume = value.parse::<i64>().ok(),
            "input volume" => input_volume = value.parse::<i64>().ok(),
            "alert volume" => alert_volume = value.parse::<i64>().ok(),
            "output muted" => muted = parse_bool_word(value),
            _ => {}
        }
    }

    Ok(json!({
        "output_volume": output_volume,
        "input_volume": input_volume,
        "alert_volume": alert_volume,
        "muted": muted,
        "raw": raw,
    }))
}

fn parse_bool_word(value: &str) -> Option<bool> {
    match value.trim().to_lowercase().as_str() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn volume_set(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let volume = required_i64(args, "volume")?.clamp(0, 100);
    let script = format!("set volume output volume {volume}");

    if ctx.dry_run {
        return dry_run_result(format!("/usr/bin/osascript -e '{script}'"));
    }

    let (success, _stdout, stderr) =
        capture_quick("/usr/bin/osascript", &["-e", script.as_str()])?;
    if !success {
        return Err(ToolError::failed(format!(
            "osascript falhou ao ajustar o volume: {}",
            stderr.trim()
        )));
    }

    Ok(json!({
        "output_volume": volume,
        "applied": true,
    }))
}

// ---------------------------------------------------------------------------
// launchctl_list
// ---------------------------------------------------------------------------

fn launchctl_list(args: &Value, ctx: &ToolCtx) -> ToolResult {
    let filter = optional_filter(args, "filter")?;

    if ctx.dry_run {
        return dry_run_result("/bin/launchctl list");
    }

    let (success, stdout, stderr) = capture_quick("/bin/launchctl", &["list"])?;
    if !success {
        return Err(ToolError::failed(format!(
            "launchctl list falhou: {}",
            stderr.trim()
        )));
    }

    let mut matched = 0usize;
    let mut entries = Vec::new();

    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("PID") {
            continue;
        }
        let fields: Vec<&str> = line.splitn(3, '\t').collect();
        if fields.len() < 3 {
            continue;
        }
        let label = fields[2].trim();
        if label.is_empty() {
            continue;
        }
        if let Some(needle) = &filter {
            if !label.to_lowercase().contains(needle.as_str()) {
                continue;
            }
        }

        matched += 1;
        if entries.len() >= MAX_LAUNCH_ROWS {
            continue;
        }

        let pid = fields[0].trim().parse::<i64>().ok();
        let status = fields[1].trim().parse::<i64>().ok();
        entries.push(json!({
            "pid": pid,
            "status": status,
            "label": label,
        }));
    }

    Ok(json!({
        "filter": filter,
        "matched": matched,
        "returned": entries.len(),
        "truncated": matched > entries.len(),
        "entries": entries,
    }))
}
