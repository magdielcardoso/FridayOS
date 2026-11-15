//! Tool modules, grouped by the macOS surface they touch.

pub mod apps;
pub mod autonomy;
pub mod codex_accounts;
pub mod audio;
pub mod ax;
pub mod files;
pub mod input;
pub mod net;
pub mod screen;
pub mod shell;
pub mod voice;
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
    audio::register(registry);
    shell::register(registry);
    screen::register(registry);
    files::register(registry);
    net::register(registry);
    voice::register(registry);
    voice_prefs::register(registry);
    reasoning::register(registry);
}
