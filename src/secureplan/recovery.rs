//! User-only recovery copies keyed by origin and survey (DSK-03).
//!
//! A bound document is never autosaved beside a file (it has none). Its
//! unapplied work is kept instead in `<config>/SecurePlanCAD/recovery/<key>/`,
//! where `<key>` is derived from the origin and survey id: the drawing, any
//! pending original (PUB-01) and `meta.json` with the base identity it was
//! made from. The folder is readable by the current user only (0700 and 0600
//! on Unix; the per-user app-data folder on Windows and macOS). A copy is
//! offered only when the same origin and survey pair again; one made from a
//! different plan than the survey's current one can be applied only through
//! the explicit "Replace current plan with recovered drawing". Copies are
//! deleted after Apply or Discard, and after 30 days with a notice at the
//! next launch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::session::{Drawing, Format};

/// How long a recovery copy is kept.
pub const RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    origin: String,
    survey: String,
    base_identity: String,
    plan_version: Option<u64>,
    saved_unix: u64,
    drawing: FileMeta,
    original: Option<FileMeta>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMeta {
    name: String,
    format: String,
    format_version: String,
}

impl FileMeta {
    fn of(drawing: &Drawing) -> Self {
        Self { name: drawing.name.expose().clone(), format: drawing.format.ext().into(), format_version: drawing.format_version.clone() }
    }

    fn drawing(&self, bytes: Vec<u8>) -> Option<Drawing> {
        let format = match self.format.as_str() {
            "dwg" => Format::Dwg,
            "dxf" => Format::Dxf,
            _ => return None,
        };
        Some(Drawing { bytes: Arc::new(bytes), name: self.name.clone().into(), format, format_version: self.format_version.clone() })
    }
}

/// One recovery copy.
#[derive(Debug, Clone)]
pub struct Entry {
    pub origin: String,
    pub survey: String,
    /// The base identity (PUB-06) the edits were made from.
    pub base_identity: String,
    pub plan_version: Option<u64>,
    pub saved: SystemTime,
    pub drawing: Drawing,
    pub original: Option<Drawing>,
}

/// The recovery folder.
#[derive(Debug, Clone)]
pub struct Store {
    root: Option<PathBuf>,
}

impl Default for Store {
    fn default() -> Self {
        Self { root: crate::config::config_dir().map(|dir| dir.join("recovery")) }
    }
}

fn unix(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn key(origin: &str, survey: &str) -> String {
    use sha2_011::{Digest, Sha256};
    let digest = Sha256::digest(format!("{origin}\n{survey}").as_bytes());
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Write `bytes` to `path` atomically, readable by the owner only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

impl Store {
    pub fn at(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    fn dir(&self, origin: &str, survey: &str) -> Option<PathBuf> {
        Some(self.root.as_ref()?.join(key(origin, survey)))
    }

    /// Save (replace) the copy for `entry.origin` and `entry.survey`.
    pub fn save(&self, entry: &Entry) -> std::io::Result<()> {
        let root = self.root.as_ref().ok_or_else(|| std::io::Error::other("no configuration directory"))?;
        let dir = self.dir(&entry.origin, &entry.survey).unwrap_or_default();
        private_dir(root)?;
        private_dir(&dir)?;
        write_private(&dir.join("drawing"), &entry.drawing.bytes)?;
        match &entry.original {
            Some(original) => write_private(&dir.join("original"), &original.bytes)?,
            None => match std::fs::remove_file(dir.join("original")) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            },
        }
        let meta = Meta {
            origin: entry.origin.clone(),
            survey: entry.survey.clone(),
            base_identity: entry.base_identity.clone(),
            plan_version: entry.plan_version,
            saved_unix: unix(entry.saved),
            drawing: FileMeta::of(&entry.drawing),
            original: entry.original.as_ref().map(FileMeta::of),
        };
        // The metadata goes last: a copy without it is incomplete and ignored.
        write_private(&dir.join("meta.json"), &serde_json::to_vec(&meta).map_err(std::io::Error::other)?)
    }

    /// The copy for this origin and survey, if one is complete.
    pub fn load(&self, origin: &str, survey: &str) -> Option<Entry> {
        let dir = self.dir(origin, survey)?;
        let meta: Meta = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).ok()?).ok()?;
        if meta.origin != origin || meta.survey != survey {
            return None;
        }
        let drawing = meta.drawing.drawing(std::fs::read(dir.join("drawing")).ok()?)?;
        let original = match &meta.original {
            Some(file) => Some(file.drawing(std::fs::read(dir.join("original")).ok()?)?),
            None => None,
        };
        Some(Entry {
            origin: meta.origin,
            survey: meta.survey,
            base_identity: meta.base_identity,
            plan_version: meta.plan_version,
            saved: UNIX_EPOCH + Duration::from_secs(meta.saved_unix),
            drawing,
            original,
        })
    }

    pub fn delete(&self, origin: &str, survey: &str) {
        if let Some(dir) = self.dir(origin, survey) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// Delete copies saved more than [`RETENTION`] before `now`, and
    /// incomplete ones; returns how many copies were deleted.
    pub fn sweep(&self, now: SystemTime) -> usize {
        let Some(entries) = self.root.as_ref().and_then(|root| std::fs::read_dir(root).ok()) else { return 0 };
        let mut deleted = 0;
        for entry in entries.flatten() {
            let dir = entry.path();
            let saved = std::fs::read(dir.join("meta.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Meta>(&bytes).ok())
                .map(|meta| UNIX_EPOCH + Duration::from_secs(meta.saved_unix));
            let expired = match saved {
                Some(saved) => now.duration_since(saved).is_ok_and(|age| age > RETENTION),
                // Incomplete: a crash while saving. Old enough to be abandoned?
                None => entry.metadata().and_then(|m| m.modified()).is_ok_and(|m| now.duration_since(m).is_ok_and(|age| age > RETENTION)),
            };
            if expired && std::fs::remove_dir_all(&dir).is_ok() {
                deleted += 1;
            }
        }
        deleted
    }
}

impl crate::app::OpenCADStudio {
    /// The recovery copy of tab `index`'s unapplied work under `base_identity`.
    pub(crate) fn secureplan_recovery_entry(&self, index: usize, base_identity: &str) -> Option<Entry> {
        let tab = &self.tabs[index];
        let bound = self.secureplan.sessions.by_tab(tab.id)?;
        let drawing = match (&bound.loaded, self.secureplan_modified(index)) {
            // Unchanged since it was loaded: those very bytes.
            (Some(loaded), false) => loaded.clone(),
            _ => {
                let (format, version, name) = match &bound.loaded {
                    Some(loaded) => (loaded.format, loaded.version(), loaded.name.expose().clone()),
                    None => (Format::Dxf, acadrust::DxfVersion::AC1032, "drawing.dxf".to_string()),
                };
                let document = &tab.scene.document;
                // A copy that cannot be written in the drawing's own format is
                // kept as DXF rather than lost.
                let (format, bytes) = match crate::io::save_to_bytes(document, format.ext(), version) {
                    Ok(bytes) => (format, bytes),
                    Err(_) => (Format::Dxf, crate::io::save_to_bytes(document, "dxf", acadrust::DxfVersion::AC1032).ok()?),
                };
                let version = if format == Format::Dwg { version.as_str().to_string() } else { document.version.as_str().to_string() };
                Drawing { bytes: Arc::new(bytes), name: name.into(), format, format_version: version }
            }
        };
        Some(Entry {
            origin: bound.origin.clone(),
            survey: bound.survey.expose().clone(),
            base_identity: base_identity.to_string(),
            plan_version: bound.plan_version,
            saved: SystemTime::now(),
            drawing,
            original: bound.pending_original.clone(),
        })
    }

    /// Keep tab `index`'s unapplied work as a recovery copy under its current
    /// base identity. Returns whether a copy was written.
    pub(crate) fn secureplan_keep_recovery(&mut self, index: usize) -> bool {
        if !self.secureplan_has_unapplied(index) {
            return false;
        }
        let Some(base) = self.secureplan.sessions.by_tab(self.tabs[index].id).map(|b| b.base_identity.clone()) else {
            return false;
        };
        let saved = self.secureplan_recovery_entry(index, &base).is_some_and(|entry| self.secureplan.recovery.save(&entry).is_ok());
        if !saved {
            self.command_line.push_error("SecurePlan: the recovery copy could not be saved.");
        }
        saved
    }

    /// Autosave for bound tabs: a recovery copy instead of a `.sv$` file.
    pub(crate) fn secureplan_autosave(&mut self) {
        for index in 0..self.tabs.len() {
            if self.secureplan_is_bound(index) && self.tabs[index].dirty {
                self.secureplan_keep_recovery(index);
            }
        }
    }

    /// On exit, keep every bound document's unapplied work (best effort).
    pub(crate) fn secureplan_keep_all_recovery(&self) {
        for (index, tab) in self.tabs.iter().enumerate() {
            let Some(bound) = self.secureplan.sessions.by_tab(tab.id) else { continue };
            if self.secureplan_has_unapplied(index) {
                if let Some(entry) = self.secureplan_recovery_entry(index, &bound.base_identity) {
                    let _ = self.secureplan.recovery.save(&entry);
                }
            }
        }
    }

    /// Offer the recovery copy for a newly opened survey. `true` when a
    /// dialog is now showing.
    pub(crate) fn secureplan_offer_recovery(&mut self, tab_id: u64) -> bool {
        let Some(bound) = self.secureplan.sessions.by_tab(tab_id) else { return false };
        let Some(entry) = self.secureplan.recovery.load(&bound.origin, bound.survey.expose()) else { return false };
        let same_base = entry.base_identity == bound.base_identity;
        let current_version = bound.plan_version;
        let saved = entry.saved.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
        let describe = |version: Option<u64>| version.map_or("no plan".to_string(), |v| format!("plan version {v}"));
        let mut lines = vec![format!(
            "SecurePlan CAD kept unapplied edits for this survey (saved {}).",
            super::ui::format_unix_time(saved)
        )];
        if !same_base {
            lines.push(format!(
                "They were made on {}; the survey now has {}. Applying them replaces the current plan.",
                describe(entry.plan_version),
                describe(current_version)
            ));
        }
        let restore = if same_base { "Restore edits" } else { "Replace current plan with recovered drawing" };
        self.secureplan.dialog = Some(super::ui::Dialog::choice(
            "Recovered edits",
            lines,
            vec![
                (restore.to_string(), super::ui::Action::RecoveryRestore(tab_id)),
                ("Discard them".to_string(), super::ui::Action::RecoveryDiscard(tab_id)),
                ("Decide later".to_string(), super::ui::Action::Dismiss),
            ],
        ));
        true
    }

    /// Put the recovery copy into the bound document.
    pub(crate) fn secureplan_restore_recovery(&mut self, tab_id: u64) {
        let Some(bound) = self.secureplan.sessions.by_tab(tab_id) else { return };
        let Some(entry) = self.secureplan.recovery.load(&bound.origin, bound.survey.expose()) else { return };
        let Some(index) = self.secureplan_tab_index(tab_id) else { return };
        let bytes = entry.drawing.bytes.as_ref().clone();
        let document = match super::import::load_drawing(entry.drawing.name.expose(), bytes) {
            Ok((document, _)) => document,
            Err(error) => {
                self.command_line.push_error(&format!("SecurePlan: the recovery copy could not be read: {error}"));
                return;
            }
        };
        self.secureplan_install(index, document, None);
        // Nothing of it is in SecurePlan yet: it is an edit of the survey.
        self.tabs[index].dirty = true;
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.replace_confirmed = entry.base_identity != bound.base_identity;
            bound.recovered_base = Some(entry.base_identity);
            bound.pending_original = entry.original;
            // The mapping of an unapplied import is chosen again.
            if bound.pending_original.is_some() {
                bound.alignment = None;
            }
        }
        self.command_line.push_info("SecurePlan: recovered edits restored. Apply them to send them to SecurePlan.");
    }

    pub(crate) fn secureplan_discard_recovery(&mut self, tab_id: u64) {
        if let Some(bound) = self.secureplan.sessions.by_tab(tab_id) {
            self.secureplan.recovery.delete(&bound.origin, bound.survey.expose());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn temp_store(tag: &str) -> Store {
        Store::at(std::env::temp_dir().join(format!("secureplan_recovery_{tag}_{}", std::process::id())).join("recovery"))
    }

    fn drawing(bytes: &[u8]) -> Drawing {
        Drawing { bytes: Arc::new(bytes.to_vec()), name: "synthetic.dxf".to_string().into(), format: Format::Dxf, format_version: "AC1032".into() }
    }

    fn entry(origin: &str, survey: &str, base: &str) -> Entry {
        Entry {
            origin: origin.into(),
            survey: survey.into(),
            base_identity: base.into(),
            plan_version: Some(3),
            saved: SystemTime::now(),
            drawing: drawing(b"0\nSECTION\n"),
            original: Some(drawing(b"original bytes")),
        }
    }

    #[test]
    fn copies_are_private_and_keyed_by_origin_and_survey() {
        let store = temp_store("keyed");
        let a = entry("https://a.example", "11111111-1111-4111-8111-111111111111", "none");
        let b = entry("https://b.example", "11111111-1111-4111-8111-111111111111", "none");
        store.save(&a).unwrap();
        assert!(store.load(&b.origin, &b.survey).is_none(), "another origin sees nothing");
        assert!(store.load(&a.origin, "22222222-2222-4222-8222-222222222222").is_none());
        let loaded = store.load(&a.origin, &a.survey).unwrap();
        assert_eq!(loaded.drawing.bytes, a.drawing.bytes);
        assert_eq!(loaded.original.unwrap().bytes.as_slice(), b"original bytes", "the pending original is kept byte for byte");
        assert_eq!(loaded.base_identity, "none");
        assert_eq!(loaded.plan_version, Some(3));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = store.dir(&a.origin, &a.survey).unwrap();
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(store.root.as_ref().unwrap()), 0o700);
            assert_eq!(mode(&dir), 0o700);
            for file in ["drawing", "original", "meta.json"] {
                assert_eq!(mode(&dir.join(file)), 0o600, "{file}");
            }
        }
        // The folder name reveals neither the origin nor the survey.
        let name = store.dir(&a.origin, &a.survey).unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains("example") && !name.contains("1111"));
        store.delete(&a.origin, &a.survey);
        assert!(store.load(&a.origin, &a.survey).is_none());
        std::fs::remove_dir_all(store.root.unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn copies_expire_after_thirty_days() {
        let store = temp_store("expiry");
        let mut old = entry("https://a.example", "11111111-1111-4111-8111-111111111111", "none");
        old.saved = SystemTime::now() - RETENTION - Duration::from_secs(60);
        store.save(&old).unwrap();
        let fresh = entry("https://a.example", "33333333-3333-4333-8333-333333333333", "none");
        store.save(&fresh).unwrap();
        assert_eq!(store.sweep(SystemTime::now()), 1);
        assert!(store.load(&old.origin, &old.survey).is_none());
        assert!(store.load(&fresh.origin, &fresh.survey).is_some());
        std::fs::remove_dir_all(store.root.unwrap().parent().unwrap()).ok();
    }
}
