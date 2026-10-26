//! The launcher each Claude Code config dir was last started from.
//!
//! A launcher (`deepseek-cc`) runs Claude Code with its own `CLAUDE_CONFIG_DIR`
//! and may do more than set it: start a proxy, point it at another model. A
//! conversation of that dir only comes back through the same launcher, but
//! once its pane is closed nothing on disk says which one it was: Claude Code's
//! files name the dir, never the command that chose it. So Kova writes down the
//! pair whenever a pane shows it (`Surface::set_agent_session`), and a closed
//! conversation found in the search palette, or a bookmark saved before the
//! launcher was known, reopens through it.

use std::collections::HashMap;
use std::path::PathBuf;

use parking_lot::Mutex;

/// `config dir → launcher`, read from disk on first use.
static KNOWN: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/launchers.json")
}

fn with_known<T>(f: impl FnOnce(&mut HashMap<String, String>) -> T) -> T {
    let mut guard = KNOWN.lock();
    let map =
        guard.get_or_insert_with(|| crate::session::load_owner_only(&path(), "launcher list"));
    f(map)
}

/// Record that `config_dir` was started from `launcher`. Called on every probe
/// of a pane, so it writes only when the pair changes.
pub fn remember(config_dir: &str, launcher: &str) {
    let changed = with_known(|map| {
        if map.get(config_dir).map(String::as_str) == Some(launcher) {
            return None;
        }
        map.insert(config_dir.to_string(), launcher.to_string());
        Some(map.clone())
    });
    // Tests probe panes too; they must never write to the user's config.
    if let Some(map) = changed.filter(|_| !cfg!(test)) {
        crate::session::save_owner_only(&path(), "launchers", &map);
    }
}

/// The launcher last seen for `config_dir`, if it is still plain text: the file
/// is read back from disk, and its value is typed into a shell.
pub fn for_config_dir(config_dir: &str) -> Option<String> {
    with_known(|map| map.get(config_dir).cloned())
        .filter(|launcher| crate::claude_session::is_plain_launcher(launcher))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remembered_launcher_is_found_again_and_replaced_by_a_newer_one() {
        let dir = "/Users/x/.claude-launchers-test";
        remember(dir, "proxy-cc");
        assert_eq!(for_config_dir(dir).as_deref(), Some("proxy-cc"));
        remember(dir, "proxy-cc --mcp a");
        assert_eq!(for_config_dir(dir).as_deref(), Some("proxy-cc --mcp a"));
        assert_eq!(for_config_dir("/Users/x/.claude-never-seen"), None);
    }

    #[test]
    fn a_launcher_that_is_not_plain_text_is_not_handed_back() {
        let dir = "/Users/x/.claude-launchers-unsafe";
        with_known(|map| map.insert(dir.to_string(), "proxy-cc; rm -rf ~".to_string()));
        assert_eq!(for_config_dir(dir), None);
    }
}
