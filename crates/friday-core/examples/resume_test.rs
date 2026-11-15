use std::time::Duration;
use friday_core::codex::{Codex, CodexConfig, CodexEvent};

async fn turn(config: CodexConfig, prompt: &str) -> (Option<String>, String) {
    let mut codex = Codex::spawn(config);
    let mut events = codex.take_events().unwrap();
    codex.prompt(prompt);
    let mut thread = None;
    let mut text = String::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(60), events.recv()).await {
            Ok(Some(CodexEvent::Online{thread_id})) => thread = Some(thread_id),
            Ok(Some(CodexEvent::Delta(t))) => text.push_str(&t),
            Ok(Some(CodexEvent::Done{..})) => break,
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    (thread, text)
}

#[tokio::main]
async fn main() {
    let base = CodexConfig { effort: Some("low".into()), developer_instructions: Some("Responda em uma frase.".into()), ..Default::default() };
    let (thread, _) = turn(base.clone(), "Meu nome é Magdiel. Guarde.").await;
    println!("thread1={:?}", thread);
    if let Some(id) = thread {
        let resumed = CodexConfig { resume_thread: Some(id), ..base };
        let (t2, text) = turn(resumed, "Qual é o meu nome?").await;
        println!("thread2={:?} resposta={text}", t2);
    }
}
