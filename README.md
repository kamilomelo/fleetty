# Fleetty

Fleetty is a terminal app for maintaining a small fleet of Arch Linux machines over SSH.
It uses your local OpenSSH configuration and installs nothing on remote hosts.
**Try it on a noncritical machine first.**

## Get started

You need Rust/Cargo and OpenSSH locally, and SSH access to your Arch hosts.
From this repository:

```sh
cargo install --path . --root "$HOME/.local" --locked
"$HOME/.local/bin/fleetty"
```

Add `~/.local/bin` to your `PATH` to run `fleetty` from anywhere. Add `--offline`
if dependencies are cached; use `--force` when reinstalling after a code change.
The menu can create and edit
`~/.config/fleetty/inventory.toml` (or `$XDG_CONFIG_HOME/fleetty/inventory.toml`).
You can also run `fleetty /path/to/inventory.toml` to skip the home menu.

## Inventory

Quote IP addresses; `users` and simple-entry values are SSH usernames,
**never passwords**:

```toml
[servers]
"192.0.2.10" = "admin"
"192.0.2.11" = ["admin", "deploy"] # Two accounts on one machine

[_core]
"192.0.2.12" = "admin" # Priority: update first, confirm reboot before continuing

[laptops_]
"192.0.2.20" = { fallback_ips = ["192.0.2.21", "2001:db8::20"], users = ["user", "admin"] }

[-optional]
"198.51.100.10" = "admin" # Excluded from "all" unless you opt in

[-guest_]
"198.51.100.20" = "user" # Optional AND personal
```

Fleetty sorts priority IPs first, then by numeric IP. Group markers apply to
the **whole IP**, even if another account belongs to a different group:

- **Leading `_`** (for example, `[_core]`): priority during updates. Update
  these machines first and verify each reboot before continuing.
- **Leading `-`** (for example, `[-optional]`): exclude from **all** by default.
  You can opt in or select the group or host directly.
- **Trailing `_`** (for example, `[laptops_]`): personal machine. Before updates,
  choose whether **none** (the default), all, or specific selected personal
  machines may reboot if packages change. Without permission, an updated machine
  stays on and is reported as **reboot pending**. `[-guest_]` combines optional
  and personal; an IP cannot be both priority and personal.

Use `[pc_]`, not `[*pc]` (invalid TOML) or `["*pc"]` (no longer a marker).
For a machine with multiple NICs, use its preferred IP as the key and list
additional IPs in `fallback_ips`, in order. Fleetty treats them as **one
machine**: it tries another IP only when SSH cannot reach the current one,
using the same selected account and credentials. It also tries alternate IPs
when verifying a reboot. Authentication and host-key failures do not trigger
fallback. Do not list those fallback IPs as separate machines.
For updates, reboots, and shutdowns, choose **one account per machine**;
checks and commands can use one or all accounts. In the selection menu,
**all** is first, optional groups follow other groups, and **individual hosts**
is last. See the
[inventory and run guide](docs/usage.md) for details.

## Routines

- **Check, then optionally update:** Read-only checks first; only passing machines
  are eligible for an update, which needs a separate confirmation.
- **Update:** Run pacman and an available AUR helper interactively; reboot only
  if installed packages change. Confirm priority reboots immediately and other
  reboots after the updates finish.
- **Reboot:** Confirm twice, send concurrent reboot requests, then verify each
  machine's boot ID changed and SSH returned.
- **Shutdown:** Confirm twice, then send concurrent shutdown requests.
- **Run command:** Execute your command as the SSH user on selected accounts.

Before contacting hosts, Fleetty asks whether to trust the first SSH key seen
for each selected IP **for this run only**; the default **No** keeps normal host-key
checks. **Yes** uses a temporary, Fleetty-only known-hosts file without changing
your `~/.ssh/known_hosts`, but a different machine at an IP could then receive
your credentials and commands. Fleetty also asks you to enter and confirm a
shared password. SSH and sudo can prompt for account-specific alternatives if
it is rejected. For encrypted SSH keys, unlock them with `ssh-add` before
starting Fleetty.
See [usage, SSH setup, outcomes, and safety notes](docs/usage.md).

## Project layout

```text
fleetty/
├── src/
│   ├── main.rs             CLI entry point
│   ├── inventory.rs        Inventory parsing and host ordering
│   ├── routines.rs         Routine orchestration and reboot verification
│   ├── ssh.rs              SSH connections and checks
│   ├── update.rs           Interactive package updates
│   └── kartouche.rs        Copied terminal-rendering template
├── inventory.example.toml  Example inventory
├── docs/usage.md           Detailed usage and safety notes
└── tests/                 CLI integration tests
```

## Development

From the repository, run `cargo fmt --all --check`, `cargo test --locked`, and
`cargo clippy --locked --all-targets -- -D warnings`. Add `--offline` to Cargo
commands when dependencies are already cached. Tests use local fixtures; they
do not contact inventory hosts.

## License

Licensed under the [MIT License](LICENSE). © 2026 Kamilo Melo.
