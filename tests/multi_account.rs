use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn run_with_inventory(inventory_text: &str, choices: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    let inventory = dir.path().join("inventory.toml");
    fs::write(&inventory, inventory_text).unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fleetty");
    let mut child = Command::new(binary)
        .arg(inventory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(choices.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_with_choices(choices: &str) -> String {
    let output = run_with_inventory(
        "[servers]\n\"192.0.2.1\" = [\"admin\", \"deploy\"]\n",
        choices,
    );
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn update_and_shutdown_choose_exactly_one_account() {
    for routine in [2, 3] {
        let output = run_with_choices(&format!("1\n{routine}\n2\nn\n"));
        assert!(output.contains("Choose ONE account"), "{output}");
        assert!(output.contains("Selected: deploy@192.0.2.1"), "{output}");
        assert!(!output.contains("Selected: admin@192.0.2.1"), "{output}");
    }
}

#[test]
fn checks_and_custom_commands_can_choose_one_or_all_accounts() {
    for routine in [1, 4] {
        let output = run_with_choices(&format!("1\n{routine}\n1\nn\n"));
        assert!(!output.contains("Choose ONE account"), "{output}");
        assert!(
            output.contains("Choose account(s) for 192.0.2.1"),
            "{output}"
        );
        assert!(output.contains("1. All accounts"), "{output}");
        assert!(
            output.contains("Selected: admin@192.0.2.1, deploy@192.0.2.1"),
            "{output}"
        );

        let output = run_with_choices(&format!("1\n{routine}\n3\nn\n"));
        assert!(output.contains("Selected: deploy@192.0.2.1"), "{output}");
        assert!(!output.contains("Selected: admin@192.0.2.1"), "{output}");
    }
}

#[test]
fn all_excludes_marked_machines_unless_explicitly_included() {
    let inventory = "[-optional]\n\"192.0.2.2\" = \"deploy\"\n\"192.0.2.3\" = \"admin\"\n[servers]\n\"192.0.2.1\" = \"admin\"\n\"192.0.2.2\" = \"admin\"\n";

    // Decline with the default answer; even the ordinary account on .2 is excluded.
    let output = run_with_inventory(inventory, "1\n\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Include excluded groups in all [y/N]"),
        "{text}"
    );
    assert!(text.contains("Selected: admin@192.0.2.1"), "{text}");
    assert!(!text.contains("Selected: admin@192.0.2.1,"), "{text}");
    assert!(
        text.contains("  2. group: servers\n  3. individual hosts\n  4. group: -optional"),
        "{text}"
    );

    // Opting in includes every account, even on an IP shared between groups.
    let output = run_with_inventory(inventory, "1\ny\n1\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains(
            "Selected: admin@192.0.2.1, deploy@192.0.2.2, admin@192.0.2.2, admin@192.0.2.3"
        ),
        "{text}"
    );

    // Explicit group and individual selections do not get the exclusion prompt.
    let output = run_with_inventory(inventory, "4\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Selected: deploy@192.0.2.2, admin@192.0.2.3"),
        "{text}"
    );
    assert!(
        !text.contains("Include excluded groups in all [y/N]"),
        "{text}"
    );

    let output = run_with_inventory(inventory, "3\n4\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Selected: admin@192.0.2.3"), "{text}");
    assert!(
        !text.contains("Include excluded groups in all [y/N]"),
        "{text}"
    );
}

#[test]
fn all_with_only_excluded_hosts_stops_before_contacting_anyone() {
    let output = run_with_inventory("[-optional]\n\"192.0.2.2\" = \"admin\"\n", "1\nn\n");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no hosts selected"));
}
