//! Hyprland's GlobalShortcuts portal (xdg-desktop-portal-hyprland)
//! registers a shortcut but ignores its preferred trigger: the shortcut
//! fires only through a `global` bind in Hyprland's config. So each chord
//! the portal bound is bound in Hyprland too, at run time over its IPC
//! socket (<https://wiki.hypr.land/IPC/>), and again after each config
//! reload, which drops run-time binds. A chord Hyprland already binds to
//! something else stays theirs, and its shortcut is refused as in use.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, Once};

/// The last chords asked for, as (trigger, portal id), and the Hyprland
/// chords bound for them here, which a new table unbinds.
static STATE: Mutex<(Vec<(String, String)>, Vec<String>)> = Mutex::new((Vec::new(), Vec::new()));
static WATCH: Once = Once::new();

fn socket(name: &str) -> Option<UnixStream> {
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let run = std::env::var("XDG_RUNTIME_DIR").ok()?;
    UnixStream::connect(format!("{run}/hypr/{signature}/{name}")).ok()
}

/// One request to Hyprland and its answer.
fn hyprctl(command: &str) -> Option<String> {
    let mut socket = socket(".socket.sock")?;
    socket.write_all(command.as_bytes()).ok()?;
    let mut answer = String::new();
    socket.read_to_string(&mut answer).ok()?;
    Some(answer)
}

fn json(command: &str) -> Vec<serde_json::Value> {
    let answer = hyprctl(&format!("j/{command}")).unwrap_or_default();
    match serde_json::from_str(&answer) {
        Ok(serde_json::Value::Array(items)) => items,
        _ => Vec::new(),
    }
}

/// A portal trigger (`CTRL+ALT+SHIFT+LOGO+3`) as Hyprland's chord
/// (`CTRL ALT SHIFT SUPER,3`), its modifier mask and its key.
fn chord(trigger: &str) -> Option<(String, u64, &str)> {
    let (mods, key) = trigger.rsplit_once('+').unwrap_or(("", trigger));
    let (mut names, mut mask) = (Vec::new(), 0);
    for m in mods.split('+').filter(|m| !m.is_empty()) {
        let (name, bit) = match m {
            "SHIFT" => ("SHIFT", 1),
            "CTRL" => ("CTRL", 4),
            "ALT" => ("ALT", 8),
            "LOGO" => ("SUPER", 64),
            _ => return None,
        };
        names.push(name);
        mask |= bit;
    }
    Some((format!("{},{key}", names.join(" ")), mask, key))
}

/// Binds `wanted` (trigger, portal id) in the Hyprland this runs under,
/// in place of the last table's binds; the portal ids whose chord Hyprland
/// binds to something else. Elsewhere it does nothing.
pub(crate) fn bind(wanted: Vec<(String, String)>, app_id: &str) -> Vec<String> {
    if socket(".socket.sock").is_none() {
        return Vec::new();
    }
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let unbind: Vec<String> = state
        .1
        .drain(..)
        .map(|c| format!("keyword unbind {c}"))
        .collect();
    if !unbind.is_empty() {
        hyprctl(&format!("[[BATCH]]{}", unbind.join(";")));
    }
    state.0 = wanted;
    let (taken, bound) = apply(&state.0, app_id);
    state.1 = bound;
    drop(state);
    let app_id = app_id.to_owned();
    WATCH.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("exact-hyprland".into())
            .spawn(move || watch(&app_id));
    });
    taken
}

/// Binds each chord Hyprland leaves free to its shortcut, by the name
/// Hyprland lists it under (`app id:portal id`): the portal ids it binds
/// to something else, and the chords bound.
fn apply(wanted: &[(String, String)], app_id: &str) -> (Vec<String>, Vec<String>) {
    let names: Vec<String> = json("globalshortcuts")
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect();
    let binds = json("binds");
    let (mut taken, mut bound, mut commands) = (Vec::new(), Vec::new(), Vec::new());
    for (trigger, id) in wanted {
        let Some((hypr, mask, key)) = chord(trigger) else {
            continue;
        };
        let name = (names
            .iter()
            .find(|n| n.ends_with(&format!(":{id}")))
            .cloned())
        .unwrap_or_else(|| format!("{app_id}:{id}"));
        let on_chord: Vec<&serde_json::Value> = (binds.iter())
            .filter(|b| b["modmask"].as_u64() == Some(mask))
            .filter(|b| {
                b["key"]
                    .as_str()
                    .is_some_and(|k| k.eq_ignore_ascii_case(key))
            })
            .collect();
        let ours =
            |b: &&serde_json::Value| b["dispatcher"] == "global" && b["arg"] == name.as_str();
        if on_chord.iter().any(ours) {
            continue;
        }
        if !on_chord.is_empty() {
            taken.push(id.clone());
            continue;
        }
        commands.push(format!("keyword bind {hypr},global,{name}"));
        bound.push(hypr);
    }
    if !commands.is_empty() {
        hyprctl(&format!("[[BATCH]]{}", commands.join(";")));
    }
    (taken, bound)
}

/// Binds the last table again after each config reload, which dropped the
/// run-time binds.
fn watch(app_id: &str) {
    let Some(events) = socket(".socket2.sock") else {
        return;
    };
    for line in BufReader::new(events).lines() {
        let Ok(line) = line else { return };
        if !line.starts_with("configreloaded>>") {
            continue;
        }
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let (_, bound) = apply(&state.0, app_id);
        state.1 = bound;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trigger_becomes_a_hyprland_chord() {
        assert_eq!(
            chord("CTRL+ALT+SHIFT+LOGO+3"),
            Some(("CTRL ALT SHIFT SUPER,3".into(), 77, "3"))
        );
        assert_eq!(chord("Print"), Some((",Print".into(), 0, "Print")));
        assert_eq!(chord("HYPER+3"), None);
    }
}
