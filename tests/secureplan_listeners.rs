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
