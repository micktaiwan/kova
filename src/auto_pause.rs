//! Auto-pause: an agent that has sat idle and unfocused for a while is paused
//! the way `p` in Cmd+P pauses it (`Surface::pause`), so a day of restored
//! conversations does not keep fifty `claude` processes in RAM.
//!
//! Two settings in `config.toml`, `[agents] auto_pause` (on/off, off by
//! default) and `auto_pause_hours` (the delay). They live in process-wide
//! atomics so the frame loop reads them without a config handle, and the
//! `auto-pause` IPC command can change them at run time; that command also
//! writes them back to the file, keeping the rest of it (comments, order).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Delay used when the file does not set one.
pub const DEFAULT_HOURS: f64 = 2.0;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// The delay as `f64::to_bits` of hours.
static HOURS_BITS: AtomicU64 = AtomicU64::new(0x4000_0000_0000_0000); // 2.0

/// The current settings: on/off and the delay in hours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub hours: f64,
}

impl Settings {
    /// The idle time after which an agent is paused; `None` when off.
    pub fn delay(self) -> Option<Duration> {
        self.enabled.then(|| Duration::from_secs_f64(self.hours * 3600.0))
    }
}

pub fn settings() -> Settings {
    Settings {
        enabled: ENABLED.load(Ordering::Relaxed),
        hours: f64::from_bits(HOURS_BITS.load(Ordering::Relaxed)),
    }
}

/// A delay Kova accepts: positive and finite (`0.25` is a quarter hour).
pub fn valid_hours(hours: f64) -> bool {
    hours.is_finite() && hours > 0.0
}

/// Apply settings for this run (launch, `reload-config`, IPC).
pub fn set(s: Settings) {
    let before = settings();
    ENABLED.store(s.enabled, Ordering::Relaxed);
    HOURS_BITS.store(s.hours.to_bits(), Ordering::Relaxed);
    if before != s {
        if s.enabled {
            log::info!("Auto-pause: on, after {} h idle", s.hours);
        } else {
            log::info!("Auto-pause: off");
        }
    }
}

/// Whether an agent idle for `idle` should be paused under `delay`.
pub fn is_due(idle: Duration, delay: Option<Duration>) -> bool {
    delay.is_some_and(|d| idle >= d)
}

/// Apply the settings and save them to `config.toml`.
pub fn set_and_save(s: Settings) -> Result<(), String> {
    let path = crate::config::config_path();
    let current = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {}", path.display(), e)),
    };
    let updated = with_settings(&current, s)?;
    if updated != current {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
        }
        std::fs::write(&path, updated).map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
    }
    set(s);
    Ok(())
}

/// `config` with the `[agents]` auto-pause keys set, everything else
/// untouched. Refuses a file that does not parse, rather than rewrite it.
fn with_settings(config: &str, s: Settings) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = config
        .parse()
        .map_err(|e| format!("config.toml does not parse, left as is: {}", e))?;
    let agents = doc
        .entry("agents")
        .or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(table) = agents.as_table_like_mut() else {
        return Err("config.toml: `agents` is not a table, left as is".to_string());
    };
    table.insert("auto_pause", toml_edit::value(s.enabled));
    table.insert("auto_pause_hours", toml_edit::value(s.hours));
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON_2H: Settings = Settings { enabled: true, hours: 2.0 };

    #[test]
    fn the_initial_delay_is_the_default() {
        assert_eq!(f64::from_bits(0x4000_0000_0000_0000), DEFAULT_HOURS);
    }

    #[test]
    fn off_never_pauses() {
        let off = Settings { enabled: false, hours: 2.0 };
        assert!(!is_due(Duration::from_secs(10 * 24 * 3600), off.delay()));
    }

    #[test]
    fn pauses_once_the_delay_is_reached() {
        assert!(!is_due(Duration::from_secs(2 * 3600 - 1), ON_2H.delay()));
        assert!(is_due(Duration::from_secs(2 * 3600), ON_2H.delay()));
        let quarter = Settings { enabled: true, hours: 0.25 };
        assert!(is_due(Duration::from_secs(15 * 60), quarter.delay()));
    }

    #[test]
    fn only_a_positive_delay_is_valid() {
        assert!(valid_hours(0.5));
        assert!(!valid_hours(0.0));
        assert!(!valid_hours(-1.0));
        assert!(!valid_hours(f64::NAN));
        assert!(!valid_hours(f64::INFINITY));
    }

    #[test]
    fn saving_adds_the_section_to_an_empty_file() {
        let out = with_settings("", Settings { enabled: true, hours: 1.5 }).unwrap();
        let back: crate::config::Config = toml::from_str(&out).unwrap();
        assert!(back.agents.auto_pause);
        assert_eq!(back.agents.auto_pause_hours, 1.5);
    }

    #[test]
    fn saving_keeps_the_rest_of_the_file() {
        let before = "# my font\n[font]\nfamily = \"Hack\" # keep\n\n[agents]\nauto_pause = true\nauto_pause_hours = 5.0\n";
        let out = with_settings(before, Settings { enabled: false, hours: 3.0 }).unwrap();
        assert!(out.contains("# my font\n[font]\nfamily = \"Hack\" # keep\n"), "{out}");
        assert!(out.contains("auto_pause = false"), "{out}");
        assert!(out.contains("auto_pause_hours = 3.0"), "{out}");
        assert!(!out.contains("5.0"), "{out}");
    }

    #[test]
    fn a_broken_file_is_not_rewritten() {
        assert!(with_settings("[font\nfamily=", ON_2H).is_err());
    }

    #[test]
    fn missing_settings_read_as_off_with_the_default_delay() {
        let c: crate::config::Config = toml::from_str("[font]\nsize = 13.0\n").unwrap();
        assert!(!c.agents.auto_pause);
        assert_eq!(c.agents.auto_pause_hours, DEFAULT_HOURS);
    }

    #[test]
    fn an_integer_delay_is_accepted_in_the_file() {
        let c: crate::config::Config = toml::from_str("[agents]\nauto_pause = true\nauto_pause_hours = 3\n").unwrap();
        assert_eq!(c.agents.auto_pause_hours, 3.0);
    }
}
