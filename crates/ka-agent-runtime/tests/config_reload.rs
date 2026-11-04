//! The hot-reload watcher must not announce "config reloaded" for the
//! file the runtime was BUILT from: seeding the mtime at
//! `set_config_path` keeps the first maintenance tick of every boot
//! silent — a PC that starts periodically would otherwise push a phone
//! notification each time for no change at all. A genuine hand edit
//! still announces (naming what applied).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use ka_agent_runtime::Paths;
use ka_agent_runtime::Runtime;

#[derive(Default)]
struct Notices(Mutex<Vec<String>>);

impl ka_agent_runtime::surface::Surface for Notices {
    fn emit(&self, env: &ka_agent_runtime::Envelope) {
        if let ka_agent_runtime::event::AgentEvent::Notice { text } = &env.ev {
            self.0.lock().unwrap().push(text.clone());
        }
    }
}

fn runtime_at(root: &std::path::Path, cfg_path: &std::path::Path) -> (Runtime, Arc<Notices>) {
    let mut port = ka_agent_port::WorkerPort::with_current_exe();
    port.binary = std::path::PathBuf::from("/nonexistent/ka-agent");
    let cfg = ka_agent_core::DaemonConfig::load(cfg_path).unwrap();
    std::fs::create_dir_all(root).unwrap();
    let store = Arc::new(ka_agent_store::Store::open(&root.join("state.db")).unwrap());
    let surface = Arc::new(Notices::default());
    let rt = Runtime::new(
        cfg,
        Paths::new(root.to_path_buf()),
        store,
        port,
        Arc::clone(&surface) as Arc<dyn ka_agent_runtime::surface::Surface>,
    )
    .unwrap();
    rt.set_config_path(cfg_path.to_path_buf());
    (rt, surface)
}

#[tokio::test]
async fn boot_is_silent_but_a_hand_edit_announces() {
    let root = std::env::temp_dir().join(format!("ka-cfg-boot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("ws")).unwrap();
    let cfg_path = root.join("ka-agent.toml");
    std::fs::write(&cfg_path, format!("workspace = {:?}\n", root.join("ws"))).unwrap();

    let (rt, surface) = runtime_at(&root, &cfg_path);
    // The first watch after boot: the file IS the config the runtime was
    // built from — nothing reloaded, nothing announced.
    rt.watch_config_file().await;
    assert!(
        surface.0.lock().unwrap().is_empty(),
        "boot announced: {:?}",
        surface.0.lock().unwrap()
    );

    // A genuine hand edit still announces, naming what applied.
    std::fs::write(
        &cfg_path,
        format!("workspace = {:?}\nmode = \"allow\"\n", root.join("ws")),
    )
    .unwrap();
    rt.watch_config_file().await;
    let notices = surface.0.lock().unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].starts_with("config reloaded"), "{notices:?}");
    assert!(notices[0].contains("mode"), "{notices:?}");

    let _ = std::fs::remove_dir_all(&root);
}
