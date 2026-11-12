# Fleetty

Fleetty is a terminal app for maintaining a small fleet of Arch Linux machines over SSH.
It uses your local OpenSSH configuration and installs nothing on remote hosts.
**Try it on a noncritical machine first.**

## Get started

You need Rust/Cargo and OpenSSH locally, and SSH access to your Arch hosts.
From this repository:

```sh
cargo install --path . --root ~/.local --locked
fleetty
```

Ensure `~/.local/bin` is on your `PATH`. Add `--offline` if dependencies are cached;
use `--force` when reinstalling after a code change. The menu can create and edit
`~/.config/fleetty/inventory.toml` (or `$XDG_CONFIG_HOME/fleetty/inventory.toml`).
You can also run `fleetty /path/to/inventory.toml` to skip the home menu.

## Inventory

Quote IP addresses; values are SSH usernames, **never passwords**:

```toml
[servers]
"192.0.2.10" = "admin"
"192.0.2.11" = ["admin", "deploy"] # Two accounts on one machine

[_core]
"192.0.2.12" = "admin" # Priority: update first, confirm reboot before continuing

[laptops_]
"192.0.2.20" = "user" # Personal: choose whether updates may reboot it

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
For updates, reboots, and shutdowns, choose **one account per IP**; checks and
commands can use one or all accounts. In the selection menu, **all** is first,
optional groups follow other groups, and **individual hosts** is last. See the
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

Fleetty asks you to enter and confirm a shared password before contacting hosts.
SSH and sudo can prompt for account-specific alternatives if it is rejected. For
encrypted SSH keys, unlock them with `ssh-add` before starting Fleetty.
See [usage, SSH setup, outcomes, and safety notes](docs/usage.md).

## License

[MIT](LICENSE) © 2026 Kamilo Melo.
