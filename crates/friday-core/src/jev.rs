//! TypeSafe Jev — "Sistema de decisão" da Friday.
//!
//! Jev é um System One model: recebe um `state` e perguntas tipadas
//! (`choice`, `score`, `noul`) e devolve decisões estruturadas com
//! probabilidades, sem gerar texto. É usado para decidir rapidamente o que
//! fazer com eventos de watchers, antes de acionar o modelo principal.
//!
//! API: `POST https://api.typesafe.ai/v1/systemone`
//! Documentação: https://docs.typesafe.ai/api

use std::time::Duration;

use serde_json::{json, Value};

pub const SYSTEM_ONE_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const DEFAULT_MODEL: &str = "jev-latest";

#[derive(Debug, Clone, PartialEq)]
pub struct JevDecision {
    /// `ignore` | `hold` | `overlay` | `push` | `voice`
    pub channel: String,
    /// Rubrica de 0 a 2: rotina, precisa atenção, crítico.
    pub urgency: f64,
    /// Probabilidade de exigir uma decisão do usuário.
    pub needs_decision: f64,
    pub confidence: f64,
    pub model: String,
    pub input_tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum JevError {
    #[error("token TypeSafe ausente")]
    NoToken,
    #[error("sistema de decisão ocupado: tentativa duplicada")]
    Busy,
    #[error("chave TypeSafe inválida (HTTP {0})")]
    Unauthorized(u16),
    #[error("limite do sistema de decisão atingido (HTTP {0})")]
    RateLimited(u16),
    #[error("serviço de decisão indisponível (HTTP {0})")]
    Unavailable(u16),
    #[error("resposta de decisão inválida: {0}")]
    Invalid(String),
    #[error("falha no sistema de decisão: {0}")]
    Failed(String),
}

/// Perguntas fixas usadas para rotear eventos. Uma única chamada responde
/// tudo, mantendo latência e custo baixos.
fn questions() -> Value {
    json!({
        "channel": {
            "type": "choice",
            "instructions": "Como a Friday deve entregar este evento ao usuário?",
            "criteria": {
                "ignore": "Evento rotineiro, sem interesse para o usuário; apenas registrar.",
                "hold": "Guardar na fila e avaliar depois, sem interromper.",
                "overlay": "Mostrar um resumo no Mac quando o usuário estiver presente.",
                "push": "Enviar push ao iPhone quando o usuário estiver ausente ou precisar decidir.",
                "voice": "Falar em voz alta imediatamente; reservado a eventos críticos com o usuário presente."
            }
        },
        "urgency": {
            "type": "score",
            "instructions": "Quão urgente é este evento para o usuário?",
            "criteria": [
                "Rotina: pode esperar horas",
                "Precisa de atenção em breve",
                "Crítico: falha, queda de serviço ou risco, precisa de decisão agora"
            ]
        },
        "needs_decision": {
            "type": "noul",
            "instructions": "Este evento exige uma decisão do usuário (aprovar, negar ou escolher)?"
        }
    })
}

/// Faz uma decisão tipada com Jev. `state` pode ser texto ou objeto estruturado.
pub async fn decide(api_key: &str, state: Value) -> Result<JevDecision, JevError> {
    if api_key.trim().is_empty() {
        return Err(JevError::NoToken);
    }
    let body = json!({
        "model": DEFAULT_MODEL,
        "state": state,
        "questions": questions(),
    });

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| JevError::Failed(error.to_string()))?
        .post(SYSTEM_ONE_URL)
        .bearer_auth(api_key.trim())
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| JevError::Failed(error.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return match status.as_u16() {
            401 | 403 => Err(JevError::Unauthorized(status.as_u16())),
            429 => Err(JevError::RateLimited(status.as_u16())),
            code if code >= 500 => Err(JevError::Unavailable(code)),
            code => Err(JevError::Failed(format!("HTTP {code}"))),
        };
    }
    let body: Value = response
        .json()
        .await
        .map_err(|error| JevError::Invalid(error.to_string()))?;
    parse(&body)
}

/// Interpreta a resposta tipada. Exposta para testes sem rede.
pub fn parse(body: &Value) -> Result<JevDecision, JevError> {
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::Invalid("campo answers ausente".into()))?;

    let channel_answer = answers
        .get("channel")
        .ok_or_else(|| JevError::Invalid("resposta channel ausente".into()))?;
    let channel = channel_answer
        .get("choice")
        .and_then(Value::as_str)
        .unwrap_or("hold")
        .to_string();
    let confidence = channel_answer
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let urgency = answers
        .get("urgency")
        .and_then(|answer| answer.get("score"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let needs_decision = answers
        .get("needs_decision")
        .and_then(|answer| answer.get("noul"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_MODEL)
        .to_string();
    let usage = body.get("usage");
    let input_tokens = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let known = ["ignore", "hold", "overlay", "push", "voice"];
    let channel = if known.contains(&channel.as_str()) {
        channel
    } else {
        "hold".to_string()
    };

    Ok(JevDecision {
        channel,
        urgency,
        needs_decision,
        confidence,
        model,
        input_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typed_answers() {
        let body = json!({
            "model": "jev-1.13.0",
            "answers": {
                "channel": {
                    "type": "choice",
                    "choice": "push",
                    "confidence": 0.91,
                    "probabilities": { "push": 0.91, "hold": 0.09 }
                },
                "urgency": { "type": "score", "score": 2.0, "confidence": 0.8 },
                "needs_decision": { "type": "noul", "noul": 0.75 }
            },
            "usage": { "input_tokens": 120, "output_tokens": 12 }
        });
        let decision = parse(&body).unwrap();
        assert_eq!(decision.channel, "push");
        assert_eq!(decision.urgency, 2.0);
        assert!(decision.needs_decision > 0.7);
        assert_eq!(decision.input_tokens, 120);
    }

    #[test]
    fn unknown_channel_falls_back_to_hold() {
        let body = json!({
            "answers": {
                "channel": { "type": "choice", "choice": "shotgun", "confidence": 0.5 }
            }
        });
        assert_eq!(parse(&body).unwrap().channel, "hold");
    }

    #[test]
    fn missing_answers_is_invalid() {
        assert!(parse(&json!({})).is_err());
    }
}
