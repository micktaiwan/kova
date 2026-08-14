//! Desktop notifications, posted by Kova itself.
//!
//! Kova is the app that owns the panes, so it is also the only process that can
//! act on a notification click: `terminal-notifier --execute` no longer runs its
//! command on macOS 26 (it still speaks the removed `NSUserNotification` API), so
//! the click had nowhere to land. Here the notification carries the pane id in its
//! `userInfo`, and clicking it queues that pane for focus on the next tick.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSBundle, NSDictionary, NSNumber, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest,
    UNNotificationResponse, UNNotificationSound, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Pane ids to focus, filled by notification clicks and drained by the tick.
///
/// A `Mutex` rather than the delegate's usual `RefCell`: `UNUserNotificationCenter`
/// does not document which thread it calls its delegate on, and this is the only
/// state that callback touches.
static PENDING_FOCUS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Whether `PENDING_FOCUS` holds anything. Read on every frame, so the common
/// case (no click) must not take the lock.
static HAS_PENDING_FOCUS: AtomicBool = AtomicBool::new(false);

/// Serial number for notification identifiers — two notifications sharing an
/// identifier replace each other in Notification Center.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The key under which the pane id travels in the notification's `userInfo`.
const PANE_ID_KEY: &str = "kova_pane_id";

/// The notification center, or `None` when Kova runs outside an app bundle.
///
/// `currentNotificationCenter` throws when the process has no bundle identifier
/// (running `~/.cargo/target/release/kova` directly instead of `Kova.app`), and an
/// Objective-C exception here would take the app down at launch.
fn center() -> Option<Retained<UNUserNotificationCenter>> {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return None;
    }
    Some(UNUserNotificationCenter::currentNotificationCenter())
}

/// Register `delegate` as the click handler and ask the user for permission.
///
/// Called once at launch: macOS only shows the authorization prompt the first
/// time, and answers from cache afterwards.
pub fn init(delegate: &ProtocolObject<dyn UNUserNotificationCenterDelegate>) {
    let Some(center) = center() else {
        log::debug!("Notifications: no bundle identifier, notifications disabled");
        return;
    };
    center.setDelegate(Some(delegate));
    let options = UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound;
    center.requestAuthorizationWithOptions_completionHandler(
        options,
        &block2::RcBlock::new(|granted: objc2::runtime::Bool, err: *mut objc2_foundation::NSError| {
            if granted.as_bool() {
                log::info!("Notifications: authorized");
            } else {
                let reason = unsafe { err.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "user declined".to_string());
                log::warn!("Notifications: not authorized ({})", reason);
            }
        }),
    );
}

/// Post a notification. `pane_id` is what a click on it will focus.
///
/// Returns `Err` only when notifications are unavailable altogether (no bundle);
/// a notification the user has muted in System Settings still counts as posted.
pub fn post(
    title: &str,
    body: &str,
    pane_id: Option<u32>,
    sound: bool,
) -> Result<(), String> {
    let center = center().ok_or_else(|| "notifications unavailable (no app bundle)".to_string())?;

    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    if sound {
        content.setSound(Some(&UNNotificationSound::defaultSound()));
    }
    if let Some(pane_id) = pane_id {
        let key = NSString::from_str(PANE_ID_KEY);
        let value = NSNumber::new_u32(pane_id);
        let typed = NSDictionary::from_slices::<NSString>(&[&*key], &[&*value]);
        // `userInfo` is an untyped dictionary; the cast only erases the generics,
        // and `handle_response` checks the value's class before reading it back.
        let info: Retained<NSDictionary> = unsafe { Retained::cast_unchecked(typed) };
        unsafe { content.setUserInfo(&info) };
    }

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&format!("kova-{}", id)),
        &content,
        // No trigger — deliver immediately.
        None,
    );
    center.addNotificationRequest_withCompletionHandler(
        &request,
        Some(&block2::RcBlock::new(|err: *mut objc2_foundation::NSError| {
            if let Some(e) = unsafe { err.as_ref() } {
                log::warn!("Notifications: delivery failed: {}", e.localizedDescription());
            }
        })),
    );
    Ok(())
}

/// Handle a click: remember the pane the notification points at.
///
/// The focus itself is deferred to the next tick, which is the only place that
/// is known to run on the main thread with the window list free to borrow.
pub fn handle_response(response: &UNNotificationResponse) {
    let info = response.notification().request().content().userInfo();
    let key = NSString::from_str(PANE_ID_KEY);
    let Some(value) = info.objectForKey(&*key) else {
        log::debug!("Notifications: clicked notification carries no pane id");
        return;
    };
    let Ok(number) = value.downcast::<NSNumber>() else {
        log::warn!("Notifications: pane id in userInfo is not a number");
        return;
    };
    let pane_id = number.as_u32();
    log::info!("Notifications: click on notification for pane {}", pane_id);
    if let Ok(mut pending) = PENDING_FOCUS.lock() {
        pending.push(pane_id);
        HAS_PENDING_FOCUS.store(true, Ordering::Release);
    }
}

/// Take the pane ids queued by clicks since the last call.
pub fn take_pending_focus() -> Vec<u32> {
    if !HAS_PENDING_FOCUS.swap(false, Ordering::Acquire) {
        return Vec::new();
    }
    match PENDING_FOCUS.lock() {
        Ok(mut pending) => std::mem::take(&mut *pending),
        Err(_) => Vec::new(),
    }
}
