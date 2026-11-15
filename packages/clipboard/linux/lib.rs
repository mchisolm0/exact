// @exact/clipboard's Linux path form (LLP 1133.006 D1, D2, D5, D7): the
// desktop's clipboard through `wl-copy` (wl-clipboard) under Wayland, or
// `xclip` under X11 alone. The host links no display library, and each tool
// stays behind to serve what it was given, as a Wayland or X11 clipboard
// owner must. Neither offers two types at once, so an item with an image and
// its text offers the image. A PNG is checked (its signature and header),
// not decoded again. Under the agent, unless EXACT_DEVICE=real, nothing
// reaches the desktop: the write is journaled and announced as on Windows.
use exact_painted::package::{json, Context, Json, Package, Reply};
use std::io::{Read, Write};
use std::process::{Command, Stdio};

pub struct Clipboard {
    context: Context,
    writes: u64,
}

type Refusal = (&'static str, String);
const MOST_FILE: u64 = 64 * 1024 * 1024;

fn refused(code: &'static str, message: &str) -> Refusal {
    (code, message.into())
}

/// What one write offers: text, a PNG's bytes and size, or both.
#[derive(Debug, Default, PartialEq)]
struct Item {
    text: Option<String>,
    png: Option<(Vec<u8>, u32, u32)>,
}

impl Item {
    /// The one type and bytes the clipboard tool is given.
    fn offer(&self) -> Option<(&'static str, &[u8])> {
        match (&self.png, &self.text) {
            (Some((bytes, ..)), _) => Some(("image/png", bytes)),
            (None, Some(text)) => Some(("text/plain;charset=utf-8", text.as_bytes())),
            (None, None) => None,
        }
    }
}

/// A PNG's width and height from its header, or why it is not one.
fn png_size(bytes: &[u8]) -> Result<(u32, u32), String> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let header = bytes.get(8..24).filter(|_| bytes.starts_with(SIGNATURE));
    let Some(header) = header.filter(|h| &h[4..8] == b"IHDR") else {
        return Err("image/png is not a PNG".into());
    };
    let side = |at: usize| {
        u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]])
    };
    match (side(8), side(12)) {
        (0, _) | (_, 0) => Err("image/png has no pixels".into()),
        size => Ok(size),
    }
}

fn prepare(
    name: &str,
    args: &Json,
    open: impl Fn(&str) -> Result<std::fs::File, Refusal>,
) -> Result<Item, Refusal> {
    let data;
    let items = match name {
        "write" => args["data"]
            .as_array()
            .ok_or_else(|| refused("TypeError", "write takes a list of ClipboardItem"))?,
        "writeText" => {
            let text = args["data"]
                .as_str()
                .ok_or_else(|| refused("TypeError", "writeText takes a string"))?;
            data = vec![json!({"text/plain": text})];
            &data
        }
        _ => return Err(refused("NotSupportedError", "unknown clipboard operation")),
    };
    let Some(first) = items.first() else {
        return Ok(Item::default());
    };
    if items.len() != 1 {
        return Err(refused(
            "NotAllowedError",
            "Support for multiple ClipboardItems is not implemented.",
        ));
    }
    let record = first
        .as_object()
        .filter(|r| !r.is_empty())
        .ok_or_else(|| refused("TypeError", "a ClipboardItem has at least one type"))?;
    if let Some(other) = record
        .keys()
        .find(|k| !matches!(k.as_str(), "text/plain" | "image/png"))
    {
        return Err((
            "NotAllowedError",
            format!("Type {other} not supported on write."),
        ));
    }
    let mut item = Item::default();
    if let Some(text) = record.get("text/plain") {
        let text = text
            .as_str()
            .ok_or_else(|| refused("TypeError", "text/plain is a string"))?;
        if text.len() > 1024 * 1024 {
            return Err(refused("DataError", "text/plain is over 1 MiB"));
        }
        item.text = Some(text.into());
    }
    if let Some(image) = record.get("image/png") {
        let path =
            image
                .as_str()
                .ok_or_else(|| match image.get("base64").and_then(Json::as_str) {
                    Some(_) => refused(
                        "NotSupportedError",
                        "image/png on Linux takes an app:/ path; in-memory bytes are not supported",
                    ),
                    None => refused("TypeError", "image/png is an app:/ path"),
                })?;
        if !path.starts_with("app:/") {
            return Err(refused("TypeError", "image/png is an app:/ path"));
        }
        let file = open(path)?;
        let meta = file
            .metadata()
            .map_err(|e| ("NotReadableError", e.to_string()))?;
        if !meta.is_file() {
            return Err(refused(
                "NotReadableError",
                "image/png is not a regular file",
            ));
        }
        if meta.len() > MOST_FILE {
            return Err(refused("DataError", "image/png is over 64 MiB"));
        }
        let mut bytes = Vec::new();
        file.take(MOST_FILE + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| ("NotReadableError", e.to_string()))?;
        let (width, height) = png_size(&bytes).map_err(|e| ("DataError", e))?;
        item.png = Some((bytes, width, height));
    }
    Ok(item)
}

/// The tool that holds the clipboard for `mime` on this desktop: Wayland's
/// first, as every toolkit prefers it where both run.
fn tool(wayland: bool, x11: bool, mime: &str) -> Result<(&'static str, Vec<String>), Refusal> {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
    match (wayland, x11) {
        (true, _) => Ok(("wl-copy", args(&["--type", mime]))),
        (false, true) => Ok((
            "xclip",
            args(&["-selection", "clipboard", "-target", mime, "-in"]),
        )),
        (false, false) => Err(refused(
            "NotAllowedError",
            "there is no Wayland or X11 display to hold the clipboard",
        )),
    }
}

/// Hands `bytes` to the tool, which forks to serve them and exits.
fn put(mime: &str, bytes: &[u8]) -> Result<(), Refusal> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    let (program, args) = tool(set("WAYLAND_DISPLAY"), set("DISPLAY"), mime)?;
    // The served copy keeps no pipe of ours, or waiting for it would wait
    // for the next copy.
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            (
                "NotAllowedError",
                format!(
                    "{program} could not run ({e}); install {}",
                    match program {
                        "wl-copy" => "wl-clipboard",
                        _ => "xclip",
                    }
                ),
            )
        })?;
    let wrote = child.stdin.take().map(|mut stdin| stdin.write_all(bytes));
    let status = child.wait().map_err(|e| ("UnknownError", e.to_string()))?;
    match (wrote, status.success()) {
        (Some(Ok(())), true) => Ok(()),
        _ => Err((
            "UnknownError",
            format!("{program} did not take the {mime} ({status})"),
        )),
    }
}

fn open(path: &str) -> Result<std::fs::File, Refusal> {
    // Admission has checked the caller's fs.read grant. The host's opener
    // keeps the read below the app root, refusing symlinks.
    exact_painted::picker::open(path).ok_or_else(|| {
        let missing = exact_painted::picker::resolve(path).is_some_and(|p| {
            std::fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        });
        refused(
            if missing {
                "NotFoundError"
            } else {
                "NotReadableError"
            },
            "image/png path could not be opened",
        )
    })
}

impl Package for Clipboard {
    fn new(context: Context) -> Self {
        Self { context, writes: 0 }
    }

    fn later(&mut self, name: &str, args: &Json, _preferred: Option<&Json>, reply: Reply) {
        let result = if self.context.origin() != "app" {
            Err(refused("NotAllowedError", "person-required"))
        } else {
            prepare(name, args, open)
        };
        let result = result.and_then(|item| {
            let Some((mime, bytes)) = item.offer() else {
                return Ok(());
            };
            let agent =
                self.context.agent() && std::env::var("EXACT_DEVICE").as_deref() != Ok("real");
            if !agent {
                put(mime, bytes)?;
            }
            self.writes += 1;
            let mut shown = json!({
                "types": [mime.split(';').next().unwrap_or(mime)],
                "pasteboard": if agent { "memory" } else { "desktop" },
            });
            match &item.png {
                Some((_, width, height)) => {
                    shown["image"] = json!({ "width": width, "height": height });
                    self.context
                        .log(format!("wrote image/png {width}×{height}"));
                }
                None => {
                    let text = item.text.as_deref().unwrap_or_default();
                    shown["text"] = json!(text.chars().take(200).collect::<String>());
                    self.context.log("wrote text/plain");
                }
            }
            self.context.changed(&format!("exact-state:{}", json!({
                "extension": "clipboard", "state": { "writes": self.writes, "lastWrite": shown },
            })));
            Ok(())
        });
        match result {
            Ok(()) => reply.send(json!({})),
            Err((code, message)) => {
                self.context
                    .log(format!("write refused: {code} ({message})"));
                reply.error(code, Some(&message));
            }
        }
    }
}
