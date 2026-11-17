//! The app's status item in the bar's tray (LLP 1133.025 D4), as the
//! Windows host places one in the notification area: the first
//! `-exact-menu="status"` button leaves the page and becomes a
//! StatusNotifierItem (`exact_painted::tray`). A primary click presses the
//! button; its menu, ending with Exit (D7), is handed to the tray as rows,
//! since no Wayland window can be placed beside the bar. With no tray in
//! the session, the button stays in the page.
use exact_painted::presenter::Presenter;
use exact_painted::tray::{self, Heard, Item};
use exact_runner::DataSource;

/// The sides the item's picture is drawn at, device pixels: a bar picks
/// the nearest to its own. Each is 16 points at its scale.
const SIDES: [u32; 3] = [16, 32, 48];

/// The kernel epoch, accepted pictures and bar scheme an item was drawn for.
type Shown = (u64, Vec<(u32, usize)>, bool);

pub(super) struct Tray {
    tray: tray::Tray,
    /// What the item was last drawn for.
    shown: Option<Shown>,
    said_more: bool,
}

impl Tray {
    /// Register with the session's tray and take the status buttons out of
    /// the page; `None`, leaving them in it, with no tray to show them.
    pub(super) fn start<D: DataSource>(
        p: &mut Presenter<D>,
        app_id: &str,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Tray> {
        p.status_items().first()?;
        let tray = tray::Tray::start(app_id, wake)?;
        p.place_status_items();
        Some(Tray {
            tray,
            shown: None,
            said_more: false,
        })
    }

    /// What the person did with the item, then the item as the tree now
    /// stands on a bar `dark` or light. Whether they chose Exit.
    pub(super) fn turn<D: DataSource>(
        &mut self,
        p: &mut Presenter<D>,
        dark: bool,
        hidden: bool,
        now: f64,
    ) -> bool {
        let heard = self.tray.take();
        if !heard.is_empty() {
            let item = p.status_items().into_iter().next();
            for heard in heard {
                match (heard, &item) {
                    (Heard::Activate, Some(item)) if item.press && !item.disabled => {
                        p.status_press(item.node, now)
                    }
                    (Heard::Chosen(row), _) => p.status_choose(row, now),
                    _ => {}
                }
            }
            if p.take_status_exit() {
                return true;
            }
        }
        // A hidden window never draws, but an image worker still wakes the loop.
        if hidden {
            p.poll_images();
        }
        let items = p.status_items();
        if items.len() > 1 && !std::mem::replace(&mut self.said_more, true) {
            p.host_mut().log("status item: the Linux tray shows the first status button; the others are not placed");
        }
        let Some(item) = items.into_iter().next() else {
            self.tray.show(None);
            self.shown = None;
            return false;
        };
        let images: Vec<_> = (item.image.iter())
            .flat_map(|id| SIDES.map(|side| (*id, side)))
            .collect();
        let accepted = p.sync_status_images(&images);
        let shown = (p.host().kernel().epoch(), accepted, dark);
        if self.shown.as_ref() == Some(&shown) {
            return false;
        }
        self.shown = Some(shown);
        let icons = (item.image.iter())
            .flat_map(|image| SIDES.map(|side| (*image, side)))
            .filter_map(|(image, side)| {
                match p.status_icon(image, side, side as f32 / 16., dark)? {
                    Ok(pixmap) => Some((side, straight(&pixmap, item.disabled))),
                    Err(error) => {
                        p.host_mut()
                            .log(format!("status item: its image did not draw: {error}"));
                        None
                    }
                }
            })
            .collect();
        // A disabled item has no menu; any other ends with Exit (D7).
        let menu = if item.disabled {
            Vec::new()
        } else {
            p.status_menu_entries(item.node, true)
        };
        self.tray.show(Some(Item {
            title: item.label,
            icons,
            activates: item.press && !item.disabled,
            menu,
        }));
        false
    }
}

/// A drawn picture's pixels as straight RGBA, at half opacity when `dimmed`.
fn straight(pixmap: &tiny_skia::Pixmap, dimmed: bool) -> Vec<u8> {
    let dim = if dimmed { 2 } else { 1 };
    (pixmap.pixels().iter())
        .map(|px| px.demultiply())
        .flat_map(|c| [c.red(), c.green(), c.blue(), c.alpha() / dim])
        .collect()
}
