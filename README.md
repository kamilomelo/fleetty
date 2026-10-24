# Fleetty

Fleetty is an early Rust terminal application for interactive SSH maintenance of a small set of Arch Linux machines. It installs nothing on remote hosts. It uses OpenSSH with your existing keys, agent, host-key checks, and any SSH config rules matching the IP address. **Try it on a noncritical machine first.**

## Get started

Install once from the project folder with `cargo install --path . --root ~/.local --locked` (add `--offline` if dependencies are already cached), then run **`fleetty`** from anywhere. This installs the executable in `~/.local/bin`, which needs to be on your shell's `PATH`. After code changes, rerun the install command with `--force` to update it. The home menu offers **Run routines**, **Edit inventory**, and **Exit**. Fleetty keeps its editable inventory at `~/.config/fleetty/inventory.toml`, or `$XDG_CONFIG_HOME/fleetty/inventory.toml` if set. If it does not exist yet, Edit inventory creates a commented example and opens `$VISUAL`, `$EDITOR`, `nano`, or `vi`. You may still specify another file: `fleetty /path/to/inventory.toml` (this skips the home menu).

```toml
[pcs]
"192.168.1.20" = "kamilo"

[servers]
"203.0.113.10" = "admin"
"203.0.113.11" = ["admin", "deploy"]

# Optional: prefix a group with _ to update it first and wait for each reboot.
[_mgt]
"192.0.2.1" = "admin"
```

Quote IPs because TOML treats unquoted dots as nested keys. Spaces around `=` are valid TOML. IPv4 and IPv6 are supported. Use a single username as before; use an array only when an IP has multiple accounts. An IP and username may appear in multiple groups; the same login is checked or commanded once when selecting `all`. Checks and custom commands can use both accounts. For updates or shutdowns, if both accounts are selected, Fleetty asks which **one** to use for that IP and acts on the machine only once. Selected machines run in numeric IP order (not alphabetical text order), with priority groups (names beginning with `_`) first; accounts on the same IP retain their inventory array order. Priority applies to the whole IP even when its accounts also appear in ordinary groups. `[mgt]` by itself has no special ordering; rename it to `[_mgt]` only if you want it prioritized. The hostname is read from `/proc/sys/kernel/hostname` after connecting (`hostname` need not be installed). The installed AUR helper is also detected remotely: `paru` is preferred over `yay`; no helper means pacman only.

Fleetty connects with `ssh -l <username> <ip>`. SSH config rules matching that IP (keys, ports, proxies) still apply, but host *aliases* aren't used. Connect manually once with `ssh username@ip` to verify and accept each host key. Fleetty asks **once per run** for a shared SSH/sudo password. If SSH authentication rejects it, Fleetty offers one alternate-password prompt for that IP and username, and reuses the alternate for that account during this run. Before an update or shutdown, Fleetty checks sudo without changing packages or power state: it tries the account's SSH password first and, if sudo rejects it, offers one separate sudo-password prompt. This also works when SSH used a key. Passwords stay only in process memory and are discarded on exit. SSH keys and agents still work, but unlock encrypted keys in your agent before starting. Do not store passwords or keys in the inventory. Fleetty requires a local terminal for password prompts.

For a host needing a specific SSH key, keep the inventory entry as `"203.0.113.10" = "admin"` and add an entry matching its IP to `~/.ssh/config`:

```sshconfig
Host 203.0.113.10
    User admin
    IdentityFile ~/.ssh/id_vps
    IdentitiesOnly yes
```

Check that `ssh 203.0.113.10` works before using Fleetty. OpenSSH reads the same config when Fleetty connects, so the key selection also works for ordinary SSH and is not duplicated across two files. **Fleetty does not currently support a key path in inventory.toml.** An optional per-host key path there could make Fleetty-only setup simpler, but then plain SSH would still need `-i` or its own config. An SSH key *passphrase* is different from the account password: Fleetty cannot answer key-passphrase prompts. Unlock encrypted keys in the same terminal before running Fleetty with `ssh-add ~/.ssh/id_vps` (if there is no agent, first run `eval "$(ssh-agent -s)"`). Confirm `ssh 203.0.113.10` now connects without another key-passphrase prompt. If the server allows only public-key authentication, an account password cannot substitute for a missing or rejected key.

## Routines

- **Check, then optionally update:** Visits every selected login with read-only SSH commands. Shows its hostname and AUR helper and checks the pacman lock, current clock time, and boot ID. After printing results, Fleetty asks which account to use on each machine with multiple passing accounts, then asks **separately** whether to update and automatically reboot only the machines that passed. Answer `n` for a check-only run. Passwords entered during checks are reused if you continue. Priority machines are checked and updated first. If an account fails the check, it is flagged and excluded from that update pass.
- **Update:** Sequential per machine, using the chosen account. Checks that the pacman database is unlocked and the remote clock differs from this computer by no more than two minutes; reads the current boot ID; runs `sudo pacman -Syu` and then `paru -Sua` or `yay -Sua` with explicit stage messages and live interactive output. When no AUR helper is detected, Fleetty **asks before running a pacman-only update** rather than silently skipping AUR. Sudo receives that account's password only when needed. Fleetty does not keep sudo authenticated in the background and invalidates the sudo timestamp before the AUR stage; AUR installation or a later reboot may ask for the sudo password again in the interactive session. Fleetty compares `pacman -Q` before and after: **if installed packages and versions are unchanged, it leaves the host running without rebooting**. If they changed, it runs `sudo reboot` in the same SSH terminal session. For a priority host (`[_mgt]`, for example), Fleetty waits up to five minutes for a different boot ID before moving on; if it cannot confirm the reboot, remaining updates are skipped. For other hosts, it immediately moves to the next update and verifies all outstanding reboots in parallel at the end, using the boot ID from before each reboot. A host that has already rebooted can still be confirmed after its five-minute deadline if it is reachable at final verification. Failure in pacman or AUR does **not** trigger a reboot: choose to repair interactively and retry, retry directly, skip, or explicitly shut down that host.
- **Shutdown:** Confirms the selection twice. Checks connectivity, then sends `sudo shutdown -h now` to up to eight hosts concurrently with password input over SSH. A successful SSH command means only that a request was sent, **not** that power-off was verified. An SSH disconnect before a response is reported as an unknown outcome.
- **Run command:** Runs the command you enter on each selected host, sequentially, with terminal output and a per-host result. Commands run as the SSH user. Only use commands you trust; shell syntax is executed remotely.

If SSH cannot connect, Fleetty retries up to three times and marks the host unreachable. It does not retry authentication or host-key failures. The final report shows each host's outcome. An offline/unreachable host appears in red; a yellow run-level warning notes that intentional shutdown is expected, but otherwise you should check the host. An unreachable host alone does not cause an error exit; a failed update or unknown shutdown outcome does. Only addresses explicitly listed in the inventory are managed.

In an interactive terminal, Fleetty colors progress counters blue, successes green, warnings yellow, and failures red. Redirected output stays plain; set `NO_COLOR=1` to disable colors in the terminal.

## Safety and limitations

- The clock comparison requires this computer's clock to be correct; it checks current accuracy, not whether an NTP service is enabled. The clock and pacman-lock preflight is not a comprehensive machine health check. Fix mirrors, Arch keyring, broken AUR builds, and time synchronization manually in the interactive repair shell if needed. Do not blindly sync package databases without completing a full system upgrade.
- A passwordless sudo policy is supported too; it does not consume the password during updates. A password-only SSH setup works if its host keys are already trusted. Sudo verification clears that account's cached sudo authentication before checking the password; the password is then reused for the update or shutdown. Passwords are not stored on disk or passed as command-line arguments or environment values. They are provided to SSH through temporary local sockets restricted to your user, and to sudo through the SSH session's standard input. Any process running as your user, or a compromised remote host, can still access a secret during a run; a shared password compromises the whole fleet if leaked.
- AUR packages run code as the SSH user. Fleetty clears its own sudo timestamp before running an AUR helper, but an AUR helper may invoke sudo later and renew the user's sudo timestamp. Do not build untrusted AUR packages on an account with privileged sudo access; consider a restricted sudo policy or a dedicated build account. A compromised remote host can still intercept passwords sent to it.
- Reboot verification needs SSH access and `/proc/sys/kernel/random/boot_id`; it does not check applications or services.
- A leading `_` marks priority ordering and synchronous reboot verification for updates; it does not make shutdown sequential. Parallel shutdown sends requests concurrently in batches of eight, even for priority hosts.
- Rust currently provides line-based terminal menus, not a full-screen TUI. Interactive SSH owns the terminal during updates. Parallel interactive updates and Konsole integration are future work.
- Results are printed, not persisted. If the local process is interrupted, review each affected host before restarting a routine. Updates, reboots, and shutdowns are inherently not transactional.

Run tests with `cargo test`.
