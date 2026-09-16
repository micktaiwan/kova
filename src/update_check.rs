//! Once a day, ask GitHub whether a newer Kova release exists, and tell the
//! user once per version with an informational sheet. The same check can be run
//! by hand from the Kova menu, and then always answers, even "up to date".
//!
//! The request runs `curl` on a worker thread: no HTTP crate in the binary, and
//! the main thread never waits on the network. Only the Kova that owns the
//! session checks on its own (see `session::owns_session`), so a second
//! instance neither doubles the daily request nor shows the popup twice.
//!
//! The first launch of a new version also shows its release notes once, from
//! `RELEASE_NOTES.md` embedded at build time.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSAlert, NSAlertStyle, NSApplication, NSModalResponse, NSWindow};
use objc2_foundation::NSString;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/micktaiwan/kova/releases/latest";
const CHECK_INTERVAL_SECS: u64 = 24 * 3600;
/// After a failed request (offline, rate limited), try again this much later
/// rather than waiting a whole day.
const RETRY_SECS: u64 = 3600;
/// `NSAlertSecondButtonReturn`.
const SECOND_BUTTON: NSModalResponse = 1001;

type Version = (u32, u32, u32);

/// What a check has to tell the user.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Newer(Version),
    /// Only reported for a check the user asked for.
    UpToDate,
    /// Only reported for a check the user asked for.
    Failed(String),
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Unix time of the last successful check.
    #[serde(default)]
    last_check: u64,
    /// Last version the user was told about, so each release pops up once.
    #[serde(default)]
    notified_version: Option<String>,
    /// Last version whose release notes were shown, so they appear once.
    #[serde(default)]
    notes_shown_version: Option<String>,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Unix time before which no automatic request is sent. In memory only, so a
/// failed request retries within the hour without being written to disk.
static NEXT_ATTEMPT: Mutex<Option<u64>> = Mutex::new(None);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Set when the user asked for a check, until the check in flight reports.
static MANUAL: AtomicBool = AtomicBool::new(false);
/// A result from the worker, waiting for the main thread to show it.
static PENDING: Mutex<Option<Outcome>> = Mutex::new(None);
/// Mirrors `PENDING.is_some()` so the per-tick check is one atomic load.
static HAS_PENDING: AtomicBool = AtomicBool::new(false);
/// Set once the release notes of this launch are shown or known to be not due.
static NOTES_DONE: AtomicBool = AtomicBool::new(false);

const RELEASE_NOTES: &str = include_str!("../RELEASE_NOTES.md");

fn state_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/kova/update_check.json")
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock();
    let state = guard.get_or_insert_with(|| {
        std::fs::read(state_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    });
    f(state)
}

fn save_state() {
    let guard = STATE.lock();
    let Some(state) = guard.as_ref() else { return };
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_vec_pretty(state) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, bytes) {
                log::warn!("Update check: cannot write {}: {}", path.display(), e);
            }
        }
        Err(e) => log::warn!("Update check: cannot serialize state: {}", e),
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Parse `v1.10.0` or `1.10.0` into a triple. Missing components count as 0;
/// anything that is not a plain number (a `-beta` suffix, garbage) is refused.
fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    let mut parts = s.split('.');
    let mut next = |required: bool| -> Option<u32> {
        match parts.next() {
            Some(p) => p.parse().ok(),
            None if required => None,
            None => Some(0),
        }
    };
    let version = (next(true)?, next(false)?, next(false)?);
    if parts.next().is_some() {
        return None;
    }
    Some(version)
}

fn format_version(v: Version) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// The release page, built from the parsed version rather than from a URL the
/// API returned: the only thing handed to `open` is three numbers we formatted.
fn release_page(v: Version) -> String {
    format!("https://github.com/micktaiwan/kova/releases/tag/v{}", format_version(v))
}

/// What to tell the user once the latest release is known. The daily check
/// stays silent unless there is a version it has not announced yet; a check the
/// user asked for always answers.
fn outcome(latest: Version, current: Version, notified: Option<&str>, manual: bool) -> Option<Outcome> {
    if latest > current {
        if manual || notified.and_then(parse_version) != Some(latest) {
            return Some(Outcome::Newer(latest));
        }
        return None;
    }
    manual.then_some(Outcome::UpToDate)
}

fn fetch_latest() -> Result<Version, String> {
    let output = std::process::Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "15", "-H", "Accept: application/vnd.github+json"])
        .arg("-A")
        .arg(concat!("Kova/", env!("CARGO_PKG_VERSION")))
        .arg(LATEST_RELEASE_URL)
        .output()
        .map_err(|e| format!("cannot run curl: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "curl exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let release: LatestRelease =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("unexpected response: {}", e))?;
    parse_version(&release.tag_name).ok_or_else(|| format!("unparsable tag {:?}", release.tag_name))
}

fn run_check() {
    let now = now_secs();
    let result = fetch_latest();
    // Read after the request, so a check asked for while this one was already
    // in flight is answered by it.
    let manual = MANUAL.swap(false, Ordering::AcqRel);
    let report = match result {
        Ok(latest) => {
            let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
            let report = with_state(|s| {
                s.last_check = now;
                outcome(latest, current, s.notified_version.as_deref(), manual)
            });
            save_state();
            *NEXT_ATTEMPT.lock() = Some(now + CHECK_INTERVAL_SECS);
            log::info!("Update check: latest release is {}", format_version(latest));
            report
        }
        Err(e) => {
            log::warn!("Update check failed, retrying in an hour: {}", e);
            *NEXT_ATTEMPT.lock() = Some(now + RETRY_SECS);
            manual.then_some(Outcome::Failed(e))
        }
    };
    if let Some(report) = report {
        *PENDING.lock() = Some(report);
        HAS_PENDING.store(true, Ordering::Release);
    }
    IN_FLIGHT.store(false, Ordering::Release);
}

fn start() {
    if !IN_FLIGHT.swap(true, Ordering::AcqRel) {
        std::thread::spawn(run_check);
    }
}

/// Run a check now, from the Kova menu. Its answer is shown even when there is
/// nothing new.
pub fn check_now() {
    MANUAL.store(true, Ordering::Release);
    start();
}

/// Called from the global timer on the main thread, about once a minute.
/// Starts the daily check when it is due.
pub fn poll() {
    if !crate::session::owns_session() || IN_FLIGHT.load(Ordering::Acquire) {
        return;
    }
    let next = *NEXT_ATTEMPT
        .lock()
        .get_or_insert_with(|| with_state(|s| s.last_check) + CHECK_INTERVAL_SECS);
    if now_secs() >= next {
        start();
    }
}

/// Split `RELEASE_NOTES.md` into its version heading and its body. The heading
/// must be `# X.Y.Z`; anything else yields `None`.
fn parse_notes(notes: &str) -> Option<(Version, &str)> {
    let (heading, body) = notes.split_once('\n').unwrap_or((notes, ""));
    let version = parse_version(heading.trim().strip_prefix('#')?)?;
    Some((version, body.trim()))
}

/// The notes to show at launch: only when they describe the running version and
/// that version's notes were not shown yet.
fn notes_due<'a>(notes: &'a str, current: Version, shown: Option<&str>) -> Option<&'a str> {
    let (version, body) = parse_notes(notes)?;
    (version == current && !body.is_empty() && shown.and_then(parse_version) != Some(current))
        .then_some(body)
}

/// Called on every tick of the global timer: shows the release notes of a newly
/// installed version, then a finished check's result.
pub fn show_pending(windows: &[Retained<NSWindow>]) {
    if !NOTES_DONE.load(Ordering::Acquire) {
        show_release_notes(windows);
    }
    if !HAS_PENDING.load(Ordering::Acquire) {
        return;
    }
    let mut pending = PENDING.lock();
    let Some(report) = pending.clone() else {
        HAS_PENDING.store(false, Ordering::Release);
        return;
    };
    // Kept when no window can take a sheet yet: retried next tick.
    if show(windows, &report) {
        *pending = None;
        HAS_PENDING.store(false, Ordering::Release);
    }
}

fn show_release_notes(windows: &[Retained<NSWindow>]) {
    if !crate::session::owns_session() {
        NOTES_DONE.store(true, Ordering::Release);
        return;
    }
    let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));
    let shown = with_state(|s| s.notes_shown_version.clone());
    let Some(body) = notes_due(RELEASE_NOTES, current, shown.as_deref()) else {
        NOTES_DONE.store(true, Ordering::Release);
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else { return };
    let Some(window) = sheet_window(mtm, windows) else { return };

    let version = format_version(current);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(&format!("What's new in Kova {}", version)));
    alert.setInformativeText(&NSString::from_str(body));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    let keep = Cell::new(Some(alert.clone()));
    let handler = RcBlock::new(move |_: NSModalResponse| {
        keep.take();
    });
    alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));

    with_state(|s| s.notes_shown_version = Some(version.clone()));
    save_state();
    NOTES_DONE.store(true, Ordering::Release);
    log::info!("Showed release notes for Kova {}", version);
}

/// The key window (or the first one), when it can take a sheet right now.
fn sheet_window(mtm: MainThreadMarker, windows: &[Retained<NSWindow>]) -> Option<Retained<NSWindow>> {
    let app = NSApplication::sharedApplication(mtm);
    let window = app
        .keyWindow()
        .filter(|w| windows.iter().any(|k| std::ptr::eq(&**k, &**w)))
        .or_else(|| windows.first().cloned())?;
    window.attachedSheet().is_none().then_some(window)
}

/// Show the sheet on the key window (or the first one). A sheet rather than
/// `runModal`: a modal run loop would re-enter the global timer that calls us.
/// Returns `false` when no window can take a sheet right now.
fn show(windows: &[Retained<NSWindow>], report: &Outcome) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let Some(window) = sheet_window(mtm, windows) else { return false };

    let current = env!("CARGO_PKG_VERSION");
    let alert = NSAlert::new(mtm);
    let (style, title, text) = match report {
        Outcome::Newer(v) => (
            NSAlertStyle::Informational,
            format!("Kova {} is available", format_version(*v)),
            format!("You are running Kova {}.", current),
        ),
        Outcome::UpToDate => (
            NSAlertStyle::Informational,
            "Kova is up to date".to_string(),
            format!("Kova {} is the latest release.", current),
        ),
        Outcome::Failed(e) => (
            NSAlertStyle::Warning,
            "Could not check for updates".to_string(),
            e.clone(),
        ),
    };
    alert.setAlertStyle(style);
    alert.setMessageText(&NSString::from_str(&title));
    alert.setInformativeText(&NSString::from_str(&text));
    // "OK" first so it is the Return default: a user typing in the terminal
    // when the sheet appears dismisses it instead of opening a browser.
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    let page = match report {
        Outcome::Newer(v) => {
            alert.addButtonWithTitle(&NSString::from_str("View Release"));
            Some(release_page(*v))
        }
        _ => None,
    };

    // Keep the alert alive until the sheet ends, then let it go.
    let keep = Cell::new(Some(alert.clone()));
    let handler = RcBlock::new(move |response: NSModalResponse| {
        keep.take();
        if let (SECOND_BUTTON, Some(page)) = (response, page.as_ref()) {
            let _ = std::process::Command::new("open").arg(page).spawn();
        }
    });
    alert.beginSheetModalForWindow_completionHandler(&window, Some(&handler));

    if let Outcome::Newer(v) = report {
        let version = format_version(*v);
        with_state(|s| s.notified_version = Some(version.clone()));
        save_state();
        log::info!("Update check: announced Kova {}", version);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tags_with_or_without_prefix() {
        assert_eq!(parse_version("v1.10.0"), Some((1, 10, 0)));
        assert_eq!(parse_version("1.10.2"), Some((1, 10, 2)));
        assert_eq!(parse_version("v2"), Some((2, 0, 0)));
        assert_eq!(parse_version("v1.11"), Some((1, 11, 0)));
    }

    #[test]
    fn refuses_what_is_not_a_plain_version() {
        assert_eq!(parse_version("v1.11.0-beta"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn compares_numerically_not_as_text() {
        assert_eq!(outcome((1, 10, 0), (1, 9, 0), None, false), Some(Outcome::Newer((1, 10, 0))));
        assert_eq!(outcome((1, 9, 0), (1, 10, 0), None, false), None);
    }

    #[test]
    fn daily_check_is_silent_when_up_to_date() {
        assert_eq!(outcome((1, 10, 0), (1, 10, 0), None, false), None);
    }

    #[test]
    fn daily_check_announces_a_version_once() {
        assert_eq!(outcome((1, 11, 0), (1, 10, 0), Some("1.11.0"), false), None);
        assert_eq!(
            outcome((1, 12, 0), (1, 10, 0), Some("1.11.0"), false),
            Some(Outcome::Newer((1, 12, 0)))
        );
    }

    #[test]
    fn manual_check_always_answers() {
        assert_eq!(outcome((1, 10, 0), (1, 10, 0), None, true), Some(Outcome::UpToDate));
        assert_eq!(
            outcome((1, 11, 0), (1, 10, 0), Some("1.11.0"), true),
            Some(Outcome::Newer((1, 11, 0)))
        );
    }

    #[test]
    fn release_notes_describe_the_current_version() {
        let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap();
        let (version, body) = parse_notes(RELEASE_NOTES).expect("RELEASE_NOTES.md must start with `# X.Y.Z`");
        assert_eq!(version, current, "RELEASE_NOTES.md was not updated for this version");
        assert!(!body.is_empty());
    }

    #[test]
    fn release_notes_show_once_per_version() {
        let notes = "# 1.12.0\n\n- a fix";
        assert_eq!(notes_due(notes, (1, 12, 0), None), Some("- a fix"));
        assert_eq!(notes_due(notes, (1, 12, 0), Some("1.11.1")), Some("- a fix"));
        assert_eq!(notes_due(notes, (1, 12, 0), Some("1.12.0")), None);
        assert_eq!(notes_due(notes, (1, 13, 0), None), None);
        assert_eq!(notes_due("no heading", (1, 12, 0), None), None);
    }

    #[test]
    fn release_page_is_built_from_the_version() {
        assert_eq!(
            release_page((1, 11, 0)),
            "https://github.com/micktaiwan/kova/releases/tag/v1.11.0"
        );
    }
}
