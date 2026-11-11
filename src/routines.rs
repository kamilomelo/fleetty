use crate::{
    aur::Aur,
    auth::Auth,
    inventory::{Host, Inventory, build_hosts},
    kartouche::{Steps, Style},
    results::{ResultKind, print_results, report_attention},
    ssh::{Credentials, boot_id, check_read_only, connect_account, prepare_sudo, shutdown, ssh},
    ui::{
        Color, ask, choose_action_accounts, choose_optional_accounts, confirm, menu, select_hosts,
        stderr_color, stdout_color,
    },
    update::{UpdateOutcome, update},
};
use std::{
    collections::HashSet,
    fs, io,
    io::Write,
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

const REBOOT_TIMEOUT: Duration = Duration::from_secs(300);

fn start_host_steps(kind: &str, hosts: &[&Host]) -> Steps {
    let labels: Vec<_> = hosts
        .iter()
        .map(|host| format!("{kind} {}", host.target()))
        .collect();
    let names: Vec<_> = labels.iter().map(String::as_str).collect();
    io::stdout().flush().expect("failed to flush host output");
    Steps::start(Style::Dots, &names)
}

fn finish_host_steps(steps: &mut Steps, results: &[(String, ResultKind)]) {
    io::stdout().flush().expect("failed to flush host output");
    if hosts_completed_successfully(results) {
        steps.done(None);
    } else {
        steps.fail(Some("review per-host results"));
    }
}

fn hosts_completed_successfully(results: &[(String, ResultKind)]) -> bool {
    results
        .iter()
        .all(|(_, result)| matches!(result, ResultKind::Done | ResultKind::Unchanged))
}

fn choose_personal_reboots(hosts: &[&Host]) -> Result<HashSet<String>, String> {
    let personal: Vec<&Host> = hosts.iter().copied().filter(|host| host.personal).collect();
    if personal.is_empty() {
        return Ok(HashSet::new());
    }
    println!("Personal machines selected for update:");
    for host in &personal {
        println!("  {}", host.target());
    }
    println!("If packages change, which personal machines may reboot?:");
    println!("  1. Do not reboot any personal machines (default/safest)");
    println!("  2. Reboot all personal machines with package changes");
    println!("  3. Choose personal machines individually");
    let answer = ask("Choice [1]: ")?;
    let choice = if answer.is_empty() {
        1
    } else {
        answer.parse::<usize>().map_err(|_| "invalid selection")?
    };
    personal_reboot_selection(&personal, choice, |host| {
        confirm(&format!(
            "Allow reboot of {} if packages change",
            host.target()
        ))
    })
}

fn personal_reboot_selection(
    personal: &[&Host],
    choice: usize,
    mut allow: impl FnMut(&Host) -> Result<bool, String>,
) -> Result<HashSet<String>, String> {
    Ok(match choice {
        1 => HashSet::new(),
        2 => personal.iter().map(|host| host.address.clone()).collect(),
        3 => {
            let mut selected = HashSet::new();
            for host in personal {
                if allow(host)? {
                    selected.insert(host.address.clone());
                }
            }
            selected
        }
        _ => return Err("invalid selection".into()),
    })
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
    let trust_current_keys = confirm(
        "Trust the first SSH host key seen for selected IPs this run (ignores saved keys; weaker identity protection)",
    )?;
    let mut credentials = Credentials::prompt(trust_current_keys)?;
    if routine == 1 {
        return check_then_optionally_update(&selected, &mut credentials);
    }
    let results = match routine {
        2 => run_updates(&selected, &mut credentials)?,
        3 => run_shutdown(&selected, &mut credentials)?,
        4 => run_commands(&selected, command.as_deref().unwrap(), &mut credentials),
        _ => unreachable!(),
    };
    print_results(&results);
    report_attention(&results)
}

fn run_shutdown(
    selected: &[&Host],
    credentials: &mut Credentials,
) -> Result<Vec<(String, ResultKind)>, String> {
    let mut results = Vec::new();
    // Preflight runs one host at a time; shutdown requests are dispatched together.
    let mut ready = Vec::new();
    let mut steps = start_host_steps("shutdown preflight", selected);
    for (i, host) in selected.iter().enumerate() {
        if i > 0 {
            io::stdout().flush().expect("failed to flush host output");
            steps.next();
        }
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
    finish_host_steps(&mut steps, &results);
    Ok(results)
}

fn run_commands(
    selected: &[&Host],
    command: &str,
    credentials: &mut Credentials,
) -> Vec<(String, ResultKind)> {
    let mut results = Vec::new();
    let mut steps = start_host_steps("command", selected);
    for (i, host) in selected.iter().enumerate() {
        if i > 0 {
            io::stdout().flush().expect("failed to flush host output");
            steps.next();
        }
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
    finish_host_steps(&mut steps, &results);
    results
}

fn check_then_optionally_update(
    selected: &[&Host],
    credentials: &mut Credentials,
) -> Result<(), String> {
    let mut checked = Vec::new();
    let mut ready = Vec::new();
    let mut steps = start_host_steps("check", selected);
    for (i, host) in selected.iter().enumerate() {
        if i > 0 {
            io::stdout().flush().expect("failed to flush host output");
            steps.next();
        }
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
    finish_host_steps(&mut steps, &checked);
    print_results(&checked);
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
    results.extend(run_updates(&ready, credentials)?);
    print_results(&results);
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
) -> Result<Vec<(String, ResultKind)>, String> {
    let personal_reboots = choose_personal_reboots(hosts)?;
    let mut results = Vec::new();
    let mut pending = Vec::new();
    let mut steps = start_host_steps("update", hosts);
    for (i, host) in hosts.iter().enumerate() {
        if i > 0 {
            io::stdout().flush().expect("failed to flush host output");
            steps.next();
        }
        let (label, outcome, auth) = match connect_account(host, credentials) {
            Ok((name, auth)) => {
                println!("Found {name} ({})", host.target());
                let outcome = match prepare_sudo(host, &auth, credentials) {
                    Ok(sudo_auth) => update(
                        host,
                        &auth,
                        &sudo_auth,
                        !host.personal || personal_reboots.contains(&host.address),
                    )?,
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
    finish_host_steps(&mut steps, &results);
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
    fn personal_reboot_choices_only_allow_explicitly_selected_ips() {
        let first = Host {
            address: "192.0.2.1".into(),
            user: "owner".into(),
            groups: vec!["laptops_".into()],
            priority: false,
            personal: true,
        };
        let second = Host {
            address: "192.0.2.2".into(),
            ..first.clone()
        };
        let hosts = [&first, &second];
        assert!(
            personal_reboot_selection(&hosts, 1, |_| panic!("must not prompt"))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            personal_reboot_selection(&hosts, 2, |_| panic!("must not prompt"))
                .unwrap()
                .len(),
            2
        );
        let selected =
            personal_reboot_selection(&hosts, 3, |host| Ok(host.address == "192.0.2.2")).unwrap();
        assert_eq!(selected, HashSet::from(["192.0.2.2".into()]));
        assert!(personal_reboot_selection(&hosts, 4, |_| panic!("must not prompt")).is_err());
    }

    #[test]
    fn steps_do_not_finish_green_for_unverified_or_failed_hosts() {
        let success = vec![
            ("done".into(), ResultKind::Done),
            ("unchanged".into(), ResultKind::Unchanged),
        ];
        assert!(hosts_completed_successfully(&success));
        for status in [
            ResultKind::RebootPending,
            ResultKind::Skipped,
            ResultKind::Unreachable,
            ResultKind::Requested,
            ResultKind::Failed,
            ResultKind::Unknown,
        ] {
            assert!(!hosts_completed_successfully(&[("host".into(), status)]));
        }
    }

    #[test]
    fn deferred_reboot_checks_even_after_timeout_and_requires_new_boot_id() {
        let host = Host {
            address: "192.0.2.1".into(),
            user: "admin".into(),
            groups: vec!["servers".into()],
            priority: false,
            personal: false,
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
