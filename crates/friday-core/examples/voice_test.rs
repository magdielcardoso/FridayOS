//! Voice smoke test.
//!
//! `cargo run -p friday-core --example voice_test` speaks a sentence.
//! `cargo run -p friday-core --example voice_test -- <file.wav>` transcribes.

use friday_core::auth::CodexAuth;
use friday_core::wav::encode_pcm16_wav;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let auth = CodexAuth::discover();

    if let Some(path) = std::env::args().nth(1) {
        let bytes = std::fs::read(&path)?;
        match friday_core::stt::transcribe(&auth, bytes).await {
            Ok(text) => println!("transcrição: {text}"),
            Err(error) => eprintln!("stt falhou: {error}"),
        }
        return Ok(());
    }

    let sentence = "Friday online. Todos os sistemas nominais.";
    let pcm = friday_core::tts::synthesize(&auth, sentence).await?;
    let wav = encode_pcm16_wav(&pcm.samples, pcm.sample_rate);
    let path = std::env::temp_dir().join("friday-voice-test.wav");
    std::fs::write(&path, &wav)?;
    println!("{} bytes de PCM @ {} Hz → {}", wav.len(), pcm.sample_rate, path.display());
    let status = std::process::Command::new("/usr/bin/afplay").arg(&path).status()?;
    println!("afplay: {status}");
    Ok(())
}
