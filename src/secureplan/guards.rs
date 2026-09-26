//! Guards that later tasks configure for SecurePlan-bound documents (DSK-02,
//! DSK-03):
//! - [`CommandGuard`] refuses named commands (PLOT, WBLOCK, SAVE, …) in bound
//!   tabs; `dispatch_command` consults it before running anything.
//! - the external-resource guard refuses Xref and image-reference resolution;
//!   `io::xref` and `scene::model::image_model` consult it before touching the
//!   disk or the network.
//!
//! Both allow everything until configured.

use std::sync::atomic::{AtomicBool, Ordering};

use rustc_hash::FxHashSet;

/// Refuses configured commands in SecurePlan-bound tabs.
#[derive(Debug, Default)]
pub struct CommandGuard {
    refused: FxHashSet<String>,
    bound_tabs: FxHashSet<u64>,
}

/// A command the guard refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub verb: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not available for a SecurePlan drawing.", self.verb)
    }
}

/// The canonical verb of a command line: its first token, upper-cased, without
/// the `_` (untranslated), `.` (built-in), `-` (command-line) or `+` prefixes.
pub fn command_verb(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches(['_', '.', '-', '+', '\''])
        .to_ascii_uppercase()
}

impl CommandGuard {
    /// Refuse `verb` (any prefix or case) in bound tabs.
    pub fn refuse(&mut self, verb: &str) {
        self.refused.insert(command_verb(verb));
    }

    /// Mark the tab with stable id `tab_id` as SecurePlan-bound.
    pub fn bind_tab(&mut self, tab_id: u64) {
        self.bound_tabs.insert(tab_id);
    }

    pub fn unbind_tab(&mut self, tab_id: u64) {
        self.bound_tabs.remove(&tab_id);
    }

    pub fn is_bound(&self, tab_id: u64) -> bool {
        self.bound_tabs.contains(&tab_id)
    }

    /// `Err` when `command` is refused in the tab with id `tab_id`.
    pub fn check(&self, tab_id: u64, command: &str) -> Result<(), Refused> {
        let verb = command_verb(command);
        if self.is_bound(tab_id) && self.refused.contains(&verb) {
            Err(Refused { verb })
        } else {
            Ok(())
        }
    }
}

/// External resources a drawing can reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalResource {
    /// An Xref drawing read from disk.
    Xref,
    /// A raster image or underlay read from disk or fetched over the network.
    Image,
}

static REFUSE_XREF: AtomicBool = AtomicBool::new(false);
static REFUSE_IMAGE: AtomicBool = AtomicBool::new(false);

fn flag(kind: ExternalResource) -> &'static AtomicBool {
    match kind {
        ExternalResource::Xref => &REFUSE_XREF,
        ExternalResource::Image => &REFUSE_IMAGE,
    }
}

/// Refuse (or allow again) every resolution of `kind`.
pub fn set_external_resource_refused(kind: ExternalResource, refused: bool) {
    flag(kind).store(refused, Ordering::SeqCst);
}

/// Whether a reference of `kind` may be resolved. Consulted before any disk
/// read or network request for it.
pub fn external_resource_allowed(kind: ExternalResource) -> bool {
    !flag(kind).load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that flip the process-wide resource flags.
    pub(crate) static RESOURCE_FLAGS: Mutex<()> = Mutex::new(());

    #[test]
    fn verbs_are_normalised() {
        assert_eq!(command_verb("_plot"), "PLOT");
        assert_eq!(command_verb("-WBLOCK name"), "WBLOCK");
        assert_eq!(command_verb("  .saveas "), "SAVEAS");
        assert_eq!(command_verb("'zoom"), "ZOOM");
        assert_eq!(command_verb(""), "");
    }

    #[test]
    fn refusal_applies_only_to_bound_tabs() {
        let mut guard = CommandGuard::default();
        guard.refuse("plot");
        assert_eq!(guard.check(1, "PLOT"), Ok(()));
        guard.bind_tab(1);
        assert_eq!(guard.check(1, "-plot"), Err(Refused { verb: "PLOT".into() }));
        assert_eq!(guard.check(1, "LINE"), Ok(()));
        assert_eq!(guard.check(2, "PLOT"), Ok(()));
        guard.unbind_tab(1);
        assert_eq!(guard.check(1, "PLOT"), Ok(()));
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("secureplan_guard_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn refused_image_references_are_never_read() {
        let _lock = RESOURCE_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("image");
        let path = dir.join("pixel.png");
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        let reference = path.to_string_lossy().into_owned();

        set_external_resource_refused(ExternalResource::Image, true);
        let refused = crate::scene::model::image_model::resolve_image(&reference);
        set_external_resource_refused(ExternalResource::Image, false);
        assert!(refused.is_none(), "a refused image reference was resolved");
        assert!(crate::scene::model::image_model::resolve_image(&reference).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refused_xrefs_are_never_read() {
        let _lock = RESOURCE_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("xref");
        let target = dir.join("inner.dwg");
        let inner = acadrust::CadDocument::new();
        let bytes = crate::io::save_to_bytes(&inner, "dwg", inner.version).unwrap();
        std::fs::write(&target, bytes).unwrap();
        let host = || {
            let mut doc = acadrust::CadDocument::new();
            let mut record = acadrust::tables::BlockRecord::new("INNER");
            record.flags.is_xref = true;
            record.xref_path = target.to_string_lossy().into_owned();
            doc.block_records.add(record).unwrap();
            doc
        };

        set_external_resource_refused(ExternalResource::Xref, true);
        let (refused, _) = crate::io::xref::resolve_xrefs(&mut host(), &dir);
        set_external_resource_refused(ExternalResource::Xref, false);
        let (allowed, _) = crate::io::xref::resolve_xrefs(&mut host(), &dir);
        use crate::io::xref::XrefStatus;
        assert!(!refused.is_empty());
        assert!(
            refused.iter().all(|info| matches!(info.status, XrefStatus::NotFound)),
            "a refused xref was resolved"
        );
        assert!(
            allowed.iter().any(|info| matches!(info.status, XrefStatus::Loaded)),
            "the fixture xref does not resolve"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
