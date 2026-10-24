use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::{self, Write},
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) type Inventory = BTreeMap<String, BTreeMap<String, Usernames>>;

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
    pub(crate) user: String,
    pub(crate) groups: Vec<String>,
    pub(crate) priority: bool,
}

impl Host {
    pub(crate) fn target(&self) -> String {
        format!("{}@{}", self.user, self.address)
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
    for (group, entries) in inventory {
        if group.trim().is_empty() || group == "_" || entries.is_empty() {
            return Err(format!("empty group name or group with no hosts: {group}"));
        }
        for (address, usernames) in entries {
            let ip: IpAddr = address
                .parse()
                .map_err(|_| format!("{address}: expected a valid IPv4 or IPv6 address"))?;
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
                        user: user.clone(),
                        groups: vec![group.clone()],
                        priority: false,
                    });
                }
            }
        }
    }
    if hosts.is_empty() {
        return Err("inventory has no hosts".into());
    }
    // Priority applies to the machine, even when another account on the same
    // IP is selected through an ordinary group.
    let priority_ips: HashSet<String> = hosts
        .iter()
        .filter(|host| host.groups.iter().any(|group| group.starts_with('_')))
        .map(|host| host.address.clone())
        .collect();
    for host in &mut hosts {
        host.priority = priority_ips.contains(&host.address);
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
}
