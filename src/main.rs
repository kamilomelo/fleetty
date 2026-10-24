mod aur;
mod auth;
mod inventory;
mod results;
mod routines;
mod ssh;
mod ui;
mod update;

use crate::{
    inventory::{default_inventory_path, edit_inventory},
    routines::run_inventory,
    ui::{Color, menu, stderr_color},
};
use std::{env, path::Path};

fn main() {
    if env::args_os().next().is_some_and(|arg| {
        std::path::Path::new(&arg).file_name() == Some(std::ffi::OsStr::new("ssh-askpass"))
    }) {
        if let Err(err) = auth::askpass() {
            eprintln!("Fleetty SSH authentication: {err}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(err) = run() {
        eprintln!("{}", stderr_color(&format!("Error: {err}"), Color::Red));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    let explicit = args.next();
    if args.next().is_some() {
        return Err("usage: fleetty [inventory.toml]".into());
    }
    if let Some(path) = explicit {
        return run_inventory(Path::new(&path));
    }
    let path = default_inventory_path()?;
    loop {
        println!("\nInventory: {}", path.display());
        match menu("Fleetty", &["Run routines", "Edit inventory", "Exit"])? {
            1 => return run_inventory(&path),
            2 => edit_inventory(&path)?,
            3 => return Ok(()),
            _ => unreachable!(),
        }
    }
}
