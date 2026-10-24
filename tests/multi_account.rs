use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn run_with_choices(choices: &str) -> String {
    let dir = tempfile::tempdir_in("/tmp/opencode").unwrap();
    let inventory = dir.path().join("inventory.toml");
    fs::write(
        &inventory,
        "[servers]\n\"192.0.2.1\" = [\"admin\", \"deploy\"]\n",
    )
    .unwrap();
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
    let output = child.wait_with_output().unwrap();
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
fn checks_and_custom_commands_keep_both_accounts() {
    for routine in [1, 4] {
        let output = run_with_choices(&format!("1\n{routine}\nn\n"));
        assert!(!output.contains("Choose ONE account"), "{output}");
        assert!(
            output.contains("Selected: admin@192.0.2.1, deploy@192.0.2.1"),
            "{output}"
        );
    }
}
