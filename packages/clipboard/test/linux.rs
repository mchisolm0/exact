use super::*;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    bytes.extend(width.to_be_bytes());
    bytes.extend(height.to_be_bytes());
    bytes.extend([8, 6, 0, 0, 0]);
    bytes
}

#[test]
fn item_refusals_and_text_keep_the_clipboard_contract() {
    let no_file = |_: &str| Err(refused("NotFoundError", "missing"));
    for (name, args, error) in [
        ("write", json!({"data": {}}), "TypeError"),
        ("write", json!({"data": [{}]}), "TypeError"),
        (
            "write",
            json!({"data": [{"text/plain":"a"}, {"text/plain":"b"}]}),
            "NotAllowedError",
        ),
        (
            "write",
            json!({"data": [{"text/html":"a"}]}),
            "NotAllowedError",
        ),
        ("writeText", json!({"data": 3}), "TypeError"),
        (
            "writeText",
            json!({"data": "x".repeat(1024 * 1024 + 1)}),
            "DataError",
        ),
        (
            "write",
            json!({"data": [{"image/png":{"base64":"AAAA"}}]}),
            "NotSupportedError",
        ),
        (
            "write",
            json!({"data": [{"image/png":"file:/other"}]}),
            "TypeError",
        ),
        (
            "write",
            json!({"data": [{"image/png":"app:/tmp/missing"}]}),
            "NotFoundError",
        ),
    ] {
        assert_eq!(
            prepare(name, &args, no_file).err().unwrap().0,
            error,
            "{name} {args}"
        );
    }
    let text = prepare("writeText", &json!({"data":"café 🦀"}), no_file).unwrap();
    assert_eq!(
        text.offer(),
        Some(("text/plain;charset=utf-8", "café 🦀".as_bytes()))
    );
    assert_eq!(
        prepare("write", &json!({"data":[]}), no_file)
            .unwrap()
            .offer(),
        None
    );
}

#[test]
fn an_image_and_its_text_offer_the_image_and_a_non_png_is_refused() {
    let dir = std::env::temp_dir().join(format!("exact-clipboard-linux-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("image.png");
    std::fs::write(&path, png(8, 4)).unwrap();
    let open =
        |_: &str| std::fs::File::open(&path).map_err(|e| ("NotReadableError", e.to_string()));
    let args = json!({"data": [{"image/png":"app:/tmp/image.png", "text/plain":"image URL"}]});
    let both = prepare("write", &args, open).unwrap();
    assert_eq!(both.png.as_ref().map(|(_, w, h)| (*w, *h)), Some((8, 4)));
    assert_eq!(both.offer().map(|(mime, _)| mime), Some("image/png"));
    std::fs::write(&path, b"not a PNG at all, only text").unwrap();
    assert_eq!(prepare("write", &args, open).err().unwrap().0, "DataError");
    std::fs::write(&path, png(0, 4)).unwrap();
    assert_eq!(prepare("write", &args, open).err().unwrap().0, "DataError");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn wayland_is_preferred_and_no_display_is_refused() {
    assert_eq!(
        tool(true, true, "image/png").unwrap(),
        ("wl-copy", vec!["--type".into(), "image/png".into()])
    );
    let (x11, args) = tool(false, true, "image/png").unwrap();
    assert_eq!(
        (x11, args.join(" ").as_str()),
        ("xclip", "-selection clipboard -target image/png -in")
    );
    assert_eq!(
        tool(false, false, "image/png").unwrap_err().0,
        "NotAllowedError"
    );
}

/// Writes to the clipboard of the Wayland display in WAYLAND_DISPLAY and reads it back with wl-paste: run
/// it only in a session of its own (`cargo test -p exact-linux live_ -- --ignored`), never a person's.
#[test]
#[ignore]
fn live_wl_copy_serves_the_png_and_text() {
    let paste = |mime: &str| {
        let out = Command::new("wl-paste")
            .args(["--no-newline", "--type", mime])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    put("image/png", &png(8, 4)).unwrap();
    assert_eq!(paste("image/png"), png(8, 4));
    put("text/plain;charset=utf-8", "café 🦀".as_bytes()).unwrap();
    assert_eq!(paste("text/plain"), "café 🦀".as_bytes());
}
