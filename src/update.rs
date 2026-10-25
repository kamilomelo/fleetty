use crate::{
    aur::Aur,
    auth::{self, Auth},
    inventory::Host,
    results::ResultKind,
    ssh::{boot_id, detect_aur, preflight, shutdown, sized_interactive_command, ssh},
    ui::{Color, confirm, menu, stderr_color, stdout_color},
};
use std::{
    io::{self, Read, Write},
    os::fd::AsRawFd,
    process::{ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const NEED: &[u8] = b"\x1eFLEETTY_NEED\x1f";
const READY: &[u8] = b"\x1eFLEETTY_READY\x1f";
const UPDATED: &[u8] = b"\x1eFLEETTY_UPDATED\x1f";
const UNCHANGED: &[u8] = b"\x1eFLEETTY_UNCHANGED\x1f";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Need,
    Ready,
    Updated,
    Unchanged,
}

#[derive(Default)]
struct SudoPasswordGate {
    ready: bool,
    sent: bool,
}

impl SudoPasswordGate {
    fn on_marker(&mut self, marker: Marker) -> Result<bool, String> {
        match marker {
            Marker::Need if self.ready => Ok(false),
            Marker::Need if self.sent => {
                Err("sudo requested a password more than once; stopping this update".into())
            }
            Marker::Need => {
                self.sent = true;
                Ok(true)
            }
            Marker::Ready => {
                self.ready = true;
                Ok(false)
            }
            Marker::Updated | Marker::Unchanged => Ok(false),
        }
    }
}

enum UpdateAttempt {
    UpdateFailed,
    RebootFailed,
    RebootRequested,
    NoChanges,
}

pub(crate) enum UpdateOutcome {
    Complete(ResultKind),
    RebootRequested { before: String, requested: Instant },
}

fn forward_output<R: Read>(
    mut source: R,
    tx: mpsc::Sender<Marker>,
    mut destination: impl Write,
) -> io::Result<()> {
    let markers: [(&[u8], Marker); 4] = [
        (NEED, Marker::Need),
        (READY, Marker::Ready),
        (UPDATED, Marker::Updated),
        (UNCHANGED, Marker::Unchanged),
    ];
    let mut chunk = [0u8; 4096];
    let mut pending = Vec::new();
    loop {
        let count = source.read(&mut chunk)?;
        if count == 0 {
            destination.write_all(&pending)?;
            destination.flush()?;
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..count]);
        loop {
            let found = markers
                .iter()
                .filter_map(|(bytes, marker)| {
                    pending
                        .windows(bytes.len())
                        .position(|window| window == *bytes)
                        .map(|pos| (pos, bytes.len(), *marker))
                })
                .min_by_key(|(pos, _, _)| *pos);
            if let Some((pos, len, marker)) = found {
                destination.write_all(&pending[..pos])?;
                let _ = tx.send(marker);
                pending.drain(..pos + len);
                continue;
            }
            // Keep only bytes that could begin a marker split across reads.
            let keep = markers
                .iter()
                .flat_map(|(bytes, _)| {
                    (1..bytes.len())
                        .rev()
                        .find(|&n| pending.ends_with(&bytes[..n]))
                })
                .max()
                .unwrap_or(0);
            let flush = pending.len() - keep;
            destination.write_all(&pending[..flush])?;
            pending.drain(..flush);
            destination.flush()?;
            break;
        }
    }
}

fn update_script(aur: Aur) -> String {
    format!(
        "if sudo -n -v >/dev/null 2>&1; then printf '\\036FLEETTY_READY\\037'; \
         else stty -echo; \
          sudo -S -p \"$(printf '\\036FLEETTY_NEED\\037')\" -v; result=$?; stty echo; [ \"$result\" -eq 0 ] || exit \"$result\"; \
          printf '\\036FLEETTY_READY\\037'; fi; \
          before=$(pacman -Q) || exit $?; \
           before=$(printf '%s\\n' \"$before\" | LC_ALL=C sort) || exit $?; \
           echo '[fleetty] Starting official package update: sudo pacman -Syu'; \
          sudo pacman -Syu || exit $?; \
          {} || exit $?; \
           echo '[fleetty] AUR stage finished (or skipped if no helper was detected)'; \
          after=$(pacman -Q) || exit $?; \
           after=$(printf '%s\\n' \"$after\" | LC_ALL=C sort) || exit $?; \
           if [ \"$before\" = \"$after\" ]; then printf '\\036FLEETTY_UNCHANGED\\037'; exit 0; fi; \
          printf '\\036FLEETTY_UPDATED\\037'; sudo reboot",
        aur.stage()
    )
}

fn interactive_update(
    host: &Host,
    aur: Aur,
    auth: &Auth,
    sudo_auth: &Auth,
) -> Result<UpdateAttempt, String> {
    // The ready/need markers let us send the password only during initial sudo
    // authentication. Later output (including AUR builds) cannot request it.
    // Interactive AUR prompts and reboot remain in this SSH terminal.
    let remote = sized_interactive_command(&update_script(aur));
    let _raw = auth::raw_mode()?;
    let mut child = ssh(host, &remote, true, auth)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel();
    let output = child.stdout.take().expect("piped SSH output");
    let printer = thread::spawn(move || forward_output(output, tx, io::stdout()));
    let mut input = child.stdin.take().expect("piped SSH input");
    let mut sudo_gate = SudoPasswordGate::default();
    let mut updated = false;
    let mut unchanged = false;
    let mut input_closed = false;
    let outcome: Result<ExitStatus, String> = (|| -> Result<ExitStatus, String> {
        loop {
            for marker in rx.try_iter() {
                if sudo_gate.on_marker(marker)? {
                    input
                        .write_all(sudo_auth.password())
                        .map_err(|e| e.to_string())?;
                    input.write_all(b"\n").map_err(|e| e.to_string())?;
                    input.flush().map_err(|e| e.to_string())?;
                }
                match marker {
                    Marker::Need | Marker::Ready => (),
                    Marker::Updated => updated = true,
                    Marker::Unchanged => unchanged = true,
                }
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                break Ok(status);
            }
            let mut pollfd = libc::pollfd {
                fd: io::stdin().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if sudo_gate.ready
                && !input_closed
                && unsafe { libc::poll(&mut pollfd, 1, 100) } > 0
                && pollfd.revents & libc::POLLIN != 0
            {
                let mut bytes = [0u8; 4096];
                let count = io::stdin().read(&mut bytes).map_err(|e| e.to_string())?;
                if count == 0 {
                    input_closed = true;
                } else {
                    input
                        .write_all(&bytes[..count])
                        .map_err(|e| e.to_string())?;
                }
            } else {
                thread::sleep(Duration::from_millis(20));
            }
        }
    })();
    drop(input);
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let print_result = printer
        .join()
        .map_err(|_| "SSH output worker panicked")?
        .map_err(|e| e.to_string());
    print_result?;
    let status = outcome?;
    for marker in rx.try_iter() {
        if marker == Marker::Updated {
            updated = true;
        } else if marker == Marker::Unchanged {
            unchanged = true;
        }
    }
    if unchanged && status.success() {
        Ok(UpdateAttempt::NoChanges)
    } else if !updated {
        eprintln!(
            "{}",
            stderr_color(
                &format!("{}: update/reboot SSH exit status: {status}", host.target()),
                Color::Red
            )
        );
        Ok(UpdateAttempt::UpdateFailed)
    } else if status.success() || status.code() == Some(255) {
        Ok(UpdateAttempt::RebootRequested)
    } else {
        eprintln!(
            "{}",
            stderr_color(
                &format!("{}: reboot command failed: {status}", host.target()),
                Color::Red
            )
        );
        Ok(UpdateAttempt::RebootFailed)
    }
}

pub(crate) fn update(host: &Host, auth: &Auth, sudo_auth: &Auth) -> Result<UpdateOutcome, String> {
    loop {
        match preflight(host, auth) {
            Ok(()) => break,
            Err(err) => eprintln!(
                "{}",
                stderr_color(
                    &format!("{}: preflight needs attention: {err}", host.target()),
                    Color::Yellow
                )
            ),
        }
        match menu(
            "Next step",
            &[
                "Open interactive SSH shell to repair, then recheck",
                "Recheck",
                "Skip and flag",
            ],
        )? {
            1 => {
                let _ = ssh(
                    host,
                    &sized_interactive_command("exec ${SHELL:-/bin/sh} -l"),
                    true,
                    auth,
                )
                .status();
            }
            2 => (),
            3 => return Ok(UpdateOutcome::Complete(ResultKind::Failed)),
            _ => unreachable!(),
        }
    }
    let before = match boot_id(host, auth) {
        Ok(id) => id,
        Err(e) => {
            eprintln!(
                "{}",
                stderr_color(&format!("{}: {e}", host.target()), Color::Red)
            );
            return Ok(UpdateOutcome::Complete(ResultKind::Failed));
        }
    };
    let aur = match detect_aur(host, auth) {
        Ok(aur) => aur,
        Err(err) => {
            eprintln!("{}", stderr_color(&err, Color::Red));
            return Ok(UpdateOutcome::Complete(ResultKind::Failed));
        }
    };
    if matches!(aur, Aur::None)
        && !confirm(&format!(
            "{}: no paru or yay detected. Update pacman packages only",
            host.target()
        ))?
    {
        return Ok(UpdateOutcome::Complete(ResultKind::Skipped));
    }
    let command = format!("sudo pacman -Syu && {}", aur.command());
    loop {
        println!("Running: {command}");
        match interactive_update(host, aur, auth, sudo_auth) {
            Ok(UpdateAttempt::RebootRequested) => break,
            Ok(UpdateAttempt::NoChanges) => {
                println!(
                    "{}",
                    stdout_color(
                        &format!(
                            "{}: no package changes; leaving it running without reboot.",
                            host.target()
                        ),
                        Color::Green
                    )
                );
                return Ok(UpdateOutcome::Complete(ResultKind::Unchanged));
            }
            Ok(UpdateAttempt::RebootFailed) => {
                return Ok(UpdateOutcome::Complete(ResultKind::Failed));
            }
            Ok(UpdateAttempt::UpdateFailed) => (),
            Err(err) => eprintln!(
                "{}",
                stderr_color(&format!("{}: {err}", host.target()), Color::Red)
            ),
        }
        println!(
            "{}",
            stdout_color(
                &format!("Update failed; {} will NOT be rebooted.", host.target()),
                Color::Red
            )
        );
        match menu(
            "Next step",
            &[
                "Retry update",
                "Open interactive SSH shell to repair, then retry",
                "Skip and flag",
                "Leave machine on and continue (flag update as failed)",
                "Shut down this host and continue",
            ],
        )? {
            1 => continue,
            2 => {
                let _ = ssh(
                    host,
                    &sized_interactive_command("exec ${SHELL:-/bin/sh} -l"),
                    true,
                    auth,
                )
                .status();
                continue;
            }
            3 => return Ok(UpdateOutcome::Complete(ResultKind::Failed)),
            4 => return Ok(UpdateOutcome::Complete(ResultKind::Failed)),
            5 => {
                if confirm(&format!(
                    "Shut down {} despite failed update",
                    host.target()
                ))? {
                    let outcome = shutdown(host, auth, sudo_auth);
                    eprintln!(
                        "{}",
                        stderr_color(
                            &format!("{}: shutdown request result: {outcome:?}", host.target()),
                            Color::Yellow
                        )
                    );
                }
                return Ok(UpdateOutcome::Complete(ResultKind::Failed));
            }
            _ => unreachable!(),
        }
    }
    Ok(UpdateOutcome::RebootRequested {
        before,
        requested: Instant::now(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
        io::{self, Read},
        process::Command,
        sync::mpsc,
    };
    struct OneByte<'a>(&'a [u8]);
    impl Read for OneByte<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            output[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    #[test]
    fn forwards_interactive_output_without_exposing_control_markers() {
        let data = [
            b"Password: ".as_slice(),
            NEED,
            b"\r\nreview? ",
            READY,
            b"done",
            UPDATED,
        ]
        .concat();
        let (tx, rx) = mpsc::channel();
        let mut output = Vec::new();
        forward_output(OneByte(&data), tx, &mut output).unwrap();
        assert_eq!(output, b"Password: \r\nreview? done");
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            [Marker::Need, Marker::Ready, Marker::Updated]
        );
        let (tx, rx) = mpsc::channel();
        let mut output = Vec::new();
        forward_output(
            OneByte(&[b"nothing to do\r\n", UNCHANGED].concat()),
            tx,
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"nothing to do\r\n");
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), [Marker::Unchanged]);
    }

    #[test]
    fn untrusted_update_output_cannot_retrigger_sudo_password() {
        let mut gate = SudoPasswordGate::default();
        assert!(gate.on_marker(Marker::Need).unwrap());
        assert!(!gate.on_marker(Marker::Ready).unwrap());
        assert!(!gate.on_marker(Marker::Need).unwrap()); // AUR output after sudo setup

        let mut gate = SudoPasswordGate::default();
        assert!(gate.on_marker(Marker::Need).unwrap());
        assert!(gate.on_marker(Marker::Need).is_err()); // Not another sudo prompt

        let mut passwordless = SudoPasswordGate::default();
        assert!(!passwordless.on_marker(Marker::Ready).unwrap());
        assert!(!passwordless.on_marker(Marker::Need).unwrap());
    }

    #[test]
    fn unchanged_packages_never_trigger_reboot() {
        use std::os::unix::fs::PermissionsExt;
        for changed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            for (name, script) in [
                (
                    "pacman",
                    "#!/bin/sh\ncase \"$1\" in\n-Q) if [ -f \"$FLEETTY_TEST_STATE\" ]; then echo 'pkg 2.0'; else echo 'pkg 1.0'; fi;;\n-Syu) if [ \"$FLEETTY_TEST_CHANGE\" = 1 ]; then touch \"$FLEETTY_TEST_STATE\"; fi;;\n*) exit 1;;\nesac\n",
                ),
                (
                    "sudo",
                    "#!/bin/sh\ncase \"$1\" in\n-n|-k) exit 0;;\nreboot) touch \"$FLEETTY_TEST_REBOOT\";;\n*) exec \"$@\";;\nesac\n",
                ),
            ] {
                let path = dir.path().join(name);
                fs::write(&path, script).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let result = Command::new("sh")
                .arg("-c")
                .arg(update_script(Aur::None))
                .env(
                    "PATH",
                    format!("{}:{}", dir.path().display(), env::var("PATH").unwrap()),
                )
                .env("FLEETTY_TEST_STATE", dir.path().join("state"))
                .env("FLEETTY_TEST_REBOOT", dir.path().join("reboot"))
                .env("FLEETTY_TEST_CHANGE", if changed { "1" } else { "0" })
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(
                result
                    .stdout
                    .windows(UNCHANGED.len())
                    .any(|m| m == UNCHANGED),
                !changed
            );
            assert_eq!(
                result.stdout.windows(UPDATED.len()).any(|m| m == UPDATED),
                changed
            );
            assert_eq!(dir.path().join("reboot").exists(), changed);
        }
    }

    #[test]
    fn aur_failure_blocks_reboot() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for (name, script) in [
            (
                "pacman",
                "#!/bin/sh\ncase \"$1\" in\n-Q) echo 'pkg 1.0';;\n-Syu) exit 0;;\nesac\n",
            ),
            (
                "sudo",
                "#!/bin/sh\ncase \"$1\" in\n-n|-k) exit 0;;\nreboot) touch \"$FLEETTY_TEST_REBOOT\";;\n*) exec \"$@\";;\nesac\n",
            ),
            (
                "paru",
                "#!/bin/sh\n[ \"$1\" = '-Sua' ] || exit 2\necho 'AUR update failed' >&2\nexit 1\n",
            ),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let output = Command::new("sh")
            .arg("-c")
            .arg(update_script(Aur::Paru))
            .env(
                "PATH",
                format!("{}:{}", dir.path().display(), env::var("PATH").unwrap()),
            )
            .env("FLEETTY_TEST_REBOOT", dir.path().join("reboot"))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("Starting AUR update: paru -Sua"));
        assert!(!dir.path().join("reboot").exists());
    }

    #[test]
    fn aur_stage_starts_without_fleetty_sudo_timestamp() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for (name, script) in [
            (
                "pacman",
                r#"#!/bin/sh
    [ "$1" = -Q ] && { echo 'pkg 1.0'; exit 0; }
    [ "$1" = -Syu ] && exit 0
    exit 1
    "#,
            ),
            (
                "sudo",
                r#"#!/bin/sh
    case "$1" in
      -n) [ ! -f "$FLEETTY_TEST_SUDO_INVALIDATED" ] ;;
      -k) touch "$FLEETTY_TEST_SUDO_INVALIDATED" ;;
      *) exec "$@" ;;
    esac
    "#,
            ),
            (
                "paru",
                r#"#!/bin/sh
    [ "$1" = -Sua ] || exit 2
    if sudo -n -v; then echo 'AUR inherited Fleetty sudo timestamp' >&2; exit 3; fi
    echo 'AUR ran without cached sudo'
    "#,
            ),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let result = Command::new("sh")
            .arg("-c")
            .arg(update_script(Aur::Paru))
            .env(
                "PATH",
                format!("{}:{}", dir.path().display(), env::var("PATH").unwrap()),
            )
            .env(
                "FLEETTY_TEST_SUDO_INVALIDATED",
                dir.path().join("invalidated"),
            )
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("AUR ran without cached sudo"));
        assert!(dir.path().join("invalidated").exists());
    }

    #[test]
    fn successful_aur_update_still_reboots_after_package_changes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for (name, script) in [
            (
                "pacman",
                r#"#!/bin/sh
    case "$1" in
      -Q) if [ -f "$FLEETTY_TEST_UPDATED" ]; then echo 'pkg 2.0'; else echo 'pkg 1.0'; fi ;;
      -Syu) exit 0 ;;
      *) exit 1 ;;
    esac
    "#,
            ),
            (
                "sudo",
                r#"#!/bin/sh
    case "$1" in
      -n|-k) exit 0 ;;
      reboot) touch "$FLEETTY_TEST_REBOOT" ;;
      *) exec "$@" ;;
    esac
    "#,
            ),
            (
                "paru",
                r#"#!/bin/sh
    [ "$1" = -Sua ] || exit 1
    touch "$FLEETTY_TEST_UPDATED"
    "#,
            ),
        ] {
            let path = dir.path().join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let result = Command::new("sh")
            .arg("-c")
            .arg(update_script(Aur::Paru))
            .env(
                "PATH",
                format!("{}:{}", dir.path().display(), env::var("PATH").unwrap()),
            )
            .env("FLEETTY_TEST_UPDATED", dir.path().join("updated"))
            .env("FLEETTY_TEST_REBOOT", dir.path().join("reboot"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            result
                .stdout
                .windows(UPDATED.len())
                .any(|bytes| bytes == UPDATED)
        );
        assert!(dir.path().join("reboot").exists());
    }
}
