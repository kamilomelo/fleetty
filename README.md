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

[-optional]
"198.51.100.10" = "admin" # Excluded from "all" unless you opt in
```

Fleetty sorts by numeric IP. Before the password prompt, choose **one account
per IP** for updates and shutdowns; checks and commands let you choose one or
all accounts per IP. The `_` prefix marks a priority group; `-` (for example,
`[-optional]`) excludes its machines from **all** unless you opt in. `-` groups
appear last in the host-selection menu, after **individual hosts**. See the
[inventory and run guide](docs/usage.md).

## Routines

- **Check, then optionally update:** Read-only checks first; only passing machines
  are eligible for an update, which needs a separate confirmation.
- **Update:** Run pacman and an available AUR helper interactively; reboot only
  if installed packages change. Confirm priority reboots immediately and other
  reboots after the updates finish.
- **Shutdown:** Confirm twice, then send concurrent shutdown requests.
- **Run command:** Execute your command as the SSH user on selected accounts.

Fleetty asks you to enter and confirm a shared password before contacting hosts.
SSH and sudo can prompt for account-specific alternatives if it is rejected. For
encrypted SSH keys, unlock them with `ssh-add` before starting Fleetty.
See [usage, SSH setup, outcomes, and safety notes](docs/usage.md).

## License

[MIT](LICENSE) © 2026 Kamilo Melo.
