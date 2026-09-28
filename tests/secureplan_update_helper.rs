//! The SecurePlan CAD update helper through the production launch path
//! (DSK-07). A parent process (the SecurePlan CAD test build in its
//! `secureplan-test` parent mode) plays the application: it finds its install
//! with the production `target()` (or is told it), calls the production
//! `prepare()` and `launch()`, then stays alive until the test lets it exit.
//! The helper must change nothing while the parent runs, then install,
//! record the outcome and relaunch.
//!
//! - Linux: with a Windows plan the helper waits for the parent, then its
//!   install fails; the outcome is recorded only after the exit. With the
//!   package plan it runs a stand-in `pkexec` (`SECUREPLAN_TEST_PKEXEC`, never
//!   the system's) with the apt-get arguments after the exit and relaunches
//!   the program; when the stand-in is refused, the verified package is kept
//!   and the outcome says how to install it.
//! - macOS: a synthetic installed app and DMG (a tiny C program, ad-hoc
//!   signed); the new app is swapped in after the parent exits and relaunched
//!   through Launch Services; a damaged image leaves the app and reopens it.
//! - Windows: SecurePlan CI builds two per-user MSIs of this test build
//!   (0.1.0 and 0.1.1, whose executable differs by one byte). 0.1.0 is
//!   installed first and the parent is that installed copy, so `target()` is
//!   the production check and the upgrade has to wait for it to exit. The
//!   installed copy is relaunched and records itself; the scheme is checked,
//!   then everything is uninstalled.
#![cfg(feature = "secureplan-test")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use OpenCADStudio::app::secureplan::update_helper::{take_outcome, Outcome, Target, TestParent, TEST_PARENT_ENV};

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("secureplan-helper-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Poll until `check` holds.
fn wait_until(limit: Duration, what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// A running parent ("the application") and the files that steer it.
struct Parent {
    child: Child,
    spec: TestParent,
}

impl Parent {
    /// Start `exe` as the parent for a download at `file` in `staging`.
    fn start(exe: &Path, dir: &Path, version: &str, file: PathBuf, target: Option<Target>, env: &[(&str, &Path)]) -> Self {
        let spec = TestParent {
            version: version.into(),
            staging: file.parent().unwrap().to_path_buf(),
            file,
            result: dir.join("update-result.json"),
            target,
            started: dir.join("parent-started"),
            exit_when: dir.join("parent-may-exit"),
        };
        let spec_path = dir.join("parent.json");
        std::fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();
        let mut command = Command::new(exe);
        command.env(TEST_PARENT_ENV, &spec_path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for (name, value) in env {
            command.env(name, value);
        }
        let child = command.spawn().expect("start the parent");
        let parent = Self { child, spec };
        wait_until(Duration::from_secs(120), "the parent to launch the helper", || parent.spec.started.exists());
        let started = std::fs::read_to_string(&parent.spec.started).unwrap();
        assert!(!started.starts_with("refused"), "the production launch refused: {started}");
        parent
    }

    fn outcome_written(&self) -> bool {
        self.spec.result.exists()
    }

    /// Let the parent exit; returns once it has.
    fn exit(&mut self) {
        std::fs::write(&self.spec.exit_when, b"").unwrap();
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the parent failed: {status:?}");
    }

    fn outcome(&self, limit: Duration) -> Outcome {
        wait_until(limit, "the helper's outcome", || self.outcome_written());
        // Written in one go, but give a partial write a moment.
        std::thread::sleep(Duration::from_millis(300));
        take_outcome(&self.spec.result).expect("a readable outcome")
    }
}

/// Wait until `marker` holds a line satisfying `wanted`.
#[cfg(any(target_os = "macos", windows))]
fn wait_for_line(marker: &Path, limit: Duration, wanted: impl Fn(&str) -> bool) -> Option<String> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(line) = std::fs::read_to_string(marker).ok().and_then(|text| text.lines().find(|line| wanted(line)).map(str::to_string)) {
            return Some(line);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

#[cfg(target_os = "linux")]
#[test]
fn the_helper_from_the_production_launch_acts_only_after_the_parent_exits() {
    let dir = temp("linux");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let msi = staging.join("SecurePlanCAD-windows-x64.msi");
    std::fs::write(&msi, b"not really an installer").unwrap();
    // A Windows plan: on Linux its installer cannot start, and the relaunch
    // target does not exist, so nothing else is run.
    let target = Target::Windows { exe: dir.join("missing").join("SecurePlanCAD.exe") };
    let mut parent = Parent::start(Path::new(env!("CARGO_BIN_EXE_OpenCADStudio")), &dir, "0.2.0", msi.clone(), Some(target), &[]);
    // The production prepare() wrote the plan and the helper copy.
    assert!(staging.join("update-plan.json").is_file() && staging.join("SecurePlanCAD-update-helper.exe").is_file());
    std::thread::sleep(Duration::from_secs(2));
    assert!(!parent.outcome_written(), "the helper acted while the application ran");
    assert!(msi.is_file(), "the download was touched while the application ran");
    parent.exit();
    let outcome = parent.outcome(Duration::from_secs(60));
    assert!(!outcome.installed);
    assert!(outcome.message.contains("Windows Installer could not start"), "{outcome:?}");
    wait_until(Duration::from_secs(30), "the staging folder to go", || !staging.exists());
    std::fs::remove_dir_all(&dir).ok();
}

/// DSK-07 on Linux: nothing runs while the application does; then the
/// system installer (a stand-in for pkexec) gets exactly the apt-get argument
/// list, and the program relaunches. A refused installer keeps the package.
#[cfg(target_os = "linux")]
#[test]
fn the_linux_helper_runs_the_system_installer_after_the_parent_exits() {
    use std::os::unix::fs::PermissionsExt;
    for (case, exit_code) in [("installed", 0), ("refused", 127)] {
        let dir = temp(&format!("linux-deb-{case}"));
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let deb = staging.join("SecurePlanCAD-linux-x64.deb");
        std::fs::write(&deb, b"not really a package").unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let argv = dir.join("pkexec-argv");
        let pkexec = script("pkexec", &format!("printf '%s\\n' \"$@\" > '{}'\nexit {exit_code}", argv.display()));
        let relaunched = dir.join("relaunched");
        let program = script("secureplan-cad", &format!("touch '{}'", relaunched.display()));
        let target = Target::Linux { exe: program.clone() };
        let mut parent = Parent::start(
            Path::new(env!("CARGO_BIN_EXE_OpenCADStudio")),
            &dir,
            "0.2.3",
            deb.clone(),
            Some(target),
            &[("SECUREPLAN_TEST_PKEXEC", &pkexec)],
        );
        assert!(staging.join("update-plan.json").is_file());
        std::thread::sleep(Duration::from_secs(2));
        assert!(!argv.exists() && !parent.outcome_written(), "{case}: the helper acted while the application ran");
        parent.exit();
        let outcome = parent.outcome(Duration::from_secs(60));
        wait_until(Duration::from_secs(30), "the relaunch", || relaunched.exists());
        let expected = ["/usr/bin/apt-get", "install", "-y", "--no-remove", "-o", "DPkg::Lock::Timeout=120", &deb.display().to_string()].join("\n");
        assert_eq!(std::fs::read_to_string(&argv).unwrap().trim_end(), expected, "{case}");
        if exit_code == 0 {
            assert!(outcome.installed, "{outcome:?}");
            wait_until(Duration::from_secs(30), "the staging folder to go", || !staging.exists());
        } else {
            assert!(!outcome.installed && outcome.message.contains("did not get permission"), "{outcome:?}");
            assert_eq!(outcome.manual_command, Some(format!("sudo apt install {}", deb.display())));
            std::thread::sleep(Duration::from_secs(1));
            assert!(deb.is_file(), "the verified package was removed");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use OpenCADStudio::app::secureplan::update_helper::{BUNDLE_ID, DMG_APP};

    /// A signed app bundle whose executable appends `which` to `marker`.
    fn build_app(folder: &Path, version: &str, marker: &Path, which: &str) -> PathBuf {
        build_app_as(folder, BUNDLE_ID, version, marker, which)
    }

    fn build_app_as(folder: &Path, bundle_id: &str, version: &str, marker: &Path, which: &str) -> PathBuf {
        let app = folder.join(DMG_APP);
        let macos = app.join("Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>fake</string>
<key>CFBundleIdentifier</key><string>{bundle_id}</string>
<key>CFBundleName</key><string>SecurePlan CAD</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
        let source = folder.join(format!("fake-{which}.c"));
        std::fs::write(
            &source,
            format!(
                "#include <stdio.h>\nint main(void) {{ FILE *f = fopen(\"{}\", \"a\"); if (f) {{ fputs(\"{which}\\n\", f); fclose(f); }} return 0; }}\n",
                marker.display()
            ),
        )
        .unwrap();
        assert!(Command::new("cc").arg("-o").arg(macos.join("fake")).arg(&source).status().unwrap().success(), "cc");
        std::fs::remove_file(&source).unwrap();
        assert!(Command::new("codesign").args(["--force", "--deep", "-s", "-"]).arg(&app).status().unwrap().success(), "codesign");
        app
    }

    fn make_dmg(source: &Path, dmg: &Path) {
        for _ in 0..5 {
            let status = Command::new("hdiutil")
                .args(["create", "-volname", "SecurePlan CAD", "-srcfolder"])
                .arg(source)
                .args(["-ov", "-format", "UDZO"])
                .arg(dmg)
                .status()
                .unwrap();
            if status.success() {
                return;
            }
            std::thread::sleep(Duration::from_secs(5));
        }
        panic!("hdiutil create failed");
    }

    fn version_of(app: &Path) -> String {
        let output = Command::new("/usr/libexec/PlistBuddy").args(["-c", "Print :CFBundleShortVersionString"]).arg(app.join("Contents/Info.plist")).output().unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    struct Setup {
        dir: PathBuf,
        app: PathBuf,
        marker: PathBuf,
        dmg: PathBuf,
    }

    /// What the disk image holds.
    enum Image {
        Good,
        Unmountable,
        OtherBundleId,
        OtherVersion,
        BrokenSignature,
    }

    fn setup(tag: &str, damaged: bool) -> Setup {
        setup_with(tag, if damaged { Image::Unmountable } else { Image::Good })
    }

    fn setup_with(tag: &str, image: Image) -> Setup {
        let dir = temp(tag);
        let marker = dir.join("launches.txt");
        let applications = dir.join("Applications");
        std::fs::create_dir_all(&applications).unwrap();
        let app = build_app(&applications, "0.1.0", &marker, "old");
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let dmg = staging.join("SecurePlanCAD-macos-arm64.dmg");
        let source = dir.join("dmg-root");
        std::fs::create_dir_all(&source).unwrap();
        match image {
            Image::Unmountable => std::fs::write(&dmg, b"not a disk image").unwrap(),
            Image::Good => {
                build_app(&source, "0.2.0", &marker, "new");
            }
            Image::OtherBundleId => {
                build_app_as(&source, "com.example.other", "0.2.0", &marker, "new");
            }
            Image::OtherVersion => {
                build_app(&source, "0.3.0", &marker, "new");
            }
            Image::BrokenSignature => {
                // Signed, then changed: the signature no longer matches.
                let app = build_app(&source, "0.2.0", &marker, "new");
                let executable = app.join("Contents/MacOS/fake");
                let mut bytes = std::fs::read(&executable).unwrap();
                let inside = bytes.len() / 3; // within the hashed pages, before the signature
                bytes[inside] ^= 0xff;
                std::fs::write(&executable, bytes).unwrap();
                let verify = Command::new("codesign").args(["--verify", "--deep", "--strict"]).arg(&app).status().unwrap();
                assert!(!verify.success(), "the tampered app still verifies");
            }
        }
        if !matches!(image, Image::Unmountable) {
            make_dmg(&source, &dmg);
        }
        Setup { dir, app, marker, dmg }
    }

    fn start(setup: &Setup) -> Parent {
        let target = Target::Mac { app: setup.app.clone() };
        Parent::start(Path::new(env!("CARGO_BIN_EXE_OpenCADStudio")), &setup.dir, "0.2.0", setup.dmg.clone(), Some(target), &[])
    }

    #[test]
    fn the_new_app_is_swapped_in_after_the_parent_exits_and_relaunched() {
        let setup = setup("mac-ok", false);
        let mut parent = start(&setup);
        // The application is still running: nothing changes.
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(version_of(&setup.app), "0.1.0");
        assert!(!setup.marker.exists() && !parent.outcome_written(), "the helper acted before the application exited");
        parent.exit();
        let outcome = parent.outcome(Duration::from_secs(180));
        assert!(outcome.installed, "{outcome:?}");
        assert_eq!(version_of(&setup.app), "0.2.0");
        let leftovers: Vec<_> = std::fs::read_dir(setup.app.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "hidden copies were left: {leftovers:?}");
        assert!(wait_for_line(&setup.marker, Duration::from_secs(60), |line| line == "new").is_some(), "the new app was not relaunched");
        let launches = std::fs::read_to_string(&setup.marker).unwrap();
        assert!(!launches.contains("old"), "the old app ran: {launches}");
        std::fs::remove_dir_all(&setup.dir).ok();
    }

    /// Images that mount but hold the wrong app are refused before the swap:
    /// the installed app stays, is reopened, and nothing is left beside it.
    #[test]
    fn a_wrong_app_in_the_image_is_refused_and_the_app_kept() {
        for (tag, image, reason) in [
            ("mac-bundle-id", Image::OtherBundleId, "does not hold SecurePlan CAD 0.2.0"),
            ("mac-version", Image::OtherVersion, "does not hold SecurePlan CAD 0.2.0"),
            ("mac-signature", Image::BrokenSignature, "signature did not check out"),
        ] {
            let setup = setup_with(tag, image);
            let mut parent = start(&setup);
            parent.exit();
            let outcome = parent.outcome(Duration::from_secs(180));
            assert!(!outcome.installed && outcome.message.contains(reason), "{tag}: {outcome:?}");
            assert_eq!(version_of(&setup.app), "0.1.0", "{tag}: the installed app changed");
            let leftovers: Vec<_> = std::fs::read_dir(setup.app.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
            assert_eq!(leftovers.len(), 1, "{tag}: copies were left beside the app: {leftovers:?}");
            assert!(wait_for_line(&setup.marker, Duration::from_secs(60), |line| line == "old").is_some(), "{tag}: the app was not reopened");
            let launches = std::fs::read_to_string(&setup.marker).unwrap();
            assert!(!launches.contains("new"), "{tag}: the refused app ran: {launches}");
            std::fs::remove_dir_all(&setup.dir).ok();
        }
    }

    #[test]
    fn a_damaged_image_leaves_the_app_and_reopens_it() {
        let setup = setup("mac-damaged", true);
        let mut parent = start(&setup);
        parent.exit();
        let outcome = parent.outcome(Duration::from_secs(120));
        assert!(!outcome.installed && outcome.message.contains("disk image"), "{outcome:?}");
        assert_eq!(version_of(&setup.app), "0.1.0", "the installed app changed");
        assert!(wait_for_line(&setup.marker, Duration::from_secs(60), |line| line == "old").is_some(), "the app was not reopened");
        std::fs::remove_dir_all(&setup.dir).ok();
    }
}

#[cfg(windows)]
mod win {
    use super::*;

    fn reg_query(args: &[&str]) -> Option<String> {
        let output = Command::new("reg").arg("query").args(args).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).to_string())
    }

    fn sha256(path: &Path) -> String {
        let output = Command::new("certutil").arg("-hashfile").arg(path).arg("SHA256").output().unwrap();
        assert!(output.status.success(), "certutil failed for {}", path.display());
        String::from_utf8_lossy(&output.stdout).lines().nth(1).unwrap_or_default().replace(' ', "").to_lowercase()
    }

    fn msiexec(args: &[&std::ffi::OsStr]) -> std::process::ExitStatus {
        Command::new("msiexec").args(args).status().unwrap()
    }

    #[test]
    fn an_installed_copy_updates_itself_after_it_exits_and_relaunches() {
        let (Some(old_msi), Some(new_msi), Some(new_exe)) = (
            std::env::var_os("SECUREPLAN_TEST_MSI_OLD").map(PathBuf::from),
            std::env::var_os("SECUREPLAN_TEST_MSI_NEW").map(PathBuf::from),
            std::env::var_os("SECUREPLAN_TEST_EXE_NEW").map(PathBuf::from),
        ) else {
            eprintln!("skipped: SecurePlan CI provides SECUREPLAN_TEST_MSI_OLD, SECUREPLAN_TEST_MSI_NEW and SECUREPLAN_TEST_EXE_NEW");
            return;
        };
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"));
        let exe = local.join("Programs").join("SecurePlan CAD").join("SecurePlanCAD.exe");
        assert!(!exe.exists(), "SecurePlan CAD is already installed on this runner");

        // The existing installation (0.1.0).
        let installed = msiexec(&["/i".as_ref(), old_msi.as_os_str(), "/qn".as_ref(), "/norestart".as_ref()]);
        assert!(installed.success() && exe.is_file(), "the old MSI did not install: {installed:?}");
        let old_hash = sha256(&exe);
        let new_hash = sha256(&new_exe);
        assert_ne!(old_hash, new_hash, "the two builds must differ");

        let dir = temp("windows");
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let staged = staging.join("SecurePlanCAD-windows-x64.msi");
        std::fs::copy(&new_msi, &staged).unwrap();
        let marker = dir.join("launches.txt");

        // The installed copy is the application: the production target()
        // must accept it, and the upgrade has to wait for it.
        let mut parent = Parent::start(&exe, &dir, "0.1.1", staged, None, &[("SECUREPLAN_TEST_RELAUNCH_MARKER", marker.as_path())]);
        assert!(staging.join("SecurePlanCAD-update-helper.exe").is_file(), "the helper does not run from a copy");
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(sha256(&exe), old_hash, "installed while the application ran");
        assert!(!parent.outcome_written() && !marker.exists(), "the helper acted before the application exited");
        let parent_pid = parent.child.id();
        parent.exit();

        let outcome = parent.outcome(Duration::from_secs(600));
        assert!(outcome.installed, "{outcome:?}");
        assert_eq!(sha256(&exe), new_hash, "the installed executable is not the new one");
        let line = wait_for_line(&marker, Duration::from_secs(120), |_| true).expect("the installed copy was not relaunched");
        let (pid, launched) = line.split_once(' ').unwrap();
        assert_ne!(pid.parse::<u32>().unwrap(), parent_pid);
        assert_eq!(launched.to_lowercase(), exe.display().to_string().to_lowercase(), "the relaunch was not the installed copy");
        // The scheme is registered for the current user (DSK-04).
        let command = reg_query(&[r"HKCU\Software\Classes\secureplan-cad\shell\open\command", "/ve"]).expect("scheme command registered");
        assert!(command.to_lowercase().contains(&exe.display().to_string().to_lowercase()), "{command}");
        assert!(reg_query(&[r"HKCU\Software\Classes\secureplan-cad", "/v", "URL Protocol"]).is_some(), "URL Protocol value missing");

        // Uninstalling removes it all again (once the relaunched copy has gone).
        std::thread::sleep(Duration::from_secs(3));
        let removed = msiexec(&["/x".as_ref(), new_msi.as_os_str(), "/qn".as_ref(), "/norestart".as_ref()]);
        assert!(removed.success(), "uninstall failed: {removed:?}");
        assert!(!exe.exists());
        assert!(reg_query(&[r"HKCU\Software\Classes\secureplan-cad"]).is_none(), "the scheme stayed registered after uninstall");
        std::fs::remove_dir_all(&dir).ok();
    }
}
