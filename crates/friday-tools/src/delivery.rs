//! Política de entrega: decidir *se* um evento merece atenção e *por onde* avisar.
//!
//! Um watcher/routine não deve abrir um overlay antes do modelo avaliar o evento.
//! O fluxo correto é: `decide` primeiro (com base na urgência, no contexto de
//! presença e nas regras do usuário) e só então acionar o canal escolhido.
//!
//! O módulo é puro e não depende de nada além de `std` e `serde_json`. A política
//! padrão é um *fallback* em código; `rules()` contém apenas regras explícitas do
//! usuário, que sempre têm precedência (a primeira que casa vence).

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Onde (ou se) o evento é entregue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Voz nativa (Siri) — só no Mac, quando não atrapalha.
    Voice,
    /// Janela de overlay no Mac.
    Overlay,
    /// Web Push no iPhone pareado.
    Push,
    /// Fila: não notificar agora, guardar para depois.
    Hold,
    /// Registrar no log, nunca notificar.
    LogOnly,
}

impl Channel {
    pub fn label(self) -> &'static str {
        match self {
            Channel::Voice => "voice",
            Channel::Overlay => "overlay",
            Channel::Push => "push",
            Channel::Hold => "hold",
            Channel::LogOnly => "log_only",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match normalize(value).as_str() {
            "voice" | "voz" | "speak" | "speech" => Some(Channel::Voice),
            "overlay" | "janela" | "on_screen" | "screen" => Some(Channel::Overlay),
            "push" | "iphone" | "phone" | "notification" => Some(Channel::Push),
            "hold" | "queue" | "segurar" | "later" => Some(Channel::Hold),
            "log_only" | "log" | "so_registro" | "só_registro" => Some(Channel::LogOnly),
            _ => None,
        }
    }
}

/// Quanto o evento importa. A ordem é significativa: `Critical` é o máximo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Background = 0,
    Routine = 1,
    ActionNeeded = 2,
    Critical = 3,
}

impl Urgency {
    pub fn label(self) -> &'static str {
        match self {
            Urgency::Background => "background",
            Urgency::Routine => "routine",
            Urgency::ActionNeeded => "action_needed",
            Urgency::Critical => "critical",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match normalize(value).as_str() {
            "background" | "fundo" | "info" => Some(Urgency::Background),
            "routine" | "rotina" | "normal" => Some(Urgency::Routine),
            "action_needed" | "action" | "decision" | "decisao" | "decisão" => {
                Some(Urgency::ActionNeeded)
            }
            "critical" | "critico" | "crítico" | "urgent" => Some(Urgency::Critical),
            _ => None,
        }
    }
}

/// Onde o usuário está quando o evento acontece.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceContext {
    AtMac,
    ScreenSharing,
    Recording,
    Away,
    Driving,
    Meeting,
    Focus,
}

impl PresenceContext {
    pub fn label(self) -> &'static str {
        match self {
            PresenceContext::AtMac => "at_mac",
            PresenceContext::ScreenSharing => "screen_sharing",
            PresenceContext::Recording => "recording",
            PresenceContext::Away => "away",
            PresenceContext::Driving => "driving",
            PresenceContext::Meeting => "meeting",
            PresenceContext::Focus => "focus",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match normalize(value).as_str() {
            "at_mac" | "mac" | "desk" | "present" => Some(PresenceContext::AtMac),
            "screen_sharing" | "sharing" | "screenshare" | "compartilhando" => {
                Some(PresenceContext::ScreenSharing)
            }
            "recording" | "gravando" | "screen_recording" => Some(PresenceContext::Recording),
            "away" | "longe" | "ausente" => Some(PresenceContext::Away),
            "driving" | "dirigindo" | "car" => Some(PresenceContext::Driving),
            "meeting" | "reuniao" | "reunião" | "call" => Some(PresenceContext::Meeting),
            "focus" | "foco" | "dnd" | "dont_disturb" => Some(PresenceContext::Focus),
            _ => None,
        }
    }
}

/// Normaliza rótulos persistidos: minúsculas, `-`/espaços viram `_`.
fn normalize(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .replace(['-', ' '], "_")
}

/// Um evento que pode (ou não) merecer uma notificação.
#[derive(Debug, Clone)]
pub struct DeliveryEvent {
    pub source: String,
    pub text: String,
    pub urgency: Urgency,
    pub wants_decision: bool,
    /// Milissegundos desde o epoch. Use [`now_ms`] ao preencher eventos reais;
    /// é comparado com a janela de silêncio para que `decide` seja puro.
    pub timestamp: u64,
}

/// Regra explícita do usuário. A primeira que casa vence.
#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    /// Substring (case-insensitive) da origem; vazio casa qualquer origem.
    pub source_pattern: String,
    /// Substring (case-insensitive) do texto; vazio casa qualquer texto.
    pub text_pattern: String,
    pub urgency_min: Urgency,
    /// `None` casa qualquer contexto.
    pub ctx: Option<PresenceContext>,
    pub channel: Channel,
    pub enabled: bool,
}

/// Política de entrega: regras do usuário + contexto + janela de silêncio.
#[derive(Debug, Clone)]
pub struct DeliveryPolicy {
    rules: Vec<Rule>,
    context: PresenceContext,
    quiet_until_ms: u64,
}

impl Default for DeliveryPolicy {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            context: PresenceContext::AtMac,
            quiet_until_ms: 0,
        }
    }
}

impl DeliveryPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn context(&self) -> PresenceContext {
        self.context
    }

    pub fn quiet_until_ms(&self) -> u64 {
        self.quiet_until_ms
    }

    /// Apenas as regras explícitas do usuário (os defaults são um fallback).
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn set_context(&mut self, context: PresenceContext) {
        self.context = context;
    }

    pub fn add_rule(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    pub fn remove_rule(&mut self, name: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|rule| rule.name != name);
        self.rules.len() != before
    }

    /// Silencia notificações não-críticas pelos próximos `ms` milissegundos.
    /// Críticos sempre atravessam a janela.
    pub fn hold_for_ms(&mut self, ms: u64) {
        let until = now_ms().saturating_add(ms);
        self.quiet_until_ms = self.quiet_until_ms.max(until);
    }

    /// Escolha determinística do canal, dado o estado e o evento.
    pub fn decide(&self, event: &DeliveryEvent) -> Channel {
        self.choose(event).0
    }

    /// Motivo curto em português, para logs e UI.
    pub fn explain(&self, event: &DeliveryEvent) -> String {
        self.choose(event).1
    }

    fn choose(&self, event: &DeliveryEvent) -> (Channel, String) {
        if let Some(rule) = self.rules.iter().find(|rule| self.rule_matches(rule, event)) {
            let reason = format!("regra {}: {}", rule.name, channel_reason(rule.channel));
            return (rule.channel, reason);
        }
        self.fallback(event)
    }

    fn rule_matches(&self, rule: &Rule, event: &DeliveryEvent) -> bool {
        if !rule.enabled || event.urgency < rule.urgency_min {
            return false;
        }
        if rule.ctx.is_some_and(|ctx| ctx != self.context) {
            return false;
        }
        pattern_matches(&rule.source_pattern, &event.source)
            && pattern_matches(&rule.text_pattern, &event.text)
    }

    fn effective_urgency(&self, event: &DeliveryEvent) -> Urgency {
        if event.wants_decision && event.urgency < Urgency::ActionNeeded {
            Urgency::ActionNeeded
        } else {
            event.urgency
        }
    }

    /// Defaults sensatos quando nenhuma regra explícita casa.
    fn fallback(&self, event: &DeliveryEvent) -> (Channel, String) {
        let urgency = self.effective_urgency(event);

        if urgency == Urgency::Background {
            return (Channel::LogOnly, "plano de fundo: só registro".into());
        }

        // A janela de silêncio nunca suprime um crítico.
        if event.timestamp < self.quiet_until_ms && urgency != Urgency::Critical {
            return (Channel::Hold, "janela de silêncio: segurando".into());
        }

        match urgency {
            Urgency::Critical => match self.context {
                PresenceContext::AtMac => (Channel::Overlay, "crítico: overlay no Mac".into()),
                _ => (Channel::Push, "crítico: push no iPhone".into()),
            },
            Urgency::ActionNeeded => match self.context {
                PresenceContext::AtMac => (
                    Channel::Overlay,
                    "decisão necessária: overlay no Mac".into(),
                ),
                PresenceContext::Away
                | PresenceContext::Driving
                | PresenceContext::ScreenSharing => (
                    Channel::Push,
                    "decisão necessária: push no iPhone".into(),
                ),
                PresenceContext::Recording => {
                    (Channel::Hold, "gravando: segurando decisão".into())
                }
                PresenceContext::Meeting => {
                    (Channel::Hold, "em reunião: segurando decisão".into())
                }
                PresenceContext::Focus => (Channel::Hold, "foco: segurando decisão".into()),
            },
            Urgency::Routine => match self.context {
                PresenceContext::AtMac => (Channel::Voice, "no Mac: voz".into()),
                PresenceContext::Recording => (Channel::Hold, "gravando: segurando".into()),
                PresenceContext::Meeting => (Channel::Hold, "em reunião: segurando".into()),
                PresenceContext::Focus => (Channel::Hold, "foco: segurando".into()),
                PresenceContext::Away => (Channel::Hold, "longe do Mac: segurando".into()),
                PresenceContext::Driving => (Channel::Hold, "dirigindo: segurando".into()),
                PresenceContext::ScreenSharing => {
                    (Channel::Hold, "compartilhando tela: segurando".into())
                }
            },
            Urgency::Background => unreachable!("tratado acima"),
        }
    }

    fn to_value(&self) -> Value {
        json!({
            "version": 1,
            "context": self.context.label(),
            "quiet_until_ms": self.quiet_until_ms,
            "rules": self.rules.iter().map(|rule| json!({
                "name": rule.name,
                "source_pattern": rule.source_pattern,
                "text_pattern": rule.text_pattern,
                "urgency_min": rule.urgency_min.label(),
                "ctx": rule.ctx.map(PresenceContext::label),
                "channel": rule.channel.label(),
                "enabled": rule.enabled,
            })).collect::<Vec<_>>(),
        })
    }

    fn from_value(value: &Value) -> Option<Self> {
        let context = value
            .get("context")
            .and_then(Value::as_str)
            .and_then(PresenceContext::parse)
            .unwrap_or(PresenceContext::AtMac);
        let quiet_until_ms = value
            .get("quiet_until_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let rules = value
            .get("rules")?
            .as_array()?
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?;
                let channel = item
                    .get("channel")
                    .and_then(Value::as_str)
                    .and_then(Channel::parse)?;
                let urgency_min = item
                    .get("urgency_min")
                    .and_then(Value::as_str)
                    .and_then(Urgency::parse)
                    .unwrap_or(Urgency::Routine);
                let ctx = item
                    .get("ctx")
                    .and_then(Value::as_str)
                    .and_then(PresenceContext::parse);
                Some(Rule {
                    name: name.to_string(),
                    source_pattern: item
                        .get("source_pattern")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    text_pattern: item
                        .get("text_pattern")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    urgency_min,
                    ctx,
                    channel,
                    enabled: item
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                })
            })
            .collect();
        Some(Self {
            rules,
            context,
            quiet_until_ms,
        })
    }

    /// Grava a política em `path` (cria o diretório pai se necessário).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("criar diretório {}: {err}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.to_value())
            .map_err(|err| format!("serializar política: {err}"))?;
        std::fs::write(path, bytes)
            .map_err(|err| format!("gravar {}: {err}", path.display()))
    }
}

/// Carrega a política de `path`. Arquivo ausente ou corrompido => padrão.
pub fn load(path: &Path) -> DeliveryPolicy {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| DeliveryPolicy::from_value(&value))
        .unwrap_or_default()
}

fn pattern_matches(pattern: &str, value: &str) -> bool {
    let needle = pattern.trim();
    if needle.is_empty() {
        return true;
    }
    value.to_lowercase().contains(&needle.to_lowercase())
}

fn channel_reason(channel: Channel) -> &'static str {
    match channel {
        Channel::Voice => "voz no Mac",
        Channel::Overlay => "overlay no Mac",
        Channel::Push => "push no iPhone",
        Channel::Hold => "segurando",
        Channel::LogOnly => "só registro",
    }
}

/// Agora em milissegundos desde o epoch — para preencher `DeliveryEvent::timestamp`.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_path() -> PathBuf {
        let id = NEXT_ID.fetch_add(1, AtomicOrdering::SeqCst);
        std::env::temp_dir().join(format!(
            "friday-delivery-test-{}-{}.json",
            std::process::id(),
            id
        ))
    }

    fn event(source: &str, text: &str, urgency: Urgency) -> DeliveryEvent {
        DeliveryEvent {
            source: source.into(),
            text: text.into(),
            urgency,
            wants_decision: false,
            timestamp: now_ms(),
        }
    }

    fn rule(name: &str, source: &str, text: &str, urgency_min: Urgency, channel: Channel) -> Rule {
        Rule {
            name: name.into(),
            source_pattern: source.into(),
            text_pattern: text.into(),
            urgency_min,
            ctx: None,
            channel,
            enabled: true,
        }
    }

    #[test]
    fn recording_suppresses_routine() {
        let mut policy = DeliveryPolicy::default();
        policy.set_context(PresenceContext::Recording);
        let ev = event("watcher", "build terminou", Urgency::Routine);
        assert_eq!(policy.decide(&ev), Channel::Hold);
        assert!(policy.explain(&ev).contains("gravando"));
    }

    #[test]
    fn vps_down_pushes_while_away() {
        let mut policy = DeliveryPolicy::default();
        policy.set_context(PresenceContext::Away);
        let ev = event("vps", "VPS CAIU", Urgency::Critical);
        assert_eq!(policy.decide(&ev), Channel::Push);
        assert!(policy.explain(&ev).contains("push no iPhone"));

        // Um evento que pede decisão e chega com o usuário longe também vai pro iPhone.
        let needs = event("webhook", "aprovar deploy", Urgency::ActionNeeded);
        assert_eq!(policy.decide(&needs), Channel::Push);
    }

    #[test]
    fn user_rule_sends_critical_vps_push_while_recording() {
        let mut policy = DeliveryPolicy::default();
        policy.set_context(PresenceContext::Recording);
        let mut vps = rule("vps-down", "VPS", "caiu", Urgency::Critical, Channel::Push);
        vps.ctx = Some(PresenceContext::Recording);
        policy.add_rule(vps);

        let ev = event("VPS-Prod", "vps caiu", Urgency::Critical);
        assert_eq!(policy.decide(&ev), Channel::Push);
        assert!(policy.explain(&ev).starts_with("regra vps-down"));

        // A regra só vale no contexto declarado.
        policy.set_context(PresenceContext::AtMac);
        assert_eq!(policy.decide(&ev), Channel::Overlay);
    }

    #[test]
    fn critical_bypasses_quiet_window() {
        let mut policy = DeliveryPolicy::default();
        policy.set_context(PresenceContext::Away);
        policy.hold_for_ms(60_000);

        let critical = DeliveryEvent {
            timestamp: 0,
            ..event("vps", "caiu", Urgency::Critical)
        };
        assert_ne!(policy.decide(&critical), Channel::Hold);
        assert_eq!(policy.decide(&critical), Channel::Push);

        let routine = DeliveryEvent {
            timestamp: 0,
            ..event("watcher", "sincronizou", Urgency::Routine)
        };
        assert_eq!(policy.decide(&routine), Channel::Hold);
        assert!(policy.explain(&routine).contains("silêncio"));
    }

    #[test]
    fn voice_only_at_mac() {
        let mut policy = DeliveryPolicy::default();
        let ev = event("watcher", "relatório pronto", Urgency::Routine);

        policy.set_context(PresenceContext::AtMac);
        assert_eq!(policy.decide(&ev), Channel::Voice);

        for ctx in [
            PresenceContext::Recording,
            PresenceContext::Meeting,
            PresenceContext::Focus,
            PresenceContext::Away,
            PresenceContext::Driving,
            PresenceContext::ScreenSharing,
        ] {
            policy.set_context(ctx);
            assert_ne!(policy.decide(&ev), Channel::Voice, "contexto {ctx:?}");
        }
    }

    #[test]
    fn rule_precedence_first_match_wins() {
        let mut policy = DeliveryPolicy::default();
        policy.add_rule(rule("primeira", "vps", "caiu", Urgency::Critical, Channel::Overlay));
        policy.add_rule(rule("segunda", "vps", "caiu", Urgency::Critical, Channel::Push));

        let ev = event("VPS-01", "CAIU agora", Urgency::Critical);
        assert_eq!(policy.decide(&ev), Channel::Overlay);
        assert!(policy.explain(&ev).contains("primeira"));

        assert!(policy.remove_rule("primeira"));
        assert_eq!(policy.decide(&ev), Channel::Push);
        assert!(!policy.remove_rule("primeira"));
        assert_eq!(policy.rules().len(), 1);
    }

    #[test]
    fn disabled_rules_and_urgency_threshold() {
        let mut policy = DeliveryPolicy::default();
        let mut disabled = rule("off", "vps", "", Urgency::Background, Channel::Push);
        disabled.enabled = false;
        policy.add_rule(disabled);
        policy.add_rule(rule("alto", "vps", "", Urgency::Critical, Channel::Push));

        let routine = event("vps", "healthcheck ok", Urgency::Routine);
        assert_eq!(policy.decide(&routine), Channel::Voice);
    }

    #[test]
    fn background_is_log_only() {
        let policy = DeliveryPolicy::default();
        let ev = event("metrics", "tick", Urgency::Background);
        assert_eq!(policy.decide(&ev), Channel::LogOnly);
        assert!(policy.explain(&ev).contains("registro"));
    }

    #[test]
    fn persistence_round_trip() {
        let path = temp_path();
        let _ = std::fs::remove_file(&path);

        // Arquivo ausente => padrão.
        let loaded = load(&path);
        assert_eq!(loaded.context(), PresenceContext::AtMac);
        assert!(loaded.rules().is_empty());

        let mut policy = DeliveryPolicy::default();
        policy.set_context(PresenceContext::Driving);
        policy.add_rule(rule("vps", "VPS", "caiu", Urgency::Critical, Channel::Push));
        policy.hold_for_ms(5_000);
        policy.save(&path).expect("salvar política");

        let restored = load(&path);
        assert_eq!(restored.context(), PresenceContext::Driving);
        assert_eq!(restored.quiet_until_ms(), policy.quiet_until_ms());
        assert_eq!(restored.rules().len(), 1);
        assert_eq!(restored.rules()[0].name, "vps");
        assert_eq!(restored.rules()[0].channel, Channel::Push);
        assert_eq!(restored.rules()[0].urgency_min, Urgency::Critical);

        let ev = event("vps", "caiu", Urgency::Critical);
        assert_eq!(restored.decide(&ev), policy.decide(&ev));

        // Arquivo corrompido => padrão.
        std::fs::write(&path, " { isso não é json ").expect("corromper arquivo");
        assert_eq!(load(&path).context(), PresenceContext::AtMac);
        assert!(load(&path).rules().is_empty());

        let _ = std::fs::remove_file(&path);
    }
}
