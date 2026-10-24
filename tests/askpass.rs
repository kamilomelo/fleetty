use std::{
    io::Write,
    os::unix::{fs::symlink, net::UnixListener},
    process::Command,
    thread,
};

#[test]
fn password_prompt_uses_local_socket_and_rejects_other_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("ssh-askpass");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fleetty");
    symlink(binary, &helper).unwrap();
    let socket = dir.path().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    let worker = thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        connection.write_all(b"test-only-secret\n").unwrap();
    });
    let output = Command::new(&helper)
        .arg("user@host's password:")
        .env("FLEETTY_ASKPASS_SOCKET", &socket)
        .output()
        .unwrap();
    worker.join().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"test-only-secret\n");

    let rejected = Command::new(&helper)
        .arg("Are you sure you want to continue connecting?")
        .env("FLEETTY_ASKPASS_SOCKET", &socket)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(rejected.stdout.is_empty());
}
