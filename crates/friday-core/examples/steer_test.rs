use std::time::Duration;

use friday_core::codex::{Codex, CodexConfig, CodexEvent};

#[tokio::main]
async fn main() {
    let config = CodexConfig {
        developer_instructions: Some("Responda em português.".into()),
        effort: Some("low".into()),
        ..CodexConfig::default()
    };
    let mut codex = Codex::spawn(config);
    let mut events = codex.take_events().expect("events");
    codex.prompt("Escreva um parágrafo longo sobre o oceano.");

    let mut steered = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, events.recv()).await {
            Ok(Some(CodexEvent::Delta(text))) => {
                print!("{text}");
                if !steered {
                    steered = true;
                    codex.steer("Pare e responda apenas: azul.");
                    println!("\n[steer enviado]");
                }
            }
            Ok(Some(CodexEvent::Done { status })) => {
                println!("\n[done {status}]");
                break;
            }
            Ok(Some(CodexEvent::Error(error))) => {
                eprintln!("\n[erro] {error}");
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
}
