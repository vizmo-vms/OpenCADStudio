#![deny(warnings, clippy::all)]
//! SecurePlan CAD: the SecurePlan companion build of Open CAD Studio.
//!
//! Compiled only with `--features secureplan`. `src/app/mod.rs` declares this
//! directory as `crate::app::secureplan`, so the application hooks in
//! [`hooks`] can reach the editor state. Upstream files call only the hooks
//! and the guards in [`guards`]; every other SecurePlan change lives here.
//! The contract with the SecurePlan web app is pinned by the protocol vectors
//! in `secureplan-vectors/` (see `secureplan-vectors/README.md`).

pub mod guards;
mod hooks;
pub use hooks::{deliver_launch, Msg, State};

// Modules for the planned SecurePlan tasks. Each is a stub until its task
// lands, so later tasks never edit a shared `mod.rs`.
pub mod align;
pub mod bridge;
pub mod channel;
pub mod convert;
pub mod export;
pub mod handoff;
pub mod hardening;
pub mod import;
pub mod layout;
pub mod overlay;
pub mod pairing;
pub mod protocol;
pub mod publish;
pub mod recovery;
pub mod redact;
pub mod ribbon;
pub mod session;
pub mod settings;
pub mod snap;
pub mod symbols;
#[cfg(feature = "secureplan-test")]
pub mod testdriver;
#[cfg(any(test, feature = "secureplan-test"))]
pub mod testutil;
pub mod transfer;
pub mod trust;
pub mod ui;
pub mod update;
pub mod update_helper;
#[cfg(test)]
mod vectors;

/// Product name shown in window titles and the About box.
pub const APP_NAME: &str = "SecurePlan CAD";
/// Per-user configuration directory name, distinct from upstream's
/// `OpenCADStudio` so the two apps never share settings (DSK-05).
pub const CONFIG_DIR_NAME: &str = "SecurePlanCAD";
/// SecurePlan CAD's own semantic version, `MAJOR.MINOR.PATCH` with MAJOR ≤ 255
/// (MSI ProductVersion). Independent of upstream's calendar version.
pub const VERSION: &str = "0.1.0";
/// Bridge protocol version (BRG-07).
pub const PROTOCOL: u32 = 1;
/// Release tags are `secureplan-cad-vX.Y.Z` (DSK-05).
pub const RELEASE_TAG_PREFIX: &str = "secureplan-cad-v";

/// Parse a `MAJOR.MINOR.PATCH` version with no leading zeros.
pub fn parse_version(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split('.');
    let mut next = || -> Option<u32> {
        let part = parts.next()?;
        let well_formed = !part.is_empty()
            && part.len() <= 9
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'));
        well_formed.then(|| part.parse().ok()).flatten()
    };
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

/// The release tag for `version`, or `None` when it is not a valid SecurePlan
/// CAD version (MAJOR must be ≤ 255 for the MSI).
pub fn release_tag(version: &str) -> Option<String> {
    let (major, _, _) = parse_version(version)?;
    (major <= 255).then(|| format!("{RELEASE_TAG_PREFIX}{version}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_a_valid_release_version() {
        assert_eq!(release_tag(VERSION).as_deref(), Some("secureplan-cad-v0.1.0"));
    }

    #[test]
    fn release_tags_follow_the_scheme() {
        assert_eq!(release_tag("255.0.0").as_deref(), Some("secureplan-cad-v255.0.0"));
        for invalid in ["256.0.0", "1.0", "1.0.0.0", "01.0.0", "1.0.0-beta", "", "v1.0.0"] {
            assert_eq!(release_tag(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn identity_differs_from_upstream() {
        assert_ne!(APP_NAME, "Open CAD Studio");
        assert_ne!(CONFIG_DIR_NAME, "OpenCADStudio");
    }
}
