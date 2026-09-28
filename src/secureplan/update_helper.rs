//! The detached install-and-relaunch helper (DSK-07).
//!
//! When the user chooses **Install and restart**, the application writes a
//! plan next to the verified download and, as it exits, starts a copy of
//! itself with `--secureplan-update-helper <plan>`. The helper holds the read
//! end of a pipe whose write end only the exiting application has; it waits
//! for that pipe to close (the application has ended; on Windows it then also
//! waits for the process object), installs, writes the outcome for the next
//! start, and opens SecurePlan CAD again. The new copy is started only after
//! the old one has gone, so nothing is handed to the exiting process (and
//! SecurePlan CAD has no unauthenticated single-instance hand-off anyway).
//!
//! - **Windows:** a per-user `msiexec /i <msi> /passive /norestart`. Windows
//!   Installer rolls back on failure, so the installed copy is untouched. The
//!   helper runs from a copy of the executable in the staging folder, so the
//!   installer can replace the installed one.
//! - **macOS:** mount the DMG read-only, `ditto` the app next to the installed
//!   one (same volume), check its signature, then swap by rename and remove
//!   the old copy. A failure before the swap leaves the installed app as it
//!   was; a failed swap is undone. The update is refused beforehand when the
//!   app runs under App Translocation, outside an app bundle, or from a folder
//!   the user cannot write.
//! - **Linux (x64 .deb):** the system installer, `pkexec apt-get install` of
//!   the verified package, which asks for the user's password. The update is
//!   refused beforehand unless the app runs as the packaged
//!   `/usr/bin/secureplan-cad`; without `pkexec` the app keeps the verified
//!   package and shows its location and the command to install it. When the
//!   installer cannot get permission (no polkit authentication agent, or the
//!   password was refused) or stops with an error, the package is kept and the
//!   next start shows the same. The command is built as an argument list and
//!   never passes through a shell.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::update::Staged;

/// The argument that starts the helper instead of the application.
pub const HELPER_ARG: &str = "--secureplan-update-helper";
/// The release app's bundle identifier (`packaging/Info.plist`).
pub const BUNDLE_ID: &str = "in.vizmo.secureplan.cad";
/// The app inside the release DMG.
pub const DMG_APP: &str = "SecurePlan CAD.app";
/// Where the per-user MSI installs, under `%LOCALAPPDATA%`
/// (`packaging/windows/main.wxs`).
pub const WINDOWS_INSTALL_DIR: [&str; 2] = ["Programs", "SecurePlan CAD"];
pub const WINDOWS_EXE: &str = "SecurePlanCAD.exe";
/// Where the Linux package installs the program
/// (`packaging/secureplan/make-release-deb.sh`).
pub const LINUX_EXE: &str = "/usr/bin/secureplan-cad";
/// The system installer the Linux helper runs, through polkit's `pkexec`.
pub const PKEXEC: &str = "/usr/bin/pkexec";
pub const APT_GET: &str = "/usr/bin/apt-get";
/// How long apt-get waits for another package manager to finish.
const DPKG_LOCK_WAIT_SECS: u32 = 120;
/// The plan's file name in the staging folder.
const PLAN_FILE: &str = "update-plan.json";
/// How long the helper waits for the application to exit.
const EXIT_WAIT: Duration = Duration::from_secs(120);

/// Where this copy is installed, when it can update itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Target {
    /// The per-user MSI's executable.
    Windows { exe: PathBuf },
    /// The `.app` bundle.
    Mac { app: PathBuf },
    /// The program the `.deb` package installed.
    Linux { exe: PathBuf },
}

/// How the helper installs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Install {
    Msi { msi: PathBuf, exe: PathBuf },
    Dmg { dmg: PathBuf, app: PathBuf },
    Deb { deb: PathBuf, exe: PathBuf },
}

/// The helper's instructions, written to the staging folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// The version being installed; the DMG's app must say the same.
    pub version: String,
    /// The application process to wait for.
    pub parent_pid: u32,
    pub install: Install,
    /// Where to write the [`Outcome`] for the next start.
    pub result: Option<PathBuf>,
    /// The staging folder, removed afterwards (as far as possible).
    pub staging: PathBuf,
}

/// A prepared helper launch.
#[derive(Debug)]
pub struct Launch {
    pub helper: PathBuf,
    pub plan_path: PathBuf,
    pub plan: Plan,
}

/// What the helper reports to the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub version: String,
    pub installed: bool,
    pub message: String,
    /// Linux: the command that installs the kept download by hand, when the
    /// system installer could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual_command: Option<String>,
}

// ── Before the update (in the application) ──────────────────────────────────

/// Where this running copy is installed, or why it cannot update itself.
pub fn target() -> Result<Target, String> {
    let exe = std::env::current_exe().map_err(|_| "SecurePlan CAD could not find its own program file.".to_string())?;
    if cfg!(target_os = "macos") {
        mac_target(&exe).map(|app| Target::Mac { app })
    } else if cfg!(windows) {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        windows_target(&exe, local.as_deref()).map(|exe| Target::Windows { exe })
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        linux_target(&exe).map(|exe| Target::Linux { exe })
    } else {
        Err(UNSUPPORTED.into())
    }
}

/// Where SecurePlan CAD does not update itself.
pub const UNSUPPORTED: &str = "SecurePlan CAD updates itself on macOS, on Windows and on Linux x64 only.";

/// The `.app` holding `exe` (`X.app/Contents/MacOS/<exe>`), if the update can
/// replace it: not translocated, and its folder writable by this user.
pub fn mac_target(exe: &Path) -> Result<PathBuf, String> {
    let app = exe
        .parent()
        .filter(|dir| dir.ends_with("Contents/MacOS"))
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|app| app.extension().is_some_and(|ext| ext == "app") && app.join("Contents/Info.plist").is_file())
        .ok_or_else(|| "This copy of SecurePlan CAD is not an installed app, so it cannot update itself.".to_string())?;
    if app.components().any(|part| part.as_os_str() == "AppTranslocation") {
        return Err("macOS is running SecurePlan CAD from a temporary, read-only location (App Translocation), so it cannot update itself. Move SecurePlan CAD to the Applications folder, open it from there, and update again.".into());
    }
    let folder = app.parent().ok_or_else(|| "SecurePlan CAD could not find its folder.".to_string())?;
    // The swap renames in this folder: prove it is writable.
    let probe = folder.join(format!(".secureplan-cad-update-probe-{}", std::process::id()));
    std::fs::create_dir(&probe)
        .and_then(|()| std::fs::remove_dir(&probe))
        .map_err(|_| "The folder that holds SecurePlan CAD is not writable by your account, so it cannot update itself. Move it to your Applications folder or ask an administrator to update it.".to_string())?;
    Ok(app.to_path_buf())
}

/// The installed per-user executable, if `exe` is it.
pub fn windows_target(exe: &Path, local_app_data: Option<&Path>) -> Result<PathBuf, String> {
    let installed = local_app_data
        .map(|local| local.join(WINDOWS_INSTALL_DIR[0]).join(WINDOWS_INSTALL_DIR[1]).join(WINDOWS_EXE))
        .ok_or_else(|| "SecurePlan CAD could not find your local application folder.".to_string())?;
    let same = |a: &Path, b: &Path| a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase();
    if !same(exe, &installed) {
        return Err("This copy of SecurePlan CAD was not installed by the SecurePlan CAD installer, so it cannot update itself. Install it with the installer from the releases page.".into());
    }
    Ok(installed)
}

/// The packaged program, if `exe` is it.
pub fn linux_target(exe: &Path) -> Result<PathBuf, String> {
    if exe != Path::new(LINUX_EXE) {
        return Err("This copy of SecurePlan CAD was not installed from the SecurePlan CAD package, so it cannot update itself. Install the package (SecurePlanCAD-linux-x64.deb) from the releases page.".into());
    }
    Ok(exe.to_path_buf())
}

/// The system installer's command for the verified package `deb`: the
/// program and its arguments, run without a shell. `-y` because the user
/// already chose Install and polkit asks for the password; `--no-remove` so
/// that the update never removes another package; the lock timeout waits for
/// a package manager that is already running (such as unattended upgrades).
pub fn deb_install_command(pkexec: &Path, deb: &Path) -> (PathBuf, Vec<std::ffi::OsString>) {
    let mut args: Vec<std::ffi::OsString> = [APT_GET, "install", "-y", "--no-remove", "-o"].iter().map(Into::into).collect();
    args.push(format!("DPkg::Lock::Timeout={DPKG_LOCK_WAIT_SECS}").into());
    args.push(deb.as_os_str().to_owned());
    (pkexec.to_path_buf(), args)
}

/// The command that installs the kept, verified package `deb` by hand.
pub fn manual_install_command(deb: &Path) -> String {
    format!("sudo apt install {}", shell_quoted(&deb.display().to_string()))
}

/// What the user can do when SecurePlan CAD cannot run the system installer
/// itself: where the verified package is, and the command that installs it.
pub fn manual_install_lines(deb: &Path) -> Vec<String> {
    vec![
        format!("The downloaded package was verified and is kept at: {}", deb.display()),
        format!("To install it, run this command in a terminal: {}", manual_install_command(deb)),
    ]
}

/// `text` quoted for a POSIX shell when it needs quoting; for the command
/// shown to the user only (SecurePlan CAD never runs it through a shell).
fn shell_quoted(text: &str) -> String {
    let plain = !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+:=,@%".contains(&b));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

/// Where `pkexec` is: the system's, or (`secureplan-test` builds only) a
/// stand-in named by `SECUREPLAN_TEST_PKEXEC`, so the helper tests never
/// start the real installer.
pub fn pkexec_path() -> PathBuf {
    #[cfg(feature = "secureplan-test")]
    if let Some(stand_in) = std::env::var_os("SECUREPLAN_TEST_PKEXEC") {
        return PathBuf::from(stand_in);
    }
    PathBuf::from(PKEXEC)
}

/// Before **Install and restart**: when this copy cannot run the system
/// installer (Linux without `pkexec`), what to tell the user instead of
/// quitting, and the command to copy. The verified download must then be
/// kept for them.
pub fn manual_install(target: &Target, staged: &Staged, pkexec: &Path) -> Option<(Vec<String>, String)> {
    match target {
        Target::Linux { .. } if !pkexec.is_file() => {
            let mut lines = vec!["SecurePlan CAD cannot install the update itself: pkexec, which asks for your password to install packages, is not available.".to_string()];
            lines.extend(manual_install_lines(&staged.file));
            Some((lines, manual_install_command(&staged.file)))
        }
        _ => None,
    }
}

/// Write the plan for `staged` into its staging folder; on Windows also copy
/// this executable there to run the helper from.
pub fn prepare(staged: &Staged, target: &Target, result: Option<PathBuf>) -> Result<Launch, String> {
    let current = std::env::current_exe().map_err(|_| "SecurePlan CAD could not find its own program file.".to_string())?;
    let (install, helper) = match target {
        Target::Windows { exe } => {
            let helper = staged.dir.join("SecurePlanCAD-update-helper.exe");
            std::fs::copy(&current, &helper).map_err(|_| "SecurePlan CAD could not prepare the installer.".to_string())?;
            (Install::Msi { msi: staged.file.clone(), exe: exe.clone() }, helper)
        }
        Target::Mac { app } => (Install::Dmg { dmg: staged.file.clone(), app: app.clone() }, current),
        // The package replaces the program by rename; the helper keeps running
        // from the replaced file.
        Target::Linux { exe } => (Install::Deb { deb: staged.file.clone(), exe: exe.clone() }, current),
    };
    let plan = Plan {
        version: staged.release.version.clone(),
        parent_pid: std::process::id(),
        install,
        result,
        staging: staged.dir.clone(),
    };
    let plan_path = staged.dir.join(PLAN_FILE);
    let json = serde_json::to_vec_pretty(&plan).map_err(|_| "SecurePlan CAD could not prepare the update.".to_string())?;
    std::fs::write(&plan_path, json).map_err(|_| "SecurePlan CAD could not prepare the update.".to_string())?;
    Ok(Launch { helper, plan_path, plan })
}

/// Where the helper leaves its outcome: SecurePlan CAD's config folder.
pub fn result_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|dir| dir.join("update-result.json"))
}

/// Start the helper, detached, as the application exits. It keeps the read
/// end of a pipe; the write end stays open, unused, until this process ends.
pub fn launch(launch: Launch) {
    let mut command = std::process::Command::new(&launch.helper);
    command
        .arg(HELPER_ARG)
        .arg(&launch.plan_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach(&mut command);
    match command.spawn() {
        Ok(mut child) => {
            // Closed by the operating system when this process has ended.
            std::mem::forget(child.stdin.take());
        }
        Err(_) => {
            if let Some(path) = &launch.plan.result {
                write_outcome(path, &Outcome {
                    version: launch.plan.version.clone(),
                    installed: false,
                    message: "The installer helper could not start, so the update was not installed.".into(),
                    manual_command: None,
                });
            }
            let _ = std::fs::remove_dir_all(&launch.plan.staging);
        }
    }
}

#[cfg(unix)]
fn detach(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut std::process::Command) {}

/// Read and delete the helper's outcome, once, at the next start.
pub fn take_outcome(path: &Path) -> Option<Outcome> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    serde_json::from_slice(&bytes).ok()
}

fn write_outcome(path: &Path, outcome: &Outcome) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(outcome) {
        let _ = std::fs::write(path, json);
    }
}

// ── The helper process ──────────────────────────────────────────────────────

/// For `main`, before anything else: run the helper when started as one, or
/// (test builds) record a relaunch. `Some(exit code)` means exit now.
pub fn main_hook() -> Option<i32> {
    let helper = std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new(HELPER_ARG));
    #[cfg(feature = "secureplan-test")]
    if let (false, Some(spec)) = (helper, std::env::var_os(TEST_PARENT_ENV)) {
        return Some(run_test_parent(Path::new(&spec)));
    }
    #[cfg(feature = "secureplan-test")]
    if let Some(marker) = std::env::var_os("SECUREPLAN_TEST_RELAUNCH_MARKER") {
        // Helper tests: the relaunched copy records itself and leaves.
        if !helper {
            use std::io::Write;
            let exe = std::env::current_exe().map(|exe| exe.display().to_string()).unwrap_or_default();
            if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(marker) {
                let _ = writeln!(file, "{} {exe}", std::process::id());
            }
            return Some(0);
        }
    }
    if !helper {
        return None;
    }
    let Some(plan_path) = std::env::args_os().nth(2) else { return Some(2) };
    Some(run(Path::new(&plan_path)))
}

/// Helper tests: the environment variable naming a [`TestParent`] file.
#[cfg(feature = "secureplan-test")]
pub const TEST_PARENT_ENV: &str = "SECUREPLAN_TEST_UPDATE_PARENT";

/// Helper tests (`secureplan-test` builds only): this process plays the
/// application that installs a verified download. It goes through the
/// production [`target`] (unless `target` is given), [`prepare`] and
/// [`launch`], says it has started, and exits only once `exit_when` exists,
/// so a test controls exactly when "the application" ends.
#[cfg(feature = "secureplan-test")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestParent {
    pub version: String,
    /// The verified download inside `staging`.
    pub file: PathBuf,
    pub staging: PathBuf,
    pub result: PathBuf,
    /// Where this copy is installed; `None` asks the production [`target`].
    pub target: Option<Target>,
    /// Written with this process id once the helper has started (or with
    /// `refused: <reason>`).
    pub started: PathBuf,
    pub exit_when: PathBuf,
}

#[cfg(feature = "secureplan-test")]
fn run_test_parent(spec_path: &Path) -> i32 {
    // Neither the helper nor the relaunched copy may play the parent again.
    std::env::remove_var(TEST_PARENT_ENV);
    let Some(spec) = std::fs::read(spec_path).ok().and_then(|bytes| serde_json::from_slice::<TestParent>(&bytes).ok()) else {
        return 2;
    };
    let prepared = spec.target.clone().map_or_else(target, Ok).and_then(|target| {
        let size = std::fs::metadata(&spec.file).map(|meta| meta.len()).unwrap_or_default();
        let name = spec.file.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let release = super::update::Release {
            version: spec.version.clone(),
            tag: super::release_tag(&spec.version).unwrap_or_default(),
            notes: String::new(),
            published: 0,
            asset: super::update::Asset { name, size },
        };
        let staged = Staged { release, dir: spec.staging.clone(), file: spec.file.clone() };
        prepare(&staged, &target, Some(spec.result.clone()))
    });
    match prepared {
        Ok(prepared) => launch(prepared),
        Err(reason) => {
            let _ = std::fs::write(&spec.started, format!("refused: {reason}"));
            return 3;
        }
    }
    let _ = std::fs::write(&spec.started, std::process::id().to_string());
    let deadline = std::time::Instant::now() + Duration::from_secs(15 * 60);
    while !spec.exit_when.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    0
}

/// The helper: wait for the application to exit, install, record the
/// outcome, relaunch. Returns the process exit code.
pub fn run(plan_path: &Path) -> i32 {
    let Some(plan) = std::fs::read(plan_path).ok().and_then(|bytes| serde_json::from_slice::<Plan>(&bytes).ok()) else {
        return 2;
    };
    // Held from the start, so the process cannot be confused with a later one.
    #[cfg(windows)]
    let parent = win_process::Process::open(plan.parent_pid);
    let waited = || -> Result<(), String> {
        wait_for_stdin_eof(EXIT_WAIT)?;
        #[cfg(windows)]
        if let Some(parent) = &parent {
            parent.wait(Duration::from_secs(30))?;
        }
        Ok(())
    };
    let outcome = run_plan(&plan, waited, || install(&plan), || relaunch(&plan.install));
    match outcome {
        Some(outcome) if outcome.installed => 0,
        _ => 1,
    }
}

/// Why the helper did not install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotInstalled {
    pub message: String,
    /// Linux: the verified download is kept, and this command installs it.
    pub manual_command: Option<String>,
}

impl From<String> for NotInstalled {
    fn from(message: String) -> Self {
        Self { message, manual_command: None }
    }
}

impl From<&str> for NotInstalled {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// The helper's steps. Nothing is installed or relaunched unless the
/// application has exited; after an install attempt, successful or not,
/// SecurePlan CAD opens again and finds the outcome.
pub fn run_plan(
    plan: &Plan,
    wait: impl FnOnce() -> Result<(), String>,
    install: impl FnOnce() -> Result<(), NotInstalled>,
    relaunch: impl FnOnce() -> Result<(), String>,
) -> Option<Outcome> {
    let record = |outcome: &Outcome| {
        if let Some(path) = &plan.result {
            write_outcome(path, outcome);
        }
    };
    if let Err(message) = wait() {
        // The application is still running: leave everything as it is.
        let outcome = Outcome { version: plan.version.clone(), installed: false, message, manual_command: None };
        record(&outcome);
        return Some(outcome);
    }
    let mut outcome = match install() {
        Ok(()) => Outcome {
            version: plan.version.clone(),
            installed: true,
            message: format!("SecurePlan CAD was updated to {}.", plan.version),
            manual_command: None,
        },
        Err(failed) => Outcome { version: plan.version.clone(), installed: false, message: failed.message, manual_command: failed.manual_command },
    };
    let keep = outcome.manual_command.is_some();
    // The download is no longer needed, unless the user is told to install
    // it themselves.
    if !keep {
        let _ = std::fs::remove_file(match &plan.install {
            Install::Msi { msi, .. } => msi,
            Install::Dmg { dmg, .. } => dmg,
            Install::Deb { deb, .. } => deb,
        });
    }
    record(&outcome);
    if relaunch().is_err() {
        outcome.message.push_str(" SecurePlan CAD could not reopen by itself; open it again.");
        record(&outcome);
    }
    // On Windows the helper runs from the staging folder and cannot delete
    // itself; the next start sweeps what is left (a kept download included,
    // once it is a day old).
    if keep {
        let _ = std::fs::remove_file(plan.staging.join(PLAN_FILE));
    } else {
        let _ = std::fs::remove_dir_all(&plan.staging);
    }
    Some(outcome)
}

/// Wait until the application has closed its end of the pipe.
fn wait_for_stdin_eof(limit: Duration) -> Result<(), String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut sink = [0u8; 64];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
        let _ = sender.send(());
    });
    receiver
        .recv_timeout(limit)
        .map_err(|_| "SecurePlan CAD did not close, so the update was not installed.".to_string())
}

fn install(plan: &Plan) -> Result<(), NotInstalled> {
    match &plan.install {
        Install::Msi { msi, .. } => Ok(install_msi(msi)?),
        Install::Dmg { dmg, app } => Ok(install_dmg(dmg, app, &plan.version, &plan.staging)?),
        Install::Deb { deb, .. } => install_deb(&pkexec_path(), deb),
    }
}

fn relaunch(install: &Install) -> Result<(), String> {
    let mut command = match install {
        Install::Msi { exe, .. } | Install::Deb { exe, .. } => std::process::Command::new(exe),
        Install::Dmg { app, .. } => {
            // Launch Services starts a new copy (the old one has exited).
            let mut open = std::process::Command::new("/usr/bin/open");
            open.arg("-n").arg(app);
            open
        }
    };
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    detach(&mut command);
    let mut child = command.spawn().map_err(|_| "could not start".to_string())?;
    if matches!(install, Install::Dmg { .. }) {
        // `open` returns once Launch Services has started the app.
        let status = child.wait().map_err(|_| "could not start".to_string())?;
        if !status.success() {
            return Err("could not start".into());
        }
    }
    Ok(())
}

/// Run `program` and fail with `message` unless it succeeds.
fn run_tool(program: &str, args: &[&std::ffi::OsStr], message: &str) -> Result<(), String> {
    let status = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|_| message.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

fn install_msi(msi: &Path) -> Result<(), String> {
    if !msi.is_file() {
        return Err("The downloaded installer is missing, so the update was not installed.".into());
    }
    let system_root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let status = std::process::Command::new(system_root.join("System32").join("msiexec.exe"))
        .arg("/i")
        .arg(msi)
        .args(["/passive", "/norestart"])
        .status()
        .map_err(|_| "Windows Installer could not start, so the update was not installed.".to_string())?;
    match status.code() {
        Some(0) | Some(3010) => Ok(()),
        Some(1602) => Err("The installation was cancelled; SecurePlan CAD is unchanged.".into()),
        Some(1618) => Err("Another installation was in progress; SecurePlan CAD is unchanged. Try the update again later.".into()),
        Some(code) => Err(format!("Windows Installer stopped with error {code}; SecurePlan CAD is unchanged.")),
        None => Err("Windows Installer stopped; SecurePlan CAD is unchanged.".into()),
    }
}

/// Install the verified package with the system installer, which asks for
/// the user's password through polkit.
fn install_deb(pkexec: &Path, deb: &Path) -> Result<(), NotInstalled> {
    if !deb.is_file() {
        return Err("The downloaded package is missing, so the update was not installed.".into());
    }
    let (program, args) = deb_install_command(pkexec, deb);
    let status = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    deb_install_result(status.ok().map(|status| status.code()), deb)
}

/// What the system installer's exit means. `None`: `pkexec` could not start;
/// `Some(None)`: it ended by a signal. pkexec exits 126 when the user
/// dismissed the password dialog and 127 when it could not get permission
/// (no authentication agent, or the password was refused); otherwise the
/// code is apt-get's (0, or 100 for an error).
pub fn deb_install_result(code: Option<Option<i32>>, deb: &Path) -> Result<(), NotInstalled> {
    let kept = |reason: String| {
        let mut lines = vec![reason];
        lines.extend(manual_install_lines(deb));
        Err(NotInstalled { message: lines.join(" "), manual_command: Some(manual_install_command(deb)) })
    };
    match code {
        Some(Some(0)) => Ok(()),
        Some(Some(126)) => Err("The installation was cancelled; SecurePlan CAD is unchanged.".into()),
        None => kept("pkexec, which asks for your password to install packages, could not start, so the update was not installed.".into()),
        Some(Some(127)) => kept("The system installer did not get permission to install the update (no password dialog was available, or the password was not accepted), so the update was not installed.".into()),
        Some(Some(code)) => kept(format!("The system installer (apt-get) stopped with error {code}, so the update was not installed.")),
        Some(None) => kept("The system installer stopped, so the update was not installed.".into()),
    }
}

/// Mount `dmg`, copy its app next to `app`, check it, and swap it in.
fn install_dmg(dmg: &Path, app: &Path, version: &str, staging: &Path) -> Result<(), String> {
    let mount = staging.join("mount");
    std::fs::create_dir_all(&mount).map_err(|_| "The update could not be prepared.".to_string())?;
    let attach: [&std::ffi::OsStr; 7] = ["attach".as_ref(), "-nobrowse".as_ref(), "-readonly".as_ref(), "-noautoopen".as_ref(), "-mountpoint".as_ref(), mount.as_os_str(), dmg.as_os_str()];
    run_tool("/usr/bin/hdiutil", &attach, "The downloaded disk image could not be opened, so the update was not installed.")?;
    let copied = copy_from_mount(&mount, app, version);
    let detach: [&std::ffi::OsStr; 2] = ["detach".as_ref(), mount.as_os_str()];
    if run_tool("/usr/bin/hdiutil", &detach, "").is_err() {
        let forced: [&std::ffi::OsStr; 3] = ["detach".as_ref(), "-force".as_ref(), mount.as_os_str()];
        let _ = run_tool("/usr/bin/hdiutil", &forced, "");
    }
    let new = copied?;
    swap_in(&new, app).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&new);
    })
}

/// Copy the mounted image's app to a hidden sibling of `app` (the same
/// volume, so the swap is a rename) and check it.
fn copy_from_mount(mount: &Path, app: &Path, version: &str) -> Result<PathBuf, String> {
    let source = mount.join(DMG_APP);
    let info = source.join("Contents/Info.plist");
    let key = |name: &str| -> Option<String> {
        let output = std::process::Command::new("/usr/libexec/PlistBuddy").arg("-c").arg(format!("Print :{name}")).arg(&info).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    if key("CFBundleIdentifier").as_deref() != Some(BUNDLE_ID) || key("CFBundleShortVersionString").as_deref() != Some(version) {
        return Err(format!("The downloaded disk image does not hold SecurePlan CAD {version}, so the update was not installed."));
    }
    let new = sibling(app, "new")?;
    let _ = std::fs::remove_dir_all(&new);
    let copy: [&std::ffi::OsStr; 2] = [source.as_os_str(), new.as_os_str()];
    let copied = run_tool("/usr/bin/ditto", &copy, "The new version could not be copied, so the update was not installed.").and_then(|()| {
        // `ditto` keeps the ad-hoc signature; make sure it is intact.
        let verify: [&std::ffi::OsStr; 4] = ["--verify".as_ref(), "--deep".as_ref(), "--strict".as_ref(), new.as_os_str()];
        run_tool("/usr/bin/codesign", &verify, "The new version's signature did not check out, so the update was not installed.")
    });
    match copied {
        Ok(()) => Ok(new),
        Err(message) => {
            let _ = std::fs::remove_dir_all(&new);
            Err(message)
        }
    }
}

/// A hidden sibling of `app`, such as `.SecurePlan CAD.app.new-<pid>`.
fn sibling(app: &Path, kind: &str) -> Result<PathBuf, String> {
    let name = app.file_name().ok_or_else(|| "The installed app could not be found.".to_string())?;
    let folder = app.parent().ok_or_else(|| "The installed app could not be found.".to_string())?;
    Ok(folder.join(format!(".{}.{kind}-{}", name.to_string_lossy(), std::process::id())))
}

/// Replace `app` with `new` by two renames in the same folder; undo the first
/// if the second fails. The old copy is removed afterwards.
pub fn swap_in(new: &Path, app: &Path) -> Result<(), String> {
    let old = sibling(app, "old")?;
    std::fs::rename(app, &old).map_err(|_| "The installed app could not be replaced, so the update was not installed.".to_string())?;
    if std::fs::rename(new, app).is_err() {
        let _ = std::fs::rename(&old, app);
        return Err("The new version could not be moved into place, so the update was not installed.".into());
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(windows)]
mod win_process {
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

    /// A handle to the application process, to wait for it to end.
    pub struct Process(HANDLE);

    impl Process {
        /// `None` when the process has already ended.
        pub fn open(pid: u32) -> Option<Self> {
            // SAFETY: plain Win32 call; a null handle is checked below.
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            (!handle.is_null()).then_some(Self(handle))
        }

        pub fn wait(&self, limit: Duration) -> Result<(), String> {
            // SAFETY: the handle is valid until `drop`.
            let waited = unsafe { WaitForSingleObject(self.0, limit.as_millis().min(u32::MAX as u128) as u32) };
            if waited == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err("SecurePlan CAD did not close, so the update was not installed.".into())
            }
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenProcess and is closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("secureplan_helper_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn plan(dir: &Path) -> Plan {
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let msi = staging.join("SecurePlanCAD-windows-x64.msi");
        std::fs::write(&msi, b"msi").unwrap();
        Plan {
            version: "0.2.0".into(),
            parent_pid: 1,
            install: Install::Msi { msi, exe: dir.join("SecurePlanCAD.exe") },
            result: Some(dir.join("config/update-result.json")),
            staging,
        }
    }

    #[test]
    fn nothing_is_installed_or_relaunched_while_the_application_runs() {
        let dir = temp("running");
        let plan = plan(&dir);
        let (installed, relaunched) = (Cell::new(false), Cell::new(false));
        let outcome = run_plan(
            &plan,
            || Err("SecurePlan CAD did not close, so the update was not installed.".into()),
            || {
                installed.set(true);
                Ok(())
            },
            || {
                relaunched.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(!installed.get() && !relaunched.get());
        assert!(!outcome.installed);
        assert!(plan.staging.join("SecurePlanCAD-windows-x64.msi").is_file(), "the download was removed");
        assert_eq!(take_outcome(plan.result.as_ref().unwrap()), Some(outcome));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn after_exit_the_helper_installs_relaunches_and_reports() {
        let dir = temp("installs");
        let plan = plan(&dir);
        let order = std::cell::RefCell::new(Vec::new());
        let outcome = run_plan(
            &plan,
            || {
                order.borrow_mut().push("exited");
                Ok(())
            },
            || {
                order.borrow_mut().push("installed");
                Ok(())
            },
            || {
                order.borrow_mut().push("relaunched");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*order.borrow(), ["exited", "installed", "relaunched"]);
        assert!(outcome.installed);
        assert!(!plan.staging.exists(), "the staging folder stayed");
        let result = plan.result.clone().unwrap();
        assert_eq!(take_outcome(&result).map(|o| o.message), Some("SecurePlan CAD was updated to 0.2.0.".into()));
        assert_eq!(take_outcome(&result), None, "the outcome is shown once");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_failed_install_still_reopens_the_old_copy_and_explains() {
        let dir = temp("fails");
        let plan = plan(&dir);
        let relaunched = Cell::new(false);
        let outcome = run_plan(
            &plan,
            || Ok(()),
            || Err("Windows Installer stopped with error 1603; SecurePlan CAD is unchanged.".into()),
            || {
                relaunched.set(true);
                Err("could not start".into())
            },
        )
        .unwrap();
        assert!(relaunched.get());
        assert!(!outcome.installed);
        assert!(outcome.message.contains("1603") && outcome.message.contains("open it again"), "{}", outcome.message);
        assert_eq!(take_outcome(plan.result.as_ref().unwrap()), Some(outcome));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn plans_round_trip_as_json() {
        let dir = temp("json");
        let plan = plan(&dir);
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("\"kind\":\"msi\"") && json.contains("\"parentPid\":1"), "{json}");
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
        std::fs::remove_dir_all(dir).ok();
    }

    fn fake_app(dir: &Path, name: &str) -> PathBuf {
        let app = dir.join(name);
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), b"<plist/>").unwrap();
        std::fs::write(app.join("Contents/MacOS/OpenCADStudio-App"), b"").unwrap();
        app
    }

    #[test]
    fn a_mac_update_needs_an_app_bundle_outside_translocation_in_a_writable_folder() {
        let dir = temp("mac");
        let app = fake_app(&dir.join("Applications"), "SecurePlan CAD.app");
        let exe = app.join("Contents/MacOS/OpenCADStudio-App");
        assert_eq!(mac_target(&exe), Ok(app.clone()));
        assert!(mac_target(&dir.join("target/debug/OpenCADStudio")).unwrap_err().contains("not an installed app"));
        let translocated = fake_app(&dir.join("private/var/folders/x/AppTranslocation/ABC/d"), "SecurePlan CAD.app");
        let error = mac_target(&translocated.join("Contents/MacOS/OpenCADStudio-App")).unwrap_err();
        assert!(error.contains("App Translocation") && error.contains("Applications folder"), "{error}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.join("Locked");
            let app = fake_app(&locked, "SecurePlan CAD.app");
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
            let probe_works = std::fs::create_dir(locked.join("root-check")).is_ok();
            if !probe_works {
                assert!(mac_target(&app.join("Contents/MacOS/OpenCADStudio-App")).unwrap_err().contains("not writable"));
            }
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_windows_update_needs_the_installed_per_user_copy() {
        let local = Path::new("C:/Users/u/AppData/Local");
        let installed = local.join("Programs/SecurePlan CAD/SecurePlanCAD.exe");
        assert_eq!(windows_target(&installed, Some(local)), Ok(installed.clone()));
        let other_case = PathBuf::from(installed.to_string_lossy().to_uppercase());
        assert!(windows_target(&other_case, Some(local)).is_ok(), "paths compare case-insensitively");
        assert!(windows_target(Path::new("C:/Downloads/OpenCADStudio.exe"), Some(local)).unwrap_err().contains("installer"));
        assert!(windows_target(&installed, None).is_err());
    }

    #[test]
    fn a_failed_swap_restores_the_installed_app() {
        let dir = temp("swap");
        let app = fake_app(&dir, "SecurePlan CAD.app");
        std::fs::write(app.join("old-marker"), b"old").unwrap();
        // A new copy that does not exist: the second rename fails.
        let error = swap_in(&dir.join(".missing.new"), &app).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        assert!(app.join("old-marker").is_file(), "the installed app was not restored");
        // A real swap replaces it and leaves no hidden copies.
        let new = fake_app(&dir, ".SecurePlan CAD.app.new-1");
        std::fs::write(new.join("new-marker"), b"new").unwrap();
        swap_in(&new, &app).unwrap();
        assert!(app.join("new-marker").is_file() && !app.join("old-marker").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    fn deb_plan(dir: &Path) -> Plan {
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = staging.join("SecurePlanCAD-linux-x64.deb");
        std::fs::write(&deb, b"deb").unwrap();
        std::fs::write(staging.join(PLAN_FILE), b"{}").unwrap();
        Plan {
            version: "0.2.3".into(),
            parent_pid: 1,
            install: Install::Deb { deb, exe: PathBuf::from(LINUX_EXE) },
            result: Some(dir.join("config/update-result.json")),
            staging,
        }
    }

    #[test]
    fn a_linux_update_needs_the_packaged_program() {
        assert_eq!(linux_target(Path::new(LINUX_EXE)), Ok(PathBuf::from("/usr/bin/secureplan-cad")));
        for other in ["/home/u/OpenCADStudio/target/release/OpenCADStudio", "/usr/local/bin/secureplan-cad", "/usr/bin/secureplan-cad (deleted)"] {
            assert!(linux_target(Path::new(other)).unwrap_err().contains("SecurePlanCAD-linux-x64.deb"), "{other}");
        }
    }

    /// The installer runs as an argument list: a path with spaces, quotes or
    /// shell syntax stays one argument and nothing is interpreted.
    #[test]
    fn the_linux_installer_command_is_an_argument_list() {
        let deb = Path::new("/tmp/odd $(touch x); 'dir'/SecurePlanCAD-linux-x64.deb");
        let (program, args) = deb_install_command(Path::new(PKEXEC), deb);
        assert_eq!(program, PathBuf::from("/usr/bin/pkexec"));
        let args: Vec<&std::ffi::OsStr> = args.iter().map(|arg| arg.as_os_str()).collect();
        let expected: [&std::ffi::OsStr; 7] = [
            "/usr/bin/apt-get".as_ref(),
            "install".as_ref(),
            "-y".as_ref(),
            "--no-remove".as_ref(),
            "-o".as_ref(),
            "DPkg::Lock::Timeout=120".as_ref(),
            deb.as_os_str(),
        ];
        assert_eq!(args, expected);
        // The command shown to the user is quoted for a shell.
        assert_eq!(manual_install_command(deb), r#"sudo apt install '/tmp/odd $(touch x); '\''dir'\''/SecurePlanCAD-linux-x64.deb'"#);
        assert_eq!(
            manual_install_command(Path::new("/tmp/secureplan-cad-update-7-9/SecurePlanCAD-linux-x64.deb")),
            "sudo apt install /tmp/secureplan-cad-update-7-9/SecurePlanCAD-linux-x64.deb"
        );
    }

    /// pkexec's and apt-get's exits: installed; cancelled (download removed);
    /// no permission, no pkexec or an installer error (download kept, with
    /// its location and the command).
    #[test]
    fn the_linux_installer_outcome_keeps_the_package_when_the_user_must_install_it() {
        let deb = Path::new("/tmp/secureplan-cad-update-7-9/SecurePlanCAD-linux-x64.deb");
        assert_eq!(deb_install_result(Some(Some(0)), deb), Ok(()));
        let cancelled = deb_install_result(Some(Some(126)), deb).unwrap_err();
        assert!(cancelled.message.contains("cancelled") && cancelled.manual_command.is_none(), "{cancelled:?}");
        for (code, reason) in [
            (Some(Some(127)), "did not get permission"),
            (None, "pkexec"),
            (Some(Some(100)), "error 100"),
            (Some(None), "stopped"),
        ] {
            let failed = deb_install_result(code, deb).unwrap_err();
            assert!(failed.message.contains(reason), "{code:?}: {}", failed.message);
            assert!(failed.message.contains(&format!("kept at: {}", deb.display())), "{}", failed.message);
            assert_eq!(failed.manual_command.as_deref(), Some("sudo apt install /tmp/secureplan-cad-update-7-9/SecurePlanCAD-linux-x64.deb"));
        }
    }

    #[test]
    fn a_kept_linux_package_stays_for_the_user_and_the_old_copy_reopens() {
        let dir = temp("deb-kept");
        let plan = deb_plan(&dir);
        let Install::Deb { deb, .. } = plan.install.clone() else { unreachable!() };
        let relaunched = Cell::new(false);
        let outcome = run_plan(
            &plan,
            || Ok(()),
            || deb_install_result(Some(Some(127)), &deb),
            || {
                relaunched.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(relaunched.get() && !outcome.installed);
        assert!(deb.is_file(), "the verified package was removed");
        assert!(!plan.staging.join(PLAN_FILE).exists(), "the plan stayed");
        assert_eq!(outcome.manual_command, Some(manual_install_command(&deb)));
        assert_eq!(take_outcome(plan.result.as_ref().unwrap()), Some(outcome));
        // A successful or cancelled install removes it as on other systems.
        for result in [Ok(()), deb_install_result(Some(Some(126)), &deb)] {
            let plan = deb_plan(&dir);
            run_plan(&plan, || Ok(()), || result.clone(), || Ok(())).unwrap();
            assert!(!plan.staging.exists(), "the download stayed after {result:?}");
        }
        std::fs::remove_dir_all(dir).ok();
    }

    /// Without pkexec the app does not quit: it shows the kept package and
    /// the command. Other systems and a present pkexec install as usual.
    #[test]
    fn without_pkexec_the_user_is_told_how_to_install() {
        let dir = temp("no-pkexec");
        let file = dir.join("SecurePlanCAD-linux-x64.deb");
        std::fs::write(&file, b"deb").unwrap();
        let release = super::super::update::Release {
            version: "0.2.3".into(),
            tag: "secureplan-cad-v0.2.3".into(),
            notes: String::new(),
            published: 0,
            asset: super::super::update::Asset { name: "SecurePlanCAD-linux-x64.deb".into(), size: 3 },
        };
        let staged = Staged { release, dir: dir.clone(), file: file.clone() };
        let linux = Target::Linux { exe: PathBuf::from(LINUX_EXE) };
        let (lines, command) = manual_install(&linux, &staged, &dir.join("missing-pkexec")).expect("no pkexec");
        assert!(lines[0].contains("pkexec") && lines.iter().any(|line| line.contains(&file.display().to_string())), "{lines:?}");
        assert_eq!(command, manual_install_command(&file));
        let present = dir.join("pkexec");
        std::fs::write(&present, b"").unwrap();
        assert_eq!(manual_install(&linux, &staged, &present), None);
        assert_eq!(manual_install(&Target::Mac { app: dir.join("SecurePlan CAD.app") }, &staged, &dir.join("missing-pkexec")), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn packaging_matches_the_helper() {
        let plist = include_str!("../../packaging/Info.plist");
        assert!(plist.contains(&format!("<string>{BUNDLE_ID}</string>")));
        let wxs = include_str!("../../packaging/windows/main.wxs");
        assert!(wxs.contains(&format!("Name='{WINDOWS_EXE}'")) && wxs.contains(&format!("Name='{}'", WINDOWS_INSTALL_DIR[1])));
        assert!(wxs.contains("<Directory Id='LocalAppDataFolder'>") && wxs.contains(&format!("Name='{}'", WINDOWS_INSTALL_DIR[0])));
        let dmg = include_str!("../../packaging/secureplan/make-release-dmg.sh");
        assert!(dmg.contains(&format!("app=\"$work/root/{DMG_APP}\"")), "the release DMG must hold {DMG_APP}");
        assert!(dmg.contains(&format!("= \"{BUNDLE_ID}\" ]")), "the release DMG must check the bundle id");
        let deb = include_str!("../../packaging/secureplan/make-release-deb.sh");
        assert!(deb.contains(&format!("program=\"{LINUX_EXE}\"")), "the package must install {LINUX_EXE}");
        assert!(deb.contains(&format!("desktop_id=\"{BUNDLE_ID}\"")), "the package's .desktop file must be {BUNDLE_ID}.desktop");
        let desktop = include_str!("../../packaging/secureplan/linux/in.vizmo.secureplan.cad.desktop");
        assert!(desktop.lines().any(|line| line == format!("Exec={LINUX_EXE} %u")), "{desktop}");
        assert!(desktop.lines().any(|line| line == format!("StartupWMClass={BUNDLE_ID}")), "{desktop}");
        assert!(desktop.lines().any(|line| line == "MimeType=x-scheme-handler/secureplan-cad;"), "{desktop}");
    }
}
