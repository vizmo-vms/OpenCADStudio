//! The SecurePlan CAD update helper on the real platforms (DSK-07). The test
//! plays the exiting application: it starts the helper with a plan and holds
//! the pipe, checks that nothing is installed while it "runs", then closes the
//! pipe. The helper must then install and relaunch the new copy, which starts
//! only after the old one has gone.
//!
//! - macOS: a synthetic app and DMG (a tiny C program as the executable,
//!   ad-hoc signed); the helper mounts the DMG, `ditto`s the app next to the
//!   installed one, swaps it in by rename and relaunches it through Launch
//!   Services. A damaged image leaves the installed app and reopens it.
//! - Windows: SecurePlan CI builds a per-user MSI of this test build and
//!   names it in `SECUREPLAN_TEST_MSI`; the helper installs it with msiexec
//!   and relaunches the installed copy, which records itself (test builds
//!   only) and exits. The test then checks the scheme registration and
//!   uninstalls.
#![cfg(feature = "secureplan-test")]
#![cfg(any(target_os = "macos", windows))]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use OpenCADStudio::app::secureplan::update_helper::{take_outcome, Install, Plan, HELPER_ARG};

fn temp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("secureplan-helper-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Start the helper as the application would, holding its stdin pipe.
fn start_helper(plan: &Plan, dir: &Path, env: &[(&str, &Path)]) -> Child {
    let plan_path = dir.join("update-plan.json");
    std::fs::write(&plan_path, serde_json::to_vec(plan).unwrap()).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"));
    command.arg(HELPER_ARG).arg(&plan_path).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    command.spawn().expect("start the update helper")
}

fn wait_exit(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the helper did not finish");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait until `marker` holds a line satisfying `wanted`.
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

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use OpenCADStudio::app::secureplan::update_helper::{BUNDLE_ID, DMG_APP};

    /// A signed app bundle whose executable appends `which` to `marker`.
    fn build_app(folder: &Path, version: &str, marker: &Path, which: &str) -> PathBuf {
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
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
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
        plan: Plan,
    }

    fn setup(tag: &str, damaged: bool) -> Setup {
        let dir = temp(tag);
        let marker = dir.join("launches.txt");
        let applications = dir.join("Applications");
        std::fs::create_dir_all(&applications).unwrap();
        let app = build_app(&applications, "0.1.0", &marker, "old");
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let dmg = staging.join("SecurePlanCAD-macos-arm64.dmg");
        if damaged {
            std::fs::write(&dmg, b"not a disk image").unwrap();
        } else {
            let source = dir.join("dmg-root");
            std::fs::create_dir_all(&source).unwrap();
            build_app(&source, "0.2.0", &marker, "new");
            make_dmg(&source, &dmg);
        }
        let plan = Plan {
            version: "0.2.0".into(),
            parent_pid: std::process::id(),
            install: Install::Dmg { dmg, app: app.clone() },
            result: Some(dir.join("update-result.json")),
            staging,
        };
        Setup { dir, app, marker, plan }
    }

    #[test]
    fn the_new_app_is_swapped_in_after_exit_and_relaunched() {
        let setup = setup("mac-ok", false);
        let mut helper = start_helper(&setup.plan, &setup.dir, &[]);
        // The application is still "running": nothing changes.
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(version_of(&setup.app), "0.1.0");
        assert!(!setup.marker.exists(), "something was launched before the application exited");
        drop(helper.stdin.take());
        assert!(wait_exit(&mut helper, Duration::from_secs(180)).success());
        let outcome = take_outcome(setup.plan.result.as_ref().unwrap()).expect("an outcome");
        assert!(outcome.installed, "{outcome:?}");
        assert_eq!(version_of(&setup.app), "0.2.0");
        let leftovers: Vec<_> = std::fs::read_dir(setup.app.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(leftovers.len(), 1, "hidden copies were left: {leftovers:?}");
        assert!(!setup.plan.staging.exists(), "the staging folder was left");
        assert!(wait_for_line(&setup.marker, Duration::from_secs(60), |line| line == "new").is_some(), "the new app was not relaunched");
        let launches = std::fs::read_to_string(&setup.marker).unwrap();
        assert!(!launches.contains("old"), "the old app ran: {launches}");
        std::fs::remove_dir_all(&setup.dir).ok();
    }

    #[test]
    fn a_damaged_image_leaves_the_app_and_reopens_it() {
        let setup = setup("mac-damaged", true);
        let mut helper = start_helper(&setup.plan, &setup.dir, &[]);
        drop(helper.stdin.take());
        assert!(!wait_exit(&mut helper, Duration::from_secs(120)).success());
        let outcome = take_outcome(setup.plan.result.as_ref().unwrap()).expect("an outcome");
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

    #[test]
    fn the_msi_installs_per_user_after_exit_and_the_installed_copy_relaunches() {
        let Some(msi) = std::env::var_os("SECUREPLAN_TEST_MSI").map(PathBuf::from) else {
            eprintln!("skipped: SECUREPLAN_TEST_MSI names no MSI (SecurePlan CI sets it)");
            return;
        };
        let dir = temp("windows");
        let staging = dir.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let staged = staging.join("SecurePlanCAD-windows-x64.msi");
        std::fs::copy(&msi, &staged).unwrap();
        let local = PathBuf::from(std::env::var_os("LOCALAPPDATA").expect("LOCALAPPDATA"));
        let exe = local.join("Programs").join("SecurePlan CAD").join("SecurePlanCAD.exe");
        assert!(!exe.exists(), "SecurePlan CAD is already installed on this runner");
        // Stands in for the exiting application's process.
        let mut app = Command::new("cmd").args(["/C", "ping -n 3 127.0.0.1 >NUL"]).spawn().unwrap();
        let marker = dir.join("launches.txt");
        let plan = Plan {
            version: "0.1.0".into(),
            parent_pid: app.id(),
            install: Install::Msi { msi: staged, exe: exe.clone() },
            result: Some(dir.join("update-result.json")),
            staging,
        };
        let mut helper = start_helper(&plan, &dir, &[("SECUREPLAN_TEST_RELAUNCH_MARKER", marker.as_path())]);
        std::thread::sleep(Duration::from_secs(2));
        assert!(!exe.exists(), "installed before the application exited");
        let _ = app.wait();
        drop(helper.stdin.take());
        let status = wait_exit(&mut helper, Duration::from_secs(600));
        let outcome = take_outcome(plan.result.as_ref().unwrap()).expect("an outcome");
        assert!(status.success() && outcome.installed, "{outcome:?}");
        assert!(exe.is_file(), "the MSI did not install the executable");
        let line = wait_for_line(&marker, Duration::from_secs(120), |_| true).expect("the installed copy was not relaunched");
        let (pid, launched) = line.split_once(' ').unwrap();
        assert_ne!(pid.parse::<u32>().unwrap(), helper.id());
        assert_eq!(launched.to_lowercase(), exe.display().to_string().to_lowercase(), "the relaunch was not the installed copy");
        // The scheme is registered for the current user (DSK-04).
        let command = reg_query(&[r"HKCU\Software\Classes\secureplan-cad\shell\open\command", "/ve"]).expect("scheme command registered");
        assert!(command.to_lowercase().contains(&exe.display().to_string().to_lowercase()), "{command}");
        assert!(reg_query(&[r"HKCU\Software\Classes\secureplan-cad", "/v", "URL Protocol"]).is_some(), "URL Protocol value missing");
        // Uninstalling removes it all again (once the relaunched copy has gone).
        std::thread::sleep(Duration::from_secs(3));
        let removed = Command::new("msiexec").arg("/x").arg(&msi).args(["/qn", "/norestart"]).status().unwrap();
        assert!(removed.success(), "uninstall failed: {removed:?}");
        assert!(!exe.exists());
        assert!(reg_query(&[r"HKCU\Software\Classes\secureplan-cad"]).is_none(), "the scheme stayed registered after uninstall");
        std::fs::remove_dir_all(&dir).ok();
    }
}
