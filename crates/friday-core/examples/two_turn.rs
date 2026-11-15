use std::time::{Duration, Instant};
use friday_core::codex::{Codex, CodexConfig, CodexEvent};

#[tokio::main]
async fn main() {
    let config = CodexConfig { developer_instructions: Some("Responda em uma frase.".into()), effort: Some("low".into()), ..CodexConfig::default() };
    let mut codex = Codex::spawn(config);
    let mut events = codex.take_events().unwrap();
    let mut sent = 0;
    let mut start = Instant::now();
    let mut first = true;
    codex.prompt("Responda apenas: ok.");
    loop {
        match tokio::time::timeout(Duration::from_secs(60), events.recv()).await {
            Ok(Some(CodexEvent::Online{..})) => { start = Instant::now(); }
            Ok(Some(CodexEvent::Delta(t))) => {
                if first { first=false; eprintln!("[turn{}_first_ms {}]", sent+1, start.elapsed().as_millis()); }
                let _=t;
            }
            Ok(Some(CodexEvent::Done{..})) => {
                eprintln!("[turn{}_done_ms {}]", sent+1, start.elapsed().as_millis());
                sent += 1;
                if sent >= 3 { break; }
                first = true; start = Instant::now();
                codex.prompt("Responda apenas: ok.");
            }
            Ok(Some(_)) => {}
            _ => break,
        }
    }
}
