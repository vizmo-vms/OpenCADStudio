//! SecurePlan CAD runs no automation listener (DSK-02): `--serve` and `--mcp`
//! exit before binding anything, and the plugin runner mode is refused. Its
//! command-line diagnostics never print file paths.
#![cfg(feature = "secureplan")]

use std::net::TcpListener;
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run the SecurePlan CAD binary")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn secureplan_serve_mode_is_refused_and_binds_nothing() {
    let port = free_port();
    let output = run(&["--serve", "--port", &port.to_string()]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("not available in SecurePlan CAD"));
    assert!(output.stdout.is_empty(), "--serve answered: {output:?}");
    // Nothing is left listening on the requested port.
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}

#[test]
fn secureplan_mcp_mode_is_refused() {
    let output = run(&["--mcp"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "--mcp answered: {output:?}");
}

#[test]
fn secureplan_plugin_runner_mode_is_refused() {
    let output = run(&["--ocs-plugin-runner", "socket", "library"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

/// A temporary directory whose name is a sentinel that must never appear in
/// the process output.
fn sentinel_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("secureplan-sentinel-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_no_sentinel(output: &std::process::Output, sentinel: &str) {
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!text.contains(sentinel), "a file path leaked into the output: {text}");
}

#[test]
fn secureplan_export_diagnostics_never_print_paths() {
    let dir = sentinel_dir("export");
    let sentinel = dir.file_name().unwrap().to_string_lossy().into_owned();
    // A failing export (missing input) and a successful one.
    let missing = run(&["--export", &dir.join("missing.dxf").to_string_lossy(), &dir.join("out.dxf").to_string_lossy()]);
    assert_ne!(missing.status.code(), Some(0));
    assert_no_sentinel(&missing, &sentinel);
    let input = dir.join("synthetic.dxf");
    let doc = acadrust::CadDocument::new();
    std::fs::write(&input, OpenCADStudio::io::save_to_bytes(&doc, "dxf", doc.version).unwrap()).unwrap();
    let exported = run(&["--export", &input.to_string_lossy(), &dir.join("copy.dxf").to_string_lossy()]);
    assert_eq!(exported.status.code(), Some(0), "{exported:?}");
    assert!(String::from_utf8_lossy(&exported.stdout).contains("[redacted]"));
    assert_no_sentinel(&exported, &sentinel);
    // Replacing the destination fails (it is a folder): the nested save error
    // names the destination, and must not print it.
    let folder = dir.join("taken.dxf");
    std::fs::create_dir_all(folder.join("inside")).unwrap();
    let replace = run(&["--export", &input.to_string_lossy(), &folder.to_string_lossy()]);
    assert_ne!(replace.status.code(), Some(0), "{replace:?}");
    assert!(String::from_utf8_lossy(&replace.stderr).contains("export: cannot write [redacted]"), "{replace:?}");
    assert_no_sentinel(&replace, &sentinel);
    // Performance tracing prints save timings, never the path.
    let traced = Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
        .args(["--export", &input.to_string_lossy(), &dir.join("traced.dxf").to_string_lossy()])
        .env("PERF", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run the SecurePlan CAD binary");
    assert_eq!(traced.status.code(), Some(0), "{traced:?}");
    assert!(String::from_utf8_lossy(&traced.stderr).contains("[perf] save"), "tracing did not run: {traced:?}");
    assert_no_sentinel(&traced, &sentinel);
    std::fs::remove_dir_all(&dir).ok();
}

/// `--script` reports an unreadable script before the GUI starts; with no
/// display the GUI then fails, which ends the process.
#[cfg(target_os = "linux")]
#[test]
fn secureplan_script_diagnostics_never_print_paths() {
    let dir = sentinel_dir("script");
    let sentinel = dir.file_name().unwrap().to_string_lossy().into_owned();
    let output = Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
        .args(["--new-instance", "--script", &dir.join("missing.scr").to_string_lossy()])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_CONFIG_HOME", &dir)
        .stdin(Stdio::null())
        .output()
        .expect("run the SecurePlan CAD binary");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--script: cannot read [redacted]"), "{output:?}");
    assert_no_sentinel(&output, &sentinel);
    std::fs::remove_dir_all(&dir).ok();
}

/// Started by a link from a website that may never pair, SecurePlan CAD opens
/// nothing: it leaves before any window or GPU work. Without a display a GUI
/// start fails, so a clean exit shows no window was attempted.
#[cfg(target_os = "linux")]
#[test]
fn secureplan_link_from_an_ineligible_website_opens_nothing() {
    let dir = sentinel_dir("link");
    let link = format!(
        "secureplan-cad://pair?v=1&origin=http%3A%2F%2Fsecureplan.example&pairing={}&token={}&survey=7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f&intent=edit",
        "A".repeat(22),
        "A".repeat(43)
    );
    let output = Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
        .arg(&link)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_CONFIG_HOME", &dir)
        .stdin(Stdio::null())
        .output()
        .expect("run the SecurePlan CAD binary");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// Release builds refuse `--export`, `--dwg-thumbnail` and `--script` before
/// reading or writing any file (DSK-08); `secureplan-test` builds apply that
/// rule when `SECUREPLAN_TEST_RELEASE_RULES` is set.
#[cfg(feature = "secureplan-test")]
#[test]
fn secureplan_release_builds_refuse_the_headless_drawing_modes() {
    let dir = sentinel_dir("release");
    let input = dir.join("synthetic.dxf");
    let doc = acadrust::CadDocument::new();
    std::fs::write(&input, OpenCADStudio::io::save_to_bytes(&doc, "dxf", doc.version).unwrap()).unwrap();
    let script = dir.join("script.scr");
    std::fs::write(&script, "LINE 0,0 1,1\n").unwrap();
    let (converted, thumbnail) = (dir.join("out.dxf"), dir.join("out.png"));
    let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
    for args in [
        vec!["--export".to_string(), path(&input), path(&converted)],
        vec!["--dwg-thumbnail".to_string(), path(&input), path(&thumbnail), "64".to_string()],
        vec!["--new-instance".to_string(), "--script".to_string(), path(&script)],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
            .args(&args)
            .env("SECUREPLAN_TEST_RELEASE_RULES", "1")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_CONFIG_HOME", &dir)
            .stdin(Stdio::null())
            .output()
            .expect("run the SecurePlan CAD binary");
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("not available in SecurePlan CAD"), "{args:?}: {output:?}");
        assert!(!converted.exists() && !thumbnail.exists(), "{args:?} wrote a file");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// One SecurePlan CAD window: a launch without a URL asks the running copy,
/// over the authenticated per-user channel, to show its window and exits
/// without one. When only a crash's stale `handoff.json` is left, the launch
/// starts as the primary and serves later launches itself.
#[cfg(target_os = "linux")]
#[test]
fn secureplan_a_second_launch_shows_the_running_window_and_exits() {
    use OpenCADStudio::app::secureplan::{handoff, CONFIG_DIR_NAME};
    let dir = sentinel_dir("single");
    let descriptor = dir.join(CONFIG_DIR_NAME).join("handoff.json");
    let launch = || {
        Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_CONFIG_HOME", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the SecurePlan CAD binary")
    };
    // The running copy holds the window's lock and serves the hand-off.
    let handoff::Claim::Primary(lock) = handoff::claim(&dir.join(CONFIG_DIR_NAME), &[], std::time::Duration::ZERO) else {
        panic!("no primary lock")
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    let sender = std::sync::Mutex::new(sender);
    let _serving = handoff::serve(&descriptor, move |request| sender.lock().unwrap().send(request).is_ok()).unwrap();
    let output = launch().wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(receiver.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), handoff::Request::Focus);

    // A crash left a descriptor behind whose port nobody serves, and the
    // lock free.
    drop(lock);
    let port = free_port();
    std::fs::write(&descriptor, format!("{{\"protocol\":1,\"port\":{port},\"secret\":\"{}\",\"pid\":1}}\n", "00".repeat(32))).unwrap();
    let primary = launch();
    let pid = primary.id();
    let _ = primary.wait_with_output().unwrap();
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(written["pid"], pid, "the launch did not start as the primary");
    std::fs::remove_dir_all(&dir).ok();
}

/// The real binary started with `args` and `XDG_CONFIG_HOME=dir`, no display.
#[cfg(target_os = "linux")]
fn start_in(dir: &std::path::Path, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_OpenCADStudio"))
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_CONFIG_HOME", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the SecurePlan CAD binary")
}

/// DSK-04: the macOS launcher's awaiting start (the real binary, with its
/// flag) waits for an exiting primary to end and then owns the window,
/// ready for the launcher's link. Next to a ready primary it leaves without
/// sending it anything.
#[cfg(target_os = "linux")]
#[test]
fn secureplan_the_launchers_awaiting_start_takes_over_from_an_exiting_primary() {
    use OpenCADStudio::app::secureplan::{handoff, CONFIG_DIR_NAME};
    let dir = sentinel_dir("awaiting");
    let config = dir.join(CONFIG_DIR_NAME);
    let descriptor = config.join("handoff.json");
    // A primary that is exiting: it owns the window, no longer serves.
    let handoff::Claim::Primary(lock) = handoff::claim(&config, &[], std::time::Duration::ZERO) else { panic!("no primary lock") };
    let awaiting = start_in(&dir, &[handoff::AWAITING_LAUNCH_ARG]);
    let pid = awaiting.id();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!descriptor.exists(), "a second primary started while the first owned the window");
    drop(lock);
    let _ = awaiting.wait_with_output().unwrap();
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(written["pid"], pid, "the awaiting start left instead of taking over");

    // A ready primary: the awaiting start leaves at once.
    let handoff::Claim::Primary(lock) = handoff::claim(&config, &[], std::time::Duration::ZERO) else { panic!("no primary lock") };
    let (sender, receiver) = std::sync::mpsc::channel();
    let sender = std::sync::Mutex::new(sender);
    let _serving = handoff::serve(&descriptor, move |request| sender.lock().unwrap().send(request).is_ok()).unwrap();
    let output = start_in(&dir, &[handoff::AWAITING_LAUNCH_ARG]).wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(receiver.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "the awaiting start asked for something");
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    assert_eq!(written["pid"], std::process::id());
    drop(lock);
    std::fs::remove_dir_all(&dir).ok();
}

/// DSK-04: a launch that cannot own the window (no lock can be made) or
/// cannot be reached by later launches (no descriptor can be written)
/// starts nothing and says why.
#[cfg(target_os = "linux")]
#[test]
fn secureplan_a_launch_that_cannot_own_the_window_starts_nothing() {
    use OpenCADStudio::app::secureplan::CONFIG_DIR_NAME;
    for blocked in ["primary.lock", "handoff.json"] {
        let dir = sentinel_dir(&format!("unowned_{}", blocked.len()));
        std::fs::create_dir_all(dir.join(CONFIG_DIR_NAME).join(blocked)).unwrap();
        let output = start_in(&dir, &[]).wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{blocked}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot use its settings folder"), "{blocked}: {output:?}");
        assert!(dir.join(CONFIG_DIR_NAME).join(blocked).is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }
}
