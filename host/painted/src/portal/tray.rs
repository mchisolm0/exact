//! A status item on the Linux desktop (LLP 1133.025 D4): a
//! StatusNotifierItem (<https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/>)
//! with its menu as a `com.canonical.dbusmenu` object, which the bar's tray
//! (KDE Plasma, Waybar, Quickshell, GNOME's AppIndicator extension) shows
//! with its own menus. Wayland lets no app place a window at the tray, so
//! the menu is handed over as rows rather than painted.
//!
//! One worker thread holds one bus connection, answers the tray's calls from
//! the item as [`Tray::show`] last gave it, and queues what the person did
//! for [`Tray::take`], waking the loop. The item's bus name is owned while
//! an item is shown, and registered again whenever the bar's watcher
//! starts again (as Omarchy's bar does on a theme change).

use super::bus::{Bus, Sender};
use super::wire::{self, Message, Value};
use crate::presenter::top::StatusEntry as Entry;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";
const MENU_PATH: &str = "/MenuBar";
const UNKNOWN: &str = "org.freedesktop.DBus.Error.UnknownMethod";
/// How long a launch waits on the bus before it places no item.
const STARTUP: Duration = Duration::from_millis(500);

/// The status item as the tray shows it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Item {
    /// Its title and tooltip: the button's label.
    pub title: String,
    /// Its picture at each side drawn, as straight (not premultiplied) RGBA
    /// rows. Empty: the app's own icon, by its id.
    pub icons: Vec<(u32, Vec<u8>)>,
    /// A primary click activates it ([`Heard::Activate`]); otherwise the
    /// tray opens its menu.
    pub activates: bool,
    /// The rows the tray's menu shows, as the presenter reads them.
    pub menu: Vec<Entry>,
}

/// What the person did with the item.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Heard {
    /// A primary click on an item that [`Item::activates`].
    Activate,
    /// A menu row, by its button ([`Entry::Row`]'s `node`).
    Chosen(u32),
}

/// One node of the menu as the tray reads it: the root is id 0.
#[derive(Debug, Clone, PartialEq)]
struct Node {
    props: Vec<(&'static str, Value)>,
    children: Vec<i32>,
    key: Option<u32>,
}

/// What the worker answers from, and what it heard.
struct Shared {
    item: Option<Item>,
    /// The menu by id, and its layout's revision.
    nodes: Vec<Node>,
    revision: u32,
    heard: Vec<Heard>,
    wake: Box<dyn Fn() + Send>,
}

/// The app's status item on the session bus.
pub struct Tray {
    sender: Arc<Sender>,
    shared: Arc<Mutex<Shared>>,
    /// The bus name the item is known by while it is shown.
    name: String,
    closer: Option<UnixStream>,
}

impl Tray {
    /// Connect and listen, when a tray is there to show an item: `None`
    /// with no session bus or no StatusNotifierWatcher. `wake` is called
    /// from the worker whenever [`Tray::take`] has something.
    pub fn start(app_id: &str, wake: impl Fn() + Send + 'static) -> Option<Tray> {
        let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok()?;
        Tray::on(&address, app_id, wake)
    }

    /// [`Tray::start`] on the bus at `address`.
    fn on(address: &str, app_id: &str, wake: impl Fn() + Send + 'static) -> Option<Tray> {
        let mut bus = Bus::connect(address, STARTUP).ok()?;
        let watched = bus.call(
            DBUS,
            DBUS_PATH,
            DBUS,
            "NameHasOwner",
            &[Value::str(WATCHER)],
        );
        if !matches!(watched.ok()?.as_slice(), [Value::Bool(true)]) {
            return None;
        }
        let rule = format!(
            "type='signal',sender='{DBUS}',interface='{DBUS}',member='NameOwnerChanged',arg0='{WATCHER}'"
        );
        bus.subscribe(&rule).ok()?;
        bus.wait_forever();
        let sender = Arc::new(bus.sender().ok()?);
        let shared = Arc::new(Mutex::new(Shared {
            item: None,
            nodes: nodes(&[]),
            revision: 1,
            heard: Vec::new(),
            wake: Box::new(wake),
        }));
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let closer = bus.closer();
        let worker = Worker {
            sender: sender.clone(),
            shared: shared.clone(),
            name: name.clone(),
            app_id: app_id.to_owned(),
        };
        std::thread::Builder::new()
            .name("exact-tray".into())
            .spawn(move || {
                while let Ok(m) = bus.next() {
                    worker.heard(&m);
                }
            })
            .ok()?;
        Some(Tray {
            sender,
            shared,
            name,
            closer,
        })
    }

    /// Show `item`, or none: the tray reads it again where it changed.
    pub fn show(&self, item: Option<Item>) {
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        if shared.item == item {
            return;
        }
        let before = std::mem::replace(&mut shared.item, item.clone());
        let Some(item) = item else {
            let name = Value::str(&self.name);
            self.sender
                .call(DBUS, DBUS_PATH, DBUS, "ReleaseName", &[name]);
            return;
        };
        if item.menu != before.as_ref().map_or(Vec::new(), |b| b.menu.clone()) {
            shared.nodes = nodes(&item.menu);
            shared.revision += 1;
        }
        let Some(before) = before else {
            register(&self.sender, &self.name);
            return;
        };
        if item.title != before.title {
            self.sender.signal(ITEM_PATH, ITEM, "NewTitle", &[]);
            self.sender.signal(ITEM_PATH, ITEM, "NewToolTip", &[]);
        }
        if item.icons != before.icons {
            self.sender.signal(ITEM_PATH, ITEM, "NewIcon", &[]);
        }
        if item.activates != before.activates {
            // No signal names ItemIsMenu; trays read it with the status.
            self.sender
                .signal(ITEM_PATH, ITEM, "NewStatus", &[Value::str("Active")]);
        }
        if item.menu != before.menu {
            let revision = Value::U32(shared.revision);
            let layout = [revision, Value::I32(0)];
            self.sender
                .signal(MENU_PATH, MENU, "LayoutUpdated", &layout);
        }
    }

    /// What the person did since the last call.
    pub fn take(&self) -> Vec<Heard> {
        self.shared
            .lock()
            .map(|mut shared| std::mem::take(&mut shared.heard))
            .unwrap_or_default()
    }
}

impl Drop for Tray {
    /// The connection ends, and with it the item and the worker.
    fn drop(&mut self) {
        if let Some(closer) = &self.closer {
            let _ = closer.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Own `name` and tell the watcher the item is there. Neither waits: the bus
/// takes a connection's messages in order, so the name is owned before the
/// watcher asks for the item.
fn register(sender: &Sender, name: &str) {
    sender.call(
        DBUS,
        DBUS_PATH,
        DBUS,
        "RequestName",
        &[Value::str(name), Value::U32(4)],
    );
    sender.call(
        WATCHER,
        WATCHER_PATH,
        WATCHER,
        "RegisterStatusNotifierItem",
        &[Value::str(name)],
    );
}

/// The menu by dbusmenu id: the root, then each entry in order.
fn nodes(menu: &[Entry]) -> Vec<Node> {
    fn add(entries: &[Entry], out: &mut Vec<Node>) -> Vec<i32> {
        let mut ids = Vec::new();
        for entry in entries {
            let id = out.len() as i32;
            ids.push(id);
            out.push(Node {
                props: Vec::new(),
                children: Vec::new(),
                key: None,
            });
            let (props, key, children) = match entry {
                Entry::Separator => (vec![("type", Value::str("separator"))], None, Vec::new()),
                Entry::Header(label) => (
                    vec![
                        ("label", Value::str(&mnemonic(label))),
                        ("enabled", Value::Bool(false)),
                    ],
                    None,
                    Vec::new(),
                ),
                Entry::Row {
                    node: key,
                    label,
                    enabled,
                    checked,
                    children,
                } => {
                    let mut props = vec![
                        ("label", Value::str(&mnemonic(label))),
                        ("enabled", Value::Bool(*enabled)),
                    ];
                    if let Some(checked) = checked {
                        props.push(("toggle-type", Value::str("checkmark")));
                        props.push(("toggle-state", Value::I32(i32::from(*checked))));
                    }
                    if !children.is_empty() {
                        props.push(("children-display", Value::str("submenu")));
                    }
                    let children = add(children, out);
                    (props, Some(*key), children)
                }
            };
            let node = &mut out[id as usize];
            (node.props, node.key, node.children) = (props, key, children);
        }
        ids
    }
    let mut out = vec![Node {
        props: vec![("children-display", Value::str("submenu"))],
        children: Vec::new(),
        key: None,
    }];
    out[0].children = add(menu, &mut out);
    out
}

/// A label as dbusmenu reads it, where `_` marks an access key.
fn mnemonic(label: &str) -> String {
    label.replace('_', "__")
}

/// `props` as an `a{sv}`, only those `names` asks for when it names any.
fn props(props: &[(&'static str, Value)], names: &[String]) -> Value {
    Value::dict(
        props
            .iter()
            .filter(|(name, _)| names.is_empty() || names.iter().any(|n| n == name))
            .map(|(name, value)| (*name, value.clone()))
            .collect(),
    )
}

/// Node `id` and its children to `depth` (-1: all), as `(ia{sv}av)`.
fn layout(nodes: &[Node], id: i32, depth: i64, names: &[String]) -> Option<Value> {
    let node = nodes.get(usize::try_from(id).ok()?)?;
    let children = if depth == 0 {
        Vec::new()
    } else {
        (node.children.iter())
            .filter_map(|child| layout(nodes, *child, depth - 1, names))
            .map(|child| Value::Variant(Box::new(child)))
            .collect()
    };
    Some(Value::Struct(vec![
        Value::I32(id),
        props(&node.props, names),
        Value::Array("v".into(), children),
    ]))
}

/// An `as` or `ai` argument's elements.
fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(_, items)) => (items.iter())
            .filter_map(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Num(n) => Some(*n),
        Value::U32(n) => Some(i64::from(*n)),
        _ => None,
    }
}

/// Straight RGBA rows as the item's `a(iiay)`: ARGB32 in network order.
fn pixmaps(icons: &[(u32, Vec<u8>)]) -> Value {
    let icons = icons.iter().map(|(side, rgba)| {
        let argb = (rgba.as_chunks::<4>().0.iter())
            .flat_map(|[r, g, b, a]| [*a, *r, *g, *b])
            .map(Value::Byte)
            .collect();
        Value::Struct(vec![
            Value::I32(*side as i32),
            Value::I32(*side as i32),
            Value::Array("y".into(), argb),
        ])
    });
    Value::Array("(iiay)".into(), icons.collect())
}

struct Worker {
    sender: Arc<Sender>,
    shared: Arc<Mutex<Shared>>,
    name: String,
    app_id: String,
}

impl Worker {
    fn heard(&self, m: &Message) {
        match m.kind {
            wire::METHOD_CALL => self.call(m),
            // The bar's watcher started again: it knows no item until told.
            wire::SIGNAL if m.member == "NameOwnerChanged" => {
                let started = matches!(m.body.get(2), Some(Value::Str(owner)) if !owner.is_empty());
                let shown = self.shared.lock().is_ok_and(|s| s.item.is_some());
                if started && shown {
                    register(&self.sender, &self.name);
                }
            }
            _ => {}
        }
    }

    fn call(&self, m: &Message) {
        let Ok(mut shared) = self.shared.lock() else {
            return;
        };
        let reply = match (m.path.as_str(), m.member.as_str()) {
            (_, "Ping") => Some(Vec::new()),
            (ITEM_PATH | MENU_PATH, "Introspect") => {
                Some(vec![Value::str(&introspection(&m.path))])
            }
            (ITEM_PATH, "Get" | "GetAll") if m.interface == PROPERTIES => {
                let item = shared.item.clone().unwrap_or_default();
                properties(m, &self.item_props(&item))
            }
            (MENU_PATH, "Get" | "GetAll") if m.interface == PROPERTIES => {
                properties(m, &menu_props())
            }
            (ITEM_PATH, "Activate") => {
                shared.heard.push(Heard::Activate);
                (shared.wake)();
                Some(Vec::new())
            }
            (
                ITEM_PATH,
                "SecondaryActivate" | "ContextMenu" | "Scroll" | "ProvideXdgActivationToken",
            ) => Some(Vec::new()),
            (MENU_PATH, "GetLayout") => {
                let parent = int(m.body.first()).unwrap_or(0) as i32;
                let depth = int(m.body.get(1)).unwrap_or(-1);
                let names = strings(m.body.get(2));
                layout(&shared.nodes, parent, depth, &names)
                    .map(|layout| vec![Value::U32(shared.revision), layout])
            }
            (MENU_PATH, "GetGroupProperties") => {
                let ids: Vec<i64> = match m.body.first() {
                    Some(Value::Array(_, ids)) => {
                        ids.iter().filter_map(|id| int(Some(id))).collect()
                    }
                    _ => Vec::new(),
                };
                let names = strings(m.body.get(1));
                let found = ids.into_iter().filter_map(|id| {
                    let node = shared.nodes.get(usize::try_from(id).ok()?)?;
                    Some(Value::Struct(vec![
                        Value::I32(id as i32),
                        props(&node.props, &names),
                    ]))
                });
                Some(vec![Value::Array("(ia{sv})".into(), found.collect())])
            }
            (MENU_PATH, "GetProperty") => {
                let node =
                    int(m.body.first()).and_then(|id| shared.nodes.get(usize::try_from(id).ok()?));
                let name = strings(Some(&Value::Array(
                    "s".into(),
                    m.body.get(1).cloned().into_iter().collect(),
                )));
                node.and_then(|node| {
                    node.props
                        .iter()
                        .find(|(n, _)| name.first().is_some_and(|w| w == n))
                })
                .map(|(_, value)| vec![Value::Variant(Box::new(value.clone()))])
            }
            (MENU_PATH, "Event") => {
                let id = int(m.body.first());
                let clicked = matches!(m.body.get(1), Some(Value::Str(e)) if e == "clicked");
                self.choose(&mut shared, id, clicked);
                Some(Vec::new())
            }
            (MENU_PATH, "EventGroup") => {
                if let Some(Value::Array(_, events)) = m.body.first() {
                    for event in events {
                        if let Value::Struct(fields) = event {
                            let clicked =
                                matches!(fields.get(1), Some(Value::Str(e)) if e == "clicked");
                            self.choose(&mut shared, int(fields.first()), clicked);
                        }
                    }
                }
                Some(vec![Value::Array("i".into(), Vec::new())])
            }
            (MENU_PATH, "AboutToShow") => Some(vec![Value::Bool(false)]),
            (MENU_PATH, "AboutToShowGroup") => Some(vec![
                Value::Array("i".into(), Vec::new()),
                Value::Array("i".into(), Vec::new()),
            ]),
            _ => None,
        };
        drop(shared);
        match reply {
            Some(body) => self.sender.reply(m, &body),
            None => self
                .sender
                .error(m, UNKNOWN, &format!("no {} here", m.member)),
        }
    }

    fn choose(&self, shared: &mut Shared, id: Option<i64>, clicked: bool) {
        let key = id
            .and_then(|id| shared.nodes.get(usize::try_from(id).ok()?))
            .and_then(|node| node.key);
        if let Some(key) = key.filter(|_| clicked) {
            shared.heard.push(Heard::Chosen(key));
            (shared.wake)();
        }
    }

    fn item_props(&self, item: &Item) -> Vec<(&'static str, Value)> {
        let none = || pixmaps(&[]);
        // With no picture drawn, the app's icon by its id, as its desktop
        // file names it.
        let icon_name = if item.icons.is_empty() {
            self.app_id.as_str()
        } else {
            ""
        };
        vec![
            ("Category", Value::str("ApplicationStatus")),
            ("Id", Value::str(&self.app_id)),
            ("Title", Value::str(&item.title)),
            ("Status", Value::str("Active")),
            ("WindowId", Value::I32(0)),
            ("IconName", Value::str(icon_name)),
            ("IconThemePath", Value::str("")),
            ("IconPixmap", pixmaps(&item.icons)),
            ("OverlayIconName", Value::str("")),
            ("OverlayIconPixmap", none()),
            ("AttentionIconName", Value::str("")),
            ("AttentionIconPixmap", none()),
            ("AttentionMovieName", Value::str("")),
            (
                "ToolTip",
                Value::Struct(vec![
                    Value::str(""),
                    none(),
                    Value::str(&item.title),
                    Value::str(""),
                ]),
            ),
            ("ItemIsMenu", Value::Bool(!item.activates)),
            ("Menu", Value::Path(MENU_PATH.into())),
        ]
    }
}

fn menu_props() -> Vec<(&'static str, Value)> {
    vec![
        ("Version", Value::U32(3)),
        ("TextDirection", Value::str("ltr")),
        ("Status", Value::str("normal")),
        ("IconThemePath", Value::Array("s".into(), Vec::new())),
    ]
}

/// `Get(interface, name)` or `GetAll(interface)` answered from `all`.
fn properties(m: &Message, all: &[(&'static str, Value)]) -> Option<Vec<Value>> {
    if m.member == "GetAll" {
        return Some(vec![props(all, &[])]);
    }
    let Some(Value::Str(name)) = m.body.get(1) else {
        return None;
    };
    let (_, value) = all.iter().find(|(n, _)| n == name)?;
    Some(vec![Value::Variant(Box::new(value.clone()))])
}

/// What the object at `path` answers, for a tray that asks first.
fn introspection(path: &str) -> String {
    let body = if path == ITEM_PATH {
        r#"<interface name="org.kde.StatusNotifierItem"><method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method><method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method><method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method><method name="Scroll"><arg type="i" direction="in"/><arg type="s" direction="in"/></method><signal name="NewTitle"/><signal name="NewIcon"/><signal name="NewToolTip"/><signal name="NewStatus"><arg type="s"/></signal><property name="Category" type="s" access="read"/><property name="Id" type="s" access="read"/><property name="Title" type="s" access="read"/><property name="Status" type="s" access="read"/><property name="WindowId" type="i" access="read"/><property name="IconName" type="s" access="read"/><property name="IconThemePath" type="s" access="read"/><property name="IconPixmap" type="a(iiay)" access="read"/><property name="OverlayIconName" type="s" access="read"/><property name="OverlayIconPixmap" type="a(iiay)" access="read"/><property name="AttentionIconName" type="s" access="read"/><property name="AttentionIconPixmap" type="a(iiay)" access="read"/><property name="AttentionMovieName" type="s" access="read"/><property name="ToolTip" type="(sa(iiay)ss)" access="read"/><property name="ItemIsMenu" type="b" access="read"/><property name="Menu" type="o" access="read"/></interface>"#
    } else {
        r#"<interface name="com.canonical.dbusmenu"><method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method><method name="GetGroupProperties"><arg type="ai" direction="in"/><arg type="as" direction="in"/><arg type="a(ia{sv})" direction="out"/></method><method name="GetProperty"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method><method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="in"/><arg type="u" direction="in"/></method><method name="EventGroup"><arg type="a(isvu)" direction="in"/><arg type="ai" direction="out"/></method><method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method><method name="AboutToShowGroup"><arg type="ai" direction="in"/><arg type="ai" direction="out"/><arg type="ai" direction="out"/></method><signal name="LayoutUpdated"><arg type="u"/><arg type="i"/></signal><signal name="ItemsPropertiesUpdated"><arg type="a(ia{sv})"/><arg type="a(ias)"/></signal><property name="Version" type="u" access="read"/><property name="TextDirection" type="s" access="read"/><property name="Status" type="s" access="read"/><property name="IconThemePath" type="as" access="read"/></interface>"#
    };
    format!(
        r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN" "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd"><node><interface name="org.freedesktop.DBus.Properties"><method name="Get"><arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="v" direction="out"/></method><method name="GetAll"><arg type="s" direction="in"/><arg type="a{{sv}}" direction="out"/></method></interface>{body}</node>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: u32, label: &str, children: Vec<Entry>) -> Entry {
        Entry::Row {
            node: key,
            label: label.into(),
            enabled: true,
            checked: None,
            children,
        }
    }

    #[test]
    fn a_menu_is_numbered_depth_first_under_the_root() {
        let menu = [
            row(7, "Open_Tally", vec![]),
            Entry::Separator,
            row(9, "More", vec![row(11, "Inner", vec![])]),
        ];
        let nodes = nodes(&menu);
        assert_eq!(nodes[0].children, vec![1, 2, 3]);
        assert_eq!(nodes[3].children, vec![4]);
        assert_eq!((nodes[1].key, nodes[4].key), (Some(7), Some(11)));
        assert_eq!(nodes[1].props[0], ("label", Value::str("Open__Tally")));
        assert_eq!(nodes[2].props, vec![("type", Value::str("separator"))]);
    }

    #[test]
    fn a_layout_stops_at_its_depth() {
        let nodes = nodes(&[row(1, "More", vec![row(2, "Inner", vec![])])]);
        let children = |v: &Value| match v {
            Value::Struct(f) => match &f[2] {
                Value::Array(_, c) => c.len(),
                _ => 0,
            },
            _ => 0,
        };
        assert_eq!(children(&layout(&nodes, 0, 0, &[]).unwrap()), 0);
        assert_eq!(children(&layout(&nodes, 0, -1, &[]).unwrap()), 1);
        assert!(layout(&nodes, 9, -1, &[]).is_none());
    }

    #[test]
    fn an_icon_is_argb_in_network_order() {
        let Value::Array(_, icons) = pixmaps(&[(1, vec![10, 20, 30, 255])]) else {
            panic!()
        };
        let Value::Struct(fields) = &icons[0] else {
            panic!()
        };
        assert_eq!(
            fields[2],
            Value::Array("y".into(), [255, 10, 20, 30].map(Value::Byte).to_vec())
        );
    }
}
