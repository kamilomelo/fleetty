use crate::{
    aur::Aur,
    auth::Auth,
    inventory::Host,
    results::ResultKind,
    ui::{Color, confirm, stdout_color},
};
use std::{
    collections::HashMap,
    io::{self, Read, Write},
    os::fd::AsRawFd,
    process::{Command, ExitStatus, Output, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CONNECT_TIMEOUT: &str = "6";
const CONNECTION_ATTEMPTS: usize = 3;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_OUTPUT_LIMIT: usize = 16 * 1024;
const MAX_CLOCK_SKEW: u64 = 120;
pub(crate) struct ConnectError {
    pub(crate) kind: ResultKind,
    pub(crate) message: String,
    pub(crate) authentication_failed: bool,
}

pub(crate) struct Credentials {
    pub(crate) shared: Arc<Auth>,
    pub(crate) alternate: HashMap<String, Arc<Auth>>,
    pub(crate) sudo_alternate: HashMap<String, Arc<Auth>>,
}

impl Credentials {
    pub(crate) fn prompt() -> Result<Self, String> {
        Ok(Self {
            shared: Arc::new(Auth::prompt()?),
            alternate: HashMap::new(),
            sudo_alternate: HashMap::new(),
        })
    }

    pub(crate) fn for_host(&self, host: &Host) -> Arc<Auth> {
        self.alternate
            .get(&host.target())
            .cloned()
            .unwrap_or_else(|| Arc::clone(&self.shared))
    }

    pub(crate) fn for_sudo(&self, host: &Host, ssh_auth: &Arc<Auth>) -> Arc<Auth> {
        self.sudo_alternate
            .get(&host.target())
            .cloned()
            .unwrap_or_else(|| Arc::clone(ssh_auth))
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub(crate) fn ssh(host: &Host, remote: &str, interactive: bool, auth: &Auth) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args([
        "-o",
        &format!("ConnectTimeout={CONNECT_TIMEOUT}"),
        "-o",
        "ConnectionAttempts=1",
        "-o",
        "NumberOfPasswordPrompts=1",
    ]);
    if interactive {
        cmd.arg("-tt");
    } else {
        cmd.arg("-T").stdin(Stdio::null());
    }
    cmd.arg("-l")
        .arg(&host.user)
        .arg("--")
        .arg(&host.address)
        .arg(remote);
    auth.configure(&mut cmd);
    cmd
}

fn set_nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_bounded(pipe: &mut impl Read, buffer: &mut Vec<u8>, limit: usize) -> io::Result<bool> {
    let mut chunk = [0u8; 2048];
    match pipe.read(&mut chunk) {
        Ok(0) => Ok(false),
        Ok(n) if n > limit.saturating_sub(buffer.len()) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote output exceeded the probe limit",
        )),
        Ok(n) => {
            buffer.extend_from_slice(&chunk[..n]);
            Ok(true)
        }
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) =>
        {
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

// Read short, noninteractive SSH commands without trusting the remote process
// to finish or produce a finite amount of output. Interactive updates and
// user-entered commands deliberately do not use this watchdog.
fn bounded_output(mut command: Command, input: Option<&[u8]>) -> io::Result<Output> {
    bounded_output_with(&mut command, input, PROBE_TIMEOUT, PROBE_OUTPUT_LIMIT)
}

fn bounded_output_with(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    limit: usize,
) -> io::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    let start = Instant::now();
    let result = (|| -> io::Result<Output> {
        let mut stdin = child.stdin.take();
        if let Some(ref pipe) = stdin {
            set_nonblocking(pipe.as_raw_fd())?;
        }
        let mut sent = 0;
        let mut stdout = child.stdout.take().expect("piped SSH stdout");
        let mut stderr = child.stderr.take().expect("piped SSH stderr");
        set_nonblocking(stdout.as_raw_fd())?;
        set_nonblocking(stderr.as_raw_fd())?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (mut out_open, mut err_open) = (true, true);
        loop {
            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "remote SSH command timed out",
                ));
            }
            if let (Some(password), Some(pipe)) = (input, stdin.as_mut()) {
                let part = if sent < password.len() {
                    &password[sent..]
                } else {
                    b"\n".as_slice()
                };
                match pipe.write(part) {
                    Ok(n) => sent += n,
                    // A remote failure may close stdin before reading it. Keep
                    // the process diagnostic rather than reporting BrokenPipe.
                    Err(err) if err.kind() == io::ErrorKind::BrokenPipe => stdin = None,
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) => {}
                    Err(err) => return Err(err),
                }
                if sent > password.len() {
                    stdin = None;
                }
            }
            if out_open {
                out_open = read_bounded(&mut stdout, &mut out, limit)?;
            }
            if err_open {
                err_open = read_bounded(&mut stderr, &mut err, limit)?;
            }
            if let Some(status) = child.try_wait()?
                && !out_open
                && !err_open
            {
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            let mut fds = [
                libc::pollfd {
                    fd: stdout.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stderr.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if !out_open {
                fds[0].fd = -1;
            }
            if !err_open {
                fds[1].fd = -1;
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            let wait = remaining.min(Duration::from_millis(50)).as_millis() as i32;
            let polled = unsafe { libc::poll(fds.as_mut_ptr(), 2, wait) };
            if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return Err(io::Error::last_os_error());
            }
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

pub(crate) fn shutdown(host: &Host, auth: &Auth, sudo_auth: &Auth) -> io::Result<ExitStatus> {
    bounded_output(
        ssh(host, "sudo -S -p '' shutdown -h now", false, auth),
        Some(sudo_auth.password()),
    )
    .map(|output| output.status)
}

fn classify_connection(code: Option<i32>, stderr: &str) -> (ResultKind, bool) {
    if code != Some(255) {
        // SSH reached the remote shell; its command failed. Retrying won't help.
        return (ResultKind::Failed, false);
    }
    let non_retryable = stderr.contains("Permission denied")
        || stderr.contains("Host key verification failed")
        || stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
        || stderr.contains("Could not resolve hostname");
    if non_retryable {
        (ResultKind::Failed, false)
    } else {
        (ResultKind::Unreachable, true)
    }
}

fn authentication_failed(code: Option<i32>, stderr: &str) -> bool {
    code == Some(255)
        && stderr.contains("Permission denied")
        && !stderr.contains("Permission denied (publickey).")
}

fn connect(host: &Host, auth: &Auth) -> Result<String, ConnectError> {
    for attempt in 1..=CONNECTION_ATTEMPTS {
        // `hostname` is an optional package on some Arch installations.
        let output = match bounded_output(
            ssh(host, "cat /proc/sys/kernel/hostname", false, auth),
            None,
        ) {
            Ok(output) => output,
            Err(err) if err.kind() == io::ErrorKind::TimedOut && attempt < CONNECTION_ATTEMPTS => {
                continue;
            }
            Err(err) => {
                return Err(ConnectError {
                    kind: if err.kind() == io::ErrorKind::TimedOut {
                        ResultKind::Unreachable
                    } else {
                        ResultKind::Failed
                    },
                    message: format!("SSH check failed: {err}"),
                    authentication_failed: false,
                });
            }
        };
        if output.status.success() {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if name.is_empty() || name.contains(|c: char| c.is_control() || c.is_whitespace()) {
                return Err(ConnectError {
                    kind: ResultKind::Failed,
                    message: "invalid or empty remote hostname".into(),
                    authentication_failed: false,
                });
            }
            return Ok(name);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let (kind, retry) = classify_connection(output.status.code(), &stderr);
        if !retry || attempt == CONNECTION_ATTEMPTS {
            return Err(ConnectError {
                kind,
                message: format!(
                    "SSH check failed (exit {}, attempt {attempt}): {}{}",
                    output.status,
                    stderr.trim(),
                    if stderr.contains("Permission denied (publickey).") {
                        "\nServer requires an SSH key; if it is encrypted, unlock it with ssh-add before starting Fleetty. An account password cannot replace the key."
                    } else {
                        ""
                    }
                ),
                authentication_failed: authentication_failed(output.status.code(), &stderr),
            });
        }
        println!(
            "{}",
            stdout_color(
                &format!("SSH unavailable; retrying ({attempt}/{CONNECTION_ATTEMPTS})..."),
                Color::Yellow
            )
        );
        thread::sleep(Duration::from_secs(attempt as u64 * 2));
    }
    unreachable!()
}

pub(crate) fn connect_account(
    host: &Host,
    credentials: &mut Credentials,
) -> Result<(String, Arc<Auth>), ConnectError> {
    connect_account_with(host, credentials, connect, |host| {
        let prompt = format!(
            "SSH authentication failed for {}. Try a different password for this account",
            host.target()
        );
        if !confirm(&prompt)? {
            return Ok(None);
        }
        let label = format!(
            "SSH and sudo password for {} (this run only): ",
            host.target()
        );
        Auth::prompt_with(&label).map(Some)
    })
}

fn connect_account_with(
    host: &Host,
    credentials: &mut Credentials,
    mut try_connect: impl FnMut(&Host, &Auth) -> Result<String, ConnectError>,
    mut prompt_alternate: impl FnMut(&Host) -> Result<Option<Auth>, String>,
) -> Result<(String, Arc<Auth>), ConnectError> {
    let auth = credentials.for_host(host);
    match try_connect(host, &auth) {
        Ok(name) => Ok((name, auth)),
        Err(err)
            if err.authentication_failed && !credentials.alternate.contains_key(&host.target()) =>
        {
            let alternate = prompt_alternate(host).map_err(|message| ConnectError {
                kind: ResultKind::Failed,
                message,
                authentication_failed: false,
            })?;
            let Some(alternate) = alternate else {
                return Err(err);
            };
            let alternate = Arc::new(alternate);
            let name = try_connect(host, &alternate)?;
            credentials
                .alternate
                .insert(host.target(), Arc::clone(&alternate));
            Ok((name, alternate))
        }
        Err(err) => Err(err),
    }
}

enum SudoCheck {
    Accepted,
    WrongPassword,
    Failed(String),
}

fn check_sudo(host: &Host, ssh_auth: &Auth, sudo_auth: &Auth) -> SudoCheck {
    // Invalidate any cached sudo timestamp so a key-based SSH login cannot mask
    // an incorrect sudo password. The first check also supports NOPASSWD.
    let passwordless = bounded_output(ssh(host, "LC_ALL=C sudo -k -n -v", false, ssh_auth), None);
    match passwordless {
        Ok(output) if output.status.success() => return SudoCheck::Accepted,
        Ok(output) if output.status.code() == Some(255) => {
            return SudoCheck::Failed("SSH failed during sudo check".into());
        }
        Err(err) => return SudoCheck::Failed(format!("could not start sudo check: {err}")),
        _ => {}
    }
    match bounded_output(
        ssh(host, "LC_ALL=C sudo -k -S -p '' -v", false, ssh_auth),
        Some(sudo_auth.password()),
    ) {
        Ok(output) if output.status.success() => SudoCheck::Accepted,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.code() != Some(255) && sudo_password_rejected(&stderr) {
                SudoCheck::WrongPassword
            } else {
                SudoCheck::Failed(format!(
                    "sudo check failed ({}): {}",
                    output.status,
                    stderr.trim()
                ))
            }
        }
        Err(err) => SudoCheck::Failed(format!("sudo check: {err}")),
    }
}

fn sudo_password_rejected(stderr: &str) -> bool {
    stderr.contains("Sorry, try again.") || stderr.contains("incorrect password attempt")
}

pub(crate) fn prepare_sudo(
    host: &Host,
    ssh_auth: &Arc<Auth>,
    credentials: &mut Credentials,
) -> Result<Arc<Auth>, String> {
    prepare_sudo_with(host, ssh_auth, credentials, check_sudo, |host| {
        let prompt = format!(
            "{} rejected the sudo password. Try a different sudo password for this account",
            host.target()
        );
        if !confirm(&prompt)? {
            return Ok(None);
        }
        Auth::prompt_with(&format!(
            "Sudo password for {} (this run only): ",
            host.target()
        ))
        .map(Some)
    })
}

fn prepare_sudo_with(
    host: &Host,
    ssh_auth: &Arc<Auth>,
    credentials: &mut Credentials,
    mut verify: impl FnMut(&Host, &Auth, &Auth) -> SudoCheck,
    mut prompt: impl FnMut(&Host) -> Result<Option<Auth>, String>,
) -> Result<Arc<Auth>, String> {
    let sudo_auth = credentials.for_sudo(host, ssh_auth);
    match verify(host, ssh_auth, &sudo_auth) {
        SudoCheck::Accepted => Ok(sudo_auth),
        SudoCheck::Failed(err) => Err(err),
        SudoCheck::WrongPassword if !credentials.sudo_alternate.contains_key(&host.target()) => {
            let alternate = prompt(host)?.ok_or("sudo password rejected; host skipped")?;
            let alternate = Arc::new(alternate);
            match verify(host, ssh_auth, &alternate) {
                SudoCheck::Accepted => {
                    credentials
                        .sudo_alternate
                        .insert(host.target(), Arc::clone(&alternate));
                    Ok(alternate)
                }
                SudoCheck::WrongPassword => {
                    Err("alternate sudo password rejected; host skipped".into())
                }
                SudoCheck::Failed(err) => Err(err),
            }
        }
        SudoCheck::WrongPassword => Err("sudo password rejected; host skipped".into()),
    }
}

pub(crate) fn boot_id(host: &Host, auth: &Auth) -> Result<String, String> {
    let output = bounded_output(
        ssh(host, "cat /proc/sys/kernel/random/boot_id", false, auth),
        None,
    )
    .map_err(|e| e.to_string())?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || id.is_empty() {
        return Err("could not read boot ID".into());
    }
    Ok(id)
}

pub(crate) fn detect_aur(host: &Host, auth: &Auth) -> Result<Aur, String> {
    let output = bounded_output(ssh(host, "if command -v paru >/dev/null 2>&1; then echo paru; elif command -v yay >/dev/null 2>&1; then echo yay; else echo none; fi", false, auth), None)
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("{}: could not detect AUR helper", host.target()));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "paru" => Ok(Aur::Paru),
        "yay" => Ok(Aur::Yay),
        "none" => Ok(Aur::None),
        _ => Err(format!("{}: unexpected AUR helper response", host.target())),
    }
}

pub(crate) fn preflight(host: &Host, auth: &Auth) -> Result<(), String> {
    let lock = bounded_output(
        ssh(host, "test ! -e /var/lib/pacman/db.lck", false, auth),
        None,
    )
    .map_err(|e| format!("pacman lock check: {e}"))?;
    if !lock.status.success() {
        return Err("pacman database is locked or the lock check failed".into());
    }
    // `timedatectl` can report "no" for hosts using a different time service.
    // Compare the actual remote clock against this computer instead, accounting
    // for the time spent establishing the SSH connection.
    let before = unix_time()?;
    let clock = bounded_output(ssh(host, "date +%s", false, auth), None)
        .map_err(|e| format!("remote clock check: {e}"))?;
    let after = unix_time()?;
    if !clock.status.success() {
        return Err(format!(
            "remote clock check failed: {}",
            String::from_utf8_lossy(&clock.stderr).trim()
        ));
    }
    let remote = String::from_utf8_lossy(&clock.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| "remote clock returned an invalid Unix timestamp".to_string())?;
    let skew = before.abs_diff(remote).min(after.abs_diff(remote));
    if !clock_within_range(before, after, remote) {
        return Err(format!(
            "clock differs from this computer by at least {skew}s (maximum allowed {MAX_CLOCK_SKEW}s); check time synchronization"
        ));
    }
    Ok(())
}

fn clock_within_range(before: u64, after: u64, remote: u64) -> bool {
    remote >= before.saturating_sub(MAX_CLOCK_SKEW)
        && remote <= after.saturating_add(MAX_CLOCK_SKEW)
}

fn unix_time() -> Result<u64, String> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("local clock check: {e}"))?
        .as_secs())
}

pub(crate) fn check_read_only(host: &Host, auth: &Auth) -> Result<Aur, String> {
    preflight(host, auth)?;
    boot_id(host, auth)?;
    detect_aur(host, auth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::ResultKind;
    use std::{collections::HashMap, process::Command};
    #[test]
    fn only_ssh_authentication_failure_offers_alternate_password() {
        assert!(authentication_failed(
            Some(255),
            "Permission denied (publickey,password)."
        ));
        for (status, message) in [
            (Some(255), "Connection refused"),
            (Some(255), "Host key verification failed"),
            (Some(255), "Permission denied (publickey)."),
            (Some(1), "Permission denied"),
        ] {
            assert!(!authentication_failed(status, message));
        }
    }

    #[test]
    fn alternate_password_is_tried_once_and_remembered_per_account() {
        let mut credentials = Credentials {
            shared: Arc::new(Auth::for_test(b"shared")),
            alternate: HashMap::new(),
            sudo_alternate: HashMap::new(),
        };
        let admin = Host {
            address: "192.0.2.1".into(),
            user: "admin".into(),
            groups: vec!["servers".into()],
            priority: false,
        };
        let deploy = Host {
            user: "deploy".into(),
            ..admin.clone()
        };
        let mut attempted = Vec::new();
        let mut prompted = Vec::new();
        let mut connect = |host: &Host, auth: &Auth| {
            attempted.push((host.user.clone(), auth.password().to_vec()));
            if host.user == "deploy" && auth.password() == b"shared" {
                Err(ConnectError {
                    kind: ResultKind::Failed,
                    message: "Permission denied".into(),
                    authentication_failed: true,
                })
            } else {
                Ok("test-machine".into())
            }
        };
        let mut prompt = |host: &Host| {
            prompted.push(host.target());
            Ok(Some(Auth::for_test(b"alternate")))
        };
        connect_account_with(&admin, &mut credentials, &mut connect, &mut prompt)
            .map_err(|err| err.message)
            .unwrap();
        connect_account_with(&deploy, &mut credentials, &mut connect, &mut prompt)
            .map_err(|err| err.message)
            .unwrap();
        connect_account_with(&deploy, &mut credentials, &mut connect, &mut prompt)
            .map_err(|err| err.message)
            .unwrap();
        assert_eq!(prompted, ["deploy@192.0.2.1"]);
        assert_eq!(
            attempted,
            [
                ("admin".into(), b"shared".to_vec()),
                ("deploy".into(), b"shared".to_vec()),
                ("deploy".into(), b"alternate".to_vec()),
                ("deploy".into(), b"alternate".to_vec()),
            ]
        );
        assert_eq!(credentials.alternate.len(), 1);
    }

    #[test]
    fn key_login_can_use_a_separate_sudo_password_without_affecting_other_accounts() {
        let mut credentials = Credentials {
            shared: Arc::new(Auth::for_test(b"shared")),
            alternate: HashMap::new(),
            sudo_alternate: HashMap::new(),
        };
        let host = Host {
            address: "192.0.2.1".into(),
            user: "deploy".into(),
            groups: vec!["servers".into()],
            priority: false,
        };
        let other = Host {
            user: "admin".into(),
            ..host.clone()
        };
        let ssh_auth = Arc::clone(&credentials.shared); // SSH connected via key
        let mut tried = Vec::new();
        let mut prompts = 0;
        let mut verify = |_: &Host, ssh: &Auth, sudo: &Auth| {
            assert_eq!(ssh.password(), b"shared");
            tried.push(sudo.password().to_vec());
            if sudo.password() == b"sudo-only" {
                SudoCheck::Accepted
            } else {
                SudoCheck::WrongPassword
            }
        };
        let mut prompt = |_: &Host| {
            prompts += 1;
            Ok(Some(Auth::for_test(b"sudo-only")))
        };
        let chosen =
            prepare_sudo_with(&host, &ssh_auth, &mut credentials, &mut verify, &mut prompt)
                .unwrap();
        assert_eq!(chosen.password(), b"sudo-only");
        prepare_sudo_with(&host, &ssh_auth, &mut credentials, &mut verify, &mut prompt).unwrap();
        assert_eq!(
            tried,
            [
                b"shared".to_vec(),
                b"sudo-only".to_vec(),
                b"sudo-only".to_vec()
            ]
        );
        assert_eq!(prompts, 1);
        assert_eq!(
            credentials.for_sudo(&other, &ssh_auth).password(),
            b"shared"
        );
        assert_eq!(credentials.for_host(&host).password(), b"shared");
    }

    #[test]
    fn sudo_non_authentication_failure_does_not_prompt() {
        assert!(sudo_password_rejected("sudo: 1 incorrect password attempt"));
        assert!(!sudo_password_rejected(
            "sudo: user is not allowed to run sudo"
        ));
        let host = Host {
            address: "192.0.2.1".into(),
            user: "admin".into(),
            groups: vec![],
            priority: false,
        };
        let mut credentials = Credentials {
            shared: Arc::new(Auth::for_test(b"shared")),
            alternate: HashMap::new(),
            sudo_alternate: HashMap::new(),
        };
        let ssh_auth = Arc::clone(&credentials.shared);
        let result = prepare_sudo_with(
            &host,
            &ssh_auth,
            &mut credentials,
            |_, _, _| SudoCheck::Failed("not permitted".into()),
            |_| panic!("must not prompt for policy failures"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn remote_failure_is_not_a_network_failure() {
        let (kind, retry) = classify_connection(Some(127), "bash: hostname: command not found");
        assert!(matches!(kind, ResultKind::Failed));
        assert!(!retry);
        let (kind, retry) = classify_connection(Some(255), "ssh: connect: No route to host");
        assert!(matches!(kind, ResultKind::Unreachable));
        assert!(retry);
    }

    #[test]
    fn noninteractive_commands_have_output_and_time_limits() {
        let mut normal = Command::new("sh");
        normal.args(["-c", "printf ok; printf warn >&2"]);
        let output = bounded_output_with(&mut normal, None, Duration::from_secs(1), 8).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok");
        assert_eq!(output.stderr, b"warn");

        let mut oversized = Command::new("sh");
        oversized.args(["-c", "printf 123456789"]);
        assert_eq!(
            bounded_output_with(&mut oversized, None, Duration::from_secs(1), 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut stalled = Command::new("sh");
        stalled.args(["-c", "exec sleep 2"]);
        assert_eq!(
            bounded_output_with(&mut stalled, None, Duration::from_millis(80), 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );

        let mut password = Command::new("sh");
        password.args(["-c", "read -r input; [ \"$input\" = test-only ]"]);
        assert!(
            bounded_output_with(&mut password, Some(b"test-only"), Duration::from_secs(1), 8)
                .unwrap()
                .status
                .success()
        );

        let mut never_reads = Command::new("sh");
        never_reads.args(["-c", "exec sleep 2"]);
        assert_eq!(
            bounded_output_with(
                &mut never_reads,
                Some(&vec![b'x'; 128 * 1024]),
                Duration::from_millis(80),
                8,
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );

        let mut floods = Command::new("sh");
        floods.args(["-c", "while :; do printf 123456789; done"]);
        assert_eq!(
            bounded_output_with(&mut floods, None, Duration::from_secs(1), 8)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn clock_check_compares_actual_time_not_sync_service() {
        assert!(clock_within_range(1_000, 1_002, 1_001));
        assert!(clock_within_range(1_000, 1_002, 880));
        assert!(!clock_within_range(1_000, 1_002, 879));
        assert!(!clock_within_range(1_000, 1_002, 1_123));
    }
}
