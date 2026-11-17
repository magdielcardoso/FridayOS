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

/// Resultado de [`verify`]: verificação estruturada de uma afirmação contra um estado.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// P(`noul`) de que o estado dá evidência direta para a afirmação (0..=1).
    pub supported: f64,
    /// P(`noul`) de que o estado contradiz a afirmação (0..=1).
    pub contradicted: f64,
    /// Nota `score` (0..=2) sobre Unsupported / Partially supported / Fully supported.
    pub confidence: f64,
    /// `pass` | `fail` | `review`, derivado dos limiares em [`decision_for`].
    pub decision: String,
}

/// Resultado de [`classify_complexity`]: complexidade estimada de uma solicitação.
#[derive(Debug, Clone, PartialEq)]
pub struct Complexity {
    /// `trivial` | `moderate` | `complex`.
    pub level: String,
    /// Confiança da escolha `choice` (0..=1).
    pub confidence: f64,
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

/// Faz uma decisão tipada com Jev. `state` pode ser texto ou objeto estruturado.
pub async fn decide(api_key: &str, state: Value) -> Result<JevDecision, JevError> {
    let body = system_one(api_key, state, questions()).await?;
    parse(&body)
}

/// Verifica uma afirmação (`claim`) contra o `state` com três perguntas tipadas
/// numa única chamada: `supported` / `contradicted` (`noul`) e `confidence`
/// (`score`). A afirmação é anexada ao estado enviado ao Jev para que o modelo
/// possa avaliar as evidências a seu favor e contra ela.
///
/// Regras de decisão (em [`decision_for`]):
/// - `pass` se `supported > 0.75 && contradicted < 0.2`
/// - `fail` se `contradicted > 0.6`
/// - caso contrário `review`
pub async fn verify(api_key: &str, state: Value, claim: &str) -> Result<Verdict, JevError> {
    // Anexa a afirmação ao estado sem sobrescrever nenhuma chave do estado original.
    let state = json!({ "state": state, "claim": claim });
    let body = system_one(api_key, state, verify_questions()).await?;
    parse_verdict(&body)
}

/// Classifica a complexidade da solicitação implícita no `state` com uma única
/// pergunta `choice` (`level`).
pub async fn classify_complexity(
    api_key: &str,
    state: Value,
) -> Result<Complexity, JevError> {
    let body = system_one(api_key, state, complexity_questions()).await?;
    parse_complexity(&body)
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

/// Perguntas usadas por [`verify`]. As instruções dos `noul` são fixas em
/// inglês conforme o contrato da API.
fn verify_questions() -> Value {
    json!({
        "supported": {
            "type": "noul",
            "instructions": "The state provides direct evidence for the claim"
        },
        "contradicted": {
            "type": "noul",
            "instructions": "The state contradicts the claim"
        },
        "confidence": {
            "type": "score",
            "instructions": "How well is the claim supported by the state?",
            "criteria": ["Unsupported", "Partially supported", "Fully supported"]
        }
    })
}

/// Perguntas usadas por [`classify_complexity`].
fn complexity_questions() -> Value {
    json!({
        "level": {
            "type": "choice",
            "instructions": "How complex is the request implied by this state?",
            "criteria": {
                "trivial": "Chit-chat or a single fact; no tools needed.",
                "moderate": "Needs 1-3 tools to answer.",
                "complex": "Multi-step work: planning, code, or research."
            }
        }
    })
}

/// Plumbing HTTP compartilhado por todas as chamadas System One. A chave é
/// usada apenas no header `Authorization` e nunca é registrada em logs ou
/// incluída em mensagens de erro.
async fn system_one(api_key: &str, state: Value, questions: Value) -> Result<Value, JevError> {
    if api_key.trim().is_empty() {
        return Err(JevError::NoToken);
    }
    let body = json!({
        "model": DEFAULT_MODEL,
        "state": state,
        "questions": questions,
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
    response
        .json()
        .await
        .map_err(|error| JevError::Invalid(error.to_string()))
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

/// Converte as notas de [`verify`] numa decisão. As duas condições são
/// mutuamente exclusivas: `pass` exige `contradicted < 0.2`, `fail` exige
/// `contradicted > 0.6`.
fn decision_for(supported: f64, contradicted: f64) -> &'static str {
    if supported > 0.75 && contradicted < 0.2 {
        "pass"
    } else if contradicted > 0.6 {
        "fail"
    } else {
        "review"
    }
}

/// Interpreta a resposta de [`verify`]. Privado; testado via JSON canned no
/// módulo de testes.
fn parse_verdict(body: &Value) -> Result<Verdict, JevError> {
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::Invalid("campo answers ausente".into()))?;

    let supported = answers
        .get("supported")
        .and_then(|answer| answer.get("noul"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let contradicted = answers
        .get("contradicted")
        .and_then(|answer| answer.get("noul"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let confidence = answers
        .get("confidence")
        .and_then(|answer| answer.get("score"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    Ok(Verdict {
        supported,
        contradicted,
        confidence,
        decision: decision_for(supported, contradicted).to_string(),
    })
}

/// Interpreta a resposta de [`classify_complexity`]. Privado; testado via JSON
/// canned no módulo de testes.
fn parse_complexity(body: &Value) -> Result<Complexity, JevError> {
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| JevError::Invalid("campo answers ausente".into()))?;

    let level_answer = answers
        .get("level")
        .ok_or_else(|| JevError::Invalid("resposta level ausente".into()))?;
    let level = level_answer
        .get("choice")
        .and_then(Value::as_str)
        .unwrap_or("moderate")
        .to_string();
    let confidence = level_answer
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);

    let known = ["trivial", "moderate", "complex"];
    let level = if known.contains(&level.as_str()) {
        level
    } else {
        "moderate".to_string()
    };

    Ok(Complexity { level, confidence })
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

    #[test]
    fn parses_verdict_pass() {
        let body = json!({
            "answers": {
                "supported": { "type": "noul", "noul": 0.86 },
                "contradicted": { "type": "noul", "noul": 0.08 },
                "confidence": { "type": "score", "score": 2.0 }
            }
        });
        let verdict = parse_verdict(&body).unwrap();
        assert!((verdict.supported - 0.86).abs() < f64::EPSILON);
        assert!((verdict.contradicted - 0.08).abs() < f64::EPSILON);
        assert_eq!(verdict.confidence, 2.0);
        assert_eq!(verdict.decision, "pass");
    }

    #[test]
    fn parses_verdict_fail() {
        let body = json!({
            "answers": {
                "supported": { "type": "noul", "noul": 0.12 },
                "contradicted": { "type": "noul", "noul": 0.74 },
                "confidence": { "type": "score", "score": 0.0 }
            }
        });
        let verdict = parse_verdict(&body).unwrap();
        assert_eq!(verdict.confidence, 0.0);
        assert_eq!(verdict.decision, "fail");
    }

    #[test]
    fn parses_verdict_review() {
        let body = json!({
            "answers": {
                "supported": { "type": "noul", "noul": 0.62 },
                "contradicted": { "type": "noul", "noul": 0.30 },
                "confidence": { "type": "score", "score": 1.0 }
            }
        });
        let verdict = parse_verdict(&body).unwrap();
        assert_eq!(verdict.confidence, 1.0);
        assert_eq!(verdict.decision, "review");
    }

    #[test]
    fn verdict_thresholds_and_boundaries() {
        assert_eq!(decision_for(0.76, 0.19), "pass");
        assert_eq!(decision_for(0.75, 0.10), "review"); // supported não é > 0.75
        assert_eq!(decision_for(0.80, 0.20), "review"); // contradicted não é < 0.2
        assert_eq!(decision_for(0.10, 0.61), "fail");
        assert_eq!(decision_for(0.10, 0.60), "review"); // contradicted não é > 0.6
    }

    #[test]
    fn missing_verdict_answers_is_invalid() {
        assert!(parse_verdict(&json!({})).is_err());
    }

    #[test]
    fn parses_complexity() {
        let body = json!({
            "answers": {
                "level": {
                    "type": "choice",
                    "choice": "complex",
                    "confidence": 0.88,
                    "probabilities": { "complex": 0.88, "moderate": 0.12 }
                }
            }
        });
        let complexity = parse_complexity(&body).unwrap();
        assert_eq!(complexity.level, "complex");
        assert!((complexity.confidence - 0.88).abs() < f64::EPSILON);
    }

    #[test]
    fn complexity_unknown_level_falls_back_to_moderate() {
        let body = json!({
            "answers": {
                "level": { "type": "choice", "choice": "impossible", "confidence": 0.5 }
            }
        });
        let complexity = parse_complexity(&body).unwrap();
        assert_eq!(complexity.level, "moderate");
        assert_eq!(complexity.confidence, 0.5);
    }

    #[test]
    fn missing_complexity_answers_is_invalid() {
        assert!(parse_complexity(&json!({})).is_err());
        assert!(parse_complexity(&json!({ "answers": {} })).is_err());
    }

    #[test]
    fn verify_questions_match_contract() {
        let questions = verify_questions();
        assert_eq!(questions["supported"]["type"], "noul");
        assert_eq!(
            questions["supported"]["instructions"],
            "The state provides direct evidence for the claim"
        );
        assert_eq!(questions["contradicted"]["type"], "noul");
        assert_eq!(
            questions["contradicted"]["instructions"],
            "The state contradicts the claim"
        );
        assert_eq!(questions["confidence"]["type"], "score");
        assert_eq!(
            questions["confidence"]["criteria"],
            json!(["Unsupported", "Partially supported", "Fully supported"])
        );
    }

    #[test]
    fn complexity_questions_match_contract() {
        let questions = complexity_questions();
        assert_eq!(questions["level"]["type"], "choice");
        assert_eq!(
            questions["level"]["criteria"]["trivial"],
            "Chit-chat or a single fact; no tools needed."
        );
        assert_eq!(
            questions["level"]["criteria"]["moderate"],
            "Needs 1-3 tools to answer."
        );
        assert_eq!(
            questions["level"]["criteria"]["complex"],
            "Multi-step work: planning, code, or research."
        );
    }

    #[tokio::test]
    async fn empty_key_is_rejected_without_network() {
        assert!(matches!(
            verify("", json!({}), "claim").await,
            Err(JevError::NoToken)
        ));
        assert!(matches!(
            classify_complexity("", json!({})).await,
            Err(JevError::NoToken)
        ));
    }
}
