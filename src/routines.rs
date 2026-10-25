use crate::{
    aur::Aur,
    auth::Auth,
    inventory::{Host, Inventory, build_hosts},
    results::{ResultKind, print_results, report_attention},
    ssh::{Credentials, boot_id, check_read_only, connect_account, prepare_sudo, shutdown, ssh},
    ui::{
        Color, ask, choose_action_accounts, choose_optional_accounts, confirm, host_step, menu,
        select_hosts, stderr_color, stdout_color,
    },
    update::{UpdateOutcome, update},
};
use std::{
    env, fs, io,
    io::Write,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const REBOOT_TIMEOUT: Duration = Duration::from_secs(300);
const STEP_DELAY_ENV: &str = "FLEETTY_STEP_DELAY_SECONDS";

fn step_delay(value: Option<&str>) -> Result<Duration, String> {
    let Some(value) = value else {
        return Ok(Duration::ZERO);
    };
    let seconds = value
        .parse::<u8>()
        .map_err(|_| format!("{STEP_DELAY_ENV} must be a whole number from 0 to 60"))?;
    if seconds > 60 {
        return Err(format!(
            "{STEP_DELAY_ENV} must be a whole number from 0 to 60"
        ));
    }
    Ok(Duration::from_secs(seconds.into()))
}

fn show_host_step(kind: &str, index: usize, total: usize, target: &str, delay: Duration) {
    println!("\n{}\n", host_step(kind, index, total, target));
    if !delay.is_zero() {
        io::stdout().flush().expect("failed to flush host step");
        thread::sleep(delay);
    }
}

pub(crate) fn run_inventory(path: &Path) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|e| {
        format!(
            "{}: {e}. Choose Edit inventory from the menu to create it",
            path.display()
        )
    })?;
    let inventory: Inventory = toml::from_str(&text).map_err(|e| e.to_string())?;
    let hosts = build_hosts(&inventory)?;
    let selected = select_hosts(&hosts)?;
    let routine = menu(
        "Routine",
        &[
            "Check all selected hosts (read-only), then optionally update",
            "Update (interactive)",
            "Shutdown (parallel)",
            "Run command (sequential)",
        ],
    )?;
    let selected = match routine {
        2 | 3 => {
            choose_action_accounts(&selected, if routine == 2 { "update" } else { "shut down" })?
        }
        1 | 4 => choose_optional_accounts(&selected)?,
        _ => unreachable!(),
    };
    println!(
        "Selected: {}",
        selected
            .iter()
            .map(|h| h.target())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !confirm("Proceed")? {
        return Ok(());
    }
    let command = if routine == 4 {
        let input = ask("Remote command (runs as SSH user): ")?;
        if input.trim().is_empty() {
            return Err("empty command".into());
        }
        println!("Command: {input}");
        if !confirm("Run this command on the selected hosts")? {
            return Ok(());
        }
        Some(input)
    } else {
        None
    };
    if routine == 3 && !confirm("Send shutdown to ALL selected hosts? This is irreversible")? {
        return Ok(());
    }
    let delay = step_delay(env::var(STEP_DELAY_ENV).ok().as_deref())?;
    let mut credentials = Credentials::prompt()?;
    if routine == 1 {
        return check_then_optionally_update(&selected, &mut credentials, delay);
    }
    let results = match routine {
        2 => run_updates(&selected, &mut credentials, delay)?,
        3 => run_shutdown(&selected, &mut credentials, delay)?,
        4 => run_commands(
            &selected,
            command.as_deref().unwrap(),
            &mut credentials,
            delay,
        ),
        _ => unreachable!(),
    };
    print_results("Run", &results);
    report_attention(&results)
}

fn run_shutdown(
    selected: &[&Host],
    credentials: &mut Credentials,
    delay: Duration,
) -> Result<Vec<(String, ResultKind)>, String> {
    let mut results = Vec::new();
    // Preflight runs one host at a time; shutdown requests are dispatched together.
    let mut ready = Vec::new();
    for (i, host) in selected.iter().enumerate() {
        show_host_step(
            "shutdown preflight",
            i + 1,
            selected.len(),
            &host.target(),
            delay,
        );
        match connect_account(host, credentials) {
            Ok((name, auth)) => {
                println!("Found {name} ({})", host.target());
                match prepare_sudo(host, &auth, credentials) {
                    Ok(sudo_auth) => ready.push(((*host).clone(), name, auth, sudo_auth)),
                    Err(err) => {
                        eprintln!(
                            "{}",
                            stderr_color(&format!("{}: {err}", host.target()), Color::Red)
                        );
                        results.push((host.target(), ResultKind::Failed));
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    stderr_color(&format!("{}: {e}", host.target()), Color::Red)
                );
                results.push((host.target(), e.kind));
            }
        }
    }
    println!(
        "Sending shutdown requests to {} reachable host(s).",
        ready.len()
    );
    for batch in ready.chunks(8) {
        let handles: Vec<_> = batch
            .iter()
            .cloned()
            .map(|(host, name, auth, sudo_auth)| {
                thread::spawn(move || {
                    // -n must fail instead of prompting in a concurrent, headless session.
                    let status = shutdown(&host, &auth, &sudo_auth);
                    (format!("{name} ({})", host.target()), status)
                })
            })
            .collect();
        for handle in handles {
            let (name, status) = handle.join().map_err(|_| "shutdown worker panicked")?;
            let result = match status {
                Ok(s) if s.success() => ResultKind::Requested,
                // Connection loss or a stalled SSH reply could mean shutdown started OR failed.
                Ok(s) if s.code() == Some(255) => ResultKind::Unknown,
                Err(err) if err.kind() == io::ErrorKind::TimedOut => ResultKind::Unknown,
                _ => ResultKind::Failed,
            };
            results.push((name, result));
        }
    }
    Ok(results)
}

fn run_commands(
    selected: &[&Host],
    command: &str,
    credentials: &mut Credentials,
    delay: Duration,
) -> Vec<(String, ResultKind)> {
    let mut results = Vec::new();
    for (i, host) in selected.iter().enumerate() {
        show_host_step("command", i + 1, selected.len(), &host.target(), delay);
        let (label, result) = match connect_account(host, credentials) {
            Err(e) => {
                eprintln!(
                    "{}",
                    stderr_color(&format!("{}: {e}", host.target()), Color::Red)
                );
                (host.target(), e.kind)
            }
            Ok((name, auth)) => {
                println!("Found {name} ({})", host.target());
                let status = ssh(host, command, true, &auth).status();
                (
                    format!("{name} ({})", host.target()),
                    if status.is_ok_and(|s| s.success()) {
                        ResultKind::Done
                    } else {
                        ResultKind::Failed
                    },
                )
            }
        };
        results.push((label, result));
    }
    results
}

fn check_then_optionally_update(
    selected: &[&Host],
    credentials: &mut Credentials,
    delay: Duration,
) -> Result<(), String> {
    let mut checked = Vec::new();
    let mut ready = Vec::new();
    for (i, host) in selected.iter().enumerate() {
        show_host_step("check", i + 1, selected.len(), &host.target(), delay);
        match connect_account(host, credentials) {
            Ok((name, auth)) => {
                let label = format!("{name} ({})", host.target());
                match check_read_only(host, &auth) {
                    Ok(aur) => {
                        println!(
                            "{label}: {} SSH, clock, pacman lock, boot ID; AUR: {}",
                            stdout_color("OK", Color::Green),
                            stdout_color(
                                aur.label(),
                                if matches!(aur, Aur::None) {
                                    Color::Yellow
                                } else {
                                    Color::Green
                                }
                            )
                        );
                        ready.push(*host);
                        checked.push((label, ResultKind::Done));
                    }
                    Err(err) => {
                        eprintln!("{}", stderr_color(&format!("{label}: {err}"), Color::Red));
                        checked.push((label, ResultKind::Failed));
                    }
                }
            }
            Err(err) => {
                eprintln!(
                    "{}",
                    stderr_color(&format!("{}: {err}", host.target()), Color::Red)
                );
                checked.push((host.target(), err.kind));
            }
        }
    }
    print_results("Read-only check", &checked);
    if ready.is_empty() {
        println!(
            "{}",
            stdout_color(
                "No hosts passed the read-only check; no updates were started.",
                Color::Yellow
            )
        );
        return report_attention(&checked);
    }
    let ready = choose_action_accounts(&ready, "update")?;
    println!(
        "Update order: {}",
        ready
            .iter()
            .map(|h| h.target())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !confirm(&format!(
        "Update these {} checked host(s); reboot only if packages change, in this order",
        ready.len()
    ))? {
        return report_attention(&checked);
    }
    let mut results: Vec<_> = checked
        .into_iter()
        .filter(|(_, result)| !matches!(result, ResultKind::Done))
        .collect();
    results.extend(run_updates(&ready, credentials, delay)?);
    print_results("Combined run", &results);
    report_attention(&results)
}

struct PendingReboot {
    result_index: usize,
    host: Host,
    auth: Arc<Auth>,
    before: String,
    requested: Instant,
}

fn run_updates(
    hosts: &[&Host],
    credentials: &mut Credentials,
    delay: Duration,
) -> Result<Vec<(String, ResultKind)>, String> {
    let mut results = Vec::new();
    let mut pending = Vec::new();
    for (i, host) in hosts.iter().enumerate() {
        show_host_step("update", i + 1, hosts.len(), &host.target(), delay);
        let (label, outcome, auth) = match connect_account(host, credentials) {
            Ok((name, auth)) => {
                println!("Found {name} ({})", host.target());
                let outcome = match prepare_sudo(host, &auth, credentials) {
                    Ok(sudo_auth) => update(host, &auth, &sudo_auth)?,
                    Err(err) => {
                        eprintln!(
                            "{}",
                            stderr_color(&format!("{}: {err}", host.target()), Color::Red)
                        );
                        UpdateOutcome::Complete(ResultKind::Failed)
                    }
                };
                (format!("{name} ({})", host.target()), outcome, Some(auth))
            }
            Err(err) => {
                eprintln!(
                    "{}",
                    stderr_color(&format!("{}: {err}", host.target()), Color::Red)
                );
                (host.target(), UpdateOutcome::Complete(err.kind), None)
            }
        };
        match outcome {
            UpdateOutcome::Complete(result) => results.push((label, result)),
            UpdateOutcome::RebootRequested { before, requested } => {
                let auth = auth.expect("connected for update");
                if host.priority {
                    println!(
                        "Verifying priority reboot of {} before continuing...",
                        host.target()
                    );
                    let result = verify_reboot(host, &auth, &before, requested);
                    results.push((label, result));
                    if matches!(result, ResultKind::Failed) {
                        eprintln!(
                            "{}",
                            stderr_color(
                                "Priority reboot not confirmed; remaining updates will not start.",
                                Color::Red,
                            )
                        );
                        for remaining in &hosts[i + 1..] {
                            results.push((remaining.target(), ResultKind::Skipped));
                        }
                        break;
                    }
                } else {
                    println!(
                        "Reboot requested for {}; confirmation deferred.",
                        host.target()
                    );
                    pending.push(PendingReboot {
                        result_index: results.len(),
                        host: (*host).clone(),
                        auth,
                        before,
                        requested,
                    });
                    results.push((label, ResultKind::Unknown)); // Replaced after verification.
                }
            }
        }
    }
    if !pending.is_empty() {
        println!("\nVerifying {} reboot(s) in parallel...", pending.len());
    }
    for batch in pending.chunks(8) {
        let workers: Vec<_> = batch
            .iter()
            .map(|reboot| {
                let host = reboot.host.clone();
                let auth = Arc::clone(&reboot.auth);
                let before = reboot.before.clone();
                let requested = reboot.requested;
                let index = reboot.result_index;
                thread::spawn(move || (index, verify_reboot(&host, &auth, &before, requested)))
            })
            .collect();
        for worker in workers {
            let (index, result) = worker
                .join()
                .map_err(|_| "reboot verification worker panicked")?;
            results[index].1 = result;
        }
    }
    Ok(results)
}

fn verify_reboot(host: &Host, auth: &Auth, before: &str, requested: Instant) -> ResultKind {
    verify_reboot_with(host, before, requested, || boot_id(host, auth))
}

fn verify_reboot_with(
    host: &Host,
    before: &str,
    requested: Instant,
    mut read_boot_id: impl FnMut() -> Result<String, String>,
) -> ResultKind {
    loop {
        // Try immediately, even when other updates took longer than the timeout.
        if let Ok(after) = read_boot_id()
            && after != before
        {
            println!(
                "{}",
                stdout_color(
                    &format!("{} rebooted and SSH is available.", host.target()),
                    Color::Green
                )
            );
            return ResultKind::Done;
        }
        if requested.elapsed() >= REBOOT_TIMEOUT {
            eprintln!(
                "{}",
                stderr_color(
                    &format!(
                        "{}: reboot not confirmed within {} seconds",
                        host.target(),
                        REBOOT_TIMEOUT.as_secs()
                    ),
                    Color::Red,
                )
            );
            return ResultKind::Failed;
        }
        thread::sleep(
            Duration::from_secs(5).min(REBOOT_TIMEOUT.saturating_sub(requested.elapsed())),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_step_delay_is_configurable() {
        assert_eq!(step_delay(None).unwrap(), Duration::ZERO);
        assert_eq!(step_delay(Some("3")).unwrap(), Duration::from_secs(3));
        for invalid in ["", "-1", "1.5", "61", "999999"] {
            assert!(step_delay(Some(invalid)).is_err());
        }
    }

    #[test]
    fn deferred_reboot_checks_even_after_timeout_and_requires_new_boot_id() {
        let host = Host {
            address: "192.0.2.1".into(),
            user: "admin".into(),
            groups: vec!["servers".into()],
            priority: false,
        };
        let requested = Instant::now() - REBOOT_TIMEOUT - Duration::from_secs(1);
        assert!(matches!(
            verify_reboot_with(&host, "old", requested, || Ok("new".into())),
            ResultKind::Done
        ));
        assert!(matches!(
            verify_reboot_with(&host, "old", requested, || Ok("old".into())),
            ResultKind::Failed
        ));
    }
}
