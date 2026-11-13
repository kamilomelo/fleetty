use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::{self, Write},
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

pub(crate) type Inventory = BTreeMap<String, BTreeMap<String, Entry>>;

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Entry {
    Simple(Usernames),
    WithFallbacks(FallbackEntry),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FallbackEntry {
    fallback_ips: Vec<String>,
    users: Usernames,
}

impl Entry {
    fn parts(&self) -> (&Usernames, &[String]) {
        match self {
            Self::Simple(users) => (users, &[]),
            Self::WithFallbacks(entry) => (&entry.users, &entry.fallback_ips),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Usernames {
    One(String),
    Several(Vec<String>),
}

impl Usernames {
    fn iter(&self) -> std::slice::Iter<'_, String> {
        match self {
            Self::One(name) => std::slice::from_ref(name).iter(),
            Self::Several(names) => names.iter(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Host {
    pub(crate) address: String,
    pub(crate) fallback_ips: Vec<String>,
    pub(crate) active_address: Arc<Mutex<Option<String>>>,
    pub(crate) user: String,
    pub(crate) groups: Vec<String>,
    pub(crate) priority: bool,
    pub(crate) personal: bool,
}

impl Host {
    pub(crate) fn target(&self) -> String {
        format!("{}@{}", self.user, self.address)
    }

    pub(crate) fn dial_address(&self) -> String {
        self.active_address
            .lock()
            .expect("active address lock")
            .as_deref()
            .unwrap_or(&self.address)
            .to_owned()
    }

    pub(crate) fn set_dial_address(&self, address: &str) {
        *self.active_address.lock().expect("active address lock") = Some(address.to_owned());
    }

    pub(crate) fn connection_order(&self) -> Vec<&str> {
        let active = self.dial_address();
        let mut ips = vec![self.address.as_str()];
        ips.extend(self.fallback_ips.iter().map(String::as_str));
        if let Some(index) = ips.iter().position(|ip| *ip == active) {
            let current = ips.remove(index);
            ips.insert(0, current);
        }
        ips
    }
}

pub(crate) fn default_inventory_path() -> Result<PathBuf, String> {
    let base = if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        PathBuf::from(dir)
    } else {
        PathBuf::from(env::var_os("HOME").ok_or("HOME is unset; pass an inventory path")?)
            .join(".config")
    };
    Ok(base.join("fleetty").join("inventory.toml"))
}

pub(crate) fn edit_inventory(path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("inventory has no parent directory")?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", parent.display()))?;
    if !path.exists() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.write_all(b"# [pcs]\n# \"192.0.2.10\" = \"username\"\n")
            .map_err(|e| e.to_string())?;
    }
    let preferred = env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok().filter(|s| !s.trim().is_empty()));
    let status = if let Some(editor) = preferred {
        let mut words = editor.split_whitespace();
        let program = words.next().ok_or("empty editor command")?;
        Command::new(program)
            .args(words)
            .arg(path)
            .status()
            .map_err(|e| format!("editor {program}: {e}"))?
    } else {
        Command::new("nano")
            .arg(path)
            .status()
            .or_else(|e| {
                if e.kind() == io::ErrorKind::NotFound {
                    Command::new("vi").arg(path).status()
                } else {
                    Err(e)
                }
            })
            .map_err(|e| format!("no terminal editor available; set VISUAL or EDITOR: {e}"))?
    };
    if !status.success() {
        return Err(format!("editor exited with {status}"));
    }
    Ok(())
}

pub(crate) fn build_hosts(inventory: &Inventory) -> Result<Vec<Host>, String> {
    let mut hosts: Vec<Host> = Vec::new();
    let mut by_target: HashMap<(IpAddr, String), usize> = HashMap::new();
    let mut by_machine: HashMap<IpAddr, Vec<String>> = HashMap::new();
    for (group, entries) in inventory {
        if group.trim().is_empty() || matches!(group.as_str(), "_" | "-") || entries.is_empty() {
            return Err(format!("empty group name or group with no hosts: {group}"));
        }
        for (address, entry) in entries {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| format!("{address}: expected a valid IPv4 or IPv6 address"))?;
            let (usernames, fallbacks) = entry.parts();
            if matches!(entry, Entry::WithFallbacks(_)) && fallbacks.is_empty() {
                return Err(format!("{address}: specify at least one fallback IP"));
            }
            let mut fallback_ips = Vec::new();
            let mut seen_ips = HashSet::from([ip]);
            for fallback in fallbacks {
                let other: IpAddr = fallback
                    .parse()
                    .map_err(|_| format!("{address}: invalid fallback IP address: {fallback}"))?;
                if !seen_ips.insert(other) {
                    return Err(format!(
                        "{address}: duplicate primary or fallback IP: {fallback}"
                    ));
                }
                fallback_ips.push(other.to_string());
            }
            let defined = by_machine.entry(ip).or_default();
            if !fallback_ips.is_empty() {
                if !defined.is_empty() && *defined != fallback_ips {
                    return Err(format!("{address}: conflicting fallback IPs across groups"));
                }
                *defined = fallback_ips;
            }
            if usernames.iter().len() == 0 {
                return Err(format!("{address}: specify at least one SSH username"));
            }
            let mut seen = HashSet::new();
            for user in usernames.iter() {
                if user.is_empty()
                    || user.starts_with('-')
                    || !user
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return Err(format!("{address}: invalid SSH username: {user}"));
                }
                if !seen.insert(user) {
                    return Err(format!("{address}: duplicate SSH username: {user}"));
                }
                if let Some(&index) = by_target.get(&(ip, user.clone())) {
                    hosts[index].groups.push(group.clone());
                } else {
                    by_target.insert((ip, user.clone()), hosts.len());
                    hosts.push(Host {
                        address: ip.to_string(),
                        fallback_ips: Vec::new(),
                        active_address: Default::default(),
                        user: user.clone(),
                        groups: vec![group.clone()],
                        priority: false,
                        personal: false,
                    });
                }
            }
        }
    }
    if hosts.is_empty() {
        return Err("inventory has no hosts".into());
    }
    let mut owners = HashMap::new();
    for (primary, fallbacks) in &by_machine {
        for address in std::iter::once(*primary).chain(
            fallbacks
                .iter()
                .map(|ip| ip.parse::<IpAddr>().expect("validated IP")),
        ) {
            if let Some(previous) = owners.insert(address, primary)
                && previous != primary
            {
                return Err(format!("{address}: IP belongs to more than one machine"));
            }
        }
    }
    for host in &mut hosts {
        host.fallback_ips =
            by_machine[&host.address.parse::<IpAddr>().expect("validated IP")].clone();
    }
    // Priority applies to the machine, even when another account on the same
    // IP is selected through an ordinary group.
    let priority_ips: HashSet<String> = hosts
        .iter()
        .filter(|host| host.groups.iter().any(|group| group.starts_with('_')))
        .map(|host| host.address.clone())
        .collect();
    let personal_ips: HashSet<String> = hosts
        .iter()
        .filter(|host| host.groups.iter().any(|group| group.ends_with('_')))
        .map(|host| host.address.clone())
        .collect();
    if let Some(ip) = priority_ips.intersection(&personal_ips).next() {
        return Err(format!(
            "{ip}: cannot be both priority (_) and personal (trailing _)"
        ));
    }
    for host in &mut hosts {
        host.priority = priority_ips.contains(&host.address);
        host.personal = personal_ips.contains(&host.address);
    }
    // Priority first, then numeric IP; accounts on the same IP retain their
    // inventory array order.
    hosts.sort_by_cached_key(|host| {
        (
            !host.priority,
            host.address
                .parse::<IpAddr>()
                .expect("validated IP address"),
        )
    });
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_ip_and_duplicate_users() {
        let invalid: Inventory = toml::from_str("[pcs]\n\"not-an-ip\" = \"kamilo\"").unwrap();
        assert!(build_hosts(&invalid).is_err());
        let duplicate: Inventory =
            toml::from_str("[servers]\n\"192.0.2.1\" = [\"kamilo\", \"kamilo\"]").unwrap();
        assert!(build_hosts(&duplicate).is_err());
        let empty: Inventory = toml::from_str("[servers]\n\"192.0.2.1\" = []").unwrap();
        assert!(build_hosts(&empty).is_err());
    }

    #[test]
    fn example_inventory_parses_and_deduplicates() {
        let inventory: Inventory =
            toml::from_str(include_str!("../inventory.example.toml")).unwrap();
        assert_eq!(build_hosts(&inventory).unwrap().len(), 3);
        let overlap: Inventory = toml::from_str(
            "[pcs]\n\"192.0.2.1\" = \"kamilo\"\n[servers]\n\"192.0.2.1\" = \"kamilo\"",
        )
        .unwrap();
        let hosts = build_hosts(&overlap).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].groups, ["pcs", "servers"]);
        let multi: Inventory = toml::from_str(
            "[pcs]\n\"192.0.2.1\" = [\"kamilo\", \"deploy\"]\n[servers]\n\"192.0.2.1\" = \"deploy\"",
        ).unwrap();
        let hosts = build_hosts(&multi).unwrap();
        assert_eq!(hosts.len(), 2);
        assert_eq!(
            hosts
                .iter()
                .find(|host| host.user == "deploy")
                .unwrap()
                .groups,
            ["pcs", "servers"]
        );
    }

    #[test]
    fn fallback_ips_belong_to_one_machine_and_all_its_accounts() {
        let inventory: Inventory = toml::from_str(
            "[pcs_]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.2\", \"2001:db8::1\"], users = [\"admin\", \"user\"] }\n[servers]\n\"192.0.2.1\" = \"admin\"",
        ).unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(hosts.len(), 2);
        assert!(hosts.iter().all(|host| host.personal));
        assert!(
            hosts
                .iter()
                .all(|host| host.fallback_ips == ["192.0.2.2", "2001:db8::1"])
        );
        assert!(hosts.iter().all(|host| host.address == "192.0.2.1"));
        assert_eq!(
            hosts[0].connection_order(),
            ["192.0.2.1", "192.0.2.2", "2001:db8::1"]
        );
        hosts[0].set_dial_address("192.0.2.2");
        assert_eq!(
            hosts[0].connection_order(),
            ["192.0.2.2", "192.0.2.1", "2001:db8::1"]
        );
    }

    #[test]
    fn rejects_invalid_duplicate_or_ambiguous_fallback_ips() {
        for invalid in [
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"not-an-ip\"], users = \"user\" }",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.1\"], users = \"user\" }",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.2\", \"192.0.2.2\"], users = \"user\" }",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.2\"], users = \"user\" }\n\"192.0.2.2\" = \"admin\"",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.3\"], users = \"user\" }\n\"192.0.2.2\" = { fallback_ips = [\"192.0.2.3\"], users = \"admin\" }",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.2\"], users = \"user\" }\n[servers]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.3\"], users = \"admin\" }",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [], users = [] }",
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [], users = \"user\" }",
        ] {
            let inventory: Inventory = toml::from_str(invalid).unwrap();
            assert!(build_hosts(&inventory).is_err(), "accepted {invalid}");
        }
        assert!(toml::from_str::<Inventory>(
            "[pcs]\n\"192.0.2.1\" = { fallback_ips = [\"192.0.2.2\"], users = \"user\", typo = true }"
        ).is_err());
    }

    #[test]
    fn spaces_around_toml_equals_are_valid() {
        let inventory: Inventory = toml::from_str(
            "[servers]\n\"10.1.20.12\"   = \"itadmin\"\n\"10.1.20.3\"    = \"itadmin\"",
        )
        .unwrap();
        assert_eq!(build_hosts(&inventory).unwrap().len(), 2);
    }

    #[test]
    fn only_marked_groups_are_priority() {
        let inventory: Inventory = toml::from_str(
            "[mgt]\n\"192.0.2.1\" = \"admin\"\n[servers]\n\"192.0.2.2\" = \"admin\"\n[_urgent]\n\"192.0.2.200\" = \"admin\"",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(
            hosts.iter().map(|h| h.address.as_str()).collect::<Vec<_>>(),
            ["192.0.2.200", "192.0.2.1", "192.0.2.2"]
        );
        assert!(hosts[0].priority);
        assert!(!hosts[1].priority);
    }

    #[test]
    fn personal_is_ip_wide_and_conflicts_with_priority_but_not_optional() {
        let inventory: Inventory = toml::from_str(
            "[laptops_]\n\"192.0.2.1\" = \"owner\"\n[-optional]\n\"192.0.2.1\" = \"admin\"",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(hosts.len(), 2);
        assert!(hosts.iter().all(|host| host.personal && !host.priority));
        let conflicting: Inventory = toml::from_str(
            "[laptops_]\n\"192.0.2.1\" = \"owner\"\n[_urgent]\n\"192.0.2.1\" = \"admin\"",
        )
        .unwrap();
        assert!(
            build_hosts(&conflicting)
                .err()
                .unwrap()
                .contains("192.0.2.1: cannot be both priority (_) and personal (trailing _)")
        );
        let combined: Inventory = toml::from_str("[-laptops_]\n\"192.0.2.2\" = \"owner\"").unwrap();
        assert!(build_hosts(&combined).unwrap()[0].personal);
        let same_group_conflict: Inventory =
            toml::from_str("[_urgent_]\n\"192.0.2.3\" = \"admin\"").unwrap();
        assert!(build_hosts(&same_group_conflict).is_err());
    }

    #[test]
    fn excluded_group_names_are_valid_toml_and_not_priority() {
        let inventory: Inventory = toml::from_str(
            "[-optional]\n\"192.0.2.1\" = \"admin\"\n[servers]\n\"192.0.2.2\" = \"admin\"",
        )
        .unwrap();
        let hosts = build_hosts(&inventory).unwrap();
        assert_eq!(hosts.len(), 2);
        assert!(hosts.iter().all(|host| !host.priority));
        let empty_name: Inventory = toml::from_str("[-]\n\"192.0.2.1\" = \"admin\"").unwrap();
        assert!(build_hosts(&empty_name).is_err());
    }
}
