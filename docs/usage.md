# Using Fleetty

Fleetty is interactive and Linux/Unix-terminal oriented. It runs OpenSSH locally;
remote machines need SSH, Arch Linux, `pacman`, and (for updates or shutdowns)
working `sudo`. An AUR helper is optional. It manages only IPs in your inventory.

## Inventory and selection

Run `fleetty` and choose **Edit inventory**, or edit
`~/.config/fleetty/inventory.toml` yourself. If `XDG_CONFIG_HOME` is set, the
path is `$XDG_CONFIG_HOME/fleetty/inventory.toml`. You can pass another path as
the only CLI argument. See [inventory.example.toml](../inventory.example.toml).

```toml
[pcs]
"192.0.2.10" = "alex"

[servers]
"203.0.113.10" = ["admin", "deploy"]

[_network]
"192.0.2.1" = "admin"
```

- Quote IPv4/IPv6 addresses: dots otherwise become nested TOML keys. Spaces
  around `=` are fine. Use an array only for multiple usernames on one IP.
- Choose all, a group, or individual accounts. The same IP/username in several
  groups is visited once. Before entering the shared password, choose one or
  all accounts for each IP with multiple usernames when checking or running
  commands. Updates and shutdowns require exactly one account per IP. A check
  of both accounts may ask again which passing account to use for an update.
- A group beginning with `_` is **priority**: its machines run first, in numeric
  IP order. Other machines follow in numeric order. Priority applies to the
  entire IP, even when an account also belongs to a normal group. `[mgt]` alone
  is not special; use `[_mgt]` if you want priority behavior.
- Do not put passwords, private keys, or key paths in the inventory.

## SSH and passwords

Fleetty calls `ssh -l <username> <ip>`. OpenSSH applies configuration for the
**IP**, not a host alias. Connect manually first to check the host key. For a
host-specific identity, put the key path in `~/.ssh/config`:

```sshconfig
Host 203.0.113.10
    User admin
    IdentityFile ~/.ssh/id_vps
    IdentitiesOnly yes
```

Test `ssh 203.0.113.10` before running Fleetty. For an encrypted private key,
use `ssh-add ~/.ssh/id_vps` in the terminal where you will run Fleetty. If no
agent is available, first run `eval "$(ssh-agent -s)"`. Fleetty cannot answer
**key-passphrase** prompts; an account password cannot replace a key on a
public-key-only server.

Fleetty always asks for a shared SSH/sudo password at the start of a run, even
when SSH uses keys. You enter it twice; if the entries differ, Fleetty asks
again before connecting to any hosts. This catches mistypes, not a password
entered incorrectly both times. If SSH rejects it and the server permits
password login, Fleetty offers one alternate password for that account. Before an update or
shutdown, it checks sudo: it first tries that account's SSH credential and
offers one separate sudo password if rejected. Passwordless sudo also works.
The check resets sudo's cached authentication, so sudo may ask again during a
long update or AUR installation.

Passwords are not saved in the inventory, command arguments, or environment.
Fleetty holds them for the run and supplies them through an owner-only local
socket for SSH or through the SSH session's stdin for sudo. A process running
as your local user, or a compromised remote machine receiving a password, can
still access it. A shared password increases the impact if one host is
compromised.

## What each routine does

1. **Check, then optionally update** runs read-only SSH checks for each
   selected account: hostname, clock (within two minutes of this computer),
   pacman lock, boot ID, and AUR helper. Failed accounts are excluded. After
   showing results, Fleetty asks which account to use if a passing IP has
   multiple accounts, then asks whether to update. Answer **No** for a
   check-only run.
2. **Update** checks the lock, clock, and boot ID again, then runs
   `sudo pacman -Syu` and `paru -Sua` or `yay -Sua` with live terminal output.
   Fleetty sets the remote terminal to your local viewport size before the
   interactive update starts; resizing the window mid-update is not synchronized.
   `paru` takes precedence. With neither helper, Fleetty asks before a
   pacman-only update. AUR review remains interactive; Fleetty does not keep
   sudo authenticated in the background and clears its timestamp before AUR,
   so a later sudo action may request your password again. If pacman or AUR
   fails, Fleetty does **not** reboot: you can retry, repair interactively,
   skip, leave the host on, or explicitly request shutdown.
3. After a successful update, Fleetty compares `pacman -Q` before and after.
   No package changes means **no reboot**. Otherwise it requests a reboot and
   requires a different boot ID on reconnection. It waits up to five minutes
   for each priority machine before continuing; an unconfirmed priority
   reboot skips remaining updates. Normal machines proceed to the next update
   immediately, then their reboots are checked in parallel at the end. Even
   after five minutes, a machine reachable at final verification can still
   be confirmed. Reboot verification does not check services or applications.
4. **Shutdown** confirms twice, checks connectivity and sudo, then dispatches
   `sudo shutdown -h now` to at most eight machines at a time. A successful
   command means the request was sent, **not** that power-off was verified.
   Priority ordering does not make shutdown sequential.
5. **Run command** executes the entered shell command sequentially on each
   selected account, as its SSH user. It does not elevate with sudo for you;
   use only commands you trust.

## Results and safety

- Final results use a rail: `✓` means done (including no package changes),
  `○` means skipped, unreachable, or a shutdown request not verified, and `✗`
  means failed or unknown shutdown outcome. The label gives the exact status.
- Noninteractive SSH checks and shutdown requests have a 30-second deadline
  and a 16 KiB output limit per stream. Interactive updates and custom commands
  have no automatic deadline, since AUR builds and user commands may take a
  long time; they still require supervision.
- Connection failures are tried up to three times. Authentication and host-key
  failures are not retried. An **unreachable** host is shown in red with a
  yellow warning, but does not by itself cause a nonzero exit status: it may
  have been intentionally shut down.
- **Skipped** hosts (such as a declined pacman-only update) produce warnings.
  Choosing to leave a machine on after an update failure now flags it as
  **failed** while continuing to other hosts. Failed operations and unknown
  shutdown outcomes cause an error exit. Review per-host results; results are
  not persisted.
- The preflight is not a health check. Your local clock must be correct; fix
  mirrors, keyrings, or broken AUR builds manually if needed. Do not sync
  pacman databases without completing a full system upgrade.
- AUR builds run code as the SSH user. Fleetty clears *its* sudo timestamp
  before the AUR stage, but a helper can invoke sudo later and recreate one.
  Do not run untrusted AUR builds with privileged sudo access; consider a
  restricted sudo policy or a dedicated build account.
- Updates, reboots, and shutdowns are not transactional. After interruption,
  inspect affected hosts before running a routine again. `NO_COLOR=1` disables
  terminal colors; redirected output is plain.

## Project map

- `src/main.rs` — CLI entry point; `src/routines.rs` — routine orchestration and reboot checks.
- `src/inventory.rs` and `src/ui.rs` — inventory, selection, and terminal menus.
- `src/ssh.rs` and `src/auth.rs` — SSH checks, credentials, and local askpass socket.
- `src/update.rs`, `src/aur.rs`, and `src/results.rs` — updates, AUR choices, and outcomes.
- `src/kartouche.rs` — locally copied [kartouche](https://github.com/kamilomelo/kartouche)
  Rust template (MIT, [v2 source](https://github.com/kamilomelo/kartouche/blob/18d6ecf491f2a06dd855ca0b1b66ab9254b771fe/rust/kartouche.rs));
  renders the gray banner and append-only dot steps (a line bar above 12 accounts).
  Follow kartouche's
  [update guide](https://github.com/kamilomelo/kartouche/blob/main/UPDATING.md)
  when refreshing this local copy.
- `inventory.example.toml` — minimal example; `tests/` — local tests.

Run checks with `cargo fmt --check`, `cargo test --locked`, and
`cargo clippy --all-targets --locked -- -D warnings`.
