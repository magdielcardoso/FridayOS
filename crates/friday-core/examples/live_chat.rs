//! Smoke test for the Codex subscription chat path:
//! `cargo run -p friday-core --example live_chat -- "diga ok"`

use std::time::Duration;

use friday_core::codex::{Codex, CodexConfig, CodexEvent};

#[tokio::main]
async fn main() {
    let prompt = std::env::args().nth(1).unwrap_or_else(|| "Responda apenas: online.".into());
    let mcp = std::env::current_exe().ok().and_then(|exe| {
        let dir = exe.parent()?;
        [dir.join("friday-mcp"), dir.join("../friday-mcp")]
            .into_iter()
            .find(|path| path.is_file())
    });

    let instructions = std::env::var("FRIDAY_PERSONA")
        .unwrap_or_else(|_| "Responda em português, em no máximo duas frases.".into());
    let env = match friday_core::codex::prepare_isolated_home(mcp.as_deref()) {
        Ok(home) => vec![("CODEX_HOME".to_string(), home.display().to_string())],
        Err(_) => Vec::new(),
    };

    let config = CodexConfig {
        developer_instructions: Some(instructions),
        effort: Some("low".into()),
        extra_args: Vec::new(),
        env,
        ..CodexConfig::default()
    };
    let mut codex = Codex::spawn(config);
    let mut events = codex.take_events().expect("events");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    codex.prompt(prompt);

    loop {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(timeout, events.recv()).await {
            Ok(Some(CodexEvent::Online { thread_id })) => {
                println!("[online thread={thread_id}]");
            }
            Ok(Some(CodexEvent::Delta(text))) => {
                print!("{text}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
            Ok(Some(CodexEvent::Reasoning(_))) => {}
            Ok(Some(CodexEvent::Done { status })) => {
                println!("\n[done {status}]");
                break;
            }
            Ok(Some(CodexEvent::Error(error))) => {
                eprintln!("\n[error] {error}");
                std::process::exit(1);
            }
            Ok(None) => {
                eprintln!("[events closed]");
                std::process::exit(1);
            }
            Err(_) => {
                eprintln!("[timeout]");
                std::process::exit(1);
            }
        }
    }
}
