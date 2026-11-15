//! The Linux desktop's hot-key table (LLP 1133.025 D11 item 2) for
//! `@exact/global-shortcuts`: the GlobalShortcuts portal
//! (<https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html>),
//! which GNOME 48 and later and KDE Plasma answer. The desktop owns the
//! chord: the first time an app binds one it shows its own dialog, and
//! remembers the answer by the app's id, so the app registers that id first
//! (`org.freedesktop.host.portal.Registry`); GNOME needs a `<id>.desktop`
//! file for it in an `applications` folder.
//!
//! One worker thread holds one bus connection and one portal session with
//! every chord the process reads. A change in the table ends that
//! connection (which closes its session) and binds the whole table again on
//! a new one, after a short pause so a launch's several registrations bind
//! together. A registration answers at once: `Ok` while the desktop is
//! asked, and the desktop's answer reaches the reader through the
//! session's [`Services::watch_reserved`] callback, which asks again. A
//! press is queued with [`crate::desktop::activate_from_any_thread`] and
//! that same callback wakes the loop.

use super::bus::Bus;
use super::wire::Value;
use crate::desktop::{Login, Refusal, Services};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SHORTCUTS: &str = "org.freedesktop.portal.GlobalShortcuts";
const REGISTRY: &str = "org.freedesktop.host.portal.Registry";
/// How long registrations may keep arriving before the table is bound.
const SETTLE: Duration = Duration::from_millis(150);

/// An `aria-keyshortcuts` chord (`Control+Alt+Shift+Meta+3`) as the
/// shortcuts specification's trigger (`CTRL+ALT+SHIFT+LOGO+3`): modifiers
/// in that order, then the key's XKB keysym name. `None` for a chord the
/// portal cannot take: no key, a key twice, or a key with no keysym here.
pub(crate) fn trigger(keys: &str) -> Option<String> {
    const MODIFIERS: [(&str, &str); 4] = [
        ("control", "CTRL"),
        ("alt", "ALT"),
        ("shift", "SHIFT"),
        ("meta", "LOGO"),
    ];
    let mut held = [false; 4];
    let mut key = None;
    for part in keys.split('+') {
        let found = MODIFIERS
            .iter()
            .position(|(name, _)| part.eq_ignore_ascii_case(name));
        match found {
            Some(m) if !held[m] => held[m] = true,
            Some(_) => return None,
            None if key.is_none() => key = Some(keysym(part)?),
            None => return None,
        }
    }
    let mut out: Vec<&str> = (MODIFIERS.iter().zip(held))
        .filter_map(|((_, xdg), on)| on.then_some(*xdg))
        .collect();
    let key = key?;
    out.push(&key);
    Some(out.join("+"))
}

/// A key's `KeyboardEvent.key` name as an XKB keysym name.
fn keysym(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let named = match c {
            'a'..='z' | '0'..='9' => return Some(c.into()),
            'A'..='Z' => return Some(c.to_ascii_lowercase().into()),
            ',' => "comma",
            '.' => "period",
            '/' => "slash",
            ';' => "semicolon",
            '\'' => "apostrophe",
            '[' => "bracketleft",
            ']' => "bracketright",
            '\\' => "backslash",
            '-' => "minus",
            '=' => "equal",
            '`' => "grave",
            _ => return None,
        };
        return Some(named.into());
    }
    if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=24).contains(&n).then(|| format!("F{n}"));
    }
    let named = match key {
        "Enter" => "Return",
        "Escape" | "Tab" | "Delete" | "Insert" | "Home" | "End" | "Pause" => key,
        "Space" | " " => "space",
        "Backspace" => "BackSpace",
        "PageUp" => "Page_Up",
        "PageDown" => "Page_Down",
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "PrintScreen" => "Print",
        "ScrollLock" => "Scroll_Lock",
        _ => return None,
    };
    Some(named.into())
}

/// One chord a session reads.
#[derive(Clone)]
struct Binding {
    session: String,
    id: String,
    keys: String,
    trigger: String,
    extension: &'static str,
    event: &'static str,
    state: State,
}

impl Binding {
    /// Its id in the portal session, unique in the table.
    fn portal_id(&self) -> String {
        format!("{}/{}/{}", self.session, self.id, self.keys)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Asked,
    Bound,
    Refused(Refusal),
}

#[derive(Default)]
struct Table {
    bindings: Vec<Binding>,
    /// Counts changes; the worker binds again when it moves.
    changes: u64,
    /// Each session's callback, which asks its readings again.
    watchers: Vec<(String, Box<dyn Fn() + Send>)>,
    /// The worker's connection, shut down to end it after a change.
    connection: Option<UnixStream>,
    worker: bool,
}

type Shared = Arc<(Mutex<Table>, Condvar)>;

/// The hot-key table, one per process (`crate::desktop::services`).
pub(crate) struct Shortcuts {
    shared: Shared,
    app_id: String,
}

impl Shortcuts {
    pub(crate) fn new(app_id: String) -> Shortcuts {
        Shortcuts {
            shared: Default::default(),
            app_id,
        }
    }

    /// The table changed: bind it again on a new connection.
    fn changed(&self, table: &mut Table) {
        table.changes += 1;
        if let Some(connection) = table.connection.take() {
            let _ = connection.shutdown(std::net::Shutdown::Both);
        }
        self.shared.1.notify_all();
        if !table.worker {
            table.worker = true;
            let (shared, app_id) = (self.shared.clone(), self.app_id.clone());
            let started = std::thread::Builder::new()
                .name("exact-shortcuts".into())
                .spawn(move || work(shared, app_id));
            if started.is_err() {
                table.worker = false;
            }
        }
    }
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Table> {
    shared.0.lock().unwrap_or_else(|e| e.into_inner())
}

impl Services for Shortcuts {
    fn register_hotkey(
        &self,
        session: &str,
        id: &str,
        keys: &str,
        _repeat: bool,
        extension: &'static str,
        event: &'static str,
    ) -> Result<(), Refusal> {
        let trigger = trigger(keys).ok_or("invalid")?;
        let mut table = lock(&self.shared);
        let same = |b: &&Binding| b.session == session && b.id == id && b.keys == keys;
        if let Some(b) = table.bindings.iter().find(same) {
            return match b.state {
                State::Refused(why) => Err(why),
                State::Asked | State::Bound => Ok(()),
            };
        }
        table.bindings.push(Binding {
            session: session.into(),
            id: id.into(),
            keys: keys.into(),
            trigger,
            extension,
            event,
            state: State::Asked,
        });
        self.changed(&mut table);
        Ok(())
    }

    fn unregister_hotkey(&self, session: &str, id: &str, keys: &str) {
        let mut table = lock(&self.shared);
        let before = table.bindings.len();
        (table.bindings).retain(|b| !(b.session == session && b.id == id && b.keys == keys));
        if table.bindings.len() != before {
            self.changed(&mut table);
        }
    }

    fn check_hotkey(&self, keys: &str) -> Result<(), Refusal> {
        trigger(keys).map(drop).ok_or("invalid")
    }

    fn attention(&self, _: bool, _: Option<u32>) -> Result<(), String> {
        Err("this host asks for attention nowhere yet".into())
    }

    fn login(&self) -> Login {
        Login {
            state: "unknown",
            hidden: false,
        }
    }

    fn set_login(&self, _: Option<bool>) -> Result<Login, String> {
        Err("this host keeps no login item yet".into())
    }

    fn open_login_settings(&self) -> Result<(), String> {
        Err("this host keeps no login item yet".into())
    }

    fn login_launch(&self) -> Option<bool> {
        None
    }

    fn watch_reserved(&self, session: &str, changed: Box<dyn Fn() + Send>) {
        let mut table = lock(&self.shared);
        table.watchers.retain(|(s, _)| s != session);
        table.watchers.push((session.into(), changed));
    }

    fn unwatch_reserved(&self, session: &str) {
        lock(&self.shared).watchers.retain(|(s, _)| s != session);
    }
}

/// The worker: bind the table whenever it changes, and hear its presses
/// in between.
fn work(shared: Shared, app_id: String) {
    let mut bound_at = 0;
    loop {
        let wanted = {
            let mut table = lock(&shared);
            while table.changes == bound_at {
                table = (shared.1).wait(table).unwrap_or_else(|e| e.into_inner());
            }
            drop(table);
            std::thread::sleep(SETTLE);
            let table = lock(&shared);
            bound_at = table.changes;
            table.bindings.clone()
        };
        if wanted.is_empty() {
            continue;
        }
        let answer = bind(&wanted, &app_id);
        let mut table = lock(&shared);
        if table.changes != bound_at {
            continue;
        }
        let (bus, session) = match answer {
            Ok((bus, session, bound)) => {
                for b in &mut table.bindings {
                    b.state = match bound.contains(&b.portal_id()) {
                        true => State::Bound,
                        false => State::Refused("not-allowed"),
                    };
                }
                (Some(bus), session)
            }
            Err(why) => {
                eprintln!("exact: global shortcuts: {why}");
                for b in &mut table.bindings {
                    b.state = State::Refused("not-allowed");
                }
                (None, String::new())
            }
        };
        for (_, ask) in &table.watchers {
            ask();
        }
        let Some(mut bus) = bus else { continue };
        table.connection = bus.closer();
        drop(table);
        hear(&shared, &mut bus, &session);
    }
}

/// Register the app, open a session and bind `wanted` in it: the
/// connection (which keeps the session), the session, and the portal ids
/// the desktop bound.
fn bind(wanted: &[Binding], app_id: &str) -> Result<(Bus, String, Vec<String>), String> {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .map_err(|_| "there is no session bus".to_string())?;
    let mut bus = Bus::connect(&address, super::TIMEOUT)?;
    // Before any other portal call. A portal without the registry (before
    // xdg-desktop-portal 1.19) names the app some other way.
    if let Err(why) = bus.call(
        PORTAL,
        PORTAL_PATH,
        REGISTRY,
        "Register",
        &[Value::str(app_id), Value::dict(vec![])],
    ) {
        if !why.contains("UnknownMethod") && !why.contains("UnknownInterface") {
            return Err(format!("the desktop does not know {app_id}: {why}"));
        }
    }
    bus.subscribe(&format!(
        "type='signal',interface='{SHORTCUTS}',member='Activated'"
    ))?;
    let options = Value::dict(vec![
        ("handle_token", Value::str("exact_session")),
        ("session_handle_token", Value::str("exact_shortcuts")),
    ]);
    let created = ask(&mut bus, "exact_session", "CreateSession", &[options])?;
    let Some(Value::Str(session)) = created.get("session_handle") else {
        return Err("the portal opened no session".into());
    };
    let session = session.clone();
    // The person may take as long as they like in the desktop's dialog.
    bus.wait_forever();
    let results = ask(
        &mut bus,
        "exact_bind",
        "BindShortcuts",
        &bind_body(&session, wanted, "exact_bind"),
    )?;
    Ok((bus, session, bound(&results)))
}

/// BindShortcuts' arguments: the session, each chord with its id as its
/// description, no parent window, and the request's token.
fn bind_body(session: &str, wanted: &[Binding], token: &str) -> Vec<Value> {
    let shortcuts = wanted
        .iter()
        .map(|b| {
            Value::Struct(vec![
                Value::Str(b.portal_id()),
                Value::dict(vec![
                    ("description", Value::str(&b.id)),
                    ("preferred_trigger", Value::str(&b.trigger)),
                ]),
            ])
        })
        .collect();
    vec![
        Value::Path(session.into()),
        Value::Array("(sa{sv})".into(), shortcuts),
        Value::str(""),
        Value::dict(vec![("handle_token", Value::str(token))]),
    ]
}

/// The ids a BindShortcuts answer's `shortcuts` (`a(sa{sv})`) holds.
fn bound(results: &Value) -> Vec<String> {
    let Some(Value::Array(_, shortcuts)) = results.get("shortcuts") else {
        return Vec::new();
    };
    (shortcuts.iter())
        .filter_map(|s| match s {
            Value::Struct(fields) => match fields.first() {
                Some(Value::Str(id)) => Some(id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Call a portal method that answers on a Request object under `token`,
/// and wait for that answer's results. A person's cancel and the portal's
/// own end are errors.
fn ask(bus: &mut Bus, token: &str, member: &str, body: &[Value]) -> Result<Value, String> {
    let path = super::request_path(&bus.unique, token);
    bus.subscribe(&format!(
        "type='signal',interface='org.freedesktop.portal.Request',member='Response',path='{path}'"
    ))?;
    bus.call(PORTAL, PORTAL_PATH, SHORTCUTS, member, body)?;
    loop {
        let m = bus.signal()?;
        if m.member != "Response" || m.path != path {
            continue;
        }
        return match m.body.as_slice() {
            [Value::U32(0), results] => Ok(results.clone()),
            [Value::U32(1), ..] => Err(format!("{member}: the person declined")),
            _ => Err(format!("{member}: the portal ended the request")),
        };
    }
}

/// Hear presses on `session` until the connection ends.
fn hear(shared: &Shared, bus: &mut Bus, session: &str) {
    while let Ok(m) = bus.signal() {
        if m.interface != SHORTCUTS || m.member != "Activated" {
            continue;
        }
        let [Value::Path(on), Value::Str(id), ..] = m.body.as_slice() else {
            continue;
        };
        if on != session {
            continue;
        }
        let table = lock(shared);
        let Some(b) = table.bindings.iter().find(|b| b.portal_id() == *id) else {
            continue;
        };
        let args = serde_json::json!([b.id, { "id": b.id, "keys": b.keys }]).to_string();
        crate::desktop::activate_from_any_thread(b.extension, b.event, args);
        // Wakes the loop, which raises the press on its next turn.
        if let Some((_, ask)) = table.watchers.iter().find(|(s, _)| *s == b.session) {
            ask();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portal::wire;

    #[test]
    fn a_chord_becomes_a_portal_trigger() {
        for (keys, xdg) in [
            ("Control+Alt+Shift+Meta+3", Some("CTRL+ALT+SHIFT+LOGO+3")),
            ("Meta+Shift+A", Some("SHIFT+LOGO+a")),
            ("shift+control+F12", Some("CTRL+SHIFT+F12")),
            ("PrintScreen", Some("Print")),
            ("Alt+ArrowLeft", Some("ALT+Left")),
            ("Control+,", Some("CTRL+comma")),
            ("Control+Space", Some("CTRL+space")),
            // A modifier alone, a modifier twice, two keys, a key with no keysym here.
            ("Meta+Meta", None),
            ("Control+Control+A", None),
            ("A+B", None),
            ("Control+MediaPlayPause", None),
            ("F25", None),
        ] {
            assert_eq!(trigger(keys).as_deref(), xdg, "{keys}");
        }
    }

    fn binding(id: &str, keys: &str) -> Binding {
        Binding {
            session: "1".into(),
            id: id.into(),
            keys: keys.into(),
            trigger: trigger(keys).unwrap(),
            extension: "global-shortcuts",
            event: "globalshortcut",
            state: State::Asked,
        }
    }

    #[test]
    fn a_bind_is_framed_as_the_portal_takes_it_and_its_answer_read() {
        let wanted = [
            binding("screen", "Control+Alt+Shift+Meta+3"),
            binding("area", "Control+Alt+Shift+Meta+4"),
        ];
        let body = bind_body(
            "/org/freedesktop/portal/desktop/session/1_42/s",
            &wanted,
            "t",
        );
        let call = wire::message(
            wire::METHOD_CALL,
            7,
            None,
            PORTAL,
            PORTAL_PATH,
            SHORTCUTS,
            "BindShortcuts",
            &body,
        );
        let read = wire::parse(&call).unwrap();
        assert_eq!(read.signature, "oa(sa{sv})sa{sv}");
        assert_eq!(read.body, body);
        let Value::Array(_, shortcuts) = &read.body[1] else {
            panic!()
        };
        let Value::Struct(first) = &shortcuts[0] else {
            panic!()
        };
        assert_eq!(first[0], Value::str("1/screen/Control+Alt+Shift+Meta+3"));
        assert_eq!(
            first[1].get("preferred_trigger"),
            Some(&Value::str("CTRL+ALT+SHIFT+LOGO+3"))
        );

        // GNOME 50's answer when the person adds only the first.
        let answer = Value::dict(vec![(
            "shortcuts",
            Value::Array(
                "(sa{sv})".into(),
                vec![Value::Struct(vec![
                    Value::str("1/screen/Control+Alt+Shift+Meta+3"),
                    Value::dict(vec![(
                        "trigger_description",
                        Value::str("Press <Shift><Control><Alt><Super>3"),
                    )]),
                ])],
            ),
        )]);
        assert_eq!(bound(&answer), ["1/screen/Control+Alt+Shift+Meta+3"]);
        assert!(bound(&Value::dict(vec![])).is_empty());
    }

    /// Against a live desktop: `EXACT_SHORTCUTS_APP_ID=<id with a .desktop file> cargo test -p exact-painted
    /// --lib live_ -- --ignored --nocapture`, approving the desktop's dialog if it shows one, then pressing
    /// Control+Alt+Shift+Super+3 within a minute.
    #[test]
    #[ignore]
    fn live_a_bound_chord_is_heard() {
        let table = Shortcuts::new(std::env::var("EXACT_SHORTCUTS_APP_ID").unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        table.watch_reserved(
            "1",
            Box::new(move || {
                let _ = tx.send(());
            }),
        );
        let keys = "Control+Alt+Shift+Meta+3";
        let register = || {
            table.register_hotkey(
                "1",
                "screen",
                keys,
                false,
                "global-shortcuts",
                "globalshortcut",
            )
        };
        assert_eq!(register(), Ok(()));
        rx.recv_timeout(Duration::from_secs(60))
            .expect("the desktop answered");
        assert_eq!(register(), Ok(()), "bound");
        println!("bound; press {keys}");
        rx.recv_timeout(Duration::from_secs(60)).expect("a press");
        assert_eq!(
            crate::desktop::take_far(),
            [r#"["screen",{"id":"screen","keys":"Control+Alt+Shift+Meta+3"}]"#]
        );
        table.unregister_hotkey("1", "screen", keys);
    }

    #[test]
    fn a_registration_answers_at_once_and_a_bad_chord_is_invalid() {
        let table = Shortcuts::new("dev.example.app".into());
        assert_eq!(table.check_hotkey("Meta+Meta"), Err("invalid"));
        assert_eq!(
            table.register_hotkey(
                "1",
                "x",
                "Meta+Meta",
                false,
                "global-shortcuts",
                "globalshortcut"
            ),
            Err("invalid")
        );
        assert_eq!(table.check_hotkey("Control+Alt+K"), Ok(()));
        // No bus here: the worker refuses later; the registration is asked.
        let none = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none();
        if none {
            assert_eq!(
                table.register_hotkey(
                    "1",
                    "k",
                    "Control+Alt+K",
                    false,
                    "global-shortcuts",
                    "globalshortcut"
                ),
                Ok(())
            );
            std::thread::sleep(SETTLE * 3);
            assert_eq!(
                table.register_hotkey(
                    "1",
                    "k",
                    "Control+Alt+K",
                    false,
                    "global-shortcuts",
                    "globalshortcut"
                ),
                Err("not-allowed")
            );
        }
    }
}
