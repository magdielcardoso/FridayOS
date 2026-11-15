//! Probes which voice names the ChatGPT read-aloud endpoint accepts.
//! `cargo run -p friday-core --example voice_probe`

use friday_core::auth::backend_headers;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let auth = friday_core::auth::CodexAuth::discover();
    let session = auth.session().await?;
    let client = reqwest::Client::new();
    let text = "Bom dia, senhor. Todos os sistemas estão nominais.";

    let voices = [
        None,
        Some("viola"),
        Some("Viola"),
        Some("marin"),
        Some("cove"),
        Some("juniper"),
        Some("breeze"),
        Some("ember"),
        Some("maple"),
        Some("sol"),
        Some("spruce"),
        Some("vale"),
        Some("arbor"),
        Some("birch"),
        Some("marin"),
        Some("cedar"),
        Some("nova"),
        Some("shimmer"),
        Some("alloy"),
    ];

    for voice in voices {
        let mut body = serde_json::json!({ "text": text });
        if let Some(voice) = voice {
            body["voice"] = serde_json::json!(voice);
        }
        let request = backend_headers(
            client
                .post("https://chatgpt.com/backend-api/read-aloud/synthesize")
                .json(&body),
            &session,
        )
        .header("Accept", "audio/*");

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("?")
                    .to_string();
                let bytes = response.bytes().await.unwrap_or_default();
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut hasher);
                println!(
                    "{:<10} {:>4} {:<12} {:>8} bytes  hash {:x}",
                    voice.unwrap_or("default"),
                    status.as_u16(),
                    content_type,
                    bytes.len(),
                    hasher.finish()
                );
                if !status.is_success() {
                    println!("   {}", String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>());
                }
            }
            Err(error) => println!("{voice:?} erro de rede: {error}"),
        }
    }
    Ok(())
}
