//! The plugin CLI (`ka-agent plugin …`): keygen, pack, install, list,
//! enable/disable, remove. Policy lives in `ka-agent-plugins`; this is
//! presentation and exit codes.

use std::path::PathBuf;

use ka_agent_runtime::paths::Paths;

use crate::PluginAction;

fn plugins_root() -> PathBuf {
    Paths::new(Paths::default_root()).root().join("plugins")
}

pub fn plugin_action(action: PluginAction) -> std::process::ExitCode {
    use ka_agent_plugins::{Lockfile, PackError};
    let show = |e: PackError| -> std::process::ExitCode {
        eprintln!("ka-agent: {e}");
        std::process::ExitCode::FAILURE
    };
    let plugins = plugins_root();
    if let Err(e) = std::fs::create_dir_all(&plugins) {
        eprintln!("ka-agent: cannot create {}: {e}", plugins.display());
        return std::process::ExitCode::FAILURE;
    }
    let lock_path = plugins.join("installed.json");
    match action {
        PluginAction::Keygen => {
            let key = match ka_agent_plugins::SigningKey::generate() {
                Ok(k) => k,
                Err(e) => return show(e),
            };
            if let Err(e) = ka_agent_plugins::write_keypair(&plugins, &key, "instance") {
                return show(e);
            }
            println!("public key: {}", key.public_hex());
            println!(
                "seed written to {}/keys/{}.seed (0600)",
                plugins.display(),
                &key.public_hex()[..16]
            );
            println!(
                "hand the public line to plugin vendors to sign your updates; TOFU pins per plugin id"
            );
            std::process::ExitCode::SUCCESS
        }
        PluginAction::Pack { dir, out } => {
            let key = match ka_agent_plugins::load_signing_key(&plugins) {
                Ok(Some(k)) => k,
                Ok(None) => {
                    eprintln!("ka-agent: no signing key — run `ka-agent plugin keygen` first");
                    return std::process::ExitCode::FAILURE;
                }
                Err(e) => return show(e),
            };
            let archive = match ka_agent_plugins::pack(&dir, &key) {
                Ok(a) => a,
                Err(e) => return show(e),
            };
            let manifest = match ka_agent_plugins::archive_manifest(&archive.bytes) {
                Ok(m) => m,
                Err(e) => return show(e),
            };
            let name = format!(
                "{}-{}.{}",
                manifest.plugin.id,
                manifest.plugin.version,
                ka_agent_plugins::ARCHIVE_EXT
            );
            // An --out that names a directory gets `<id>-<version>.kplug`
            // inside it; otherwise the path is used as written.
            let path = match out {
                Some(p) if p.is_dir() => p.join(&name),
                Some(p) => p,
                None => std::env::current_dir().unwrap_or_default().join(&name),
            };
            if let Err(e) = ka_agent_plugins::write_archive(&path, &archive.bytes) {
                return show(e);
            }
            match ka_agent_plugins::write_signature(&path, &archive.signature) {
                Ok(sig) => {
                    println!("wrote {}", path.display());
                    println!("wrote {}", sig.display());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => show(e),
            }
        }
        PluginAction::Install { archive } => {
            // Plugins run under bubblewrap, which is Linux-only (plan 4.6):
            // refuse here rather than fail later with a spawn error.
            if !cfg!(target_os = "linux") {
                eprintln!(
                    "ka-agent: plugins need Linux (the bubblewrap sandbox) — this is {}",
                    std::env::consts::OS
                );
                return std::process::ExitCode::FAILURE;
            }
            let trust = match ka_agent_plugins::load_trust(&plugins) {
                Ok(t) => t,
                Err(e) => return show(e),
            };
            let signed = match ka_agent_plugins::read_archive(&archive) {
                Ok(a) => a,
                Err(e) => return show(e),
            };
            match ka_agent_plugins::install_archive(&signed, &trust, &plugins) {
                Ok(record) => {
                    println!(
                        "installed {} {} (digest {})",
                        record.id,
                        record.version,
                        &record.digest[..16]
                    );
                    println!("still disabled: `ka-agent plugin enable {}`", record.id);
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => show(e),
            }
        }
        PluginAction::List => match Lockfile::load(&lock_path) {
            Ok(lock) => {
                if lock.plugins.is_empty() {
                    println!("no plugins installed");
                    return std::process::ExitCode::SUCCESS;
                }
                for record in &lock.plugins {
                    let state = if record.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    println!(
                        "{} {} [{state}] digest {}",
                        record.id,
                        record.version,
                        &record.digest[..16]
                    );
                    let dir = plugins.join(&record.id);
                    match ka_agent_plugins::Manifest::load(
                        &dir.join(ka_agent_plugins::MANIFEST_NAME),
                    ) {
                        Ok(m) => {
                            for tool in &m.tools {
                                println!("  {} ({})", tool.name, tool.effect);
                            }
                        }
                        Err(e) => println!("  manifest error: {e}"),
                    }
                }
                std::process::ExitCode::SUCCESS
            }
            Err(e) => show(e),
        },
        PluginAction::Enable { .. } if !cfg!(target_os = "linux") => {
            eprintln!(
                "ka-agent: plugins need Linux (the bubblewrap sandbox) — this is {}",
                std::env::consts::OS
            );
            std::process::ExitCode::FAILURE
        }
        PluginAction::Enable { id } => plugin_toggle(id, true),
        PluginAction::Disable { id } => plugin_toggle(id, false),
        PluginAction::Rm { id } => plugin_rm(id),
    }
}

#[allow(clippy::too_many_lines)]
fn plugin_toggle(id: String, want: bool) -> std::process::ExitCode {
    use ka_agent_plugins::{Lockfile, PackError};
    let plugins = plugins_root();
    let show = |e: PackError| -> std::process::ExitCode {
        eprintln!("ka-agent: {e}");
        std::process::ExitCode::FAILURE
    };
    let lock_path = plugins.join("installed.json");
    let toggle = |lock: &mut Lockfile| -> Result<bool, PackError> {
        let mut found = false;
        for record in &mut lock.plugins {
            if record.id == id {
                record.enabled = want;
                found = true;
            }
        }
        Ok(found)
    };
    match Lockfile::load(&lock_path)
        .and_then(|mut l| toggle(&mut l).map(|found| (l, found)))
        .and_then(|(l, found)| l.save(&lock_path).map(|_| found))
    {
        Ok(true) => {
            println!("{} {}", if want { "enabled" } else { "disabled" }, id);
            println!(
                "takes effect on the next daemon start (the engine re-discovers tools per cycle)"
            );
            std::process::ExitCode::SUCCESS
        }
        Ok(false) => {
            eprintln!("ka-agent: no plugin {id:?}");
            std::process::ExitCode::FAILURE
        }
        Err(e) => show(e),
    }
}

fn plugin_rm(id: String) -> std::process::ExitCode {
    use ka_agent_plugins::{Lockfile, PackError};
    let plugins = plugins_root();
    let show = |e: PackError| -> std::process::ExitCode {
        eprintln!("ka-agent: {e}");
        std::process::ExitCode::FAILURE
    };
    let lock_path = plugins.join("installed.json");
    let lock = match Lockfile::load(&lock_path) {
        Ok(l) => l,
        Err(e) => return show(e),
    };
    let Some(record) = lock.get(&id) else {
        eprintln!("ka-agent: no plugin {id:?}");
        return std::process::ExitCode::FAILURE;
    };
    if record.enabled {
        eprintln!("ka-agent: {id} is enabled — `ka-agent plugin disable {id}` first");
        return std::process::ExitCode::FAILURE;
    }
    let dir = plugins.join(&id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("ka-agent: remove {}: {e}", dir.display());
        return std::process::ExitCode::FAILURE;
    }
    let mut lock = lock;
    lock.plugins.retain(|p| p.id != id);
    match lock.save(&lock_path) {
        Ok(()) => {
            println!("removed {id}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => show(e),
    }
}
