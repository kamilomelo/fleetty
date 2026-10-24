use std::{
    env,
    fs::File,
    io::{self, BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
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
        Self::prompt_with("Shared SSH and sudo password (held only for this run): ")
    }

    pub fn prompt_with(label: &str) -> Result<Self, String> {
        let mut tty = File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|e| format!("a local terminal is required for the password: {e}"))?;
        write!(tty, "{label}").map_err(|e| e.to_string())?;
        tty.flush().map_err(|e| e.to_string())?;
        let guard = EchoGuard::disable(tty.as_raw_fd())?;
        let mut secret = Vec::new();
        BufReader::new(&tty)
            .read_until(b'\n', &mut secret)
            .map_err(|e| e.to_string())?;
        drop(guard);
        writeln!(tty).map_err(|e| e.to_string())?;
        while secret.last().is_some_and(|c| *c == b'\n' || *c == b'\r') {
            secret.pop();
        }
        if secret.is_empty() || secret.iter().any(u8::is_ascii_control) {
            secret.fill(0);
            return Err("password must be nonempty and on one line".into());
        }
        Self::from_secret(secret)
    }

    fn from_secret(secret: Vec<u8>) -> Result<Self, String> {
        let secret = Arc::new(Secret(secret));
        // The socket is only discoverable within a private, owner-only directory.
        let dir = tempfile::Builder::new()
            .prefix("fleetty-")
            .tempdir_in("/tmp/opencode")
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
