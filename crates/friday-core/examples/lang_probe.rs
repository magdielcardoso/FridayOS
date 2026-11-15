use friday_core::auth::backend_headers;
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let auth = friday_core::auth::CodexAuth::discover();
    let session = auth.session().await?;
    let client = reqwest::Client::new();
    for language in [None, Some("pt-BR"), Some("pt")] {
        let mut body = serde_json::json!({"text":"Bom dia, senhor.","voice":"viola"});
        if let Some(l) = language { body["language"] = serde_json::json!(l); }
        let r = backend_headers(client.post("https://chatgpt.com/backend-api/read-aloud/synthesize").json(&body), &session)
            .header("Accept","audio/*").send().await?;
        let status = r.status();
        let bytes = r.bytes().await?;
        println!("language={:?} status={} bytes={} head={:?}", language, status.as_u16(), bytes.len(), String::from_utf8_lossy(&bytes[..bytes.len().min(120)]));
    }
    Ok(())
}
