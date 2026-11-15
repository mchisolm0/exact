//! The host services LLP 1133.025's desktop modules use (D11 item 2): the
//! process's one hot-key handler, the taskbar button and the launch. Each
//! platform object is one per process, so the host keeps it and a package
//! reaches it through its [`Context`](crate::package::Context), keyed by the
//! calling session.
//!
//! A host that has these installs its [`Services`] before the loop starts
//! (the Windows host, `host/windows/src/desktop.rs`). On Linux the first
//! read makes the desktop portal's hot-key table (`portal/shortcuts.rs`),
//! which keeps hot keys only: attention and the login item answer that
//! they are not kept. Elsewhere [`services`] is `None` and the packages
//! answer `NotSupportedError`.
//!
//! A hot key's press is an activation the host raises (D11 item 3): the
//! host's callback calls [`activate`], and the presenter's next turn raises
//! the registration's event with a person's origin on the runner, which a
//! package's own `emit` never gets.

use std::cell::RefCell;
use std::rc::Rc;

/// Why a chord is not registered, in `@exact/global-shortcuts`' words.
pub type Refusal = &'static str;

/// The login item as the platform keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct Login {
    /// `enabled`, `disabled`, `requires-approval` or `unknown`.
    pub state: &'static str,
    /// Whether the registration asked for a launch with no window.
    pub hidden: bool,
}

/// What a desktop host keeps for every session of the process. Called on
/// the loop's thread.
pub trait Services {
    /// Register `keys` (an `aria-keyshortcuts` chord) for `session`'s `id`;
    /// a press raises `extension`'s `event` with `[id, {id, keys}]`. With
    /// `repeat`, a held chord raises a press per keyboard repeat.
    fn register_hotkey(
        &self,
        session: &str,
        id: &str,
        keys: &str,
        repeat: bool,
        extension: &'static str,
        event: &'static str,
    ) -> Result<(), Refusal>;
    /// `session` no longer reads `keys` for `id`.
    fn unregister_hotkey(&self, session: &str, id: &str, keys: &str);
    /// Whether `keys` could be registered now, keeping nothing.
    fn check_hotkey(&self, keys: &str) -> Result<(), Refusal>;
    /// Ask for the person's attention on the app's taskbar button or tile:
    /// `count` flashes when the author names a number, which takes the
    /// place of a critical request's flashing; `Err` says why nothing was
    /// asked.
    fn attention(&self, critical: bool, count: Option<u32>) -> Result<(), String>;
    /// The login item's registration as it stands.
    fn login(&self) -> Login;
    /// Register the login item (`Some(hidden)`) or remove it (`None`).
    fn set_login(&self, register: Option<bool>) -> Result<Login, String>;
    /// Open the system's settings for the login item.
    fn open_login_settings(&self) -> Result<(), String>;
    /// This launch was the login item's: `Some(hidden)`, read before the
    /// session booted.
    fn login_launch(&self) -> Option<bool>;
    /// Call `changed`, from any thread, when the system takes or gives back
    /// a chord it can reserve (Windows: bare Print Screen, as Snipping
    /// Tool's setting changes), until `session` is unwatched.
    fn watch_reserved(&self, session: &str, changed: Box<dyn Fn() + Send>) {
        let _ = (session, changed);
    }
    /// Stop calling `session`'s [`Services::watch_reserved`] callback.
    fn unwatch_reserved(&self, session: &str) {
        let _ = session;
    }
}

/// A press the host heard, waiting for the presenter's turn.
struct Activation {
    extension: &'static str,
    event: &'static str,
    args: String,
}

thread_local! {
    static SERVICES: RefCell<Option<Rc<dyn Services>>> = const { RefCell::new(None) };
    static ACTIVATIONS: RefCell<Vec<Activation>> = const { RefCell::new(Vec::new()) };
}

/// Presses heard off the loop's thread (the Linux portal's reader).
static FAR: std::sync::Mutex<Vec<Activation>> = std::sync::Mutex::new(Vec::new());

/// The app's id, which the Linux desktop keeps a portal's answers by.
static APP_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The running app's id, told by the host at boot.
pub(crate) fn know_app(id: &str) {
    let _ = APP_ID.set(id.to_owned());
}

/// The host's services, from now on, for every session on this thread.
pub fn install(services: Rc<dyn Services>) {
    SERVICES.with(|s| *s.borrow_mut() = Some(services));
}

/// The host's services, or `None` where the host keeps none.
pub fn services() -> Option<Rc<dyn Services>> {
    SERVICES.with(|s| {
        #[cfg(target_os = "linux")]
        if s.borrow().is_none() {
            let app_id = APP_ID.get().cloned().unwrap_or_default();
            *s.borrow_mut() = Some(Rc::new(crate::portal::shortcuts::Shortcuts::new(app_id)));
        }
        s.borrow().clone()
    })
}

/// The host's platform callback heard a person's act for `extension`'s
/// `event`: raised on the next turn with a person's origin. `args` is a
/// JSON array. The host wakes its loop after.
pub fn activate(extension: &'static str, event: &'static str, args: String) {
    ACTIVATIONS.with(|a| {
        a.borrow_mut().push(Activation {
            extension,
            event,
            args,
        })
    });
}

/// [`activate`] from any thread, for a platform that reports presses on
/// one of its own: raised on the loop's next turn. The caller wakes the
/// loop after.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn activate_from_any_thread(extension: &'static str, event: &'static str, args: String) {
    let heard = Activation {
        extension,
        event,
        args,
    };
    FAR.lock().unwrap_or_else(|e| e.into_inner()).push(heard);
}

/// The presses waiting from other threads, taken: their arguments.
#[cfg(test)]
pub(crate) fn take_far() -> Vec<String> {
    let far = std::mem::take(&mut *FAR.lock().unwrap_or_else(|e| e.into_inner()));
    far.into_iter().map(|a| a.args).collect()
}

/// The activations heard since the last turn, raised on `runner`.
pub(crate) fn raise<D: exact_runner::DataSource>(runner: &mut exact_runner::Runner<D>) {
    let mut heard = ACTIVATIONS.with(|a| std::mem::take(&mut *a.borrow_mut()));
    heard.append(&mut FAR.lock().unwrap_or_else(|e| e.into_inner()));
    for a in heard {
        let origin = exact_runner::CallOrigin::App;
        if let Err(e) = runner.raise_module_event(a.extension, a.event, &a.args, origin) {
            runner.log(e);
        }
    }
}

#[cfg(test)]
#[path = "desktop_tests.rs"]
mod tests;
