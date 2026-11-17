//! Sistema de decisão — motor de decisões local, tipado e determinístico.
//!
//! Camadas de "edge intelligence": watchers produzem [`Fact`]s e o motor
//! escolhe, sem chamar o modelo, o que fazer com cada evento. Tudo é avaliado
//! com tipos fortes (a AST nunca é JSON dinâmico no caminho quente), a ordem é
//! determinística e a função [`DecisionEngine::evaluate`] é total — nunca
//! faz `panic!`.
//!
//! O módulo depende apenas de `std` e `serde_json`. Nenhum token é gravado em
//! log, impresso ou devolvido em mensagens de erro.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value as JsonValue};

// ---------------------------------------------------------------------------
// AST tipada
// ---------------------------------------------------------------------------

/// Campo observável de um evento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Source,
    Text,
    App,
    Mode,
    Hour,
    Minute,
    Urgency,
    MissionStatus,
    ClipboardPreview,
}

impl Field {
    fn key(self) -> &'static str {
        match self {
            Field::Source => "source",
            Field::Text => "text",
            Field::App => "app",
            Field::Mode => "mode",
            Field::Hour => "hour",
            Field::Minute => "minute",
            Field::Urgency => "urgency",
            Field::MissionStatus => "mission_status",
            Field::ClipboardPreview => "clipboard_preview",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "source" => Some(Field::Source),
            "text" => Some(Field::Text),
            "app" => Some(Field::App),
            "mode" => Some(Field::Mode),
            "hour" => Some(Field::Hour),
            "minute" => Some(Field::Minute),
            "urgency" => Some(Field::Urgency),
            "mission_status" => Some(Field::MissionStatus),
            "clipboard_preview" => Some(Field::ClipboardPreview),
            _ => None,
        }
    }
}

/// Operador de comparação. `Gt`/`Lt`/`Gte`/`Lte` são numéricos; os demais são
/// de texto (case-insensitive). `In` espera uma lista separada por vírgulas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Contains,
    StartsWith,
    EndsWith,
    Gt,
    Lt,
    Gte,
    Lte,
    In,
    MatchesGlob,
}

impl Op {
    fn key(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Contains => "contains",
            Op::StartsWith => "starts_with",
            Op::EndsWith => "ends_with",
            Op::Gt => "gt",
            Op::Lt => "lt",
            Op::Gte => "gte",
            Op::Lte => "lte",
            Op::In => "in",
            Op::MatchesGlob => "matches_glob",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "eq" => Some(Op::Eq),
            "ne" => Some(Op::Ne),
            "contains" => Some(Op::Contains),
            "starts_with" => Some(Op::StartsWith),
            "ends_with" => Some(Op::EndsWith),
            "gt" => Some(Op::Gt),
            "lt" => Some(Op::Lt),
            "gte" => Some(Op::Gte),
            "lte" => Some(Op::Lte),
            "in" => Some(Op::In),
            "matches_glob" => Some(Op::MatchesGlob),
            _ => None,
        }
    }
}

/// Valor literal de uma condição.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Num(f64),
    Bool(bool),
}

/// Ação determinística escolhida para um evento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Hold,
    NotifyPush,
    NotifyOverlay,
    Speak,
    Escalate,
    Drop,
}

impl Decision {
    fn key(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Hold => "hold",
            Decision::NotifyPush => "notify_push",
            Decision::NotifyOverlay => "notify_overlay",
            Decision::Speak => "speak",
            Decision::Escalate => "escalate",
            Decision::Drop => "drop",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key {
            "allow" => Some(Decision::Allow),
            "hold" => Some(Decision::Hold),
            "notify_push" => Some(Decision::NotifyPush),
            "notify_overlay" => Some(Decision::NotifyOverlay),
            "speak" => Some(Decision::Speak),
            "escalate" => Some(Decision::Escalate),
            "drop" => Some(Decision::Drop),
            _ => None,
        }
    }
}

/// Uma condição atômica. `negate` inverte o resultado da comparação.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub field: Field,
    pub op: Op,
    pub value: Value,
    pub negate: bool,
}

/// Uma regra. `all` é AND, `any` é OR (vazio = verdadeiro). Regras com maior
/// `priority` vencem; em empate, o nome ordenado alfabeticamente desempata.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub name: String,
    pub priority: i32,
    pub all: Vec<Condition>,
    pub any: Vec<Condition>,
    pub decision: Decision,
    pub cooldown_secs: u64,
    pub enabled: bool,
}

/// Fato observável no instante da decisão.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub source: String,
    pub text: String,
    pub app: String,
    pub mode: String,
    pub hour: u32,
    pub minute: u32,
    pub urgency: i32,
    pub mission_status: String,
    pub clipboard_preview: String,
}

/// Resultado de uma avaliação.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionResult {
    pub decision: Decision,
    pub rule: Option<String>,
    pub reason: String,
    pub confidence: f32,
}

// ---------------------------------------------------------------------------
// Comparação de valores
// ---------------------------------------------------------------------------

fn as_num(value: &Value) -> Option<f64> {
    match value {
        Value::Num(n) => Some(*n),
        Value::Str(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Num(n) => Some(*n != 0.0),
        Value::Str(s) => match s.trim().to_lowercase().as_str() {
            "true" | "1" | "sim" | "yes" => Some(true),
            "false" | "0" | "nao" | "não" | "no" => Some(false),
            _ => None,
        },
    }
}

fn lower_str(value: &Value) -> String {
    match value {
        Value::Str(s) => s.to_lowercase(),
        Value::Num(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Bool(_), _) | (_, Value::Bool(_)) => match (as_bool(a), as_bool(b)) {
            (Some(x), Some(y)) => x == y,
            _ => lower_str(a) == lower_str(b),
        },
        (Value::Num(_), _) | (_, Value::Num(_)) => match (as_num(a), as_num(b)) {
            (Some(x), Some(y)) => {
                (x - y).abs() <= f64::EPSILON * x.abs().max(y.abs()).max(1.0)
            }
            _ => lower_str(a) == lower_str(b),
        },
        _ => lower_str(a) == lower_str(b),
    }
}

/// Glob minimalista: `*` casa qualquer sequência, `?` casa um caractere.
/// Iterativo (sem recursão e sem regex), total para qualquer entrada.
fn glob_match(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut retry = 0usize;

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            retry = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            retry += 1;
            ti = retry;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn compare(lhs: &Value, op: Op, rhs: &Value) -> bool {
    match op {
        Op::Gt | Op::Lt | Op::Gte | Op::Lte => {
            let (Some(a), Some(b)) = (as_num(lhs), as_num(rhs)) else {
                return false;
            };
            match op {
                Op::Gt => a > b,
                Op::Lt => a < b,
                Op::Gte => a >= b,
                Op::Lte => a <= b,
                _ => false,
            }
        }
        Op::Eq => values_equal(lhs, rhs),
        Op::Ne => !values_equal(lhs, rhs),
        Op::Contains => lower_str(lhs).contains(&lower_str(rhs)),
        Op::StartsWith => lower_str(lhs).starts_with(&lower_str(rhs)),
        Op::EndsWith => lower_str(lhs).ends_with(&lower_str(rhs)),
        Op::MatchesGlob => glob_match(&lower_str(lhs), &lower_str(rhs)),
        Op::In => {
            let needle = lower_str(lhs);
            lower_str(rhs)
                .split(',')
                .map(str::trim)
                .any(|part| part == needle)
        }
    }
}

fn value_for_field(field: Field, fact: &Fact) -> Value {
    match field {
        Field::Source => Value::Str(fact.source.clone()),
        Field::Text => Value::Str(fact.text.clone()),
        Field::App => Value::Str(fact.app.clone()),
        Field::Mode => Value::Str(fact.mode.clone()),
        Field::Hour => Value::Num(fact.hour as f64),
        Field::Minute => Value::Num(fact.minute as f64),
        Field::Urgency => Value::Num(fact.urgency as f64),
        Field::MissionStatus => Value::Str(fact.mission_status.clone()),
        Field::ClipboardPreview => Value::Str(fact.clipboard_preview.clone()),
    }
}

fn eval_condition(condition: &Condition, fact: &Fact) -> bool {
    let lhs = value_for_field(condition.field, fact);
    let result = compare(&lhs, condition.op, &condition.value);
    if condition.negate {
        !result
    } else {
        result
    }
}

fn reason_for(decision: Decision, rule_name: &str) -> String {
    let phrase = match decision {
        Decision::Allow => "permitido",
        Decision::Hold => "decisão segurada",
        Decision::NotifyPush => "push disparado",
        Decision::NotifyOverlay => "overlay disparado",
        Decision::Speak => "fala enfileirada",
        Decision::Escalate => "escalado para o modelo",
        Decision::Drop => "evento descartado",
    };
    format!("{} · {}", rule_name, phrase)
}

// ---------------------------------------------------------------------------
// Motor
// ---------------------------------------------------------------------------

const MAX_RULES: usize = 200;

/// Motor de decisão determinístico. `history` guarda o último disparo de cada
/// regra (por nome) para aplicar o `cooldown_secs`.
pub struct DecisionEngine {
    rules: Vec<Rule>,
    history: HashMap<String, u64>,
}

impl Default for DecisionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl DecisionEngine {
    pub fn new() -> Self {
        Self {
            rules: default_rules(),
            history: HashMap::new(),
        }
    }

    /// Adiciona (ou substitui, pelo mesmo nome) uma regra, respeitando o teto.
    pub fn add_rule(&mut self, rule: Rule) {
        if let Some(existing) = self.rules.iter_mut().find(|r| r.name == rule.name) {
            *existing = rule;
            return;
        }
        if self.rules.len() < MAX_RULES {
            self.rules.push(rule);
        }
    }

    /// Remove a regra pelo nome. Devolve `true` se algo foi removido.
    pub fn remove_rule(&mut self, name: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.name != name);
        let removed = self.rules.len() != before;
        if removed {
            self.history.remove(name);
        }
        removed
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }

    /// Avalia um fato. Total e determinística; nunca faz `panic!`.
    pub fn evaluate(&mut self, fact: &Fact, now_secs: u64) -> DecisionResult {
        let mut ordered: Vec<Rule> = self
            .rules
            .iter()
            .filter(|r| r.enabled)
            .cloned()
            .collect();
        ordered.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.name.cmp(&b.name))
        });

        for rule in ordered {
            // `all` é AND; `any` é OR (vazio casa).
            if !rule.all.iter().all(|c| eval_condition(c, fact)) {
                continue;
            }
            if !rule.any.is_empty() && !rule.any.iter().any(|c| eval_condition(c, fact)) {
                continue;
            }

            // Cooldown por regra. Escalate com urgência crítica (>= 3) bypassa.
            let bypass = matches!(rule.decision, Decision::Escalate) && fact.urgency >= 3;
            if !bypass && rule.cooldown_secs > 0 {
                if let Some(&last) = self.history.get(&rule.name) {
                    if now_secs < last.saturating_add(rule.cooldown_secs) {
                        continue;
                    }
                }
            }

            self.history.insert(rule.name.clone(), now_secs);

            let hits = rule.all.len() + rule.any.len();
            let mut confidence = 0.60 + 0.05 * hits.min(8) as f32;
            if fact.urgency >= 3 {
                confidence += 0.10;
            }
            confidence = confidence.min(0.99);

            return DecisionResult {
                decision: rule.decision,
                rule: Some(rule.name.clone()),
                reason: reason_for(rule.decision, &rule.name),
                confidence,
            };
        }

        DecisionResult {
            decision: Decision::Allow,
            rule: None,
            reason: "nenhuma regra aplicável · permitido".to_string(),
            confidence: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Regras padrão
// ---------------------------------------------------------------------------

fn cond(field: Field, op: Op, value: Value) -> Condition {
    Condition {
        field,
        op,
        value,
        negate: false,
    }
}

fn default_rules() -> Vec<Rule> {
    vec![
        // (a) VPS / erro / down / urgência crítica -> escala para o modelo.
        Rule {
            name: "critico-vps".to_string(),
            priority: 100,
            all: Vec::new(),
            any: vec![
                cond(Field::Text, Op::Contains, Value::Str("vps".into())),
                cond(Field::Text, Op::Contains, Value::Str("erro".into())),
                cond(Field::Text, Op::Contains, Value::Str("error".into())),
                cond(Field::Text, Op::Contains, Value::Str("down".into())),
                cond(Field::Urgency, Op::Gte, Value::Num(3.0)),
            ],
            decision: Decision::Escalate,
            cooldown_secs: 60,
            enabled: true,
        },
        // (b) gravação / reunião / foco -> segura a decisão.
        Rule {
            name: "presenca-ocupada".to_string(),
            priority: 50,
            all: Vec::new(),
            any: vec![cond(
                Field::Mode,
                Op::In,
                Value::Str("gravando,reuniao,meeting,foco,focus".into()),
            )],
            decision: Decision::Hold,
            cooldown_secs: 0,
            enabled: true,
        },
        // (c) rotina -> permite sem custo de modelo.
        Rule {
            name: "rotina".to_string(),
            priority: 10,
            all: vec![cond(
                Field::Source,
                Op::In,
                Value::Str("routine,rotina,background".into()),
            )],
            any: Vec::new(),
            decision: Decision::Allow,
            cooldown_secs: 0,
            enabled: true,
        },
    ]
}

// ---------------------------------------------------------------------------
// Persistência (conversão manual de serde_json::Value, sem derive)
// ---------------------------------------------------------------------------

fn value_to_json(value: &Value) -> JsonValue {
    match value {
        Value::Str(s) => json!(s),
        Value::Num(n) => json!(n),
        Value::Bool(b) => json!(b),
    }
}

fn value_from_json(value: &JsonValue) -> Option<Value> {
    match value {
        JsonValue::String(s) => Some(Value::Str(s.clone())),
        JsonValue::Number(n) => n.as_f64().map(Value::Num),
        JsonValue::Bool(b) => Some(Value::Bool(*b)),
        _ => None,
    }
}

fn condition_to_json(condition: &Condition) -> JsonValue {
    json!({
        "field": condition.field.key(),
        "op": condition.op.key(),
        "value": value_to_json(&condition.value),
        "negate": condition.negate,
    })
}

fn condition_from_json(value: &JsonValue) -> Option<Condition> {
    let field = Field::from_key(value.get("field")?.as_str()?)?;
    let op = Op::from_key(value.get("op")?.as_str()?)?;
    let literal = value_from_json(value.get("value")?)?;
    let negate = value.get("negate").and_then(JsonValue::as_bool).unwrap_or(false);
    Some(Condition {
        field,
        op,
        value: literal,
        negate,
    })
}

fn conditions_from_json(value: Option<&JsonValue>) -> Vec<Condition> {
    let Some(array) = value else { return Vec::new() };
    let Some(items) = array.as_array() else {
        return Vec::new();
    };
    items.iter().filter_map(condition_from_json).collect()
}

fn rule_to_json(rule: &Rule) -> JsonValue {
    json!({
        "name": rule.name,
        "priority": rule.priority,
        "all": rule.all.iter().map(condition_to_json).collect::<Vec<_>>(),
        "any": rule.any.iter().map(condition_to_json).collect::<Vec<_>>(),
        "decision": rule.decision.key(),
        "cooldown_secs": rule.cooldown_secs,
        "enabled": rule.enabled,
    })
}

fn rule_from_json(value: &JsonValue) -> Option<Rule> {
    let name = value.get("name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let priority = value
        .get("priority")
        .and_then(JsonValue::as_i64)
        .unwrap_or(0) as i32;
    let decision = value
        .get("decision")
        .and_then(JsonValue::as_str)
        .and_then(Decision::from_key)
        .unwrap_or(Decision::Allow);
    let cooldown_secs = value
        .get("cooldown_secs")
        .and_then(JsonValue::as_u64)
        .unwrap_or(0);
    let enabled = value
        .get("enabled")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);
    Some(Rule {
        name,
        priority,
        all: conditions_from_json(value.get("all")),
        any: conditions_from_json(value.get("any")),
        decision,
        cooldown_secs,
        enabled,
    })
}

/// Carrega as regras de `path`. Arquivo ausente ou corrompido -> regras padrão.
pub fn load(path: &Path) -> DecisionEngine {
    let mut engine = DecisionEngine {
        rules: Vec::new(),
        history: HashMap::new(),
    };

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => {
            engine.rules = default_rules();
            return engine;
        }
    };
    let root: JsonValue = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            engine.rules = default_rules();
            return engine;
        }
    };
    let Some(items) = root.get("rules").and_then(JsonValue::as_array) else {
        engine.rules = default_rules();
        return engine;
    };

    engine.rules = items
        .iter()
        .filter_map(rule_from_json)
        .take(MAX_RULES)
        .collect();
    engine
}

impl DecisionEngine {
    /// Salva as regras em `path` (JSON legível). Cria o diretório pai.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let rules: Vec<JsonValue> = self.rules.iter().map(rule_to_json).collect();
        let payload = json!({ "rules": rules });
        let text = serde_json::to_string_pretty(&payload)
            .map_err(|_| "falha ao serializar regras".to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|_| "falha ao criar diretório de regras".to_string())?;
        }
        std::fs::write(path, text).map_err(|_| "falha ao gravar regras".to_string())
    }
}

// ---------------------------------------------------------------------------
// Token de escalonamento remoto (opcional, nunca exibido)
// ---------------------------------------------------------------------------

const MAX_TOKEN_CHARS: usize = 4096;
static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Caminho do token: `~/Library/Application Support/FridayOS/decision-token.json`.
pub fn token_path() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("FridayOS")
        .join("decision-token.json")
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Escreve o token de forma atômica (temp + rename) com 0600 no unix.
/// O conteúdo do token nunca entra nas mensagens de erro.
fn write_token_file(path: &Path, token: &str) -> Result<(), String> {
    if token.chars().count() > MAX_TOKEN_CHARS {
        return Err("token muito longo".to_string());
    }

    let parent = path
        .parent()
        .ok_or_else(|| "caminho de token inválido".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|_| "falha ao criar diretório de token".to_string())?;

    let payload = json!({ "token": token, "saved_at": now_nanos() }).to_string();
    let unique = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".decision-token-{}-{}-{}.tmp",
        std::process::id(),
        now_nanos(),
        unique
    ));

    let write_result = (|| -> Result<(), String> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|_| "falha ao criar arquivo de token".to_string())?;
            file.write_all(payload.as_bytes())
                .map_err(|_| "falha ao gravar token".to_string())?;
            // Garante 0600 mesmo que o umask tenha interferido na criação.
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                .map_err(|_| "falha ao proteger token".to_string())?;
            file.sync_all()
                .map_err(|_| "falha ao sincronizar token".to_string())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, payload.as_bytes())
                .map_err(|_| "falha ao gravar token".to_string())?;
        }
        std::fs::rename(&tmp, path).map_err(|_| "falha ao instalar token".to_string())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

fn read_token_file(path: &Path) -> Result<String, String> {
    let text =
        std::fs::read_to_string(path).map_err(|_| "token não encontrado".to_string())?;
    let root: JsonValue =
        serde_json::from_str(&text).map_err(|_| "token inválido".to_string())?;
    root.get("token")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| "token inválido".to_string())
}

/// Grava o token no caminho padrão. Nunca imprime, loga ou devolve o valor.
pub fn save_token(token: &str) -> Result<(), String> {
    write_token_file(&token_path(), token)
}

/// Lê o token do caminho padrão. Erros nunca contêm o valor do token.
pub fn read_token() -> Result<String, String> {
    read_token_file(&token_path())
}

/// `true` quando existe um token não vazio no caminho padrão.
pub fn has_token() -> bool {
    read_token().map(|token| !token.is_empty()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "friday-decision-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn fact(text: &str) -> Fact {
        Fact {
            source: "watcher".into(),
            text: text.into(),
            app: "Finder".into(),
            mode: "normal".into(),
            hour: 12,
            minute: 30,
            urgency: 1,
            mission_status: "idle".into(),
            clipboard_preview: String::new(),
        }
    }

    fn rule(name: &str, priority: i32, decision: Decision, cooldown: u64) -> Rule {
        Rule {
            name: name.into(),
            priority,
            all: vec![cond(Field::Text, Op::Contains, Value::Str("vps".into()))],
            any: Vec::new(),
            decision,
            cooldown_secs: cooldown,
            enabled: true,
        }
    }

    fn empty_engine() -> DecisionEngine {
        DecisionEngine {
            rules: Vec::new(),
            history: HashMap::new(),
        }
    }

    #[test]
    fn precedence_by_priority() {
        let mut engine = empty_engine();
        engine.add_rule(rule("baixa", 1, Decision::Drop, 0));
        engine.add_rule(rule("alta", 10, Decision::Escalate, 0));
        let result = engine.evaluate(&fact("vps caiu"), 100);
        assert_eq!(result.decision, Decision::Escalate);
        assert_eq!(result.rule.as_deref(), Some("alta"));
        assert!(result.confidence > 0.0);
    }

    #[test]
    fn disabled_rule_is_ignored() {
        let mut engine = empty_engine();
        let mut r = rule("off", 10, Decision::Drop, 0);
        r.enabled = false;
        engine.add_rule(r);
        let result = engine.evaluate(&fact("vps caiu"), 100);
        assert_eq!(result.decision, Decision::Allow);
        assert_eq!(result.rule, None);
        assert_eq!(result.confidence, 0.0);
    }

    #[test]
    fn cooldown_blocks_then_expires() {
        let mut engine = empty_engine();
        engine.add_rule(rule("cd", 5, Decision::NotifyPush, 60));
        assert_eq!(engine.evaluate(&fact("vps"), 100).decision, Decision::NotifyPush);
        assert_eq!(engine.evaluate(&fact("vps"), 120).decision, Decision::Allow);
        assert_eq!(engine.evaluate(&fact("vps"), 161).decision, Decision::NotifyPush);
    }

    #[test]
    fn escalate_with_high_urgency_bypasses_cooldown() {
        let mut engine = empty_engine();
        engine.add_rule(rule("esc", 5, Decision::Escalate, 60));
        assert_eq!(engine.evaluate(&fact("vps"), 100).decision, Decision::Escalate);
        let mut critical = fact("vps");
        critical.urgency = 3;
        assert_eq!(engine.evaluate(&critical, 101).decision, Decision::Escalate);
    }

    #[test]
    fn clear_history_resets_cooldown() {
        let mut engine = empty_engine();
        engine.add_rule(rule("cd", 5, Decision::Hold, 60));
        assert_eq!(engine.evaluate(&fact("vps"), 100).decision, Decision::Hold);
        engine.clear_history();
        assert_eq!(engine.evaluate(&fact("vps"), 101).decision, Decision::Hold);
    }

    #[test]
    fn glob_star_and_question() {
        assert!(glob_match("vps-01", "vps-??"));
        assert!(glob_match("error-critical", "*critical*"));
        assert!(glob_match("abc", "a?c"));
        assert!(!glob_match("vps-1", "vps-??"));
        assert!(!glob_match("abcd", "a?c"));
    }

    #[test]
    fn numeric_coercion_from_strings() {
        let mut text = fact("42");
        let gt = cond(Field::Text, Op::Gt, Value::Num(40.0));
        let lt = cond(Field::Text, Op::Lt, Value::Num(40.0));
        assert!(eval_condition(&gt, &text));
        assert!(!eval_condition(&lt, &text));
        text.text = "não-numero".into();
        assert!(!eval_condition(&gt, &text));
        let negated_lt = Condition {
            negate: true,
            ..cond(Field::Text, Op::Lt, Value::Num(40.0))
        };
        assert!(eval_condition(&negated_lt, &text));
    }

    #[test]
    fn string_ops_are_lowercase() {
        let f = fact("VPS.em.ALTA");
        assert!(eval_condition(
            &cond(Field::Text, Op::StartsWith, Value::Str("vps".into())),
            &f
        ));
        assert!(eval_condition(
            &cond(Field::Text, Op::EndsWith, Value::Str("alta".into())),
            &f
        ));
        assert!(eval_condition(
            &cond(Field::Mode, Op::In, Value::Str("normal,gravando".into())),
            &f
        ));
    }

    #[test]
    fn persistence_round_trip() {
        let dir = tmp_dir();
        let path = dir.join("rules.json");
        let mut engine = empty_engine();
        engine.add_rule(rule("critico", 7, Decision::Escalate, 30));
        engine.save(&path).expect("save");
        let loaded = load(&path);
        assert_eq!(loaded.rules(), engine.rules());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let dir = tmp_dir();
        let path = dir.join("broken.json");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path).rules().len(), 3);
        assert_eq!(load(&dir.join("inexistente.json")).rules().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_file_permissions_and_no_leak_in_errors() {
        let dir = tmp_dir();
        let path = dir.join("token.json");
        let secret = "sk-super-secreto-NAO-EXIBIR-123";

        write_token_file(&path, secret).expect("write token");
        assert_eq!(read_token_file(&path).unwrap(), secret);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let long = "x".repeat(4097);
        let err = write_token_file(&path, &long).unwrap_err();
        assert!(!err.contains(secret));
        let err = read_token_file(&dir.join("ausente.json")).unwrap_err();
        assert!(!err.contains(secret));
        assert!(!err.contains("x"));

        assert_eq!(
            token_path().file_name().and_then(|n| n.to_str()),
            Some("decision-token.json")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
