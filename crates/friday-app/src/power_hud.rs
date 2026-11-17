//! Power HUD — reads the system state and exposes what Friday can do NOW.
//!
//! Pure logic, no GPUI types. One fixed red accent, no blinking, no colour
//! changes, no MediaPipe. The caller renders the returned plain data.

use serde_json::Value;

pub const ACCENT: u32 = 0xff2f2f;

#[derive(Debug, Clone)]
pub struct ActionChip {
    pub label: String,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct DockEntry {
    pub label: String,
    pub detail: String,
    pub urgent: bool,
}

#[derive(Debug, Clone)]
pub struct PowerHud {
    pub status_line: String,
    pub chips: Vec<ActionChip>,
    pub dock: Vec<DockEntry>,
    pub activity: Vec<String>,
}

fn data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_default();
    std::path::PathBuf::from(home).join("Library/Application Support/FridayOS")
}

fn read_json(path: std::path::PathBuf) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
}

pub fn snapshot() -> PowerHud {
    let dir = data_dir();
    let context = read_json(dir.join("context-snapshot.json")).unwrap_or(Value::Null);
    let active_app = context
        .get("active_app")
        .and_then(Value::as_str)
        .unwrap_or("—")
        .to_string();

    let native = context.get("native_events");
    let last_app = native
        .and_then(|value| value.get("last_app"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let last_paths: Vec<String> = native
        .and_then(|value| value.get("last_paths"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .take(3)
                .collect()
        })
        .unwrap_or_default();

    let mut chips: Vec<ActionChip> = Vec::new();
    if !last_app.is_empty() && last_app != active_app {
        chips.push(ActionChip {
            label: format!("App mudou: {last_app}"),
            prompt: format!("O app ativo mudou para {last_app}. Precisa que eu faça algo a respeito?"),
        });
    }
    for path in last_paths {
        chips.push(ActionChip {
            label: format!("Arquivo: {}", path.rsplit('/').next().unwrap_or(&path)),
            prompt: format!("Analise a mudança recente em {path} e me diga se é relevante."),
        });
    }

    let mut dock: Vec<DockEntry> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("missions")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Some(mission) = read_json(path.clone()) {
                let status = mission
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("active")
                    .to_string();
                if status == "completed" || status == "cancelled" {
                    continue;
                }
                let name = mission
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("missão")
                    .to_string();
                let needs = mission
                    .get("needs_approval")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                dock.push(DockEntry {
                    label: name,
                    detail: if needs { "precisa de você".into() } else { status },
                    urgent: needs,
                });
            }
        }
    }

    if let Some(pending) = read_json(dir.join("approvals.json")) {
        let count = pending
            .get("pending")
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0);
        if count > 0 {
            dock.insert(
                0,
                DockEntry {
                    label: format!("{count} aprovação(ões)"),
                    detail: "diga sim ou não".into(),
                    urgent: true,
                },
            );
        }
    }

    let runs = friday_tools::artifact::run_summary(3);
    for run in runs {
        if let Some(id) = run.get("id").and_then(Value::as_str) {
            dock.push(DockEntry {
                label: format!("run {}", &id[..id.len().min(18)]),
                detail: run
                    .get("verification")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                urgent: false,
            });
        }
    }

    let active_missions = dock.iter().filter(|entry| entry.detail != "completed").count();
    let status_line = format!(
        "ouvindo · {active_missions}missão(ões) · app: {active_app} · decisão ativa"
    );

    PowerHud {
        status_line,
        chips: chips.into_iter().take(3).collect(),
        dock: dock.into_iter().take(6).collect(),
        activity: Vec::new(),
    }
}
