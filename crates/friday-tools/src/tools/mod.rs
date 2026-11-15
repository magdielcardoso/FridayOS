//! Tool modules, grouped by the macOS surface they touch.

pub mod apps;
pub mod autonomy;
pub mod codex_accounts;
pub mod diagnose;
pub mod memory;
pub mod audio;
pub mod assistants;
pub mod ax;
pub mod files;
pub mod input;
pub mod net;
pub mod overlay;
pub mod screen;
pub mod shell;
pub mod speech;
pub mod voice;
pub mod webhooks;
pub mod voice_prefs;
pub mod reasoning;

use crate::registry::Registry;

/// Registers every tool known to Friday.
pub fn register_all(registry: &mut Registry) {
    ax::register(registry);
    input::register(registry);
    apps::register(registry);
    autonomy::register(registry);
    codex_accounts::register(registry);
    diagnose::register(registry);
    memory::register(registry);
    audio::register(registry);
    assistants::register(registry);
    shell::register(registry);
    speech::register(registry);
    screen::register(registry);
    files::register(registry);
    net::register(registry);
    overlay::register(registry);
    voice::register(registry);
    webhooks::register(registry);
    voice_prefs::register(registry);
    reasoning::register(registry);
}
