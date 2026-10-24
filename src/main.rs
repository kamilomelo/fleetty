mod auth;

use auth::Auth;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::{self, IsTerminal, Read, Write},
    net::IpAddr,
    os::fd::AsRawFd,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CONNECT_TIMEOUT: &str = "6";
const CONNECTION_ATTEMPTS: usize = 3;
const REBOOT_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CLOCK_SKEW: u64 = 120;
const NEED: &[u8] = b"\x1eFLEETTY_NEED\x1f";
const READY: &[u8] = b"\x1eFLEETTY_READY\x1f";
const UPDATED: &[u8] = b"\x1eFLEETTY_UPDATED\x1f";
const UNCHANGED: &[u8] = b"\x1eFLEETTY_UNCHANGED\x1f";

#[derive(Clone, Copy)]
enum Color {
    Blue,
    Green,
    Yellow,
    Red,
}

impl Color {
    fn code(self) -> u8 {
        match self {
            Self::Blue => 34,
            Self::Green => 32,
            Self::Yellow => 33,
            Self::Red => 31,
        }
    }
}

fn colorize(text: &str, color: Color, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{}m{text}\x1b[0m", color.code())
    } else {
        text.to_owned()
    }
}

fn colors_enabled() -> bool {
    env::var_os("NO_COLOR").is_none() && env::var("TERM").ok().as_deref() != Some("dumb")
}

fn stdout_color(text: &str, color: Color) -> String {
    colorize(text, color, colors_enabled() && io::stdout().is_terminal())
}

fn stderr_color(text: &str, color: Color) -> String {
    colorize(text, color, colors_enabled() && io::stderr().is_terminal())
}

type Inventory = BTreeMap<String, BTreeMap<String, Usernames>>;

#[derive(Deserialize)]
#[serde(untagged)]
enum Usernames {
    One(String),
    Several(Vec<String>),
}

impl Usernames {
    fn iter(&self) -> std::slice::Iter<'_, String> {
        match self {
            Self::One(name) => std::slice::from_ref(name).iter(),
            Self::Several(names) => names.iter(),
        }
    }
}

#[derive(Clone)]
struct Host {
    address: String,
    user: String,
    groups: Vec<String>,
    priority: bool,
}

impl Host {
    fn target(&self) -> String {
        format!("{}@{}", self.user, self.address)
    }
}

#[derive(Clone, Copy)]
enum Aur {
    Paru,
    Yay,
    None,
}

impl Aur {
    fn label(self) -> &'static str {
        match self {
            Self::Paru => "paru",
            Self::Yay => "yay",
            Self::None => "none",
        }
    }

    fn command(self) -> &'static str {
        match self {
            Aur::Paru => "paru -Sua",
            Aur::Yay => "yay -Sua",
            Aur::None => "true",
        }
    }

    fn stage(self) -> &'static str {
        match self {
            Self::Paru => "echo '[fleetty] Starting AUR update: paru -Sua'; paru -Sua",
            Self::Yay => "echo '[fleetty] Starting AUR update: yay -Sua'; yay -Sua",
            Self::None => "echo '[fleetty] AUR NOT checked: no paru or yay detected'; true",
        }
    }
}

#[derive(Clone, Copy)]
enum ResultKind {
    Done,
    Unchanged,
    Failed,
    Skipped,
    Unreachable,
    Requested,
    Unknown,
}

struct ConnectError {
    kind: ResultKind,
    message: String,
    authentication_failed: bool,
}

struct Credentials {
    shared: Arc<Auth>,
    alternate: HashMap<String, Arc<Auth>>,
    sudo_alternate: HashMap<String, Arc<Auth>>,
}

impl Credentials {
    fn prompt() -> Result<Self, String> {
        Ok(Self {
            shared: Arc::new(Auth::prompt()?),
            alternate: HashMap::new(),
            sudo_alternate: HashMap::new(),
        })
    }

    fn for_host(&self, host: &Host) -> Arc<Auth> {
        self.alternate
            .get(&host.target())
            .cloned()
            .unwrap_or_else(|| Arc::clone(&self.shared))
    }

    fn for_sudo(&self, host: &Host, ssh_auth: &Arc<Auth>) -> Arc<Auth> {
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

impl ResultKind {
    fn label(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Unchanged => "up to date (no reboot)",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::Unreachable => "unreachable",
            Self::Requested => "request sent (not verified)",
            Self::Unknown => "shutdown outcome unknown",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Done | Self::Unchanged => Color::Green,
            Self::Skipped | Self::Requested => Color::Yellow,
            Self::Failed | Self::Unreachable | Self::Unknown => Color::Red,
        }
    }
}

fn main() {
    if env::args_os().next().is_some_and(|arg| {
        std::path::Path::new(&arg).file_name() == Some(std::ffi::OsStr::new("ssh-askpass"))
    }) {
        if let Err(err) = auth::askpass() {
            eprintln!("Fleetty SSH authentication: {err}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(err) = run() {
        eprintln!("{}", stderr_color(&format!("Error: {err}"), Color::Red));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    let explicit = args.next();
    if args.next().is_some() {
        return Err("usage: fleetty [inventory.toml]".into());
    }
    if let Some(path) = explicit {
        return run_inventory(Path::new(&path));
    }
    let path = default_inventory_path()?;
    loop {
        println!("\nInventory: {}", path.display());
        match menu("Fleetty", &["Run routines", "Edit inventory", "Exit"])? {
            1 => return run_inventory(&path),
            2 => edit_inventory(&path)?,
            3 => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn default_inventory_path() -> Result<PathBuf, String> {
    let base = if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        PathBuf::from(dir)
    } else {
        PathBuf::from(env::var_os("HOME").ok_or("HOME is unset; pass an inventory path")?)
            .join(".config")
    };
    Ok(base.join("fleetty").join("inventory.toml"))
}

fn edit_inventory(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("inventory has no parent directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    if !path.exists() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.write_all(b"# [pcs]\n# \"192.0.2.10\" = \"username\"\n")
            .map_err(|e| e.to_string())?;
    }
    let preferred = env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()));
    let status = if let Some(editor) = preferred {
        let mut words = editor.split_whitespace();
        let program = words.next().ok_or("empty editor command")?;
        Command::new(program)
            .args(words)
            .arg(path)
            .status()
            .map_err(|e| format!("editor {program}: {e}"))?
    } else {
        Command::new("nano")
            .arg(path)
            .status()
            .or_else(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Command::new("vi").arg(path).status()
                } else {
                    Err(e)
                }
            })
            .map_err(|e| format!("no terminal editor available; set VISUAL or EDITOR: {e}"))?
    };
    if !status.success() {
        return Err(format!("editor exited with {status}"));
    }
    Ok(())
}

fn run_inventory(path: &Path) -> Result<(), String> {
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
    let selected = if routine == 2 || routine == 3 {
        choose_action_accounts(&selected, if routine == 2 { "update" } else { "shut down" })?
    } else {
        selected
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
    let mut credentials = Credentials::prompt()?;
    if routine == 1 {
        let mut checked = Vec::new();
        let mut ready = Vec::new();
        for (i, host) in selected.iter().enumerate() {
            println!(
                "\n{} {}",
                stdout_color(
                    &format!("[check {}/{}]", i + 1, selected.len()),
                    Color::Blue
                ),
                host.target()
            );
            match connect_account(host, &mut credentials) {
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
        results.extend(run_updates(&ready, &mut credentials)?);
        print_results("Combined run", &results);
        return report_attention(&results);
    }
    let mut results = Vec::new();
    if routine == 3 {
        // Preflight runs one host at a time; shutdown requests are dispatched together.
        let mut ready = Vec::new();
        for (i, host) in selected.iter().enumerate() {
            println!(
                "\n{} Checking {}",
                stdout_color(&format!("[{}/{}]", i + 1, selected.len()), Color::Blue),
                host.target()
            );
            match connect_account(host, &mut credentials) {
                Ok((name, auth)) => {
                    println!("Found {name} ({})", host.target());
                    match prepare_sudo(host, &auth, &mut credentials) {
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
                    // Connection loss could mean shutdown started OR SSH failed.
                    Ok(s) if s.code() == Some(255) => ResultKind::Unknown,
                    _ => ResultKind::Failed,
                };
                results.push((name, result));
            }
        }
    } else if routine == 2 {
        results = run_updates(&selected, &mut credentials)?;
    } else {
        for (i, host) in selected.iter().enumerate() {
            println!(
                "\n{} {}",
                stdout_color(&format!("[{}/{}]", i + 1, selected.len()), Color::Blue),
                host.target()
            );
            let (label, result) = match connect_account(host, &mut credentials) {
                Err(e) => {
                    eprintln!(
                        "{}",
                        stderr_color(&format!("{}: {e}", host.target()), Color::Red)
                    );
                    (host.target(), e.kind)
                }
                Ok((name, auth)) => {
                    println!("Found {name} ({})", host.target());
                    let status = ssh(host, command.as_deref().unwrap(), true, &auth).status();
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
    }
    print_results("Run", &results);
    report_attention(&results)
}

fn print_results(title: &str, results: &[(String, ResultKind)]) {
    println!("\n{title} results:");
    for (name, result) in results {
        println!("  {name}: {}", stdout_color(result.label(), result.color()));
    }
}

fn unreachable_warning(count: usize) -> String {
    let host = if count == 1 { "host" } else { "hosts" };
    format!(
        "Warning: {count} unreachable {host}. If intentionally shut down, this is expected.\nOtherwise, check power and SSH/network connectivity."
    )
}

fn report_attention(results: &[(String, ResultKind)]) -> Result<(), String> {
    let unreachable = results
        .iter()
        .filter(|(_, result)| matches!(result, ResultKind::Unreachable))
        .count();
    let skipped = results
        .iter()
        .filter(|(_, result)| matches!(result, ResultKind::Skipped))
        .count();
    let failed = results
        .iter()
        .filter(|(_, result)| matches!(result, ResultKind::Failed | ResultKind::Unknown))
        .count();
    if unreachable > 0 {
        eprintln!(
            "{}",
            stderr_color(&unreachable_warning(unreachable), Color::Yellow)
        );
    }
    if skipped > 0 {
        let host = if skipped == 1 { "host" } else { "hosts" };
        eprintln!(
            "{}",
            stderr_color(
                &format!("Warning: {skipped} skipped {host}."),
                Color::Yellow
            )
        );
    }
    if failed > 0 {
        return Err(format!(
            "{failed} host(s) failed or have an unknown outcome"
        ));
    }
    Ok(())
}

fn run_updates(
    hosts: &[&Host],
    credentials: &mut Credentials,
) -> Result<Vec<(String, ResultKind)>, String> {
    let mut results = Vec::new();
    let mut pending = Vec::new();
    for (i, host) in hosts.iter().enumerate() {
        println!(
            "\n{} {}",
            stdout_color(&format!("[update {}/{}]", i + 1, hosts.len()), Color::Blue),
            host.target()
        );
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

fn build_hosts(inventory: &Inventory) -> Result<Vec<Host>, String> {
    let mut hosts: Vec<Host> = Vec::new();
    let mut by_target: HashMap<(IpAddr, String), usize> = HashMap::new();
    for (group, entries) in inventory {
        if group.trim().is_empty() || group == "_" || entries.is_empty() {
            return Err(format!("empty group name or group with no hosts: {group}"));
        }
        for (address, usernames) in entries {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| format!("{address}: expected a valid IPv4 or IPv6 address"))?;
            if usernames.iter().len() == 0 {
                return Err(format!("{address}: specify at least one SSH username"));
            }
            let mut seen = HashSet::new();
            for user in usernames.iter() {
                if user.is_empty()
                    || user.starts_with('-')
                    || !user
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return Err(format!("{address}: invalid SSH username: {user}"));
                }
                if !seen.insert(user) {
                    return Err(format!("{address}: duplicate SSH username: {user}"));
                }
                if let Some(&index) = by_target.get(&(ip, user.clone())) {
                    hosts[index].groups.push(group.clone());
                } else {
                    by_target.insert((ip, user.clone()), hosts.len());
                    hosts.push(Host {
                        address: ip.to_string(),
                        user: user.clone(),
                        groups: vec![group.clone()],
                        priority: false,
                    });
                }
            }
        }
    }
    if hosts.is_empty() {
        return Err("inventory has no hosts".into());
    }
    // Priority applies to the machine, even when another account on the same
    // IP is selected through an ordinary group.
    let priority_ips: HashSet<String> = hosts
        .iter()
        .filter(|host| host.groups.iter().any(|group| group.starts_with('_')))
        .map(|host| host.address.clone())
        .collect();
    for host in &mut hosts {
        host.priority = priority_ips.contains(&host.address);
    }
    // Priority first, then numeric IP; accounts on the same IP retain their
    // inventory array order.
    hosts.sort_by_cached_key(|host| {
        (
            !host.priority,
            host.address
                .parse::<IpAddr>()
                .expect("validated IP address"),
        )
    });
    Ok(hosts)
}

fn select_hosts(hosts: &[Host]) -> Result<Vec<&Host>, String> {
    let mut groups: Vec<&str> = hosts
        .iter()
        .flat_map(|h| h.groups.iter().map(String::as_str))
        .collect();
    groups.sort_unstable();
    groups.dedup();
    println!("Select hosts:");
    println!("  1. all");
    for (i, group) in groups.iter().enumerate() {
        println!("  {}. group: {group}", i + 2);
    }
    println!("  {}. individual hosts", groups.len() + 2);
    let answer = ask("Choice: ")?;
    let choice: usize = answer.trim().parse().map_err(|_| "invalid selection")?;
    let selected = if choice == 1 {
        hosts.iter().collect()
    } else if choice == groups.len() + 2 {
        for (i, host) in hosts.iter().enumerate() {
            println!("  {}. {}", i + 1, host.target());
        }
        let input = ask("Host numbers (comma-separated): ")?;
        let mut indices = HashSet::new();
        for part in input.split(',') {
            let n: usize = part.trim().parse().map_err(|_| "invalid host number")?;
            if n == 0 || n > hosts.len() {
                return Err("host number out of range".into());
            }
            indices.insert(n - 1);
        }
        hosts
            .iter()
            .enumerate()
            .filter(|(i, _)| indices.contains(i))
            .map(|(_, h)| h)
            .collect()
    } else if (2..groups.len() + 2).contains(&choice) {
        hosts
            .iter()
            .filter(|h| h.groups.iter().any(|g| g == groups[choice - 2]))
            .collect()
    } else {
        return Err("invalid selection".into());
    };
    Ok(selected)
}

fn choose_action_accounts<'a>(hosts: &[&'a Host], action: &str) -> Result<Vec<&'a Host>, String> {
    choose_accounts_with(hosts, |address, accounts| {
        let labels: Vec<_> = accounts.iter().map(|host| host.user.as_str()).collect();
        Ok(menu(
            &format!("Choose ONE account to {action} {address}"),
            &labels,
        )? - 1)
    })
}

fn choose_accounts_with<'a>(
    hosts: &[&'a Host],
    mut choose: impl FnMut(&str, &[&Host]) -> Result<usize, String>,
) -> Result<Vec<&'a Host>, String> {
    let mut by_ip: HashMap<&str, usize> = HashMap::new();
    let mut machines: Vec<Vec<&Host>> = Vec::new();
    for host in hosts {
        if let Some(&index) = by_ip.get(host.address.as_str()) {
            machines[index].push(host);
        } else {
            by_ip.insert(&host.address, machines.len());
            machines.push(vec![host]);
        }
    }
    let mut chosen = Vec::new();
    for accounts in machines {
        if accounts.len() == 1 {
            chosen.push(accounts[0]);
            continue;
        }
        let choice = choose(&accounts[0].address, &accounts)?;
        chosen.push(*accounts.get(choice).ok_or("invalid account selection")?);
    }
    chosen.sort_by_cached_key(|host| {
        (
            !host.priority,
            host.address
                .parse::<IpAddr>()
                .expect("validated IP address"),
        )
    });
    Ok(chosen)
}

fn menu(title: &str, options: &[&str]) -> Result<usize, String> {
    println!("{title}:");
    for (i, option) in options.iter().enumerate() {
        println!("  {}. {option}", i + 1);
    }
    let input = ask("Choice: ")?;
    let n: usize = input.trim().parse().map_err(|_| "invalid selection")?;
    if n == 0 || n > options.len() {
        return Err("invalid selection".into());
    }
    Ok(n)
}

fn ask(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut input = String::new();
    if io::stdin()
        .read_line(&mut input)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Err("input closed".into());
    }
    Ok(input.trim().to_string())
}

fn confirm(prompt: &str) -> Result<bool, String> {
    Ok(ask(&format!("{prompt} [y/N]: "))?.eq_ignore_ascii_case("y"))
}

fn ssh(host: &Host, remote: &str, interactive: bool, auth: &Auth) -> Command {
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

fn shutdown(host: &Host, auth: &Auth, sudo_auth: &Auth) -> io::Result<ExitStatus> {
    let mut child = ssh(host, "sudo -S -p '' shutdown -h now", false, auth)
        .stdin(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().expect("piped SSH input");
    input.write_all(sudo_auth.password())?;
    input.write_all(b"\n")?;
    drop(input);
    child.wait()
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
        let output = ssh(host, "cat /proc/sys/kernel/hostname", false, auth)
            .output()
            .map_err(|e| ConnectError {
                kind: ResultKind::Failed,
                message: format!("could not start SSH: {e}"),
                authentication_failed: false,
            })?;
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

fn connect_account(
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
    let passwordless = ssh(host, "LC_ALL=C sudo -k -n -v", false, ssh_auth).output();
    match passwordless {
        Ok(output) if output.status.success() => return SudoCheck::Accepted,
        Ok(output) if output.status.code() == Some(255) => {
            return SudoCheck::Failed("SSH failed during sudo check".into());
        }
        Err(err) => return SudoCheck::Failed(format!("could not start sudo check: {err}")),
        _ => {}
    }
    let mut child = match ssh(host, "LC_ALL=C sudo -k -S -p '' -v", false, ssh_auth)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => return SudoCheck::Failed(format!("could not start sudo check: {err}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        // If sudo exits early, wait for its diagnostic instead of losing it to BrokenPipe.
        let _ = stdin.write_all(sudo_auth.password());
        let _ = stdin.write_all(b"\n");
    }
    match child.wait_with_output() {
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

fn prepare_sudo(
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

fn boot_id(host: &Host, auth: &Auth) -> Result<String, String> {
    let output = ssh(host, "cat /proc/sys/kernel/random/boot_id", false, auth)
        .output()
        .map_err(|e| e.to_string())?;
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || id.is_empty() {
        return Err("could not read boot ID".into());
    }
    Ok(id)
}

fn detect_aur(host: &Host, auth: &Auth) -> Result<Aur, String> {
    let output = ssh(host, "if command -v paru >/dev/null 2>&1; then echo paru; elif command -v yay >/dev/null 2>&1; then echo yay; else echo none; fi", false, auth)
        .output()
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

fn preflight(host: &Host, auth: &Auth) -> Result<(), String> {
    let lock = ssh(host, "test ! -e /var/lib/pacman/db.lck", false, auth)
        .status()
        .map_err(|e| format!("pacman lock check: {e}"))?;
    if !lock.success() {
        return Err("pacman database is locked or the lock check failed".into());
    }
    // `timedatectl` can report "no" for hosts using a different time service.
    // Compare the actual remote clock against this computer instead, accounting
    // for the time spent establishing the SSH connection.
    let before = unix_time()?;
    let clock = ssh(host, "date +%s", false, auth)
        .output()
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

fn check_read_only(host: &Host, auth: &Auth) -> Result<Aur, String> {
    preflight(host, auth)?;
    boot_id(host, auth)?;
    detect_aur(host, auth)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    Need,
    Ready,
    Updated,
    Unchanged,
}

enum UpdateAttempt {
    UpdateFailed,
    RebootFailed,
    RebootRequested,
    NoChanges,
}

enum UpdateOutcome {
    Complete(ResultKind),
    RebootRequested { before: String, requested: Instant },
}

struct PendingReboot {
    result_index: usize,
    host: Host,
    auth: Arc<Auth>,
    before: String,
    requested: Instant,
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
         (while sleep 30; do sudo -n -v >/dev/null 2>&1; done) </dev/null >/dev/null 2>&1 & keepalive=$!; \
          trap 'kill \"$keepalive\" 2>/dev/null' EXIT; \
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
    // The ready/need markers let us send the password only if sudo requests it.
    // Everything after that stays in this SSH terminal, including AUR review and reboot.
    let remote = update_script(aur);
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
    let mut ready = false;
    let mut updated = false;
    let mut unchanged = false;
    let mut input_closed = false;
    let outcome: Result<ExitStatus, String> = (|| -> Result<ExitStatus, String> {
        loop {
            for marker in rx.try_iter() {
                match marker {
                    Marker::Need => {
                        input
                            .write_all(sudo_auth.password())
                            .map_err(|e| e.to_string())?;
                        input.write_all(b"\n").map_err(|e| e.to_string())?;
                        input.flush().map_err(|e| e.to_string())?;
                    }
                    Marker::Ready => ready = true,
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
            if ready
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

fn update(host: &Host, auth: &Auth, sudo_auth: &Auth) -> Result<UpdateOutcome, String> {
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
                let _ = ssh(host, "exec ${SHELL:-/bin/sh} -l", true, auth).status();
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
                "Leave machine on and continue",
                "Shut down this host and continue",
            ],
        )? {
            1 => continue,
            2 => {
                let _ = ssh(host, "exec ${SHELL:-/bin/sh} -l", true, auth).status();
                continue;
            }
            3 => return Ok(UpdateOutcome::Complete(ResultKind::Failed)),
            4 => return Ok(UpdateOutcome::Complete(ResultKind::Skipped)),
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

    #[test]
    fn progress_colors_are_optional() {
        assert_eq!(
            colorize("[update 5/5]", Color::Blue, true),
            "\x1b[34m[update 5/5]\x1b[0m"
        );
        assert_eq!(colorize("failed", Color::Red, false), "failed");
    }

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
    fn rejects_invalid_ip_and_duplicate_users() {
        let invalid: Inventory = toml::from_str("[pcs]\n\"not-an-ip\" = \"kamilo\"").unwrap();
        assert!(build_hosts(&invalid).is_err());
        let duplicate: Inventory =
            toml::from_str("[servers]\n\"192.0.2.1\" = [\"kamilo\", \"kamilo\"]").unwrap();
        assert!(build_hosts(&duplicate).is_err());
        let empty: Inventory = toml::from_str("[servers]\n\"192.0.2.1\" = []").unwrap();
        assert!(build_hosts(&empty).is_err());
    }

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
    fn example_inventory_parses_and_deduplicates() {
        let inventory: Inventory =
            toml::from_str(include_str!("../inventory.example.toml")).unwrap();
        assert_eq!(build_hosts(&inventory).unwrap().len(), 3);
        let overlap: Inventory = toml::from_str(
            "[pcs]\n\"192.0.2.1\" = \"kamilo\"\n[servers]\n\"192.0.2.1\" = \"kamilo\"",
        )
        .unwrap();
        let hosts = build_hosts(&overlap).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].groups, ["pcs", "servers"]);
        let multi: Inventory = toml::from_str(
            "[pcs]\n\"192.0.2.1\" = [\"kamilo\", \"deploy\"]\n[servers]\n\"192.0.2.1\" = \"deploy\"",
        ).unwrap();
        let hosts = build_hosts(&multi).unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(
            hosts
                .iter()
                .find(|host| host.user == "deploy")
                .unwrap()
                .groups,
            ["pcs", "servers"]
        );
    }

    #[test]
    fn checks_and_commands_keep_both_accounts_but_actions_pick_one_per_ip() {
        let inventory: Inventory = toml::from_str(
            "[_mgt]\n\"192.0.2.2\" = \"admin\"\n[pcs]\n\"192.0.2.1\" = [\"admin\", \"deploy\"]\n\"192.0.2.2\" = \"deploy\"",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        let selected: Vec<_> = hosts.iter().collect();
        assert_eq!(selected.len(), 4); // check / command: both accounts on both machines
        let mut prompts = Vec::new();
        let action = choose_accounts_with(&selected, |ip, accounts| {
            prompts.push((ip.to_string(), accounts.len()));
            Ok(accounts
                .iter()
                .position(|host| host.user == "deploy")
                .unwrap())
        })
        .unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts.iter().all(|(_, count)| *count == 2));
        assert_eq!(action.len(), 2);
        assert_eq!(action[0].target(), "deploy@192.0.2.2"); // priority applies across accounts
        assert_eq!(action[1].target(), "deploy@192.0.2.1");
        assert!(choose_accounts_with(&selected, |_, _| Ok(2)).is_err());
    }

    #[test]
    fn spaces_around_toml_equals_are_valid() {
        let inventory: Inventory = toml::from_str(
            "[servers]\n\"10.1.20.12\"   = \"itadmin\"\n\"10.1.20.3\"    = \"itadmin\"",
        )
        .unwrap();
        assert_eq!(build_hosts(&inventory).unwrap().len(), 2);
    }

    #[test]
    fn unreachable_hosts_are_warnings_but_real_failures_are_errors() {
        assert!(unreachable_warning(1).contains("1 unreachable host."));
        assert!(unreachable_warning(2).contains("2 unreachable hosts."));
        assert!(unreachable_warning(1).contains("If intentionally shut down"));
        assert!(
            unreachable_warning(1)
                .contains("\nOtherwise, check power and SSH/network connectivity.")
        );
        let offline = vec![("powered off".into(), ResultKind::Unreachable)];
        assert!(report_attention(&offline).is_ok());
        let failed = vec![("update failed".into(), ResultKind::Failed)];
        assert!(report_attention(&failed).is_err());
        let unknown = vec![("shutdown outcome unknown".into(), ResultKind::Unknown)];
        assert!(report_attention(&unknown).is_err());
    }

    #[test]
    fn only_marked_groups_are_priority() {
        let inventory: Inventory = toml::from_str(
            "[mgt]\n\"192.0.2.1\" = \"admin\"\n[servers]\n\"192.0.2.2\" = \"admin\"\n[_urgent]\n\"192.0.2.200\" = \"admin\"",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(
            hosts.iter().map(|h| h.address.as_str()).collect::<Vec<_>>(),
            ["192.0.2.200", "192.0.2.1", "192.0.2.2"]
        );
        assert!(hosts[0].priority);
        assert!(!hosts[1].priority);
    }

    #[test]
    fn addresses_run_in_numeric_order_with_priority_first() {
        let inventory: Inventory = toml::from_str(
            "[_mgt]\n\"10.1.0.4\" = \"admin\"\n[servers]\n\"10.1.20.12\" = [\"admin\", \"deploy\"]\n\"10.1.20.2\" = \"admin\"\n\"10.1.12.2\" = \"admin\"\n\"10.1.0.10\" = \"admin\"\n",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(
            hosts.iter().map(Host::target).collect::<Vec<_>>(),
            [
                "admin@10.1.0.4",
                "admin@10.1.0.10",
                "admin@10.1.12.2",
                "admin@10.1.20.2",
                "admin@10.1.20.12",
                "deploy@10.1.20.12",
            ]
        );
        let selected: Vec<_> = hosts.iter().collect();
        let updates = choose_accounts_with(&selected, |_, accounts| {
            Ok(accounts
                .iter()
                .position(|host| host.user == "deploy")
                .unwrap())
        })
        .unwrap();
        assert_eq!(
            updates.iter().map(|host| host.target()).collect::<Vec<_>>(),
            [
                "admin@10.1.0.4",
                "admin@10.1.0.10",
                "admin@10.1.12.2",
                "admin@10.1.20.2",
                "deploy@10.1.20.12",
            ]
        );
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
    fn clock_check_compares_actual_time_not_sync_service() {
        assert!(clock_within_range(1_000, 1_002, 1_001));
        assert!(clock_within_range(1_000, 1_002, 880));
        assert!(!clock_within_range(1_000, 1_002, 879));
        assert!(!clock_within_range(1_000, 1_002, 1_123));
    }

    #[test]
    fn unchanged_packages_never_trigger_reboot() {
        use std::os::unix::fs::PermissionsExt;
        for changed in [false, true] {
            let dir = tempfile::tempdir_in("/tmp/opencode").unwrap();
            for (name, script) in [
                (
                    "pacman",
                    "#!/bin/sh\ncase \"$1\" in\n-Q) if [ -f \"$FLEETTY_TEST_STATE\" ]; then echo 'pkg 2.0'; else echo 'pkg 1.0'; fi;;\n-Syu) if [ \"$FLEETTY_TEST_CHANGE\" = 1 ]; then touch \"$FLEETTY_TEST_STATE\"; fi;;\n*) exit 1;;\nesac\n",
                ),
                (
                    "sudo",
                    "#!/bin/sh\ncase \"$1\" in\n-n) exit 0;;\nreboot) touch \"$FLEETTY_TEST_REBOOT\";;\n*) exec \"$@\";;\nesac\n",
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
        let dir = tempfile::tempdir_in("/tmp/opencode").unwrap();
        for (name, script) in [
            (
                "pacman",
                "#!/bin/sh\ncase \"$1\" in\n-Q) echo 'pkg 1.0';;\n-Syu) exit 0;;\nesac\n",
            ),
            (
                "sudo",
                "#!/bin/sh\ncase \"$1\" in\n-n) exit 0;;\nreboot) touch \"$FLEETTY_TEST_REBOOT\";;\n*) exec \"$@\";;\nesac\n",
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
}
