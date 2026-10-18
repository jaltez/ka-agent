//! `ka-agent service` and `ka-agent logs`: run Ka Agent in the background
//! under the platform's user service manager (systemd `--user` on Linux,
//! launchd on macOS). Unit/plist text is generated from the current
//! executable and paths, so `install` is idempotent and upgrade-safe.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ka_agent_core::DaemonConfig;
use ka_agent_runtime::paths::{InstanceLock, Paths};

#[derive(Debug, clap::Subcommand)]
pub enum ServiceAction {
    /// Install (or refresh) the background service and start it at login.
    Install,
    /// Stop the service and remove its unit files.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the running service.
    Stop,
    /// Show whether the service runs and whether the daemon answers.
    Status,
    /// Print the generated service definitions.
    Print,
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const UNSUPPORTED: &str =
    "background service install isn't supported on this OS yet — run `ka-agent run`";

const FOREGROUND_RUNNING: &str = "a foreground Ka Agent (ka-agent run or a local chat) is running — stop it first, then run ka-agent service install";

/// Whether this OS has a supported user service manager.
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "macos"))
}

// ---------------------------------------------------------------- pure text

fn parent_or_self(p: &Path) -> &Path {
    p.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(p)
}

/// The main systemd user unit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_unit(exe: &Path, root: &Path, config: &Path, workspace: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Ka Agent resident assistant\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         StartLimitIntervalSec=600\n\
         StartLimitBurst=5\n\
         [Service]\n\
         ExecStart={exe} run\n\
         Restart=always\n\
         RestartSec=3\n\
         Environment=KA_AGENT_DATA_DIR={root}\n\
         Environment=KA_AGENT_CONFIG={config}\n\
         NoNewPrivileges=true\n\
         ProtectSystem=strict\n\
         ProtectHome=read-only\n\
         ReadWritePaths={root} {config_dir} {workspace}\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        root = root.display(),
        config = config.display(),
        config_dir = parent_or_self(config).display(),
        workspace = workspace.display(),
    )
}

/// The nightly backup oneshot unit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_backup_service(exe: &Path, root: &Path, config: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Ka Agent nightly backup\n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe} backup\n\
         Environment=KA_AGENT_DATA_DIR={root}\n\
         Environment=KA_AGENT_CONFIG={config}\n",
        exe = exe.display(),
        root = root.display(),
        config = config.display(),
    )
}

/// The timer that runs the backup unit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn systemd_backup_timer() -> String {
    "[Unit]\n\
     Description=Ka Agent nightly backup\n\
     [Timer]\n\
     OnCalendar=*-*-* 03:30:00\n\
     Persistent=true\n\
     RandomizedDelaySec=300\n\
     [Install]\n\
     WantedBy=timers.target\n"
        .to_string()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const DAEMON_LABEL: &str = "dev.ka-agent.daemon";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const BACKUP_LABEL: &str = "dev.ka-agent.backup";

fn plist_head(label: &str, exe: &Path, arg: &str, root: &Path, config: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \t<key>Label</key>\n\t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{exe}</string>\n\t\t<string>{arg}</string>\n\t</array>\n\
         \t<key>EnvironmentVariables</key>\n\t<dict>\n\
         \t\t<key>KA_AGENT_DATA_DIR</key>\n\t\t<string>{root}</string>\n\
         \t\t<key>KA_AGENT_CONFIG</key>\n\t\t<string>{config}</string>\n\
         \t</dict>\n",
        exe = xml_escape(&exe.display().to_string()),
        root = xml_escape(&root.display().to_string()),
        config = xml_escape(&config.display().to_string()),
    )
}

/// The launchd agent that keeps the daemon running.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchd_plist(exe: &Path, root: &Path, config: &Path) -> String {
    let log = xml_escape(&root.join("logs/daemon.log").display().to_string());
    format!(
        "{head}\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         </dict>\n</plist>\n",
        head = plist_head(DAEMON_LABEL, exe, "run", root, config),
    )
}

/// The launchd agent that runs the nightly backup.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchd_backup_plist(exe: &Path, root: &Path, config: &Path) -> String {
    format!(
        "{head}\
         \t<key>StartCalendarInterval</key>\n\t<dict>\n\
         \t\t<key>Hour</key>\n\t\t<integer>3</integer>\n\
         \t\t<key>Minute</key>\n\t\t<integer>30</integer>\n\
         \t</dict>\n\
         </dict>\n</plist>\n",
        head = plist_head(BACKUP_LABEL, exe, "backup", root, config),
    )
}

// ---------------------------------------------------------------- context

struct Ctx {
    exe: PathBuf,
    root: PathBuf,
    config: PathBuf,
    workspace: PathBuf,
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

fn ctx(paths: &Paths, config: &Path) -> Result<Ctx, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate ka-agent: {e}"))?;
    let workspace = DaemonConfig::load(config)
        .map(|c| c.workspace_path())
        .unwrap_or_else(|_| DaemonConfig::default().workspace_path());
    Ok(Ctx {
        exe: absolute(&exe),
        root: absolute(paths.root()),
        config: absolute(config),
        workspace: absolute(&workspace),
    })
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn run_quiet(cmd: &str, args: &[&str]) -> Result<String, String> {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(format!(
            "`{cmd} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) => Err(format!("cannot run {cmd}: {e}")),
    }
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------- linux

#[cfg(target_os = "linux")]
mod imp {
    use super::*;

    const NO_SYSTEMD: &str = "systemd user services aren't available here (containers/WSL without systemd). Run `ka-agent run` in a terminal multiplexer instead.";

    fn unit_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".config"))
            .join("systemd/user")
    }

    fn units(c: &Ctx) -> [(&'static str, String); 3] {
        [
            (
                "ka-agent.service",
                systemd_unit(&c.exe, &c.root, &c.config, &c.workspace),
            ),
            (
                "ka-agent-backup.service",
                systemd_backup_service(&c.exe, &c.root, &c.config),
            ),
            ("ka-agent-backup.timer", systemd_backup_timer()),
        ]
    }

    fn systemctl(args: &[&str]) -> Result<String, String> {
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        run_quiet("systemctl", &full)
    }

    pub fn is_installed() -> bool {
        unit_dir().join("ka-agent.service").exists()
    }

    pub fn is_active() -> bool {
        // `is-active` exits non-zero when inactive; read stdout regardless.
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", "ka-agent.service"])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
    }

    pub fn install(c: &Ctx) -> Result<Vec<String>, String> {
        if systemctl(&["show-environment"]).is_err() {
            return Err(NO_SYSTEMD.to_string());
        }
        for dir in [&c.root, &c.workspace] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        if let Some(d) = c.config.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let dir = unit_dir();
        let mut lines = Vec::new();
        for (name, text) in units(c) {
            let path = dir.join(name);
            write_file(&path, &text)?;
            lines.push(format!("wrote {}", path.display()));
        }
        systemctl(&["daemon-reload"])?;
        systemctl(&[
            "enable",
            "--now",
            "ka-agent.service",
            "ka-agent-backup.timer",
        ])?;
        systemctl(&["restart", "ka-agent.service"])?;
        lines.push("✓ Ka Agent runs in the background and starts at login".into());
        lines.push("  logs: ka-agent logs -f · status: ka-agent service status".into());
        let user = std::env::var("USER").unwrap_or_default();
        if !user.is_empty()
            && let Ok(out) = run_quiet("loginctl", &["show-user", &user, "-p", "Linger"])
            && out.trim() == "Linger=no"
        {
            lines.push(format!(
                "tip: run `sudo loginctl enable-linger {user}` so Ka Agent keeps running after you log out"
            ));
        }
        Ok(lines)
    }

    pub fn uninstall() -> Result<Vec<String>, String> {
        let _ = systemctl(&[
            "disable",
            "--now",
            "ka-agent.service",
            "ka-agent-backup.timer",
        ]);
        let dir = unit_dir();
        let mut lines = Vec::new();
        for name in [
            "ka-agent.service",
            "ka-agent-backup.service",
            "ka-agent-backup.timer",
        ] {
            let p = dir.join(name);
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                lines.push(format!("removed {}", p.display()));
            }
        }
        let _ = systemctl(&["daemon-reload"]);
        lines.push("background service removed".into());
        Ok(lines)
    }

    pub fn start() -> Result<(), String> {
        systemctl(&["start", "ka-agent.service"]).map(|_| ())
    }

    pub fn stop() -> Result<(), String> {
        systemctl(&["stop", "ka-agent.service"]).map(|_| ())
    }

    pub fn state() -> String {
        if !is_installed() {
            return "not installed".into();
        }
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "ka-agent.service"])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(e) => format!("unknown ({e})"),
        }
    }

    pub fn print(c: &Ctx) {
        for (name, text) in units(c) {
            println!("# {}", unit_dir().join(name).display());
            println!("{text}");
        }
    }

    pub fn logs(_root: &Path, follow: bool, n: usize) -> ExitCode {
        let n = n.to_string();
        let mut args = vec!["--user", "-u", "ka-agent.service", "-n", &n, "-o", "cat"];
        if follow {
            args.push("-f");
        }
        match std::process::Command::new("journalctl")
            .args(&args)
            .status()
        {
            Ok(s) if s.success() => ExitCode::SUCCESS,
            Ok(_) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("ka-agent: cannot run journalctl: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

// ---------------------------------------------------------------- macOS

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    fn agents_dir() -> PathBuf {
        home().join("Library/LaunchAgents")
    }

    fn plists(c: &Ctx) -> [(&'static str, String); 2] {
        [
            (DAEMON_LABEL, launchd_plist(&c.exe, &c.root, &c.config)),
            (
                BACKUP_LABEL,
                launchd_backup_plist(&c.exe, &c.root, &c.config),
            ),
        ]
    }

    fn plist_path(label: &str) -> PathBuf {
        agents_dir().join(format!("{label}.plist"))
    }

    fn uid() -> Result<String, String> {
        run_quiet("id", &["-u"])
    }

    pub fn is_installed() -> bool {
        plist_path(DAEMON_LABEL).exists()
    }

    pub fn is_active() -> bool {
        uid().is_ok_and(|u| {
            run_quiet("launchctl", &["print", &format!("gui/{u}/{DAEMON_LABEL}")]).is_ok()
        })
    }

    pub fn install(c: &Ctx) -> Result<Vec<String>, String> {
        let u = uid()?;
        std::fs::create_dir_all(c.root.join("logs"))
            .map_err(|e| format!("{}: {e}", c.root.display()))?;
        let mut lines = Vec::new();
        for (label, text) in plists(c) {
            let path = plist_path(label);
            write_file(&path, &text)?;
            let _ = run_quiet("launchctl", &["bootout", &format!("gui/{u}/{label}")]);
            let p = path.display().to_string();
            run_quiet("launchctl", &["bootstrap", &format!("gui/{u}"), &p])?;
            lines.push(format!("wrote {p}"));
        }
        lines.push("✓ Ka Agent runs in the background and starts at login".into());
        lines.push("  logs: ka-agent logs -f · status: ka-agent service status".into());
        Ok(lines)
    }

    pub fn uninstall() -> Result<Vec<String>, String> {
        let u = uid()?;
        let mut lines = Vec::new();
        for label in [DAEMON_LABEL, BACKUP_LABEL] {
            let _ = run_quiet("launchctl", &["bootout", &format!("gui/{u}/{label}")]);
            let p = plist_path(label);
            if p.exists() {
                std::fs::remove_file(&p).map_err(|e| format!("{}: {e}", p.display()))?;
                lines.push(format!("removed {}", p.display()));
            }
        }
        lines.push("background service removed".into());
        Ok(lines)
    }

    pub fn start() -> Result<(), String> {
        let u = uid()?;
        run_quiet(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{u}/{DAEMON_LABEL}")],
        )
        .map(|_| ())
    }

    pub fn stop() -> Result<(), String> {
        let u = uid()?;
        run_quiet(
            "launchctl",
            &["kill", "SIGTERM", &format!("gui/{u}/{DAEMON_LABEL}")],
        )
        .map(|_| ())
    }

    pub fn state() -> String {
        if !is_installed() {
            "not installed".into()
        } else if is_active() {
            "loaded".into()
        } else {
            "not loaded".into()
        }
    }

    pub fn print(c: &Ctx) {
        for (label, text) in plists(c) {
            println!("<!-- {} -->", plist_path(label).display());
            println!("{text}");
        }
    }

    fn tail(path: &Path, n: usize) -> (Vec<String>, u64) {
        let bytes = std::fs::read(path).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let start = lines.len().saturating_sub(n);
        (lines[start..].to_vec(), bytes.len() as u64)
    }

    pub fn logs(root: &Path, follow: bool, n: usize) -> ExitCode {
        use std::io::{Read, Seek, SeekFrom, Write};
        let path = root.join("logs/daemon.log");
        if !path.exists() && !follow {
            eprintln!("ka-agent: no log yet at {}", path.display());
            return ExitCode::FAILURE;
        }
        let (lines, mut pos) = tail(&path, n);
        for l in lines {
            println!("{l}");
        }
        if !follow {
            return ExitCode::SUCCESS;
        }
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let Ok(mut f) = std::fs::File::open(&path) else {
                continue;
            };
            let len = f.metadata().map(|m| m.len()).unwrap_or(0);
            if len < pos {
                pos = 0; // truncated/rotated
            }
            if len == pos || f.seek(SeekFrom::Start(pos)).is_err() {
                continue;
            }
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                pos += buf.len() as u64;
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&buf);
                let _ = out.flush();
            }
        }
    }
}

// ---------------------------------------------------------------- other OS

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::*;

    pub fn is_installed() -> bool {
        false
    }
    pub fn is_active() -> bool {
        false
    }
    pub fn install(_c: &Ctx) -> Result<Vec<String>, String> {
        Err(UNSUPPORTED.into())
    }
    pub fn uninstall() -> Result<Vec<String>, String> {
        Err(UNSUPPORTED.into())
    }
    pub fn start() -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn stop() -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn state() -> String {
        "unsupported".into()
    }
    pub fn print(_c: &Ctx) {
        println!("{UNSUPPORTED}");
    }
    pub fn logs(_root: &Path, _follow: bool, _n: usize) -> ExitCode {
        eprintln!("{UNSUPPORTED}");
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------- contract

/// Whether the service definition is installed for this user.
pub fn is_installed() -> bool {
    imp::is_installed()
}

/// Whether the service manager reports the daemon service running.
pub fn is_active() -> bool {
    imp::is_active()
}

/// Write (or refresh) the service definitions, enable and (re)start them.
/// Returns the lines to print.
pub fn install(paths: &Paths, config: &Path) -> Result<Vec<String>, String> {
    if InstanceLock::is_held(paths) && !is_active() {
        return Err(FOREGROUND_RUNNING.to_string());
    }
    let c = ctx(paths, config)?;
    imp::install(&c)
}

// ---------------------------------------------------------------- CLI

fn fail(msg: &str) -> ExitCode {
    eprintln!("ka-agent: {msg}");
    ExitCode::FAILURE
}

pub fn service_action(action: ServiceAction, config: PathBuf) -> ExitCode {
    let paths = Paths::new(Paths::default_root());
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (action, config, paths);
        eprintln!("{UNSUPPORTED}");
        ExitCode::FAILURE
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    match action {
        ServiceAction::Install => match install(&paths, &config) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        ServiceAction::Uninstall => match imp::uninstall() {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        ServiceAction::Start => {
            if !is_installed() {
                return fail(
                    "the background service isn't installed — run `ka-agent service install`",
                );
            }
            match imp::start() {
                Ok(()) => {
                    println!("started");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e),
            }
        }
        ServiceAction::Stop => match imp::stop() {
            Ok(()) => {
                println!("stopped");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
        ServiceAction::Status => {
            println!("service: {}", imp::state());
            let answering = crate::admin::is_running(&paths);
            println!(
                "daemon : {}",
                if answering {
                    "running (admin socket answers)"
                } else {
                    "not answering on the admin socket"
                }
            );
            if answering {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        ServiceAction::Print => match ctx(&paths, &config) {
            Ok(c) => {
                imp::print(&c);
                ExitCode::SUCCESS
            }
            Err(e) => fail(&e),
        },
    }
}

/// `ka-agent logs [-f] [-n N]`.
pub fn logs(follow: bool, n: usize) -> ExitCode {
    let root = absolute(&Paths::default_root());
    imp::logs(&root, follow, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_runs_exe_and_opens_workspace() {
        let u = systemd_unit(
            Path::new("/opt/bin/ka-agent"),
            Path::new("/data"),
            Path::new("/cfg/ka-agent.toml"),
            Path::new("/home/me/ws"),
        );
        assert!(u.contains("ExecStart=/opt/bin/ka-agent run\n"), "{u}");
        assert!(u.contains("ReadWritePaths=/data /cfg /home/me/ws\n"), "{u}");
        assert!(u.contains("Environment=KA_AGENT_DATA_DIR=/data\n"));
        assert!(u.contains("Environment=KA_AGENT_CONFIG=/cfg/ka-agent.toml\n"));
        assert!(!u.contains("PrivateTmp"));
        assert!(u.contains("Restart=always"));
    }

    #[test]
    fn backup_units() {
        let s = systemd_backup_service(
            Path::new("/x/ka-agent"),
            Path::new("/d"),
            Path::new("/c.toml"),
        );
        assert!(s.contains("Type=oneshot") && s.contains("ExecStart=/x/ka-agent backup\n"));
        let t = systemd_backup_timer();
        assert!(t.contains("OnCalendar=*-*-* 03:30:00") && t.contains("Persistent=true"));
    }

    #[test]
    fn launchd_plist_runs_daemon_and_logs() {
        let p = launchd_plist(
            Path::new("/x/ka-agent"),
            Path::new("/d&r"),
            Path::new("/c.toml"),
        );
        assert!(p.contains("<string>/x/ka-agent</string>"), "{p}");
        assert!(p.contains("<string>run</string>"));
        assert!(
            p.contains("<string>/d&amp;r/logs/daemon.log</string>"),
            "{p}"
        );
        assert!(p.contains("<key>KeepAlive</key>\n\t<true/>"));
        let b = launchd_backup_plist(
            Path::new("/x/ka-agent"),
            Path::new("/d"),
            Path::new("/c.toml"),
        );
        assert!(b.contains("<string>backup</string>") && b.contains("<integer>30</integer>"));
    }
}
