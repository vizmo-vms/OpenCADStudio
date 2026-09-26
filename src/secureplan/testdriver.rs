//! The `secureplan-test`-only stdin test driver (BRG-01 smoke tests).
//!
//! Compiled only with `--features secureplan-test`, which no release
//! workflow enables (the SecurePlan CI checks both). It reads commands from
//! stdin, one per line, and acknowledges each on stdout:
//!
//! - `pair <launch URL>`: deliver the launch URL exactly as the OS hand-off
//!   would, so it goes through the same parsing and trust checks.
//!
//! Later tasks add import, align and Apply commands.

use std::io::{BufRead, Write};

/// Printed before anything else, so a harness can tell a test build started.
pub const READY: &str = "secureplan-test: driver ready";

/// Start reading stdin commands on a background thread.
pub fn start() {
    println!("{READY}");
    let _ = std::io::stdout().flush();
    std::thread::spawn(|| {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let reply = handle(line.trim());
            println!("secureplan-test: {reply}");
            let _ = std::io::stdout().flush();
        }
    });
}

fn handle(line: &str) -> &'static str {
    match line.split_once(' ') {
        Some(("pair", url)) if url.trim().starts_with("secureplan-cad://") => {
            super::deliver_launch(url.trim().to_string());
            "pair delivered"
        }
        _ => "unknown command",
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pair_delivers_a_launch_and_other_input_is_refused() {
        assert_eq!(super::handle("pair secureplan-cad://pair?v=1"), "pair delivered");
        assert_eq!(super::handle("pair https://example.com"), "unknown command");
        assert_eq!(super::handle("rm -rf /"), "unknown command");
    }
}
