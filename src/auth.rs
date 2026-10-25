use std::{
    env,
    fs::{File, Permissions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct Secret(Vec<u8>);
const MAX_PASSWORD_BYTES: usize = 1024;

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct EchoGuard {
    fd: i32,
    original: libc::termios,
}

impl EchoGuard {
    fn disable(fd: i32) -> Result<Self, String> {
        // The fd is an open local terminal. Restore its exact original settings on drop.
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut hidden = original;
        // Avoid a Ctrl-C killing the process before the guard can restore echo.
        hidden.c_lflag &= !(libc::ECHO | libc::ISIG);
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(Self { fd, original })
    }

    fn raw(fd: i32) -> Result<Self, String> {
        let mut original = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        Ok(Self { fd, original })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

pub fn raw_mode() -> Result<Option<impl Drop>, String> {
    let fd = io::stdin().as_raw_fd();
    if unsafe { libc::isatty(fd) } == 0 {
        return Ok(None);
    }
    EchoGuard::raw(fd).map(Some)
}

// SSH's stdin is piped during interactive updates, so it cannot infer the
// local terminal's geometry from stdin when allocating the remote PTY.
pub fn terminal_size() -> Option<(u16, u16)> {
    let tty = File::open("/dev/tty").ok()?;
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0
        || size.ws_row == 0
        || size.ws_col == 0
    {
        return None;
    }
    Some((size.ws_row, size.ws_col))
}

pub struct Auth {
    secret: Arc<Secret>,
    path: PathBuf,
    _dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    executable: PathBuf,
}

impl Auth {
    pub fn prompt() -> Result<Self, String> {
        let mut tty = open_tty()?;
        loop {
            let first = read_password(
                &mut tty,
                "Shared SSH and sudo password (held only for this run): ",
            )?;
            let repeated = read_password(&mut tty, "Repeat shared password: ")?;
            if let Some(secret) = confirmed_password(first, repeated) {
                return Self::from_secret(secret);
            }
            writeln!(tty, "Passwords do not match; please try again.")
                .map_err(|e| e.to_string())?;
        }
    }

    pub fn prompt_with(label: &str) -> Result<Self, String> {
        let mut tty = open_tty()?;
        let mut secret = read_password(&mut tty, label)?;
        Self::from_secret(std::mem::take(&mut secret.0))
    }

    fn from_secret(secret: Vec<u8>) -> Result<Self, String> {
        let secret = Arc::new(Secret(secret));
        // The socket is only discoverable within a private, owner-only directory.
        let dir = tempfile::Builder::new()
            .prefix("fleetty-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .map_err(|e| e.to_string())?;
        let executable = dir.path().join("ssh-askpass");
        std::os::unix::fs::symlink(env::current_exe().map_err(|e| e.to_string())?, &executable)
            .map_err(|e| e.to_string())?;
        let path = dir.path().join("askpass.sock");
        let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_secret, worker_stop) = (Arc::clone(&secret), Arc::clone(&stop));
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.write_all(&worker_secret.0);
                        let _ = stream.write_all(b"\n");
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20))
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            secret,
            path,
            _dir: dir,
            stop,
            worker: Some(worker),
            executable,
        })
    }

    pub fn configure(&self, command: &mut Command) {
        command
            .env("SSH_ASKPASS", &self.executable)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "fleetty")
            .env("FLEETTY_ASKPASS_SOCKET", &self.path);
    }

    pub fn password(&self) -> &[u8] {
        &self.secret.0
    }

    #[cfg(test)]
    pub fn for_test(secret: &[u8]) -> Self {
        Self::from_secret(secret.to_vec()).unwrap()
    }
}

fn open_tty() -> Result<File, String> {
    File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("a local terminal is required for the password: {e}"))
}

fn read_password(tty: &mut File, label: &str) -> Result<Secret, String> {
    write!(tty, "{label}").map_err(|e| e.to_string())?;
    tty.flush().map_err(|e| e.to_string())?;
    let guard = EchoGuard::disable(tty.as_raw_fd())?;
    let mut secret = Secret(Vec::new());
    let result = (|| -> Result<(), String> {
        loop {
            let mut byte = [0];
            match tty.read(&mut byte).map_err(|e| e.to_string())? {
                0 => return Err("password input closed".into()),
                _ if byte[0] == b'\n' => break,
                _ if secret.0.len() >= MAX_PASSWORD_BYTES => {
                    return Err(format!("password exceeds {MAX_PASSWORD_BYTES} bytes"));
                }
                _ => secret.0.push(byte[0]),
            }
        }
        if secret.0.last() == Some(&b'\r') {
            secret.0.pop();
        }
        if secret.0.is_empty() || secret.0.iter().any(u8::is_ascii_control) {
            return Err("password must be nonempty and on one line".into());
        }
        Ok(())
    })();
    drop(guard);
    writeln!(tty).map_err(|e| e.to_string())?;
    result?;
    Ok(secret)
}

fn confirmed_password(mut first: Secret, repeated: Secret) -> Option<Vec<u8>> {
    (first.0 == repeated.0).then(|| std::mem::take(&mut first.0))
}

impl Drop for Auth {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn askpass() -> Result<(), String> {
    let prompt = env::args().nth(1).unwrap_or_default();
    // Never treat a host-key or key-passphrase question as a password prompt.
    if !prompt.to_ascii_lowercase().contains("password") {
        return Err("unsupported SSH prompt; verify host keys and unlock SSH keys first".into());
    }
    let socket = env::var_os("FLEETTY_ASKPASS_SOCKET").ok_or("missing askpass socket")?;
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    io::copy(&mut stream, &mut io::stdout()).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secret_socket_is_in_a_private_temporary_directory() {
        let auth = Auth::for_test(b"test-secret");
        assert_eq!(
            std::fs::metadata(auth._dir.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(auth.path.exists());
    }

    #[test]
    fn shared_password_requires_matching_second_entry() {
        assert!(confirmed_password(Secret(b"first".to_vec()), Secret(b"typo".to_vec())).is_none());
        assert_eq!(
            confirmed_password(Secret(b"secret".to_vec()), Secret(b"secret".to_vec())),
            Some(b"secret".to_vec())
        );
    }
}
