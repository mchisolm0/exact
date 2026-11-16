//! A desktop window: the painted host's frames in a Wayland or X11 window,
//! with the window's pointer, keys and text as input.
//!
//! @ref LLP 1015 §7 (Wayland/X11 windows)
//!
//! winit opens the window (Wayland when `WAYLAND_DISPLAY` is set, else X11),
//! draws client-side decorations where the compositor draws none (GNOME),
//! reads keys through xkbcommon and composes text with the input method.
//! A frame is tiny-skia's pixels copied into a shared-memory buffer
//! (softbuffer), the pair the Windows host uses. winit and softbuffer open
//! libwayland-client, libxkbcommon and libxcb with dlopen, so the binary
//! still links no system library.
//!
//! The loop stays `event_loop::run`: the window is one more [`Output`] whose
//! descriptor is winit's, and winit is pumped without blocking when it is
//! readable. A submitted frame is pending until the compositor's frame
//! callback, which winit reports as `RedrawRequested` after
//! `pre_present_notify`, so paint follows the compositor as a page flip
//! paces it on KMS. X11 has no frame callback, so there a frame completes
//! at the next pump.
//!
//! The window opens at `EXACT_SIZE` (points), else 1024×720, the Windows
//! host's default; its title is the binary's app name, and its Wayland app
//! id and X11 class are the app's id, which name its desktop entry.
//!
//! An app that keeps running (`host.linux.window.keepRunning`) has its
//! window put away by the person's close, with nothing asked, as the
//! Windows host does: the window and its buffer are destroyed, since
//! Wayland has no hidden toplevel, and the presenter, its hatches, packages
//! and timers run on; the loop paints nothing while there is no window. A
//! later launch of the app (`instance`) or a bare `focus()` makes the window
//! again at its last size, with the launch's activation token, so the
//! compositor raises it; `hideWindow()` puts it away.

use super::event_loop::{self, Output};
use super::instance::Claim;
use crate::input::LINE;
use exact_painted::app::Config;
use exact_painted::presenter::ime::Target;
use exact_painted::presenter::{PointerKind, Presenter, SubmittedFrame};
use exact_runner::DataSource;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, Size};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy, OwnedDisplayHandle};
use winit::keyboard::{Key, NamedKey, PhysicalKey};
use winit::platform::pump_events::{EventLoopExtPumpEvents, PumpStatus};
use winit::platform::startup_notify::WindowAttributesExtStartupNotify;
use winit::platform::wayland::{
    ActiveEventLoopExtWayland, EventLoopExtWayland, WindowAttributesExtWayland,
};
use winit::platform::x11::WindowAttributesExtX11;
use winit::window::{ActivationToken, CursorIcon, Window, WindowAttributes, WindowId};

/// A window rather than KMS: a desktop session is there (`WAYLAND_DISPLAY`
/// or `DISPLAY`), and neither `EXACT_DRM` nor `EXACT_VNC`, which are the KMS
/// path's, was asked for.
pub(super) fn wanted() -> bool {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    (set("WAYLAND_DISPLAY") || set("DISPLAY")) && !set("EXACT_DRM") && !set("EXACT_VNC")
}

/// The window loop: open the window, boot at its size and scale, run the
/// display loop over it. Exit code.
pub(super) fn run<D: DataSource + Default>(
    config: &mut Config,
    started: Instant,
    opening: crate::Window,
) -> i32 {
    let name = exact_painted::frames::app_name();
    let id = D::default().app_id().to_owned();
    let size = if std::env::var_os("EXACT_SIZE").is_some() {
        config.size
    } else {
        (1024., 720.)
    };
    let class = if id.is_empty() { &name } else { &id };
    let keep_running = opening.keep_running;
    // The token the launcher gave this launch, for its window (or the
    // running instance's) and for no process this one starts.
    let token = keep_running.then(launch_token).flatten();
    let instance = match keep_running.then(|| super::instance::path(class)).flatten() {
        None => None,
        Some(path) => match super::instance::claim(&path, token.as_deref()) {
            Ok(Claim::First(instance)) => Some(instance),
            Ok(Claim::Handed) => return 0,
            Ok(Claim::Busy) => {
                eprintln!("exact: {name} is running and did not answer; this launch ends");
                return 1;
            }
            Err(e) => {
                eprintln!("exact: {}: {e}; this launch runs alone", path.display());
                None
            }
        },
    };
    let mut surface = match Surface::open(&name, class, size, keep_running, token) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("exact: no window: {e}");
            return 1;
        }
    };
    let window = surface.handler.window().expect("opened");
    let scale = window.scale_factor();
    let pixels = window.inner_size();
    let viewport = (
        pixels.width as f32 / scale as f32,
        pixels.height as f32 / scale as f32,
    );
    config.scale = scale as f32;
    let Some((mut p, marks)) = super::boot::<D>(config, started, viewport) else {
        return 1;
    };
    // The app's bare `focus()` and `hideWindow()`, carried to winit's next
    // turn, since the presenter asks them from inside its own.
    let proxy = surface.event_loop.create_proxy();
    if keep_running {
        let focus = proxy.clone();
        p.set_window_focuser(move || drop(focus.send_event(Ask::Show(None))));
        let hide = proxy.clone();
        p.set_window_hider(move || drop(hide.send_event(Ask::Hide)));
    }
    println!(
        "exact: window {}x{} on {}, scale {scale}, boot {:.1} ms",
        pixels.width,
        pixels.height,
        if surface.event_loop.is_wayland() {
            "wayland"
        } else {
            "x11"
        },
        started.elapsed().as_secs_f64() * 1000.0
    );
    let mut events = Events {
        window: Some(window),
        proxy,
        instance,
        queue: surface.handler.queue.clone(),
        clock_set: super::clock_set::ClockSet::open(),
        scale,
        pointer: None,
        last: (0., 0.),
        buttons: 0,
        clicks: Clicks::default(),
        focused: true,
        cursor: CursorIcon::Default,
        cursor_stale: true,
        ime: InputMethod::default(),
        closed: false,
    };
    event_loop::run(
        config,
        started,
        p,
        &mut surface,
        &mut events,
        marks,
        D::default,
    )
}

/// The launcher's activation token (Wayland's `XDG_ACTIVATION_TOKEN`, X11's
/// `DESKTOP_STARTUP_ID`), taken out of the environment.
fn launch_token() -> Option<String> {
    let token = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|t| !t.is_empty()));
    winit::platform::startup_notify::reset_activation_token_env();
    token
}

/// What the app or a later launch asks of the window, at winit's next turn.
enum Ask {
    /// Show the window, raised with a launch's activation token if it has one.
    Show(Option<String>),
    /// Put the window away.
    Hide,
}

/// What the window did, for the presenter's side of the loop.
enum Note {
    Event(WindowEvent),
    /// A window was made: the first, or one shown again.
    Shown(Rc<Window>),
    /// The window was put away.
    Hidden,
}

/// The window and the buffer its frames are copied into.
struct Shown {
    buffer: softbuffer::Surface<OwnedDisplayHandle, Rc<Window>>,
    window: Rc<Window>,
}

/// winit's side of the loop: makes the window when the loop starts (and
/// again when it shows after being put away), and keeps what it reports for
/// the next turn.
struct Handler {
    /// What every window of the app is made with; `size` is the last one's.
    attributes: WindowAttributes,
    size: Size,
    context: softbuffer::Context<OwnedDisplayHandle>,
    shown: Option<Shown>,
    keep_running: bool,
    /// This launch's activation token, for the first window.
    token: Option<String>,
    error: Option<String>,
    queue: Rc<RefCell<VecDeque<Note>>>,
    /// The compositor asked for a frame since the last submit.
    redraw: bool,
}

impl Handler {
    fn window(&self) -> Option<Rc<Window>> {
        self.shown.as_ref().map(|s| s.window.clone())
    }

    /// A new window at the last size, raised with `token`; one that is
    /// showing is replaced.
    fn make(&mut self, event_loop: &ActiveEventLoop, token: Option<String>) {
        self.shown = None;
        let mut attributes = self.attributes.clone().with_inner_size(self.size);
        if let Some(token) = token {
            attributes = attributes.with_activation_token(ActivationToken::from_raw(token));
        }
        let made = event_loop
            .create_window(attributes)
            .map_err(|e| e.to_string())
            .and_then(|window| {
                let window = Rc::new(window);
                softbuffer::Surface::new(&self.context, window.clone())
                    .map(|buffer| Shown { buffer, window })
                    .map_err(|e| e.to_string())
            });
        match made {
            Ok(shown) => {
                let window = shown.window.clone();
                self.shown = Some(shown);
                // The new window takes a frame now; one a replaced window
                // held is done with.
                self.redraw = true;
                self.queue.borrow_mut().push_back(Note::Shown(window));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// The app's `focus()` or a later launch. A window that is showing is
    /// raised: on X11 by asking, and on Wayland, where winit's
    /// `focus_window` does nothing, by a new window that carries the
    /// launch's token, which is how the compositor lets a window come forward.
    fn show(&mut self, event_loop: &ActiveEventLoop, token: Option<String>) {
        if let Some(shown) = &self.shown {
            if token.is_none() || !event_loop.is_wayland() {
                shown.window.focus_window();
                return;
            }
        }
        self.make(event_loop, token);
    }

    /// The window put away, remembering its size.
    fn hide(&mut self) {
        let Some(shown) = self.shown.take() else {
            return;
        };
        let window = &shown.window;
        self.size = window
            .inner_size()
            .to_logical::<f64>(window.scale_factor())
            .into();
        self.redraw = false;
        self.queue.borrow_mut().push_back(Note::Hidden);
    }
}

impl ApplicationHandler<Ask> for Handler {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.shown.is_none() && self.error.is_none() {
            let token = self.token.take();
            self.make(event_loop, token);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, ask: Ask) {
        match ask {
            Ask::Show(token) => self.show(event_loop, token),
            Ask::Hide => self.hide(),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // What a window put away (or replaced) still reports is no one's.
        if self.shown.as_ref().is_none_or(|s| s.window.id() != id) {
            return;
        }
        match event {
            WindowEvent::RedrawRequested => self.redraw = true,
            // As the Windows host: nothing unloads, so nothing is asked.
            WindowEvent::CloseRequested if self.keep_running => self.hide(),
            event => self.queue.borrow_mut().push_back(Note::Event(event)),
        }
    }
}

/// The window as the loop's output.
struct Surface {
    event_loop: EventLoop<Ask>,
    handler: Handler,
    /// The frame shown, until the compositor's frame callback releases it.
    pending: Option<SubmittedFrame>,
}

impl Surface {
    fn open(
        title: &str,
        class: &str,
        (width, height): (f32, f32),
        keep_running: bool,
        token: Option<String>,
    ) -> Result<Surface, String> {
        let mut event_loop = EventLoop::with_user_event()
            .build()
            .map_err(|e| e.to_string())?;
        let context = softbuffer::Context::new(event_loop.owned_display_handle())
            .map_err(|e| e.to_string())?;
        let attributes = Window::default_attributes().with_title(title);
        // The Wayland app id and the X11 WM_CLASS, which docks match to a
        // desktop file: `<app id>.desktop`, as the build writes it.
        let attributes = WindowAttributesExtWayland::with_name(attributes, class, title);
        let attributes = WindowAttributesExtX11::with_name(attributes, class, title);
        let mut handler = Handler {
            attributes,
            size: LogicalSize::new(width, height).into(),
            context,
            shown: None,
            keep_running,
            token,
            error: None,
            queue: Rc::default(),
            redraw: false,
        };
        // The loop's first iteration resumes it, which makes the window.
        while handler.shown.is_none() && handler.error.is_none() {
            if let PumpStatus::Exit(code) =
                event_loop.pump_app_events(Some(Duration::ZERO), &mut handler)
            {
                return Err(format!("the event loop ended ({code})"));
            }
        }
        if let Some(e) = handler.error.take() {
            return Err(e);
        }
        Ok(Surface {
            event_loop,
            handler,
            pending: None,
        })
    }

    fn millihertz(&self) -> u32 {
        self.handler
            .window()
            .and_then(|w| w.current_monitor())
            .and_then(|m| m.refresh_rate_millihertz())
            .filter(|mhz| *mhz > 0)
            .unwrap_or(60_000)
    }
}

impl Output for Surface {
    fn refresh(&self) -> u32 {
        self.millihertz().div_ceil(1000)
    }
    fn period_ms(&self) -> f64 {
        1e6 / f64::from(self.millihertz())
    }
    /// No window takes a frame while it is put away: the loop paints
    /// nothing and runs no animation frames until it shows.
    fn pending(&self) -> bool {
        self.pending.is_some() || self.handler.shown.is_none()
    }
    fn sequence(&self) -> Option<u32> {
        None
    }
    fn last_flip(&self) -> Option<Duration> {
        None
    }
    fn fd(&self) -> i32 {
        self.event_loop.as_raw_fd()
    }

    fn submit(&mut self, frame: SubmittedFrame) -> Result<Option<SubmittedFrame>, String> {
        let pixels = frame.pixels();
        let (Some(width), Some(height)) = (
            NonZeroU32::new(pixels.width()),
            NonZeroU32::new(pixels.height()),
        ) else {
            return Ok(Some(frame));
        };
        let Some(shown) = &mut self.handler.shown else {
            return Ok(Some(frame));
        };
        // The buffer is the frame's size: a fractional scale's rounding can
        // differ from the window's by a pixel, which the compositor scales.
        shown
            .buffer
            .resize(width, height)
            .map_err(|e| format!("window buffer: {e}"))?;
        let mut buffer = shown
            .buffer
            .buffer_mut()
            .map_err(|e| format!("window buffer: {e}"))?;
        copy_xrgb(pixels, &mut buffer);
        // Ask for the compositor's frame callback in the commit that shows
        // this frame; `RedrawRequested` reports it.
        shown.window.pre_present_notify();
        buffer
            .present()
            .map_err(|e| format!("window present: {e}"))?;
        shown.window.request_redraw();
        self.handler.redraw = false;
        self.pending = Some(frame);
        Ok(None)
    }

    fn ready(&mut self) -> Result<Option<SubmittedFrame>, String> {
        if let PumpStatus::Exit(code) = self
            .event_loop
            .pump_app_events(Some(Duration::ZERO), &mut self.handler)
        {
            return Err(format!("the window's display connection ended ({code})"));
        }
        if let Some(e) = self.handler.error.take() {
            return Err(e);
        }
        // A frame shown in a window since put away is done with.
        Ok(
            if std::mem::take(&mut self.handler.redraw) || self.handler.shown.is_none() {
                self.pending.take()
            } else {
                None
            },
        )
    }
}

/// Premultiplied RGBA into softbuffer's 0RGB words; the page is opaque, so
/// premultiplied is straight.
fn copy_xrgb(frame: &Pixmap, out: &mut [u32]) {
    for (out, px) in out.iter_mut().zip(frame.data().as_chunks::<4>().0) {
        *out = u32::from(px[0]) << 16 | u32::from(px[1]) << 8 | u32::from(px[2]);
    }
}

/// The window's input, fed to the presenter each turn.
struct Events {
    /// `None` while the window is put away.
    window: Option<Rc<Window>>,
    proxy: EventLoopProxy<Ask>,
    /// This app's one process, listening for later launches.
    instance: Option<super::instance::Instance>,
    queue: Rc<RefCell<VecDeque<Note>>>,
    clock_set: Option<super::clock_set::ClockSet>,
    /// Device pixels per point, as the compositor says.
    scale: f64,
    /// Where the pointer is in the window, points; `None` once it left.
    pointer: Option<(f32, f32)>,
    /// Where it was last, which a button released outside the window uses.
    last: (f32, f32),
    /// The buttons held, by their `PointerEvent.buttons` bits.
    buttons: u32,
    clicks: Clicks,
    focused: bool,
    cursor: CursorIcon,
    /// The cursor may have changed: the pointer moved or a frame landed.
    cursor_stale: bool,
    ime: InputMethod,
    closed: bool,
}

/// The modifier codes `hardware_key` holds; a window that loses the
/// keyboard releases them, since their releases go to another window.
const MODIFIERS: [&str; 8] = [
    "ShiftLeft",
    "ShiftRight",
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "MetaLeft",
    "MetaRight",
];

impl Events {
    fn note<D: DataSource>(&mut self, p: &mut Presenter<D>, note: Note, now: f64) {
        match note {
            Note::Event(event) => self.event(p, event, now),
            Note::Shown(window) => {
                let returning = self.window.replace(window).is_none();
                self.cursor = CursorIcon::Default;
                self.cursor_stale = true;
                self.ime = InputMethod::default();
                self.resized(p);
                if returning {
                    // A page put away comes back (`pageshow` with `persisted`).
                    tell_page(p, |page| {
                        page.hidden = false;
                        if std::mem::take(&mut page.persisted) {
                            page.restores = page.restores.wrapping_add(1);
                        }
                    });
                    p.surface_lifecycle(false, !self.focused);
                }
                p.repaint();
            }
            Note::Hidden => {
                self.window = None;
                self.pointer = None;
                self.ime = InputMethod::default();
                self.focused = false;
                p.set_window_active(false);
                self.let_go(p, now);
                tell_page(p, |page| {
                    page.hidden = true;
                    page.persisted = true;
                });
                p.surface_lifecycle(true, true);
            }
        }
    }

    /// The window lost the keyboard, or went: held buttons and keys let go.
    fn let_go<D: DataSource>(&mut self, p: &mut Presenter<D>, now: f64) {
        self.buttons = 0;
        self.clicks = Clicks::default();
        let _ = p.pointer_lost(now);
        for code in MODIFIERS {
            p.hold_modifier(code, false);
        }
    }

    fn resized<D: DataSource>(&mut self, p: &mut Presenter<D>) {
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        self.scale = window.scale_factor();
        let s = self.scale as f32;
        if let Some(e) = p.resize_scaled(size.width as f32 / s, size.height as f32 / s, s) {
            eprintln!("exact: resize: {e}");
        }
    }

    fn event<D: DataSource>(&mut self, p: &mut Presenter<D>, event: WindowEvent, now: f64) {
        match event {
            // As the Windows host: `beforeunload` can keep the window open;
            // otherwise the hatches and the packages end with it.
            WindowEvent::CloseRequested => {
                if p.before_unload(now) {
                    p.end_hatches();
                    p.end_packages();
                    self.closed = true;
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.resized(p),
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                p.set_window_active(focused);
                p.surface_lifecycle(false, !focused);
                if !focused {
                    self.let_go(p, now);
                }
            }
            WindowEvent::Occluded(occluded) => p.surface_lifecycle(occluded, !self.focused),
            WindowEvent::CursorMoved { position, .. } => {
                let at = (
                    (position.x / self.scale) as f32,
                    (position.y / self.scale) as f32,
                );
                self.pointer = Some(at);
                self.last = at;
                self.cursor_stale = true;
                p.set_pointer_kind(PointerKind::Mouse);
                let _ = p.pointer_move(at.0, at.1, now);
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer = None;
                self.cursor_stale = true;
                // A held button keeps its drag: the compositor still sends
                // its motion and release (an implicit grab).
                if self.buttons == 0 {
                    let _ = p.pointer_lost(now);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let bit = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 2,
                    MouseButton::Middle => 4,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 16,
                    MouseButton::Other(_) => return,
                };
                let down = state == ElementState::Pressed;
                if down {
                    self.buttons |= bit;
                } else {
                    self.buttons &= !bit;
                }
                let (x, y) = self.last;
                p.set_pointer_kind(PointerKind::Mouse);
                let _ = match (bit, down) {
                    (1, true) => {
                        let clicks = self.clicks.down(self.last, Instant::now());
                        p.pointer_down_with_clicks(x, y, now, clicks)
                    }
                    (1, false) => p.pointer_up(x, y, now),
                    _ => {
                        self.clicks = Clicks::default();
                        // Back and Forward act at their release (LLP 1115.000 D5.5).
                        p.pointer_aux(bit, down, x, y, now);
                        Ok(false)
                    }
                };
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = wheel(delta, self.scale);
                p.wheel_at(self.last.0, self.last.1, dx, dy);
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                let (code, key) = key_names(event.physical_key, &event.logical_key);
                let down = event.state == ElementState::Pressed;
                // Keys held as the window gains the keyboard, and released as
                // it loses it, are held state, not presses (a browser fires
                // no keydown for them).
                if is_synthetic {
                    p.hold_modifier(&code, down);
                    return;
                }
                // Escape also ends a pointer hold, as on KMS.
                if code == "Escape" && down {
                    let _ = p.pointer_lost(now);
                }
                press(p, (&code, &key), event.text.as_deref(), down, event.repeat);
            }
            WindowEvent::Ime(event) => self.ime.event(p, event),
            _ => {}
        }
    }
}

impl<D: DataSource> event_loop::Events<D> for Events {
    fn fds(&self) -> Vec<i32> {
        let clock_set = self.clock_set.iter().map(|set| set.fd());
        clock_set
            .chain(self.instance.iter().map(|i| i.fd()))
            .collect()
    }
    fn idle(&self) -> bool {
        false
    }
    fn publish(&mut self, _: std::sync::Arc<Pixmap>) {
        self.cursor_stale = true;
    }
    fn turn(&mut self, p: &mut Presenter<D>, wall: impl Fn() -> f64) {
        loop {
            // The queue's borrow ends before the event is handled.
            let Some(note) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            self.note(p, note, wall());
        }
        // A later launch shows the window, raised with its token.
        for token in self.instance.iter().flat_map(|i| i.take()) {
            let _ = self.proxy.send_event(Ask::Show(token));
        }
        let Some(window) = &self.window else {
            return;
        };
        self.ime.sync(window, p);
        if std::mem::take(&mut self.cursor_stale) {
            // A CSS cursor keyword is winit's cursor name (cursor-icon parses
            // CSS's); `auto` and `none` show the default arrow.
            let cursor = self
                .pointer
                .and_then(|(x, y)| p.cursor_at(x, y).name().parse().ok())
                .unwrap_or_default();
            if cursor != self.cursor {
                window.set_cursor(cursor);
                self.cursor = cursor;
            }
        }
    }
    fn date_jumped(&self) -> bool {
        self.clock_set.as_ref().is_some_and(|set| set.take())
    }
    fn trace_requested(&self) -> bool {
        super::TRACE.swap(false, std::sync::atomic::Ordering::Relaxed)
    }
    fn closed(&self) -> bool {
        self.closed
    }
}

/// The page's visibility as the window has it: put away, or shown again.
fn tell_page<D: DataSource>(p: &mut Presenter<D>, change: impl FnOnce(&mut exact_runner::Page)) {
    let mut page = p.host().runner().page();
    change(&mut page);
    if let Some(error) = p.set_page(page) {
        eprintln!("exact: page: {error}");
    }
}

/// The DOM's `code` and `key` for a winit key: winit's key codes are the
/// DOM's names, but for Super, which the DOM calls Meta, and Space, whose
/// `key` is the character.
fn key_names(physical: PhysicalKey, logical: &Key) -> (String, String) {
    let code = match physical {
        PhysicalKey::Code(code) => format!("{code:?}").replace("Super", "Meta"),
        PhysicalKey::Unidentified(_) => "Unidentified".into(),
    };
    let key = match logical {
        Key::Character(s) => s.to_string(),
        Key::Named(NamedKey::Space) => " ".into(),
        Key::Named(NamedKey::Super) => "Meta".into(),
        Key::Named(named) => format!("{named:?}"),
        Key::Dead(_) => "Dead".into(),
        Key::Unidentified(_) => "Unidentified".into(),
    };
    (code, key)
}

/// A key to the presenter. When xkbcommon's text for a press is not its
/// key (a dead key's composition), the press types that text, as a
/// browser's `keydown` names the key and its `input` carries the text. A
/// control character (Enter's `"\r"`) is no text.
fn press<D: DataSource>(
    p: &mut Presenter<D>,
    (code, key): (&str, &str),
    text: Option<&str>,
    down: bool,
    repeat: bool,
) {
    match text.filter(|t| down && *t != key && !t.chars().any(char::is_control)) {
        Some(text) => p.hardware_key_typing(code, key, text, repeat),
        None => p.hardware_key(code, key, down, repeat),
    }
}

/// A wheel or touchpad scroll in points, the web's sign (a positive `dy`
/// scrolls down). A notch is the browser's line; winit's positive deltas
/// move the content right and down, which scrolls left and up.
fn wheel(delta: MouseScrollDelta, scale: f64) -> (f32, f32) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (-x * LINE, -y * LINE),
        MouseScrollDelta::PixelDelta(at) => ((-at.x / scale) as f32, (-at.y / scale) as f32),
    }
}

/// The click count of a primary press: a press within GTK's double-click
/// time and distance (400 ms, 5 px) of the last one counts on from it.
#[derive(Default)]
struct Clicks {
    last: Option<(Instant, (f32, f32))>,
    count: u32,
}

impl Clicks {
    fn down(&mut self, at: (f32, f32), now: Instant) -> u32 {
        let near = self.last.is_some_and(|(then, from)| {
            now.duration_since(then) <= Duration::from_millis(400)
                && (from.0 - at.0).abs() <= 5.
                && (from.1 - at.1).abs() <= 5.
        });
        self.count = if near { self.count + 1 } else { 1 };
        self.last = Some((now, at));
        self.count
    }
}

/// The input method: allowed while a field that composes has the focus
/// (Wayland's text-input, X11's XIM). Its preedit and commits go to that
/// field; `ime_begin` refuses them once the focus has moved on.
#[derive(Default)]
struct InputMethod {
    allowed: Option<Target>,
    area: Option<(f32, f32, f32, f32)>,
    /// A preedit is showing.
    composing: bool,
}

impl InputMethod {
    fn sync<D: DataSource>(&mut self, window: &Window, p: &Presenter<D>) {
        let target = p.ime_target();
        if self.allowed != target {
            // Allowing again moves the input method to the new field.
            if self.allowed.is_some() {
                window.set_ime_allowed(false);
            }
            if target.is_some() {
                window.set_ime_allowed(true);
            }
            self.allowed = target;
            self.area = None;
        }
        let area = p.ime_cursor_area();
        if area != self.area {
            self.area = area;
            if let Some((x, y, width, height)) = area {
                window.set_ime_cursor_area(
                    LogicalPosition::new(x, y),
                    LogicalSize::new(width, height),
                );
            }
        }
    }

    fn event<D: DataSource>(&mut self, p: &mut Presenter<D>, event: Ime) {
        let field = self.allowed;
        let begin = |p: &mut Presenter<D>| field.is_some_and(|target| p.ime_begin(target));
        match event {
            // winit reports an empty preedit after each of the input
            // method's `done`s, composing or not: only one that clears a
            // showing preedit is the field's.
            Ime::Preedit(text, cursor) => {
                if (self.composing || !text.is_empty()) && begin(p) {
                    p.ime_preedit(&text, cursor);
                    self.composing = !text.is_empty();
                }
            }
            Ime::Commit(text) => {
                if begin(p) {
                    p.ime_commit(&text);
                }
                self.composing = false;
            }
            Ime::Disabled => {
                if std::mem::take(&mut self.composing) {
                    p.ime_end();
                }
            }
            Ime::Enabled => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use exact_painted::presenter::PainterChoice;
    use winit::keyboard::{KeyCode, NativeKeyCode};

    #[test]
    fn winit_keys_take_the_dom_names() {
        let names = |code, key: Key| key_names(PhysicalKey::Code(code), &key);
        assert_eq!(
            names(KeyCode::KeyQ, Key::Character("@".into())),
            ("KeyQ".into(), "@".into())
        );
        assert_eq!(
            names(KeyCode::Space, Key::Named(NamedKey::Space)),
            ("Space".into(), " ".into())
        );
        assert_eq!(
            names(KeyCode::SuperLeft, Key::Named(NamedKey::Super)),
            ("MetaLeft".into(), "Meta".into())
        );
        assert_eq!(
            names(KeyCode::ArrowLeft, Key::Named(NamedKey::ArrowLeft)),
            ("ArrowLeft".into(), "ArrowLeft".into())
        );
        assert_eq!(
            names(KeyCode::BrowserBack, Key::Named(NamedKey::BrowserBack)),
            ("BrowserBack".into(), "BrowserBack".into())
        );
        assert_eq!(
            key_names(
                PhysicalKey::Unidentified(NativeKeyCode::Xkb(0)),
                &Key::Dead(Some('´'))
            ),
            ("Unidentified".into(), "Dead".into())
        );
    }

    /// What xkbcommon types reaches a field: a layout's own characters, a
    /// dead key's composition, and Backspace and Enter, whose text is a
    /// control character.
    #[test]
    fn a_keymaps_text_types_into_a_field() {
        let plan = contract::compile(
            "component App\n  view\n    input testId=\"field\" width=200 height=40\n",
        )
        .unwrap()
        .encode();
        let mut p = Presenter::boot_with(
            &plan,
            (),
            (240., 80.),
            1.,
            Default::default(),
            PainterChoice::Cpu,
        )
        .unwrap()
        .0;
        let k = p.host().kernel();
        let field = k.node_by_key(k.find_by_test_id("field")[0]).unwrap().id;
        p.tap(field).unwrap();
        let mut key = |code, key, text| {
            press(&mut p, (code, key), text, true, false);
            press(&mut p, (code, key), None, false, false);
        };
        key("KeyZ", "z", Some("z"));
        key("Semicolon", "ö", Some("ö")); // a German layout's key
        key("BracketRight", "Dead", None); // the dead key types nothing
        key("KeyE", "e", Some("é")); // and composes with the next
        key("Space", " ", Some(" "));
        key("KeyX", "x", Some("x"));
        key("Backspace", "Backspace", Some("\u{8}"));
        let value = p
            .access_tree()
            .0
            .into_iter()
            .find(|n| n.view == field)
            .unwrap()
            .value;
        assert_eq!(value, "zöé ");
    }

    #[test]
    fn a_wheel_notch_is_a_line_and_a_touchpad_moves_points() {
        assert_eq!(wheel(MouseScrollDelta::LineDelta(0., -1.), 2.), (0., LINE));
        assert_eq!(
            wheel(
                MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(-6., 20.)),
                2.
            ),
            (3., -10.)
        );
    }

    #[test]
    fn presses_close_in_time_and_place_count_on() {
        let mut clicks = Clicks::default();
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        assert_eq!(clicks.down((10., 10.), t), 1);
        assert_eq!(clicks.down((12., 9.), ms(300)), 2);
        assert_eq!(clicks.down((12., 9.), ms(600)), 3);
        assert_eq!(clicks.down((12., 9.), ms(1100)), 1, "too late");
        assert_eq!(clicks.down((30., 9.), ms(1200)), 1, "too far");
    }
}
