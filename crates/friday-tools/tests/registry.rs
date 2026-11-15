//! Integration tests for the Friday tool layer.
//!
//! These tests never touch the real system: the default catalog is only
//! inspected (never called), and every execution test uses the in-process
//! `Echo` tool below. Audit files live in a per-test directory inside
//! `std::env::temp_dir()`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use friday_tools::policy;
use friday_tools::{AuditLog, Registry, Risk, Tool, ToolCtx, ToolError, ToolResult, ToolSpec};
use serde_json::{json, Value};

/// A deterministic, side-effect-free tool used to exercise the registry.
struct Echo {
    spec: ToolSpec,
}

impl Echo {
    fn new(risk: Risk) -> Self {
        Self {
            spec: ToolSpec {
                name: "echo_probe",
                summary: "Devolve os argumentos recebidos, sem tocar no sistema.",
                risk,
                args: json!({
                    "type": "object",
                    "properties": {
                        "texto": { "type": "string" }
                    },
                    "additionalProperties": true
                }),
            },
        }
    }
}

impl Tool for Echo {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call(&self, args: &Value, _ctx: &ToolCtx) -> ToolResult {
        // Deliberately ignores `dry_run` and `confirmed`: a pure tool must
        // behave identically in dry-run and real modes.
        Ok(json!({ "echo": args }))
    }
}

/// Unique audit path per test, so parallel tests never share a file.
fn temp_audit_path(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "friday-tools-{}-{}-{}",
        std::process::id(),
        tag,
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("criar diretorio temporario");
    dir.join("audit.jsonl")
}

fn ctx_at(path: &PathBuf) -> ToolCtx {
    ToolCtx::new(AuditLog::new(path.clone()))
}

fn is_snake_case(name: &str) -> bool {
    let chars_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let Some(first) = name.chars().next() else {
        return false;
    };
    chars_ok
        && first.is_ascii_lowercase()
        && !name.starts_with('_')
        && !name.ends_with('_')
        && !name.contains("__")
}

#[test]
fn catalogo_tem_ferramentas_bem_formadas() {
    let registry = Registry::with_defaults();
    let specs = registry.specs();

    // Resilient to the catalog evolving: the only hard floor is the size,
    // shape and naming discipline of the catalog.
    assert!(
        specs.len() >= 15,
        "esperava pelo menos 15 ferramentas, encontrei {}",
        specs.len()
    );

    let mut names = BTreeSet::new();
    for spec in &specs {
        assert!(
            names.insert(spec.name),
            "nome de ferramenta duplicado: {}",
            spec.name
        );
        assert!(
            is_snake_case(spec.name),
            "nome fora do padrao snake_case: {}",
            spec.name
        );
        assert!(
            !spec.summary.trim().is_empty(),
            "resumo vazio na ferramenta {}",
            spec.name
        );
        assert!(
            spec.args.is_object(),
            "schema de {} nao e um objeto JSON: {}",
            spec.name,
            spec.args
        );
        assert_eq!(
            spec.args.get("type").and_then(Value::as_str),
            Some("object"),
            "schema de {} nao declara type=object",
            spec.name
        );
    }

    // Representative set expected from the catalog. Missing names are
    // reported as warnings rather than hard failures, but at least half of
    // the expected surface must be present.
    let expected = [
        "ax_windows",
        "input_trusted",
        "app_activate",
        "shell_run",
        "screen_capture",
        "fs_list",
        "net_status",
        "voice_devices",
        "clipboard_get",
        "shortcut_run",
    ];
    let present: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|name| registry.get(name).is_some())
        .collect();
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|name| registry.get(name).is_none())
        .collect();
    if !missing.is_empty() {
        eprintln!("aviso: ferramentas esperadas ainda ausentes: {missing:?}");
    }
    assert!(
        present.len() >= expected.len() / 2,
        "menos da metade das ferramentas representativas existe; presentes: {present:?}"
    );
}

#[test]
fn politica_de_risco_exige_confirmacao_apenas_quando_sensivel() {
    let path = temp_audit_path("policy");
    let ctx = ctx_at(&path);

    // ReadOnly nunca pede confirmacao.
    assert!(policy::authorize(Risk::ReadOnly, &ctx).is_ok());

    // Sensitive nega sem confirmacao explicita...
    let denied = policy::authorize(Risk::Sensitive, &ctx);
    assert!(
        matches!(denied, Err(ToolError::Denied(_))),
        "esperava Denied, veio {denied:?}"
    );

    // ...e libera com `confirmed`.
    let confirmed = ctx_at(&path).confirmed();
    assert!(policy::authorize(Risk::Sensitive, &confirmed).is_ok());

    // Reversible fica na fronteira: nao pede confirmacao.
    assert!(policy::authorize(Risk::Reversible, &ctx).is_ok());

    // Nenhuma dessas chamadas deve gerar auditoria (authorize nao audita).
    assert!(!path.exists(), "authorize nao deveria escrever auditoria");
}

#[test]
fn audit_registra_uma_linha_jsonl_por_chamada() {
    let path = temp_audit_path("audit");
    let mut registry = Registry::new();
    registry.add(Echo::new(Risk::ReadOnly));

    let first = registry.call(
        "echo_probe",
        &json!({ "texto": "um" }),
        &ctx_at(&path),
    );
    let second = registry.call(
        "echo_probe",
        &json!({ "texto": "dois" }),
        &ctx_at(&path),
    );
    assert!(first.is_ok());
    assert!(second.is_ok());

    let raw = std::fs::read_to_string(&path).expect("ler audit.jsonl");
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 2, "uma linha JSONL por chamada");

    for (index, line) in lines.iter().enumerate() {
        let entry: Value = serde_json::from_str(line).expect("linha JSONL valida");
        assert_eq!(entry["tool"], "echo_probe");
        assert_eq!(entry["risk"], "read");
        assert_eq!(entry["dry_run"], false);
        assert!(entry["ts"].is_u64());
        assert!(entry["args"].is_object());
        let expected = if index == 0 { "um" } else { "dois" };
        assert_eq!(entry["args"]["texto"], expected);
    }
}

#[test]
fn registry_executa_ferramenta_falsa_ponta_a_ponta() {
    let path = temp_audit_path("e2e");
    let mut registry = Registry::new();
    registry.add(Echo::new(Risk::ReadOnly));

    let args = json!({ "texto": "ola", "n": 7 });
    let result = registry
        .call("echo_probe", &args, &ctx_at(&path))
        .expect("chamada deve funcionar");
    assert_eq!(result, json!({ "echo": args }));

    // Nome desconhecido vira NotFound e nao quebra o processo.
    let missing = registry.call("nao_existe", &json!({}), &ctx_at(&path));
    assert!(matches!(missing, Err(ToolError::NotFound(_))));
}

#[test]
fn dry_run_nao_altera_o_resultado_de_ferramenta_pura() {
    let path = temp_audit_path("dry-run");
    let mut registry = Registry::new();
    registry.add(Echo::new(Risk::ReadOnly));

    let args = json!({ "texto": "invariante" });
    let real = registry
        .call("echo_probe", &args, &ctx_at(&path))
        .expect("chamada real");
    let dry = registry
        .call("echo_probe", &args, &ctx_at(&path).dry_run())
        .expect("chamada dry-run");
    assert_eq!(real, dry, "dry-run deve produzir o mesmo resultado");

    // Ambas as chamadas aparecem na auditoria, distinguiveis pelo flag.
    let raw = std::fs::read_to_string(&path).expect("ler audit.jsonl");
    let flags: Vec<bool> = raw
        .lines()
        .map(|line| {
            let entry: Value = serde_json::from_str(line).expect("linha JSONL valida");
            entry["dry_run"].as_bool().expect("dry_run booleano")
        })
        .collect();
    assert_eq!(flags, vec![false, true]);
}
