use crate::inventory::Host;
use std::{
    collections::{HashMap, HashSet},
    env,
    io::{self, IsTerminal, Write},
    net::IpAddr,
};

pub(crate) enum Color {
    Blue,
    Green,
    Yellow,
    Red,
}

impl Color {
    fn code(self) -> u8 {
        match self {
            Self::Blue => 34,
            Self::Green => 32,
            Self::Yellow => 33,
            Self::Red => 31,
        }
    }
}

fn colorize(text: &str, color: Color, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{}m{text}\x1b[0m", color.code())
    } else {
        text.to_owned()
    }
}

fn colors_enabled() -> bool {
    env::var_os("NO_COLOR").is_none() && env::var("TERM").ok().as_deref() != Some("dumb")
}

pub(crate) fn stdout_color(text: &str, color: Color) -> String {
    colorize(text, color, colors_enabled() && io::stdout().is_terminal())
}

pub(crate) fn stderr_color(text: &str, color: Color) -> String {
    colorize(text, color, colors_enabled() && io::stderr().is_terminal())
}

pub(crate) fn select_hosts(hosts: &[Host]) -> Result<Vec<&Host>, String> {
    let mut groups: Vec<&str> = hosts
        .iter()
        .flat_map(|h| h.groups.iter().map(String::as_str))
        .collect();
    groups.sort_unstable();
    groups.dedup();
    println!("Select hosts:");
    println!("  1. all");
    for (i, group) in groups.iter().enumerate() {
        println!("  {}. group: {group}", i + 2);
    }
    println!("  {}. individual hosts", groups.len() + 2);
    let answer = ask("Choice: ")?;
    let choice: usize = answer.trim().parse().map_err(|_| "invalid selection")?;
    let selected = if choice == 1 {
        hosts.iter().collect()
    } else if choice == groups.len() + 2 {
        for (i, host) in hosts.iter().enumerate() {
            println!("  {}. {}", i + 1, host.target());
        }
        let input = ask("Host numbers (comma-separated): ")?;
        let mut indices = HashSet::new();
        for part in input.split(',') {
            let n: usize = part.trim().parse().map_err(|_| "invalid host number")?;
            if n == 0 || n > hosts.len() {
                return Err("host number out of range".into());
            }
            indices.insert(n - 1);
        }
        hosts
            .iter()
            .enumerate()
            .filter(|(i, _)| indices.contains(i))
            .map(|(_, h)| h)
            .collect()
    } else if (2..groups.len() + 2).contains(&choice) {
        hosts
            .iter()
            .filter(|h| h.groups.iter().any(|g| g == groups[choice - 2]))
            .collect()
    } else {
        return Err("invalid selection".into());
    };
    Ok(selected)
}

pub(crate) fn choose_action_accounts<'a>(
    hosts: &[&'a Host],
    action: &str,
) -> Result<Vec<&'a Host>, String> {
    choose_accounts_with(hosts, |address, accounts| {
        let labels: Vec<_> = accounts.iter().map(|host| host.user.as_str()).collect();
        Ok(menu(
            &format!("Choose ONE account to {action} {address}"),
            &labels,
        )? - 1)
    })
}

fn choose_accounts_with<'a>(
    hosts: &[&'a Host],
    mut choose: impl FnMut(&str, &[&Host]) -> Result<usize, String>,
) -> Result<Vec<&'a Host>, String> {
    let mut by_ip: HashMap<&str, usize> = HashMap::new();
    let mut machines: Vec<Vec<&Host>> = Vec::new();
    for host in hosts {
        if let Some(&index) = by_ip.get(host.address.as_str()) {
            machines[index].push(host);
        } else {
            by_ip.insert(&host.address, machines.len());
            machines.push(vec![host]);
        }
    }
    let mut chosen = Vec::new();
    for accounts in machines {
        if accounts.len() == 1 {
            chosen.push(accounts[0]);
            continue;
        }
        let choice = choose(&accounts[0].address, &accounts)?;
        chosen.push(*accounts.get(choice).ok_or("invalid account selection")?);
    }
    chosen.sort_by_cached_key(|host| {
        (
            !host.priority,
            host.address
                .parse::<IpAddr>()
                .expect("validated IP address"),
        )
    });
    Ok(chosen)
}

pub(crate) fn menu(title: &str, options: &[&str]) -> Result<usize, String> {
    println!("{title}:");
    for (i, option) in options.iter().enumerate() {
        println!("  {}. {option}", i + 1);
    }
    let input = ask("Choice: ")?;
    let n: usize = input.trim().parse().map_err(|_| "invalid selection")?;
    if n == 0 || n > options.len() {
        return Err("invalid selection".into());
    }
    Ok(n)
}

pub(crate) fn ask(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut input = String::new();
    if io::stdin()
        .read_line(&mut input)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Err("input closed".into());
    }
    Ok(input.trim().to_string())
}

pub(crate) fn confirm(prompt: &str) -> Result<bool, String> {
    Ok(ask(&format!("{prompt} [y/N]: "))?.eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Inventory, build_hosts};
    #[test]
    fn progress_colors_are_optional() {
        assert_eq!(
            colorize("[update 5/5]", Color::Blue, true),
            "\x1b[34m[update 5/5]\x1b[0m"
        );
        assert_eq!(colorize("failed", Color::Red, false), "failed");
    }

    #[test]
    fn checks_and_commands_keep_both_accounts_but_actions_pick_one_per_ip() {
        let inventory: Inventory = toml::from_str(
            "[_mgt]\n\"192.0.2.2\" = \"admin\"\n[pcs]\n\"192.0.2.1\" = [\"admin\", \"deploy\"]\n\"192.0.2.2\" = \"deploy\"",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        let selected: Vec<_> = hosts.iter().collect();
        assert_eq!(selected.len(), 4); // check / command: both accounts on both machines
        let mut prompts = Vec::new();
        let action = choose_accounts_with(&selected, |ip, accounts| {
            prompts.push((ip.to_string(), accounts.len()));
            Ok(accounts
                .iter()
                .position(|host| host.user == "deploy")
                .unwrap())
        })
        .unwrap();
        assert_eq!(prompts.len(), 2);
        assert!(prompts.iter().all(|(_, count)| *count == 2));
        assert_eq!(action.len(), 2);
        assert_eq!(action[0].target(), "deploy@192.0.2.2"); // priority applies across accounts
        assert_eq!(action[1].target(), "deploy@192.0.2.1");
        assert!(choose_accounts_with(&selected, |_, _| Ok(2)).is_err());
    }

    #[test]
    fn addresses_run_in_numeric_order_with_priority_first() {
        let inventory: Inventory = toml::from_str(
            "[_mgt]\n\"10.1.0.4\" = \"admin\"\n[servers]\n\"10.1.20.12\" = [\"admin\", \"deploy\"]\n\"10.1.20.2\" = \"admin\"\n\"10.1.12.2\" = \"admin\"\n\"10.1.0.10\" = \"admin\"\n",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(
            hosts.iter().map(Host::target).collect::<Vec<_>>(),
            [
                "admin@10.1.0.4",
                "admin@10.1.0.10",
                "admin@10.1.12.2",
                "admin@10.1.20.2",
                "admin@10.1.20.12",
                "deploy@10.1.20.12",
            ]
        );
        let selected: Vec<_> = hosts.iter().collect();
        let updates = choose_accounts_with(&selected, |_, accounts| {
            Ok(accounts
                .iter()
                .position(|host| host.user == "deploy")
                .unwrap())
        })
        .unwrap();
        assert_eq!(
            updates.iter().map(|host| host.target()).collect::<Vec<_>>(),
            [
                "admin@10.1.0.4",
                "admin@10.1.0.10",
                "admin@10.1.12.2",
                "admin@10.1.20.2",
                "deploy@10.1.20.12",
            ]
        );
    }
}
