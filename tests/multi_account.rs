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
        .env("TERM", "xterm")
        .env("NO_COLOR", "1")
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
fn update_reboot_and_shutdown_choose_exactly_one_account() {
    for routine in [2, 3, 4] {
        let output = run_with_choices(&format!("1\n{routine}\n2\nn\n"));
        assert!(output.contains("├─ 2. Update (interactive)\n├─ 3. Reboot (parallel, verify)\n├─ 4. Shutdown (parallel)\n├─ 5. Run command (sequential)"), "{output}");
        assert!(output.contains("Choose ONE account"), "{output}");
        assert!(output.contains("Selected: deploy@192.0.2.1"), "{output}");
        assert!(!output.contains("Selected: admin@192.0.2.1"), "{output}");
    }
}

#[test]
fn multiple_ips_are_one_machine_with_one_chosen_update_account() {
    let inventory = "[servers]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.2\", \"192.0.2.3\"], users = [\"admin\", \"user\"] }\n";
    let output = run_with_inventory(inventory, "1\n2\n2\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Choose ONE account to update 192.0.2.1"));
    assert!(text.contains("Selected: user@192.0.2.1"));
    assert!(!text.contains("Selected: admin@192.0.2.1,"));
    assert!(!text.contains("Selected: user@192.0.2.2"));
}

#[test]
fn declining_reboot_confirmation_stops_before_any_ssh() {
    let output = run_with_inventory("[servers]\n\"192.0.2.1\" = \"admin\"\n", "1\n3\ny\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Reboot ALL selected hosts now (interrupts running work) [y/N]"));
    assert!(!text.contains("Trust the first SSH host key"));
}

#[test]
fn checks_and_custom_commands_can_choose_one_or_all_accounts() {
    for routine in [1, 5] {
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
        text.contains("╭─ ● Include excluded groups in all\n╰─❯ [y/N]:"),
        "{text}"
    );
    assert!(text.contains("Selected: admin@192.0.2.1"), "{text}");
    assert!(!text.contains("Selected: admin@192.0.2.1,"), "{text}");
    assert!(
        text.contains("├─ 2. group: servers\n├─ 3. group: -optional\n├─ 4. individual hosts"),
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
    let output = run_with_inventory(inventory, "3\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Selected: deploy@192.0.2.2, admin@192.0.2.3"),
        "{text}"
    );
    assert!(!text.contains("Include excluded groups in all"), "{text}");

    let output = run_with_inventory(inventory, "4\n4\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Selected: admin@192.0.2.3"), "{text}");
    assert!(!text.contains("Include excluded groups in all"), "{text}");
}

#[test]
fn all_stays_first_and_individual_stays_last_as_groups_are_added() {
    let inventory = "[_mgt]\n\"192.0.2.1\" = \"admin\"\n[servers]\n\"192.0.2.2\" = \"admin\"\n[vps]\n\"192.0.2.3\" = \"admin\"\n[-pc_]\n\"192.0.2.4\" = \"admin\"\n[-vpn]\n\"192.0.2.5\" = \"admin\"";
    let output = run_with_inventory(inventory, "6\n1\nn\n");
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("╭─ ● Select hosts\n├─ 1. all\n├─ 2. group: _mgt\n├─ 3. group: servers\n├─ 4. group: vps\n├─ 5. group: -pc_\n├─ 6. group: -vpn\n├─ 7. individual hosts\n╰─❯ Choice:"), "{text}");
    assert!(text.contains("Selected: admin@192.0.2.5"), "{text}");
}

#[test]
fn all_with_only_excluded_hosts_stops_before_contacting_anyone() {
    let output = run_with_inventory("[-optional]\n\"192.0.2.2\" = \"admin\"\n", "1\nn\n");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no hosts selected"));
}

#[test]
fn personal_priority_conflict_is_rejected_before_any_connections() {
    let output = run_with_inventory(
        "[laptops_]\n\"192.0.2.1\" = \"owner\"\n[_urgent]\n\"192.0.2.1\" = \"admin\"",
        "",
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("cannot be both priority (_) and personal (trailing _)")
    );
}
