//! The Linux desktop's light or dark preference, which `setScheme("system")`
//! follows: the Settings portal's `org.freedesktop.appearance`
//! `color-scheme` (<https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Settings.html>),
//! where 1 prefers dark, 2 prefers light and 0 says nothing.
//!
//! [`watch`] reads it once before the first frame, so a dark desktop never
//! sees a light one, then hears `SettingChanged` on a thread of its own.

use super::bus::Bus;
use super::wire::Value;
use std::time::Duration;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";
const NAMESPACE: &str = "org.freedesktop.appearance";
const KEY: &str = "color-scheme";
/// How long a launch waits on the portal before it draws light.
const STARTUP: Duration = Duration::from_millis(500);

/// Whether a `color-scheme` value prefers dark. `ReadOne` answers one
/// variant; the older `Read` (and some portals' `SettingChanged`) another
/// around it.
pub(crate) fn dark(value: &Value) -> Option<bool> {
    match value {
        Value::Variant(inner) => dark(inner),
        Value::U32(scheme) => Some(*scheme == 1),
        _ => None,
    }
}

/// Whether the desktop prefers dark now, and `changed` with each change
/// after. `None`: no session bus or no Settings portal, so nothing is
/// heard.
pub fn watch(changed: impl Fn(bool) + Send + 'static) -> Option<bool> {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()?;
    let mut bus = Bus::connect(&address, STARTUP).ok()?;
    let rule = format!(
        "type='signal',interface='{SETTINGS}',member='SettingChanged',arg0='{NAMESPACE}',arg1='{KEY}'"
    );
    bus.subscribe(&rule).ok()?;
    let body = bus
        .call(
            PORTAL,
            PORTAL_PATH,
            SETTINGS,
            "ReadOne",
            &[Value::str(NAMESPACE), Value::str(KEY)],
        )
        .ok()?;
    let now = dark(body.first()?)?;
    bus.wait_forever();
    std::thread::Builder::new()
        .name("exact-appearance".into())
        .spawn(move || {
            while let Ok(m) = bus.signal() {
                if let [Value::Str(namespace), Value::Str(key), value] = m.body.as_slice() {
                    if namespace == NAMESPACE && key == KEY {
                        if let Some(dark) = dark(value) {
                            changed(dark);
                        }
                    }
                }
            }
        })
        .ok()?;
    Some(now)
}
