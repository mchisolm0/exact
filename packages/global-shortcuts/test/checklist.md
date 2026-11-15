# @exact/global-shortcuts by hand (LLP 1133.025 D3)

The drives and the person's steps are the desktop fixture's, shared by the
three desktop packages: `packages/dock/test/desktop.mjs` (drives 4 and 7)
and `packages/dock/test/checklist.md` (Mac steps 7 and 8). The runner's
tests are `contract/cli/tests/it/desktop.rs`.

A press needs a key pressed at the keyboard, which the agent does not make:
the substitute's `tap @ref` is the drive's press.

`bun test packages/global-shortcuts/test/pairs.test.mjs` checks pair delivery
with synthetic macOS modifier events, including missed presses and releases.
For the keyboard check, mount a reader of `Meta+Meta` while left Command is
held, release it, and press right Command alone: no shortcut. Then hold both:
one shortcut; releasing either raises none.

## Linux (GNOME 48 and later, KDE Plasma)

The host's table is the GlobalShortcuts portal (`host/painted/src/portal/shortcuts.rs`); its unit tests run
with `cargo test -p exact-painted --lib shortcuts`. GNOME binds only for an app id with a `<id>.desktop`
file in an `applications` folder. By hand, in a session of its own:

1. Run the fixture: the desktop's "Add Keyboard Shortcuts" dialog lists each chord (Control+Alt+Shift+Meta+3
   shows as Shift+Ctrl+Alt+Super+3). Press Add: the readers answer `registered` and the press raises
   `globalshortcut`. Run it again: no dialog.
2. With `EXACT_SHORTCUTS_APP_ID=<id>`, `cargo test -p exact-painted --lib live_ -- --ignored --nocapture`
   binds one chord and waits for a press.
