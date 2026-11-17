//! Tool modules, grouped by the macOS surface they touch.

pub mod annotate;
pub mod audit_tool;
pub mod apps;
pub mod autonomy;
pub mod codex_accounts;
pub mod context;
pub mod dashboard;
pub mod diagnose;
pub mod memory;
pub mod missions;
pub mod audio;
pub mod assistants;
pub mod ax;
pub mod files;
pub mod forensics;
pub mod input;
pub mod knowledge;
pub mod net;
pub mod overlay;
pub mod screen;
pub mod shell;
pub mod speech;
pub mod undo;
pub mod voice;
pub mod watchers;
pub mod webhooks;
pub mod voice_prefs;
pub mod reasoning;
pub mod reason;
pub mod self_knowledge;

use crate::registry::Registry;

/// Registers every tool known to Friday.
pub fn register_all(registry: &mut Registry) {
    ax::register(registry);
    context::register(registry);
    input::register(registry);
    knowledge::register(registry);
    annotate::register(registry);
    audit_tool::register(registry);
    apps::register(registry);
    autonomy::register(registry);
    codex_accounts::register(registry);
    dashboard::register(registry);
    diagnose::register(registry);
    memory::register(registry);
    missions::register(registry);
    audio::register(registry);
    assistants::register(registry);
    shell::register(registry);
    speech::register(registry);
    screen::register(registry);
    files::register(registry);
    forensics::register(registry);
    net::register(registry);
    overlay::register(registry);
    voice::register(registry);
    watchers::register(registry);
    webhooks::register(registry);
    voice_prefs::register(registry);
    reasoning::register(registry);
    reason::register(registry);
    self_knowledge::register(registry);
    undo::register(registry);
}
