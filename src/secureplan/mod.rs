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
pub use hooks::{begin_awaiting_launch, begin_cold_start, cold_start, deliver_launch, hand_off, ColdStart, Msg, Receipt, State, COLD_START_WAIT};

// Modules for the planned SecurePlan tasks. Each is a stub until its task
// lands, so later tasks never edit a shared `mod.rs`.
pub mod align;
pub mod bridge;
pub mod channel;
pub mod convert;
pub mod export;
pub mod handoff;
pub mod hardening;
pub mod icons;
pub mod home;
#[cfg(test)]
mod win_resources;
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
pub const VERSION: &str = "0.2.5";
/// Bridge protocol version (BRG-07).
pub const PROTOCOL: u32 = 1;
/// Release tags are `secureplan-cad-vX.Y.Z` (DSK-05).
pub const RELEASE_TAG_PREFIX: &str = "secureplan-cad-v";
/// The fork commit this build came from (DSK-05), sent with every Apply.
/// All zeros when the build had no git checkout.
pub const FORK_COMMIT: &str = match option_env!("OCS_GIT_COMMIT") {
    Some(commit) => commit,
    None => "0000000000000000000000000000000000000000",
};

/// A value moved once from a worker to the UI thread inside a `Message`,
/// which must be `Clone` and `Debug` (drawings are neither cheap to clone
/// nor safe to print).
pub struct Carry<T>(std::sync::Arc<std::sync::Mutex<Option<T>>>);

impl<T> Carry<T> {
    pub fn new(value: T) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Some(value))))
    }

    /// The value, the first time only.
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl<T> Clone for Carry<T> {
    fn clone(&self) -> Self {
        Self(std::sync::Arc::clone(&self.0))
    }
}

impl<T> std::fmt::Debug for Carry<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Carry(..)")
    }
}

/// Report an event to the `secureplan-test` stdin driver's harness. Does
/// nothing in other builds.
pub(crate) fn testdriver_event(name: &str, detail: &str) {
    #[cfg(feature = "secureplan-test")]
    testdriver::event(name, detail);
    #[cfg(not(feature = "secureplan-test"))]
    let _ = (name, detail);
}

/// Whether SecurePlan may open native file dialogs: never in unit tests, nor
/// while the `secureplan-test` driver runs the app.
pub(crate) fn native_dialogs_allowed() -> bool {
    #[cfg(test)]
    return false;
    #[cfg(all(not(test), feature = "secureplan-test"))]
    return !testdriver::active();
    #[cfg(all(not(test), not(feature = "secureplan-test")))]
    true
}

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
        assert_eq!(release_tag(VERSION).as_deref(), Some("secureplan-cad-v0.2.5"));
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
