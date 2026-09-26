//! Guards that later tasks configure for SecurePlan-bound documents (DSK-02,
//! DSK-03):
//! - [`CommandGuard`] refuses named commands (PLOT, WBLOCK, SAVE, …) in bound
//!   tabs; `dispatch_command` consults it before running anything.
//! - the external-resource guard refuses Xref and image-reference resolution
//!   (raster images and PDF underlays); `io::xref`, `io::resolve_image_file`,
//!   `scene::model::image_model` and `scene::model::pdf_raster` consult it
//!   before touching the disk or the network. References to another machine
//!   (UNC paths, URLs) are refused in SecurePlan builds whatever the setting.
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
    /// A raster image, underlay, material texture or light profile read from
    /// disk or fetched over the network.
    Image,
    /// A table's data link: a spreadsheet or CSV file the drawing names, read
    /// by DATALINKUPDATE and written by DATALINKUPDATE WRITE.
    DataLink,
}

static REFUSE_XREF: AtomicBool = AtomicBool::new(false);
static REFUSE_IMAGE: AtomicBool = AtomicBool::new(false);
static REFUSE_DATA_LINK: AtomicBool = AtomicBool::new(false);

fn flag(kind: ExternalResource) -> &'static AtomicBool {
    match kind {
        ExternalResource::Xref => &REFUSE_XREF,
        ExternalResource::Image => &REFUSE_IMAGE,
        ExternalResource::DataLink => &REFUSE_DATA_LINK,
    }
}

// Unit tests run in parallel threads of one process, and many upstream tests
// resolve images and Xrefs: a test that binds a SecurePlan document must not
// refuse them for every other test. So in tests the setting is per thread.
#[cfg(test)]
thread_local! {
    static TEST_REFUSED: std::cell::Cell<[bool; 3]> = const { std::cell::Cell::new([false; 3]) };
}

/// Tests that run alone in a child process set this to use the process-wide
/// flags as the application does (code on other threads then sees them too).
#[cfg(test)]
pub(crate) static TEST_PROCESS_WIDE: AtomicBool = AtomicBool::new(false);

/// Refuse (or allow again) every resolution of `kind`.
pub fn set_external_resource_refused(kind: ExternalResource, refused: bool) {
    #[cfg(test)]
    if TEST_PROCESS_WIDE.load(Ordering::SeqCst) {
        flag(kind).store(refused, Ordering::SeqCst);
        return;
    }
    #[cfg(test)]
    TEST_REFUSED.with(|cell| {
        let mut flags = cell.get();
        flags[kind as usize] = refused;
        cell.set(flags);
    });
    #[cfg(not(test))]
    flag(kind).store(refused, Ordering::SeqCst);
}

/// Whether a reference of `kind` may be resolved. Consulted before any disk
/// read or network request for it.
pub fn external_resource_allowed(kind: ExternalResource) -> bool {
    #[cfg(test)]
    return !TEST_REFUSED.with(|cell| cell.get()[kind as usize]) && !flag(kind).load(Ordering::SeqCst);
    #[cfg(not(test))]
    !flag(kind).load(Ordering::SeqCst)
}

/// Whether a drawing reference names another machine: a UNC path
/// (`\\host\share`, `//host/share`, `\\?\UNC\…`, other `\\?\` and `\\.\`
/// device paths) or a URL. Even a stat of such a path can reach the network
/// (and on Windows send the user's credentials), so a SecurePlan build never
/// touches one (DSK-02).
pub fn is_remote_reference(reference: &str) -> bool {
    let normalised = reference.trim().replace('\\', "/");
    normalised.starts_with("//") || normalised.contains("://")
}

/// Whether the reference `reference` of `kind` may be resolved: the kind is
/// not refused and the reference is local.
pub fn reference_allowed(kind: ExternalResource, reference: &str) -> bool {
    external_resource_allowed(kind) && !is_remote_reference(reference)
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
    fn remote_references_are_recognised() {
        for remote in [
            "\\\\qa-host\\share\\plan.png",
            "//qa-host/share/plan.png",
            "\\\\?\\UNC\\qa-host\\share\\plan.pdf",
            "\\\\?\\C:\\plans\\plan.png",
            "\\\\.\\pipe\\x",
            "/\\qa-host/share/plan.png",
            "  \\\\qa-host\\share\\plan.png",
            "file://qa-host/share/plan.png",
            "smb://qa-host/share/plan.png",
            "https://example.com/plan.png",
        ] {
            assert!(is_remote_reference(remote), "{remote}");
        }
        for local in ["C:\\plans\\plan.png", "/home/user/plan.png", "plans/plan.png", "plan.png", "C:/plans/plan.png"] {
            assert!(!is_remote_reference(local), "{local}");
        }
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

    /// A drawing whose table links to `path`, as the link manager stores it.
    fn data_link_drawing(path: &std::path::Path, writable: bool) -> (acadrust::CadDocument, acadrust::Handle) {
        use acadrust::objects::{ClassObject, ClassObjectData, DataLink, ObjectType};
        let mut doc = acadrust::CadDocument::new();
        let handle = doc.allocate_handle();
        let link = DataLink { connection_string: path.to_string_lossy().into_owned(), option: i32::from(writable), path_option: 1, ..Default::default() };
        let mut object = ClassObject::new(ClassObjectData::DataLink(link));
        object.handle = handle;
        doc.objects.insert(handle, ObjectType::ClassObject(object));
        (doc, handle)
    }

    #[test]
    fn refused_data_links_are_never_read_or_written() {
        let _lock = RESOURCE_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("datalink");
        let csv = dir.join("linked.csv");
        std::fs::write(&csv, "a,b\nc,d\n").unwrap();
        // A local link reads and is writable while data links are allowed.
        let (doc, handle) = data_link_drawing(&csv, true);
        assert!(crate::app::annotation_data::read_data_link(&doc, handle).is_ok(), "the fixture link reads");
        assert!(crate::app::annotation_data::data_link_write_path(&doc, handle).is_ok());
        set_external_resource_refused(ExternalResource::DataLink, true);
        let read = crate::app::annotation_data::read_data_link(&doc, handle);
        let write = crate::app::annotation_data::data_link_write_path(&doc, handle);
        set_external_resource_refused(ExternalResource::DataLink, false);
        assert!(read.is_err(), "a refused link was read");
        assert!(write.is_err(), "a refused link can be written");
        // A link to another computer is never touched, refused or not.
        let (doc, handle) = data_link_drawing(std::path::Path::new(&format!("/{}", csv.display())), true);
        assert!(crate::app::annotation_data::read_data_link(&doc, handle).is_err());
        assert!(crate::app::annotation_data::data_link_write_path(&doc, handle).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refused_material_textures_are_never_read() {
        let _lock = RESOURCE_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("texture");
        let path = dir.join("brick.png");
        image::RgbaImage::new(2, 2).save(&path).unwrap();
        let map = acadrust::objects::MaterialMap {
            source: 1,
            file_name: path.to_string_lossy().into_owned(),
            ..Default::default()
        };
        set_external_resource_refused(ExternalResource::Image, true);
        let refused = crate::scene::model::material_model::load_map_image(&map, None);
        set_external_resource_refused(ExternalResource::Image, false);
        assert!(refused.is_none(), "a refused texture was read");
        assert!(crate::scene::model::material_model::load_map_image(&map, None).is_some(), "the fixture texture loads");
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

    /// A synthetic one-page PDF on disk.
    fn write_pdf(path: &std::path::Path) {
        use printpdf::{Mm, PdfDocument, PdfPage, PdfSaveOptions};
        let mut document = PdfDocument::new("Synthetic underlay");
        document.pages.push(PdfPage::new(Mm(25.4), Mm(25.4), Vec::new()));
        std::fs::write(path, document.save(&PdfSaveOptions::default(), &mut Vec::new())).unwrap();
    }

    #[test]
    fn references_to_another_machine_are_never_touched() {
        // `//tmp/...` is a real local path on Linux, so a successful read
        // would prove the guard missed it; on Windows it would be UNC.
        let _lock = RESOURCE_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("remote");
        let image = dir.join("pixel.png");
        image::RgbaImage::new(1, 1).save(&image).unwrap();
        let pdf = dir.join("plan.pdf");
        write_pdf(&pdf);
        let unc = |path: &std::path::Path| format!("/{}", path.display());
        assert!(crate::scene::model::image_model::resolve_image(&unc(&image)).is_none());
        assert!(crate::io::resolve_image_file(&unc(&image), None).is_none());
        assert!(crate::io::resolve_image_file("pixel.png", Some(std::path::Path::new(&unc(&dir)))).is_none());
        assert!(crate::scene::model::pdf_raster::rasterize_page(&unc(&pdf), "1").is_none());
        // The same files by their ordinary paths resolve.
        assert!(crate::io::resolve_image_file(&image.to_string_lossy(), None).is_some());
        assert!(crate::scene::model::pdf_raster::rasterize_page(&pdf.to_string_lossy(), "1").is_some());

        let target = dir.join("inner.dwg");
        let inner = acadrust::CadDocument::new();
        std::fs::write(&target, crate::io::save_to_bytes(&inner, "dwg", inner.version).unwrap()).unwrap();
        let mut host = acadrust::CadDocument::new();
        let mut record = acadrust::tables::BlockRecord::new("INNER");
        record.flags.is_xref = true;
        record.xref_path = unc(&target);
        host.block_records.add(record).unwrap();
        let (infos, _) = crate::io::xref::resolve_xrefs(&mut host, &dir);
        assert!(infos.iter().all(|info| matches!(info.status, crate::io::xref::XrefStatus::NotFound)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_drawing_with_a_pdf_underlay_loads_without_reading_it_while_refused() {
        use acadrust::entities::{Underlay, UnderlayDefinition, UnderlayType};
        use acadrust::objects::ObjectType;
        let _lock = RESOURCE_FLAGS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("underlay");
        write_pdf(&dir.join("plan.pdf"));
        let mut doc = acadrust::CadDocument::new();
        let mut definition = UnderlayDefinition::new(UnderlayType::Pdf);
        definition.handle = doc.allocate_handle();
        definition.file_path = "plan.pdf".into();
        definition.page_name = "1".into();
        definition.name = "plan".into();
        let definition_handle = definition.handle;
        doc.objects.insert(definition_handle, ObjectType::UnderlayDefinition(definition));
        let mut underlay = Underlay::new(UnderlayType::Pdf);
        underlay.definition_handle = definition_handle;
        doc.add_entity(acadrust::EntityType::Underlay(underlay)).unwrap();
        let drawing = dir.join("synthetic.dwg");
        std::fs::write(&drawing, crate::io::save_to_bytes(&doc, "dwg", doc.version).unwrap()).unwrap();

        // Load the whole drawing and build its images, as opening it does.
        let load = || {
            let doc = crate::io::load_file(&drawing).expect("load the synthetic drawing");
            let (underlay, definition) = doc
                .entities()
                .find_map(|entity| match entity {
                    acadrust::EntityType::Underlay(u) => match doc.objects.get(&u.definition_handle) {
                        Some(ObjectType::UnderlayDefinition(def)) => Some((u.clone(), def.clone())),
                        _ => None,
                    },
                    _ => None,
                })
                .expect("the underlay survives the round trip");
            let image = crate::scene::model::image_model::ImageModel::from_underlay(&underlay, &definition);
            (definition.file_path, image.is_some())
        };
        set_external_resource_refused(ExternalResource::Image, true);
        let refused = load();
        set_external_resource_refused(ExternalResource::Image, false);
        let allowed = load();
        assert_eq!(refused, ("plan.pdf".to_string(), false), "the underlay was probed or read while refused");
        assert!(allowed.0.ends_with("plan.pdf") && allowed.0 != "plan.pdf" && allowed.1, "{allowed:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A minimal compiled SHX font: header, one shape (#0, the font header
    /// `above=9, below=2`).
    fn write_shx_font(path: &std::path::Path) {
        let mut bytes = b"AutoCAD-86 shapes 1.0\r\n\x1a".to_vec();
        let blob: Vec<u8> = [b"FONT\0".as_slice(), &[9, 2, 0, 0]].concat();
        for value in [0u16, 0, 1, 0, blob.len() as u16] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&blob);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn drawing_font_references_to_another_machine_are_never_touched() {
        use crate::scene::text::font_face::Face;
        let dir = temp_dir("font");
        let font = dir.join("synthetic.shx");
        write_shx_font(&font);
        let local = font.to_string_lossy().into_owned();
        // The fixture is a real SHX font: by its ordinary path it resolves.
        assert!(matches!(Face::resolve(&local), Face::Shx { .. }));
        assert!(crate::scene::text::shx::font_metrics(&local).is_some());

        // A drawing whose TEXT names a style that does not exist falls back to
        // the style name as the font: `//tmp/...` is that same file on Linux
        // and a UNC path on Windows, so it must never be stat'ed or read.
        let remote = format!("/{local}");
        for style in [remote.clone(), "\\\\attacker\\share\\font.shx".to_string()] {
            let mut doc = acadrust::CadDocument::new();
            let mut text = acadrust::entities::Text::new();
            text.value = "SYNTHETIC".into();
            text.style = style.clone();
            doc.add_entity(acadrust::EntityType::Text(text)).unwrap();
            let resolved = crate::entities::text_support::resolve_text_style(&style, &doc);
            assert!(!matches!(Face::resolve(&resolved.font_name), Face::Shx { .. }), "{style} resolved to an SHX file");
            let mut scene = crate::scene::Scene::new();
            scene.document = doc;
            scene.rebuild_derived_caches();
        }
        assert!(crate::scene::text::shx::font_metrics(&remote).is_none(), "a remote SHX was read");
        // A style that names a remote font file is refused the same way.
        let mut doc = acadrust::CadDocument::new();
        let mut style = acadrust::tables::TextStyle::new("REMOTE");
        style.font_file = remote.clone();
        doc.text_styles.add(style).unwrap();
        let resolved = crate::entities::text_support::resolve_text_style("REMOTE", &doc);
        assert!(!matches!(Face::resolve(&resolved.font_name), Face::Shx { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }
}
