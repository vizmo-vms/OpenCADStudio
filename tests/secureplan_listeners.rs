//! SecurePlan CAD runs no automation listener (DSK-02): `--serve` and `--mcp`
//! exit before binding anything, and the plugin runner mode is refused.
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
