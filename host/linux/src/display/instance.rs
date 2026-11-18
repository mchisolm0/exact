//! One process per app that keeps running (`host.linux.window.keepRunning`):
//! the first launch listens on a unix socket in `$XDG_RUNTIME_DIR` named by
//! the app's id, and a later launch (the app grid, a desktop file) hands it
//! its activation token there and ends, so the running app shows its window.
//! A socket nobody listens on is a crashed instance's, taken over.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a later launch waits for the running instance's answer.
const ANSWER: Duration = Duration::from_secs(2);

/// What a launch is, once it asked for the app's socket.
pub(crate) enum Claim {
    /// The app's one instance: it listens for later launches.
    First(Instance),
    /// The running instance took this launch's token; this launch ends.
    Handed,
    /// An instance listens but did not answer.
    Busy,
}

/// The running instance's socket, removed when it ends.
pub(crate) struct Instance {
    listener: UnixListener,
    path: PathBuf,
}

/// Where the app's instance listens: `$XDG_RUNTIME_DIR/exact-<app id>.sock`,
/// or nowhere without a runtime directory.
pub(crate) fn path(app: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty())?;
    Some(Path::new(&dir).join(format!("exact-{app}.sock")))
}

/// Listen at `path`, or hand `token` (the launch's activation token) to the
/// instance already listening there, which shows its window; unless `show`
/// is false, which leaves it as it is.
pub(crate) fn claim(path: &Path, token: Option<&str>, show: bool) -> std::io::Result<Claim> {
    match UnixListener::bind(path) {
        Ok(listener) => return Instance::listening(listener, path),
        Err(e) if e.kind() != ErrorKind::AddrInUse => return Err(e),
        Err(_) => {}
    }
    match UnixStream::connect(path) {
        // A hidden launch leaves the running instance as it is: it connects
        // and says nothing.
        Ok(_) if !show => Ok(Claim::Handed),
        Ok(stream) => Ok(hand(stream, token)),
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            std::fs::remove_file(path)?;
            Instance::listening(UnixListener::bind(path)?, path)
        }
        Err(e) => Err(e),
    }
}

/// One line, the token or nothing; the instance answers one byte once it has it.
fn hand(mut stream: UnixStream, token: Option<&str>) -> Claim {
    let token = token.filter(|t| !t.contains('\n')).unwrap_or("");
    let _ = stream.set_read_timeout(Some(ANSWER));
    let _ = stream.set_write_timeout(Some(ANSWER));
    let mut answer = [0];
    match stream
        .write_all(format!("{token}\n").as_bytes())
        .and_then(|()| stream.read_exact(&mut answer))
    {
        Ok(()) => Claim::Handed,
        Err(_) => Claim::Busy,
    }
}

impl Instance {
    fn listening(listener: UnixListener, path: &Path) -> std::io::Result<Claim> {
        listener.set_nonblocking(true)?;
        Ok(Claim::First(Instance {
            listener,
            path: path.to_owned(),
        }))
    }

    /// Readable when a later launch connects.
    pub(crate) fn fd(&self) -> i32 {
        self.listener.as_raw_fd()
    }

    /// The later launches since the last call, each with its activation
    /// token if it had one.
    pub(crate) fn take(&self) -> Vec<Option<String>> {
        let mut launches = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let mut line = String::new();
            // A launch that ends without its line asks for nothing.
            if !BufReader::new(&stream)
                .read_line(&mut line)
                .is_ok_and(|n| n > 0)
            {
                continue;
            }
            let _ = (&stream).write_all(b"1");
            let token = line.trim_end_matches('\n');
            launches.push((!token.is_empty()).then(|| token.to_owned()));
        }
        launches
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn socket(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("exact-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn first(claim: std::io::Result<Claim>) -> Instance {
        match claim.unwrap() {
            Claim::First(instance) => instance,
            _ => panic!("not the first instance"),
        }
    }

    #[test]
    fn a_later_launch_hands_its_token_to_the_running_instance() {
        let path = socket("handoff");
        let running = first(claim(&path, None, true));
        let later = {
            let path = path.clone();
            std::thread::spawn(move || {
                let tokenless = claim(&path, None, true).unwrap();
                let hidden = claim(&path, None, false).unwrap();
                let tokened = claim(&path, Some("gnome-shell/Glint/1234"), true).unwrap();
                [tokenless, hidden, tokened].map(|c| matches!(c, Claim::Handed))
            })
        };
        let mut launches = Vec::new();
        // Each launch is answered from `take`, so both are in once it ends.
        while !later.is_finished() {
            launches.extend(running.take());
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(later.join().unwrap(), [true, true, true]);
        // The hidden launch asked for nothing.
        assert_eq!(launches, [None, Some("gnome-shell/Glint/1234".to_owned())]);
        drop(running);
        assert!(!path.exists(), "the socket goes with its instance");
    }

    #[test]
    fn a_crashed_instances_socket_is_taken_over() {
        let path = socket("stale");
        // A listener dropped without its file, as a killed process leaves it.
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let instance = first(claim(&path, Some("token"), true));
        assert!(instance.take().is_empty());
    }
}
