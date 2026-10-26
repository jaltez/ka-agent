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

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const UNSUPPORTED: &str =
    "background service install isn't supported on this OS yet — run `ka-agent run`";

/// The Windows scheduled task that carries the daemon (plan 4.5). One bare
/// name in the Task Scheduler root folder: every `schtasks.exe` call
/// (install, query, run, end, delete) uses this exact name, so `uninstall`
/// and `update` reach the task `install` created.
#[cfg(any(target_os = "windows", test))]
pub fn windows_task_name(profile: &str) -> String {
    if profile == "default" {
        "KaAgent".to_string()
    } else {
        format!("KaAgent-{profile}")
    }
}

/// Windows scheduled-task plumbing: pure text and argument vectors, so the
/// task name and the command line are testable on every platform. Only the
/// Windows `imp` below runs `schtasks.exe`.
#[cfg(any(target_os = "windows", test))]
pub(crate) mod win_task {
    use std::path::Path;

    /// `schtasks /TR` accepts at most 261 characters (`schtasks /?`); a
    /// longer command is rejected here instead of being truncated there.
    pub(crate) const TASK_COMMAND_MAX: usize = 261;

    /// The command line Task Scheduler runs for the daemon.
    ///
    /// Task Scheduler does not go through a shell, so the log redirection
    /// needs `cmd /C`. The quoting is the shape cmd reduces (it strips the
    /// first and last quote when the line holds more than two) to
    /// `"<exe>" [-p <profile>] run >> "<root>\logs\daemon.log" 2>&1` — the
    /// file `ka-agent logs` tails. Absolute paths only: the task starts
    /// without the install shell's cwd or environment.
    pub(crate) fn task_command(exe: &Path, root: &Path, profile: &str) -> String {
        let mut run = format!("\"{}\"", exe.display());
        if profile != "default" {
            run.push_str(&format!(" -p {profile}"));
        }
        run.push_str(" run");
        // A Windows command line uses `\`, and spelling it keeps the string
        // this function produces (and its Linux unit test asserts) the same
        // one Task Scheduler stores.
        let root = root.display().to_string();
        let root = root.trim_end_matches(['\\', '/']);
        format!("cmd /C \"{run} >> \"{root}\\logs\\daemon.log\" 2>&1\"")
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    /// `/Create`: the ONLOGON task, no elevation (`/RL LIMITED`).
    pub(crate) fn task_create_args(name: &str, command: &str) -> Vec<String> {
        args(&[
            "/Create", "/F", "/TN", name, "/SC", "ONLOGON", "/RL", "LIMITED", "/TR", command,
        ])
    }

    pub(crate) fn task_query_args(name: &str) -> Vec<String> {
        args(&["/Query", "/TN", name])
    }

    pub(crate) fn task_state_args(name: &str) -> Vec<String> {
        args(&["/Query", "/TN", name, "/FO", "LIST", "/V"])
    }

    pub(crate) fn task_run_args(name: &str) -> Vec<String> {
        args(&["/Run", "/TN", name])
    }

    pub(crate) fn task_end_args(name: &str) -> Vec<String> {
        args(&["/End", "/TN", name])
    }

    pub(crate) fn task_delete_args(name: &str) -> Vec<String> {
        args(&["/Delete", "/F", "/TN", name])
    }

    /// 0.12 and earlier created the task inside a `\KaAgent` folder; the
    /// fix keeps one bare name in the root folder. Install and uninstall
    /// delete the old entry best-effort — an upgrade must not leave a second
    /// ONLOGON task behind that relaunches the daemon.
    pub(crate) fn legacy_task_name(name: &str) -> String {
        format!("\\KaAgent\\{name}")
    }
}

const FOREGROUND_RUNNING: &str = "a foreground Ka Agent (ka-agent run or a local chat) is running — stop it first, then run ka-agent service install";

/// Whether this OS has a supported user service manager.
pub fn supported() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    ))
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

/// Profile-scoped service names. The default profile keeps the historical
/// unsuffixed names (existing installs are untouched); every other profile
/// gets its own unit/plist set and can run beside the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNames {
    /// systemd unit stem: `ka-agent` or `ka-agent-<profile>`.
    pub daemon: String,
    pub daemon_unit: String,
    pub backup_service: String,
    pub backup_timer: String,
    /// launchd labels: `dev.ka-agent.daemon` or `dev.ka-agent.<profile>.daemon`.
    pub daemon_label: String,
    pub backup_label: String,
}

/// Pure: the names for one profile.
pub fn unit_names(profile: &str) -> ServiceNames {
    let (stem, dot) = if profile == "default" {
        (String::new(), String::new())
    } else {
        (format!("-{profile}"), format!(".{profile}"))
    };
    ServiceNames {
        daemon: format!("ka-agent{stem}"),
        daemon_unit: format!("ka-agent{stem}.service"),
        backup_service: format!("ka-agent-backup{stem}.service"),
        backup_timer: format!("ka-agent-backup{stem}.timer"),
        daemon_label: format!("dev.ka-agent{dot}.daemon"),
        backup_label: format!("dev.ka-agent{dot}.backup"),
    }
}

fn names() -> ServiceNames {
    unit_names(&ka_agent_core::layout::profile_name())
}

/// Profiles that may own a service: systemd unit names and launchd labels
/// allow a limited alphabet.
fn service_safe_profile(profile: &str) -> bool {
    !profile.is_empty()
        && profile
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

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
    launchd_plist_labeled(exe, root, config, &names().daemon_label)
}

/// Pure core of [`launchd_plist`] with an explicit label (tests).
pub fn launchd_plist_labeled(exe: &Path, root: &Path, config: &Path, label: &str) -> String {
    let log = xml_escape(&root.join("logs/daemon.log").display().to_string());
    format!(
        "{head}\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<true/>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         </dict>\n</plist>\n",
        head = plist_head(label, exe, "run", root, config),
    )
}

/// The launchd agent that runs the nightly backup.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn launchd_backup_plist(exe: &Path, root: &Path, config: &Path) -> String {
    launchd_backup_plist_labeled(exe, root, config, &names().backup_label)
}

/// Pure core of [`launchd_backup_plist`] with an explicit label (tests).
pub fn launchd_backup_plist_labeled(exe: &Path, root: &Path, config: &Path, label: &str) -> String {
    format!(
        "{head}\
         \t<key>StartCalendarInterval</key>\n\t<dict>\n\
         \t\t<key>Hour</key>\n\t\t<integer>3</integer>\n\
         \t\t<key>Minute</key>\n\t\t<integer>30</integer>\n\
         \t</dict>\n\
         </dict>\n</plist>\n",
        head = plist_head(label, exe, "backup", root, config),
    )
}

// ---------------------------------------------------------------- context

struct Ctx {
    exe: PathBuf,
    root: PathBuf,
    config: PathBuf,
    /// systemd only: `ReadWritePaths`. launchd needs no sandbox exceptions.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

    fn units(c: &Ctx) -> [(String, String); 3] {
        let n = names();
        [
            (
                n.daemon_unit,
                systemd_unit(&c.exe, &c.root, &c.config, &c.workspace),
            ),
            (
                n.backup_service,
                systemd_backup_service(&c.exe, &c.root, &c.config),
            ),
            (n.backup_timer, systemd_backup_timer()),
        ]
    }

    fn systemctl(args: &[&str]) -> Result<String, String> {
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        run_quiet("systemctl", &full)
    }

    pub fn is_installed_for(profile: &str) -> bool {
        unit_dir().join(unit_names(profile).daemon_unit).exists()
    }

    pub fn is_active() -> bool {
        // `is-active` exits non-zero when inactive; read stdout regardless.
        std::process::Command::new("systemctl")
            .args(["--user", "is-active", &names().daemon_unit])
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
        let n = names();
        systemctl(&["enable", "--now", &n.daemon_unit, &n.backup_timer])?;
        systemctl(&["restart", &n.daemon_unit])?;
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
        uninstall_by_names(&names())
    }

    pub fn uninstall_by_names(n: &ServiceNames) -> Result<Vec<String>, String> {
        let _ = systemctl(&["disable", "--now", &n.daemon_unit, &n.backup_timer]);
        let dir = unit_dir();
        let mut lines = Vec::new();
        for name in [&n.daemon_unit, &n.backup_service, &n.backup_timer] {
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
        systemctl(&["start", &names().daemon_unit]).map(|_| ())
    }

    pub fn stop() -> Result<(), String> {
        systemctl(&["stop", &names().daemon_unit]).map(|_| ())
    }

    pub fn state() -> String {
        if !is_installed() {
            return "not installed".into();
        }
        let out = std::process::Command::new("systemctl")
            .args(["--user", "is-active", &names().daemon_unit])
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
        let unit = names().daemon_unit;
        let mut args = vec!["--user", "-u", &unit, "-n", &n, "-o", "cat"];
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

    fn plists(c: &Ctx) -> [(String, String); 2] {
        let n = names();
        [
            (n.daemon_label, launchd_plist(&c.exe, &c.root, &c.config)),
            (
                n.backup_label,
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

    pub fn is_installed_for(profile: &str) -> bool {
        plist_path(&unit_names(profile).daemon_label).exists()
    }

    pub fn is_active() -> bool {
        let label = names().daemon_label;
        uid().is_ok_and(|u| run_quiet("launchctl", &["print", &format!("gui/{u}/{label}")]).is_ok())
    }

    pub fn install(c: &Ctx) -> Result<Vec<String>, String> {
        let u = uid()?;
        std::fs::create_dir_all(c.root.join("logs"))
            .map_err(|e| format!("{}: {e}", c.root.display()))?;
        let mut lines = Vec::new();
        for (label, text) in plists(c) {
            let path = plist_path(&label);
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
        uninstall_by_names(&names())
    }

    pub fn uninstall_by_names(n: &ServiceNames) -> Result<Vec<String>, String> {
        let u = uid()?;
        let mut lines = Vec::new();
        for label in [&n.daemon_label, &n.backup_label] {
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
            &[
                "kickstart",
                "-k",
                &format!("gui/{u}/{}", names().daemon_label),
            ],
        )
        .map(|_| ())
    }

    pub fn stop() -> Result<(), String> {
        let u = uid()?;
        run_quiet(
            "launchctl",
            &[
                "kill",
                "SIGTERM",
                &format!("gui/{u}/{}", names().daemon_label),
            ],
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
            println!("<!-- {} -->", plist_path(&label).display());
            println!("{text}");
        }
    }

    pub fn logs(root: &Path, follow: bool, n: usize) -> ExitCode {
        file_logs(root, follow, n)
    }
}

/// The last `n` lines of a log file plus its length (the follow offset).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn tail(path: &Path, n: usize) -> (Vec<String>, u64) {
    let bytes = std::fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.len().saturating_sub(n);
    (lines[start..].to_vec(), bytes.len() as u64)
}

/// Tail the daemon's file log (`<root>/logs/daemon.log`), optionally
/// following it — the macOS and Windows service implementations.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn file_logs(root: &Path, follow: bool, n: usize) -> ExitCode {
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

// ---------------------------------------------------------------- windows

#[cfg(target_os = "windows")]
mod imp {
    use super::win_task::{
        TASK_COMMAND_MAX, legacy_task_name, task_command, task_create_args, task_delete_args,
        task_end_args, task_query_args, task_run_args, task_state_args,
    };
    use super::*;

    const NO_SCHTASKS: &str =
        "schtasks.exe isn't available — the Task Scheduler service must be running";

    fn task_name() -> String {
        windows_task_name(&ka_agent_core::layout::profile_name())
    }

    fn schtasks(args: &[&str]) -> Result<String, String> {
        run_quiet("schtasks.exe", args)
    }

    fn schtasks_args(args: Vec<String>) -> Result<String, String> {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        schtasks(&refs)
    }

    pub fn is_installed_for(profile: &str) -> bool {
        schtasks_args(task_query_args(&windows_task_name(profile))).is_ok()
    }

    pub fn is_active() -> bool {
        task_state().is_some_and(|s| s == "Running")
    }

    fn task_state() -> Option<String> {
        let out = schtasks_args(task_state_args(&task_name())).ok()?;
        out.lines()
            .find_map(|l| l.trim().strip_prefix("Status:"))
            .map(|s| s.trim().to_string())
    }

    pub fn install(c: &Ctx) -> Result<Vec<String>, String> {
        // The task runs at logon; no elevation is requested (/RL LIMITED).
        schtasks(&["/Query", "/?"]).map_err(|_| NO_SCHTASKS.to_string())?;
        for dir in [&c.root, &c.workspace] {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        if let Some(d) = c.config.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        std::fs::create_dir_all(c.root.join("logs"))
            .map_err(|e| format!("{}: {e}", c.root.display()))?;
        let profile = ka_agent_core::layout::profile_name();
        let name = windows_task_name(&profile);
        let command = task_command(&c.exe, &c.root, &profile);
        if command.chars().count() > TASK_COMMAND_MAX {
            return Err(format!(
                "the scheduled-task command is {} characters; schtasks allows {TASK_COMMAND_MAX} — \
                 point KA_AGENT_DATA_DIR or the install path at a shorter directory",
                command.chars().count()
            ));
        }
        let mut lines = Vec::new();
        match schtasks_args(task_create_args(&name, &command)) {
            Ok(_) => lines.push(format!("created scheduled task {name}")),
            Err(e) => {
                return Err(format!(
                    "{e}\nhint: run the shell as this user and try again"
                ));
            }
        }
        let _ = schtasks_args(task_run_args(&name));
        // 0.12 and older kept the task in a `\KaAgent` folder; drop that
        // entry so an upgrade does not leave a second ONLOGON task.
        let _ = schtasks_args(task_delete_args(&legacy_task_name(&name)));
        lines.push("✓ Ka Agent runs in the background and starts at logon".into());
        lines.push("  logs: ka-agent logs -f · status: ka-agent service status".into());
        Ok(lines)
    }

    pub fn uninstall() -> Result<Vec<String>, String> {
        uninstall_by_names(&names())
    }

    pub fn uninstall_by_names(n: &ServiceNames) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        // `daemon` is `ka-agent` or `ka-agent-<profile>`; the task name is
        // derived from the profile it stands for.
        let profile = n
            .daemon
            .strip_prefix("ka-agent")
            .unwrap_or("")
            .trim_start_matches('-');
        let profile = if profile.is_empty() {
            "default"
        } else {
            profile
        };
        let name = windows_task_name(profile);
        let _ = schtasks_args(task_end_args(&name));
        if schtasks_args(task_query_args(&name)).is_ok() {
            schtasks_args(task_delete_args(&name))?;
            lines.push(format!("removed scheduled task {name}"));
        }
        // The folder task installs from 0.12 and older left behind: it
        // would relaunch the daemon after this "remove".
        let legacy = legacy_task_name(&name);
        if schtasks_args(task_query_args(&legacy)).is_ok() {
            let _ = schtasks_args(task_end_args(&legacy));
            let _ = schtasks_args(task_delete_args(&legacy));
            lines.push(format!("removed legacy scheduled task {legacy}"));
        }
        lines.push("background service removed".into());
        Ok(lines)
    }

    pub fn start() -> Result<(), String> {
        schtasks_args(task_run_args(&task_name())).map(|_| ())
    }

    pub fn stop() -> Result<(), String> {
        schtasks_args(task_end_args(&task_name())).map(|_| ())
    }

    pub fn state() -> String {
        if !is_installed() {
            return "not installed".into();
        }
        task_state().unwrap_or_else(|| "unknown".into())
    }

    pub fn print(c: &Ctx) {
        let profile = ka_agent_core::layout::profile_name();
        let name = windows_task_name(&profile);
        let command = task_command(&c.exe, &c.root, &profile);
        println!("# scheduled task {name}");
        println!(
            "schtasks.exe /Create /F /TN \"{name}\" /SC ONLOGON /RL LIMITED /TR \"{command}\""
        );
        if command.chars().count() > TASK_COMMAND_MAX {
            println!(
                "# warning: {} characters; schtasks allows {TASK_COMMAND_MAX}",
                command.chars().count()
            );
        }
    }

    pub fn logs(root: &Path, follow: bool, n: usize) -> ExitCode {
        super::file_logs(root, follow, n)
    }
}

// ---------------------------------------------------------------- other OS

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    use super::*;

    pub fn is_installed_for(_profile: &str) -> bool {
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
    pub fn uninstall_by_names(_n: &ServiceNames) -> Result<Vec<String>, String> {
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
    imp::is_installed_for(&ka_agent_core::layout::profile_name())
}

/// Whether `profile`'s daemon unit/plist is installed.
pub fn is_installed_for(profile: &str) -> bool {
    imp::is_installed_for(profile)
}

/// Remove `profile`'s units and stop them (profile removal, rename).
pub fn uninstall_for(profile: &str) -> Result<Vec<String>, String> {
    imp::uninstall_by_names(&unit_names(profile))
}

/// Whether the service manager reports the daemon service running.
pub fn is_active() -> bool {
    imp::is_active()
}

/// Write (or refresh) the service definitions, enable and (re)start them.
/// Returns the lines to print.
pub fn install(paths: &Paths, config: &Path) -> Result<Vec<String>, String> {
    let profile = ka_agent_core::layout::profile_name();
    if !service_safe_profile(&profile) {
        return Err(format!(
            "profile name {profile:?} cannot be used for a service unit — [a-z0-9-] only"
        ));
    }
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
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (action, config, paths);
        eprintln!("{UNSUPPORTED}");
        ExitCode::FAILURE
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
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
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn windows_task_names_follow_the_profile() {
        assert_eq!(windows_task_name("default"), "KaAgent");
        assert_eq!(windows_task_name("work"), "KaAgent-work");
        // The unit/label naming stays as it was for the Unix managers.
        assert_eq!(unit_names("default").daemon, "ka-agent");
        assert_eq!(unit_names("work").daemon, "ka-agent-work");
    }

    /// The `/TN` value of one schtasks argument vector.
    fn tn(args: &[String]) -> &str {
        let i = args.iter().position(|a| a == "/TN").expect("a /TN flag");
        &args[i + 1]
    }

    #[test]
    fn every_schtasks_call_uses_the_same_bare_task_name() {
        let name = windows_task_name("work");
        let command = win_task::task_command(
            Path::new(r"C:\Users\ka\bin\ka-agent.exe"),
            Path::new(r"C:\Users\ka\.ka-agent"),
            "work",
        );
        let vectors = [
            win_task::task_create_args(&name, &command),
            win_task::task_query_args(&name),
            win_task::task_state_args(&name),
            win_task::task_run_args(&name),
            win_task::task_end_args(&name),
            win_task::task_delete_args(&name),
        ];
        for args in &vectors {
            assert_eq!(tn(args), name, "{args:?}");
            assert!(
                args.iter().all(|a| !a.starts_with('\\')),
                "the task stays in the root folder: {args:?}"
            );
        }
        // Only the migration cleanup names the pre-0.13 folder.
        assert_eq!(win_task::legacy_task_name(&name), r"\KaAgent\KaAgent-work");
        assert_eq!(
            win_task::legacy_task_name(&windows_task_name("default")),
            r"\KaAgent\KaAgent"
        );
    }

    #[test]
    fn task_command_redirects_the_daemon_log() {
        let exe = Path::new(r"C:\Users\ka\bin\ka-agent.exe");
        let root = Path::new(r"C:\Users\ka\.ka-agent");
        assert_eq!(
            win_task::task_command(exe, root, "default"),
            r#"cmd /C ""C:\Users\ka\bin\ka-agent.exe" run >> "C:\Users\ka\.ka-agent\logs\daemon.log" 2>&1""#,
            "cmd strips the outer quotes back to `\"<exe>\" run >> \"<log>\"`"
        );
        let work = win_task::task_command(exe, root, "work");
        assert_eq!(
            work,
            r#"cmd /C ""C:\Users\ka\bin\ka-agent.exe" -p work run >> "C:\Users\ka\.ka-agent\logs\daemon.log" 2>&1""#
        );
        assert!(
            work.chars().count() < win_task::TASK_COMMAND_MAX,
            "{} characters, schtasks allows {}",
            work.chars().count(),
            win_task::TASK_COMMAND_MAX
        );
    }

    #[test]
    fn unit_names_scope_per_profile() {
        let d = unit_names("default");
        assert_eq!(d.daemon_unit, "ka-agent.service");
        assert_eq!(d.backup_timer, "ka-agent-backup.timer");
        assert_eq!(d.daemon_label, "dev.ka-agent.daemon");
        let w = unit_names("work");
        assert_eq!(w.daemon_unit, "ka-agent-work.service");
        assert_eq!(w.backup_service, "ka-agent-backup-work.service");
        assert_eq!(w.backup_timer, "ka-agent-backup-work.timer");
        assert_eq!(w.daemon_label, "dev.ka-agent.work.daemon");
        assert_eq!(w.backup_label, "dev.ka-agent.work.backup");
    }

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
