//! Fila de aprovações pendentes e briefing falado — lógica pura, sem GPUI.
//!
//! O fluxo é o do JARVIS pessoal do usuário: várias decisões chegam, ficam
//! agrupadas numa fila, e a Friday entrega um `digest()` curto em português
//! para pedir todas de uma vez. O `Briefing` faz o papel inverso: resume
//! missões, linha do tempo de ações e erros num texto markdown-ish com um
//! resumo falado de 1–3 frases.
//!
//! O carimbo `received_ms` é gerado por [`now_ms`] no momento em que a
//! aprovação entra na fila; a lógica de decisão não depende de relógio de
//! parede. O único IO é `std::fs` em [`ApprovalQueue::load`] /
//! [`ApprovalQueue::save`].

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// Número máximo de linhas do texto de um [`Briefing`].
const MAX_BRIEFING_LINES: usize = 40;
/// Limite de caracteres do `digest()`.
const DIGEST_CAP: usize = 400;
/// Limite de caracteres do resumo falado.
const SPOKEN_CAP: usize = 300;

/// Carimbo de tempo atual em milissegundos desde o epoch Unix.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Trunca por pontos de código Unicode (não por bytes), cabendo em `max`.
/// Quando há truncamento, o último caractere é `…` e o total continua `<= max`.
fn cap_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut chars = input.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        let mut out: String = head.chars().take(max - 1).collect();
        out.push('…');
        out
    } else {
        head
    }
}

/// Uma decisão aguardando resposta do usuário.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub id: u64,
    pub method: String,
    pub summary: String,
    pub received_ms: u64,
}

/// Conjunto de aprovações ainda não decididas, com persistência em JSON.
#[derive(Debug, Clone, Default)]
pub struct ApprovalQueue {
    pending: Vec<Approval>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Adiciona uma aprovação. Retorna `false` (e não altera a fila) se o `id`
    /// já estiver pendente — dedupe estrito por id.
    pub fn add(&mut self, id: u64, method: &str, summary: &str) -> bool {
        if self.pending.iter().any(|a| a.id == id) {
            return false;
        }
        self.pending.push(Approval {
            id,
            method: method.to_string(),
            summary: summary.to_string(),
            received_ms: now_ms(),
        });
        true
    }

    /// Remove e devolve a aprovação de `id`, se existir. O booleano é a
    /// decisão do usuário (aprovar / rejeitar); ela é consumida pelo chamador,
    /// então a fila apenas retira a pendência de qualquer forma.
    pub fn decide(&mut self, id: u64, approved: bool) -> Option<Approval> {
        let _ = approved;
        let pos = self.pending.iter().position(|a| a.id == id)?;
        Some(self.pending.remove(pos))
    }

    pub fn pending(&self) -> &[Approval] {
        &self.pending
    }

    pub fn count(&self) -> usize {
        self.pending.len()
    }

    /// Carimbo da aprovação mais antiga, ou `None` se a fila estiver vazia.
    pub fn oldest_ms(&self) -> Option<u64> {
        self.pending.iter().map(|a| a.received_ms).min()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Pedido curto em português para falar / mostrar todas as pendências de
    /// uma vez. Sempre com no máximo [`DIGEST_CAP`] caracteres.
    pub fn digest(&self) -> String {
        let n = self.pending.len();
        let mut out = match n {
            0 => return "Sem decisões pendentes.".to_string(),
            1 => "Tenho 1 decisão pendente: ".to_string(),
            _ => format!("Tenho {} decisões pendentes: ", n),
        };
        for (i, a) in self.pending.iter().enumerate() {
            if i > 0 {
                out.push_str("; ");
            }
            out.push_str(&a.method);
            out.push_str(": ");
            out.push_str(&a.summary);
        }
        cap_chars(&out, DIGEST_CAP)
    }

    /// Lê a fila de `path`. Qualquer falha (arquivo ausente, JSON inválido)
    /// resulta numa fila vazia — persistência é best-effort, nunca fatal.
    pub fn load(path: &Path) -> ApprovalQueue {
        let root: Value = match std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
        {
            Some(v) => v,
            None => return Self::new(),
        };

        let mut queue = Self::new();
        if let Some(items) = root.get("pending").and_then(Value::as_array) {
            for item in items {
                let id = match item.get("id").and_then(Value::as_u64) {
                    Some(id) => id,
                    None => continue,
                };
                let method = item
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let summary = item
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let received_ms = item.get("received_ms").and_then(Value::as_u64).unwrap_or(0);
                // `add` faria o dedupe, mas aqui queremos preservar o carimbo
                // original: inserimos direto e só ignoramos ids repetidos.
                if !queue.pending.iter().any(|a| a.id == id) {
                    queue.pending.push(Approval {
                        id,
                        method,
                        summary,
                        received_ms,
                    });
                }
            }
        }
        queue
    }

    /// Grava a fila em `path` com escrita atômica (tmp + rename).
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("criar diretório {}: {e}", parent.display()))?;
        }

        let items: Vec<Value> = self
            .pending
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "method": a.method,
                    "summary": a.summary,
                    "received_ms": a.received_ms,
                })
            })
            .collect();
        let root = json!({ "pending": items });
        let bytes = serde_json::to_vec_pretty(&root).map_err(|e| format!("serializar: {e}"))?;

        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes)
            .map_err(|e| format!("escrever {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("renomear para {}: {e}", path.display()))?;
        Ok(())
    }
}

/// Classificação aproximada de uma seção do briefing, usada pelo resumo falado.
fn section_kind(title: &str) -> u8 {
    let t = title.to_lowercase();
    if t.contains("miss") || t.contains("ativ") || t.contains("mission") {
        1 // missões ativas
    } else if t.contains("erro")
        || t.contains("error")
        || t.contains("pend")
        || t.contains("falha")
        || t.contains("issue")
    {
        2 // pendências / erros
    } else {
        0
    }
}

/// Resumo do estado da Friday, formatado para leitura e para fala.
#[derive(Debug, Clone)]
pub struct Briefing {
    lines: Vec<String>,
    /// Texto markdown-ish já concatenado, para `text()` poder devolver `&str`.
    text: String,
    generated_ms: u64,
}

impl Briefing {
    /// Monta o briefing a partir de seções nomeadas (ex.: "Resumo de missões",
    /// "Linha do tempo de ações", "Erros"). Cada seção vira um cabeçalho `##`
    /// seguido de linhas `- item`. O total de linhas é limitado a 40.
    pub fn from_sections(sections: Vec<(&str, Vec<String>)>) -> Briefing {
        let mut lines: Vec<String> = Vec::new();
        for (title, items) in sections {
            if items.is_empty() || lines.len() >= MAX_BRIEFING_LINES {
                continue;
            }
            lines.push(format!("## {}", title));
            for item in items {
                if lines.len() >= MAX_BRIEFING_LINES {
                    break;
                }
                let clean = item.trim();
                if clean.is_empty() {
                    continue;
                }
                lines.push(format!("- {}", clean));
            }
        }
        let text = lines.join("\n");
        Briefing {
            lines,
            text,
            generated_ms: now_ms(),
        }
    }

    /// Texto completo em markdown-ish (cabeçalhos `##` e itens `- `).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Resumo falado de 1–3 frases, em português, com no máximo
    /// [`SPOKEN_CAP`] caracteres. As contagens de missões e pendências são
    /// inferidas dos títulos das seções montadas em [`Self::from_sections`].
    pub fn spoken(&self) -> String {
        let mut missions = 0usize;
        let mut pendencies = 0usize;
        let mut current = "";

        for line in &self.lines {
            if let Some(title) = line.strip_prefix("## ") {
                current = title;
            } else if line.starts_with("- ") {
                match section_kind(current) {
                    1 => missions += 1,
                    2 => pendencies += 1,
                    _ => {}
                }
            }
        }

        let mut out = format!(
            "Boa manhã. Missões ativas: {}. Pendências: {}.",
            missions, pendencies
        );

        let first_items: Vec<&str> = self
            .lines
            .iter()
            .filter(|line| line.starts_with("- "))
            .take(3)
            .map(|line| line[2..].trim())
            .collect();
        if !first_items.is_empty() {
            out.push(' ');
            out.push_str(&first_items.join(" "));
        }

        cap_chars(&out, SPOKEN_CAP)
    }

    /// Carimbo de geração do briefing (ms desde o epoch Unix).
    pub fn generated_ms(&self) -> u64 {
        self.generated_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_por_id() {
        let mut q = ApprovalQueue::new();
        assert!(q.add(7, "shell", "apagar /tmp/foo"));
        assert!(!q.add(7, "shell", " Outra coisa "));
        assert_eq!(q.count(), 1);
        assert_eq!(q.pending()[0].summary, "apagar /tmp/foo");
        assert!(q.oldest_ms().is_some());

        let removed = q.decide(7, true).expect("aprovação 7 deveria existir");
        assert_eq!(removed.id, 7);
        assert_eq!(q.count(), 0);
        assert_eq!(q.oldest_ms(), None);
        assert!(q.decide(7, false).is_none());

        // Depois de decidida, o id pode voltar.
        assert!(q.add(7, "shell", "nova"));
        q.clear();
        assert_eq!(q.count(), 0);
    }

    #[test]
    fn digest_respeita_limite() {
        let mut q = ApprovalQueue::new();
        for id in 0..40 {
            assert!(q.add(id, "ferramenta_muito_longa", &"resumo extenso ".repeat(60)));
        }
        let d = q.digest();
        assert!(d.chars().count() <= DIGEST_CAP, "digest tem {} chars", d.chars().count());
        assert!(d.starts_with("Tenho 40 decisões pendentes:"));

        // Truncamento por chars preserva UTF-8 (acentos portugueses).
        let mut accented = ApprovalQueue::new();
        assert!(accented.add(1, "ação", &"coração ".repeat(200)));
        let short = ApprovalQueue::new();
        assert!(short.add(1, "ação", "coração — conclusão"));
        assert_eq!(short.digest(), "Tenho 1 decisão pendente: ação: coração — conclusão");
        assert!(accented.digest().chars().count() <= DIGEST_CAP);
        assert!(std::str::from_utf8(accented.digest().as_bytes()).is_ok());

        let empty = ApprovalQueue::new();
        assert_eq!(empty.digest(), "Sem decisões pendentes.");
    }

    #[test]
    fn persistencia_round_trip() {
        let path = std::env::temp_dir().join(format!(
            "friday-approval-{}-{}.json",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_file(&path);

        let mut q = ApprovalQueue::new();
        assert!(q.add(1, "email", "enviar relatório"));
        assert!(q.add(2, "shell", "reiniciar serviço"));
        let saved_oldest = q.oldest_ms();
        q.save(&path).expect("save deveria funcionar");

        let loaded = ApprovalQueue::load(&path);
        assert_eq!(loaded.count(), 2);
        assert_eq!(loaded.pending()[0].id, 1);
        assert_eq!(loaded.pending()[0].method, "email");
        assert_eq!(loaded.pending()[0].summary, "enviar relatório");
        assert_eq!(loaded.pending()[1].id, 2);
        assert_eq!(loaded.oldest_ms(), saved_oldest);

        let _ = std::fs::remove_file(&path);
        assert_eq!(ApprovalQueue::load(&path).count(), 0);
    }

    #[test]
    fn briefing_spoken_e_limitado_e_legivel() {
        let missions = vec!["checar e-mail".to_string(), "responder Ana".to_string()];
        let errors = vec!["API fora do ar".to_string()];
        let mut long_timeline = Vec::new();
        for i in 0..80 {
            long_timeline.push(format!("ação número {i}: {}", "detalhe ".repeat(30)));
        }

        // Erros vêm antes da timeline longa: assim o limite de 40 linhas
        // corta a timeline, mas não remove a seção de erros do briefing.
        let b = Briefing::from_sections(vec![
            ("Resumo de missões", missions),
            ("Erros", errors),
            ("Linha do tempo de ações", long_timeline),
        ]);

        let spoken = b.spoken();
        assert!(spoken.chars().count() <= SPOKEN_CAP, "spoken tem {} chars", spoken.chars().count());
        assert!(spoken.starts_with("Boa manhã."));
        assert!(spoken.contains("Missões ativas: 2."));
        assert!(spoken.contains("Pendências: 1."));

        let text = b.text();
        assert!(text.starts_with("## Resumo de missões\n"));
        assert!(text.contains("## Erros\n- API fora do ar"));
        assert!(text.lines().count() <= MAX_BRIEFING_LINES);
        assert!(b.generated_ms() > 0);
    }
}
