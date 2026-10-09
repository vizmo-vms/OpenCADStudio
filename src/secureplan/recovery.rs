//! User-only recovery copies keyed by origin and survey (DSK-03).
//!
//! A bound document is never autosaved beside a file (it has none). Its
//! unapplied work is kept instead in `<config>/SecurePlanCAD/recovery/<key>/`,
//! where `<key>` is derived from the origin and survey id, as one file
//! (`entry`) holding the metadata (with the base identity the work was made
//! from), the drawing and any pending original (PUB-01). Each save replaces
//! that file atomically (a temporary file, then a rename), so a crash leaves
//! either the previous copy or the new one, never a mix of the two. The
//! folder is readable by the current user only (0700 and 0600 on Unix; the
//! per-user app-data folder on Windows and macOS). A copy is
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

/// The one file of a recovery copy, and its signature.
const ENTRY: &str = "entry";
const MAGIC: &[u8; 8] = b"SPRECOV1";

/// Split an `entry` file: `MAGIC`, `u32le` metadata length, metadata JSON,
/// `u64le` drawing length, the drawing, `u64le` original length (`u64::MAX`
/// for none), the original, and nothing after. Anything malformed or cut
/// short is `None`.
fn parse_entry(bytes: &[u8]) -> Option<(Meta, Vec<u8>, Option<Vec<u8>>)> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let meta_len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
    let meta: Meta = serde_json::from_slice(rest.get(4..4 + meta_len)?).ok()?;
    let rest = &rest[4 + meta_len..];
    let drawing_len = usize::try_from(u64::from_le_bytes(rest.get(..8)?.try_into().ok()?)).ok()?;
    let drawing = rest.get(8..8usize.checked_add(drawing_len)?)?.to_vec();
    let rest = &rest[8 + drawing_len..];
    let original_len = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
    let tail = &rest[8..];
    let original = match (&meta.original, original_len) {
        (None, u64::MAX) if tail.is_empty() => None,
        (Some(_), len) if usize::try_from(len).ok() == Some(tail.len()) => Some(tail.to_vec()),
        _ => return None,
    };
    Some((meta, drawing, original))
}

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
    /// Damaged items the reader had dropped from the drawing (they stay
    /// dropped in this copy, and publishing still needs acknowledgement).
    #[serde(default)]
    lost_entities: usize,
    /// The alignment the work was made with; `None` means align again.
    #[serde(default)]
    alignment: Option<StoredAlignment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAlignment {
    units: String,
    cad_origin: [f64; 2],
    anchor_mm: [f64; 2],
    scale_mm_per_cad_unit: f64,
    quarter_turns: u8,
}

impl StoredAlignment {
    fn of(alignment: &super::align::Alignment) -> Self {
        let m = &alignment.mapping;
        Self {
            units: alignment.units.as_str().into(),
            cad_origin: m.cad_origin,
            anchor_mm: m.anchor_mm,
            scale_mm_per_cad_unit: m.scale_mm_per_cad_unit,
            quarter_turns: m.quarter_turns,
        }
    }

    fn alignment(&self) -> Option<super::align::Alignment> {
        let finite = |v: &[f64; 2]| v.iter().all(|x| x.is_finite());
        (finite(&self.cad_origin) && finite(&self.anchor_mm) && self.scale_mm_per_cad_unit > 0.0 && self.quarter_turns < 4).then_some(())?;
        Some(super::align::Alignment {
            units: super::align::Units::parse(&self.units)?,
            mapping: super::publish::Mapping {
                cad_origin: self.cad_origin,
                anchor_mm: self.anchor_mm,
                scale_mm_per_cad_unit: self.scale_mm_per_cad_unit,
                quarter_turns: self.quarter_turns,
            },
        })
    }
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
    /// Damaged items the reader had dropped (see [`Meta`]).
    pub lost_entities: usize,
    pub alignment: Option<super::align::Alignment>,
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
    #[cfg(test)]
    if let Some(cut) = tests::CRASH_AFTER.with(|c| c.take()) {
        // A crash part-way through writing (tests only).
        file.write_all(&bytes[..cut.min(bytes.len())])?;
        return Err(std::io::Error::other("simulated crash"));
    }
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

    /// Save (replace) the copy for `entry.origin` and `entry.survey`, as one
    /// generation: every part or none.
    pub fn save(&self, entry: &Entry) -> std::io::Result<()> {
        let root = self.root.as_ref().ok_or_else(|| std::io::Error::other("no configuration directory"))?;
        let dir = self.dir(&entry.origin, &entry.survey).unwrap_or_default();
        private_dir(root)?;
        private_dir(&dir)?;
        let meta = Meta {
            origin: entry.origin.clone(),
            survey: entry.survey.clone(),
            base_identity: entry.base_identity.clone(),
            plan_version: entry.plan_version,
            saved_unix: unix(entry.saved),
            drawing: FileMeta::of(&entry.drawing),
            original: entry.original.as_ref().map(FileMeta::of),
            lost_entities: entry.lost_entities,
            alignment: entry.alignment.as_ref().map(StoredAlignment::of),
        };
        let meta = serde_json::to_vec(&meta).map_err(std::io::Error::other)?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + 12 + meta.len() + entry.drawing.bytes.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&(meta.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&meta);
        bytes.extend_from_slice(&(entry.drawing.bytes.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&entry.drawing.bytes);
        match &entry.original {
            Some(original) => {
                bytes.extend_from_slice(&(original.bytes.len() as u64).to_le_bytes());
                bytes.extend_from_slice(&original.bytes);
            }
            None => bytes.extend_from_slice(&u64::MAX.to_le_bytes()),
        }
        write_private(&dir.join(ENTRY), &bytes)
    }

    /// The copy for this origin and survey, if one is whole.
    pub fn load(&self, origin: &str, survey: &str) -> Option<Entry> {
        let dir = self.dir(origin, survey)?;
        let (meta, drawing, original) = parse_entry(&std::fs::read(dir.join(ENTRY)).ok()?)?;
        if meta.origin != origin || meta.survey != survey {
            return None;
        }
        let drawing = meta.drawing.drawing(drawing)?;
        let original = match (&meta.original, original) {
            (Some(file), Some(bytes)) => Some(file.drawing(bytes)?),
            (None, None) => None,
            _ => return None,
        };
        Some(Entry {
            origin: meta.origin,
            survey: meta.survey,
            base_identity: meta.base_identity,
            plan_version: meta.plan_version,
            saved: UNIX_EPOCH + Duration::from_secs(meta.saved_unix),
            drawing,
            original,
            lost_entities: meta.lost_entities,
            alignment: meta.alignment.as_ref().and_then(StoredAlignment::alignment),
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
            let saved = std::fs::read(dir.join(ENTRY))
                .ok()
                .and_then(|bytes| parse_entry(&bytes))
                .map(|(meta, ..)| UNIX_EPOCH + Duration::from_secs(meta.saved_unix));
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
                    None => (Format::Dxf, codec::DxfVersion::AC1032, "drawing.dxf".to_string()),
                };
                // As a file stores it: no dynamic dimension's screen-size overrides.
                let mut document = tab.scene.document.clone();
                tab.scene.strip_dynamic_dimension_overrides(&mut document);
                let document = &document;
                // A copy that would drop content is no copy: the caller
                // reports the preservation as failed.
                let lossless = |format: Format, version| crate::io::dropped_on_save_count(document, version, format == Format::Dxf) == 0;
                // A copy that cannot be written in the drawing's own format is
                // kept as DXF rather than lost.
                let written = lossless(format, version).then(|| crate::io::save_to_bytes(document, format.ext(), version).ok()).flatten();
                let (format, bytes) = match written {
                    Some(bytes) => (format, bytes),
                    None if lossless(Format::Dxf, codec::DxfVersion::AC1032) => {
                        (Format::Dxf, crate::io::save_to_bytes(document, "dxf", codec::DxfVersion::AC1032).ok()?)
                    }
                    None => return None,
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
            lost_entities: bound.lost_entities,
            alignment: bound.alignment,
        })
    }

    /// Keep tab `index`'s unapplied work as a recovery copy under its current
    /// base identity (the plan it was made from).
    pub(crate) fn secureplan_keep_recovery(&mut self, index: usize) -> super::session::Preserve {
        use super::session::Preserve;
        if !self.secureplan_has_unapplied(index) {
            return Preserve::Nothing;
        }
        let Some(base) = self.secureplan.sessions.by_tab(self.tabs[index].id).map(|b| b.base_identity.clone()) else {
            return Preserve::Nothing;
        };
        let saved = self.secureplan_recovery_entry(index, &base).is_some_and(|entry| self.secureplan.recovery.save(&entry).is_ok());
        if saved {
            Preserve::Saved
        } else {
            self.command_line.push_error("SecurePlan: the recovery copy could not be saved.");
            Preserve::Failed
        }
    }

    /// Autosave for bound tabs: a recovery copy of any unapplied work (an
    /// edit, an import, a restored copy) instead of a `.sv$` file.
    pub(crate) fn secureplan_autosave(&mut self) {
        for index in 0..self.tabs.len() {
            if self.secureplan_is_bound(index) {
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
                ("Decide later".to_string(), super::ui::Action::RecoveryLater(tab_id)),
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
        let (document, report) = match super::import::load_drawing(entry.drawing.name.expose(), bytes) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.command_line.push_error(&format!("SecurePlan: the recovery copy could not be read: {error}"));
                return;
            }
        };
        // The recovered drawing is the loaded source, in its own format and
        // version; nothing of it is in SecurePlan yet (`recovered_base`).
        self.secureplan_install(index, document, Some(entry.drawing.clone()));
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            // Items dropped when the work began stay dropped in the copy.
            bound.lost_entities = entry.lost_entities.max(report.lost_entities);
            // The alignment the work was made with, never the current plan's
            // for another drawing; without one, align again. A different
            // mapping from the stored one is disclosed as a re-alignment.
            let before = bound.alignment;
            bound.alignment = entry.alignment;
            bound.realigned = bound.has_plan && bound.alignment.is_some() && bound.alignment != before;
            bound.replace_confirmed = entry.base_identity != bound.base_identity;
            bound.recovered_base = Some(entry.base_identity);
            bound.pending_original = entry.original;
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

    thread_local! {
        /// Makes the next private write stop after this many bytes, as a crash would.
        pub(crate) static CRASH_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

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
            lost_entities: 0,
            alignment: None,
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
            assert_eq!(mode(&dir.join(ENTRY)), 0o600);
        }
        // The folder name reveals neither the origin nor the survey.
        let name = store.dir(&a.origin, &a.survey).unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains("example") && !name.contains("1111"));
        store.delete(&a.origin, &a.survey);
        assert!(store.load(&a.origin, &a.survey).is_none());
        std::fs::remove_dir_all(store.root.unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn a_save_interrupted_part_way_leaves_the_previous_copy_whole() {
        let store = temp_store("atomic");
        let first = entry("https://a.example", "11111111-1111-4111-8111-111111111111", "none");
        store.save(&first).unwrap();
        let mut second = entry(&first.origin, &first.survey, "0".repeat(64).as_str());
        second.drawing = drawing(b"0\nSECTION\nnewer\n");
        second.original = None;
        // The real save dies part-way through writing the new generation, at
        // several points: the previous complete copy is what loads.
        let whole_len = std::fs::read(store.dir(&first.origin, &first.survey).unwrap().join(ENTRY)).unwrap().len();
        for cut in [0, 12, 40, whole_len / 2, whole_len - 1] {
            CRASH_AFTER.with(|c| c.set(Some(cut)));
            assert!(store.save(&second).is_err(), "the crash was not injected");
            let loaded = store.load(&first.origin, &first.survey).unwrap_or_else(|| panic!("no copy after a crash at byte {cut}"));
            assert_eq!(loaded.base_identity, "none", "crash at byte {cut}");
            assert_eq!(loaded.drawing.bytes, first.drawing.bytes);
            assert_eq!(loaded.original.unwrap().bytes.as_slice(), b"original bytes", "one generation, not a mix");
        }
        // A cut-short entry is never read as a mix of parts either.
        let dir = store.dir(&first.origin, &first.survey).unwrap();
        let whole = std::fs::read(dir.join(ENTRY)).unwrap();
        std::fs::write(dir.join(ENTRY), &whole[..whole.len() - 1]).unwrap();
        assert!(store.load(&first.origin, &first.survey).is_none());
        // The next save replaces it whole.
        store.save(&second).unwrap();
        let loaded = store.load(&first.origin, &first.survey).unwrap();
        assert_eq!((loaded.base_identity.len(), loaded.original.is_none()), (64, true));
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
