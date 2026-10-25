use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

#[test]
fn bare_command_opens_menu_and_creates_private_inventory_for_editing() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fleetty");
    let mut child = Command::new(binary)
        .env("XDG_CONFIG_HOME", &config)
        .env("TERM", "xterm")
        .env("NO_COLOR", "1")
        .env("VISUAL", "true")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"2\n3\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let banner = format!(
        "\n╭─ ● fleetty  v{}\n╰─❯ your fleet, one terminal\n",
        env!("CARGO_PKG_VERSION")
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with(&banner));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Edit inventory"));
    let directory = config.join("fleetty");
    let inventory = directory.join("inventory.toml");
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&inventory).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(fs::read_to_string(inventory).unwrap().contains("# [pcs]"));
}

#[test]
fn linux_console_uses_ascii_banner() {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fleetty");
    let config = tempfile::tempdir().unwrap();
    let mut child = Command::new(binary)
        .env("XDG_CONFIG_HOME", config.path())
        .env("TERM", "linux")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"3\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with(&format!(
            "\n,- * fleetty  v{}\n`-> your fleet, one terminal\n",
            env!("CARGO_PKG_VERSION")
        ))
    );
}
