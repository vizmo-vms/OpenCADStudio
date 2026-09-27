//! Drawing import from the native file dialog (PUB-01), loading drawings
//! that arrive over the bridge, and the entity admission limit (DSK-01).
//!
//! Import checks the size (≤ 50 MiB) before reading, parses on a worker with
//! a Cancel, and reports the format, version, declared units, extents and
//! warnings. Xrefs and external images are never fetched or read: the bound
//! document refuses them (see [`super::guards`]), and the report names them so
//! the user can supply a self-contained drawing. Invalid, empty or unsupported
//! input changes nothing. The chosen file's exact bytes become the session's
//! pending original, sent separately with the next Apply.

use std::path::PathBuf;
use std::sync::Arc;

use iced::Task;

use super::session::{Drawing, ErrorCode, Format, Mode};
use crate::app::{Message, OpenCADStudio};

/// The largest drawing SecurePlan stores (a managed file, BRG-06).
pub const MAX_DRAWING_BYTES: u64 = 50 * 1024 * 1024;
/// DSK-01: at least 1,200,000 expanded entities and block nesting depth 32.
pub const MAX_EXPANDED_ENTITIES: u64 = 1_200_000;
pub const MAX_BLOCK_DEPTH: usize = 32;

/// Why a drawing was refused. Carries no path or content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportError {
    pub code: ErrorCode,
    pub message: String,
}

impl ImportError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// DSK-01 admission: count the entities `document` expands to through block
/// references (MINSERT rows and columns included) and its block nesting
/// depth, and refuse a drawing over the limits with a clear message instead
/// of running out of memory.
pub fn admit(document: &acadrust::CadDocument) -> Result<(), String> {
    use acadrust::EntityType;
    use std::collections::HashMap;
    // Per block record: its direct entity count and the blocks it inserts.
    type Blocks = HashMap<acadrust::Handle, (u64, Vec<(String, u64)>)>;
    let mut blocks: Blocks = HashMap::new();
    for entity in document.entities() {
        let slot = blocks.entry(entity.common().owner_handle).or_default();
        slot.0 += 1;
        if let EntityType::Insert(insert) = entity {
            let copies = u64::from(insert.column_count.max(1)) * u64::from(insert.row_count.max(1));
            slot.1.push((insert.block_name.clone(), copies));
        }
    }
    let by_name: HashMap<String, acadrust::Handle> =
        document.block_records.iter().map(|record| (record.name.to_ascii_uppercase(), record.handle)).collect();
    let too_many = || {
        format!(
            "The drawing expands to more than {} entities, the most SecurePlan CAD opens. Split it or purge unused content, then try again.",
            MAX_EXPANDED_ENTITIES
        )
    };
    let too_deep = || format!("The drawing nests blocks more than {MAX_BLOCK_DEPTH} deep, the most SecurePlan CAD opens.");

    // (expanded count, nesting depth) of each block, memoised.
    let mut memo: HashMap<acadrust::Handle, (u64, usize)> = HashMap::new();
    fn expand(
        block: acadrust::Handle,
        stack: &mut Vec<acadrust::Handle>,
        blocks: &Blocks,
        by_name: &HashMap<String, acadrust::Handle>,
        memo: &mut HashMap<acadrust::Handle, (u64, usize)>,
    ) -> Result<(u64, usize), bool> {
        if let Some(done) = memo.get(&block) {
            return Ok(*done);
        }
        // A block that contains itself, or nesting beyond the limit.
        if stack.contains(&block) || stack.len() > MAX_BLOCK_DEPTH {
            return Err(false);
        }
        stack.push(block);
        let (direct, children) = blocks.get(&block).cloned().unwrap_or_default();
        let mut count = direct;
        let mut depth = 0;
        for (name, copies) in children {
            let Some(child) = by_name.get(&name.to_ascii_uppercase()) else { continue };
            let (inner, inner_depth) = expand(*child, stack, blocks, by_name, memo)?;
            count = count.saturating_add(copies.saturating_mul(inner));
            depth = depth.max(inner_depth + 1);
            if count > MAX_EXPANDED_ENTITIES {
                stack.pop();
                return Err(true);
            }
        }
        stack.pop();
        memo.insert(block, (count, depth));
        Ok((count, depth))
    }

    let mut total: u64 = 0;
    for record in document.block_records.iter() {
        let name = record.name.to_ascii_uppercase();
        if !(name.starts_with("*MODEL_SPACE") || name.starts_with("*PAPER_SPACE")) {
            continue;
        }
        let (count, depth) = expand(record.handle, &mut Vec::new(), &blocks, &by_name, &mut memo)
            .map_err(|too_large| if too_large { too_many() } else { too_deep() })?;
        if depth > MAX_BLOCK_DEPTH {
            return Err(too_deep());
        }
        total = total.saturating_add(count);
        if total > MAX_EXPANDED_ENTITIES {
            return Err(too_many());
        }
    }
    Ok(())
}

/// What an import found, shown to the user (PUB-01).
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub format: Format,
    pub version: String,
    pub units: Option<super::align::Units>,
    pub entities: usize,
    pub extents: Option<[f64; 4]>,
    pub warnings: Vec<String>,
    /// Damaged entities the reader could not read and dropped.
    pub lost_entities: usize,
}

impl Report {
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            format!("Format: {} {}", self.format.ext().to_ascii_uppercase(), self.version),
            format!(
                "Declared units: {}",
                self.units.map_or("none (choose them when aligning)".to_string(), |u| u.as_str().to_string())
            ),
            format!("Entities: {}", self.entities),
        ];
        if let Some([x0, y0, x1, y1]) = self.extents {
            lines.push(format!("Extents: ({x0:.3}, {y0:.3}) to ({x1:.3}, {y1:.3})"));
        }
        if self.warnings.is_empty() {
            lines.push("No warnings.".to_string());
        } else {
            lines.push("Warnings:".to_string());
            lines.extend(self.warnings.iter().map(|w| format!("• {w}")));
        }
        lines
    }
}

/// Warnings about content the drawing references but SecurePlan CAD does not
/// read or draw.
fn warnings(document: &acadrust::CadDocument) -> Vec<String> {
    use acadrust::objects::ObjectType;
    use acadrust::EntityType;
    let mut out = Vec::new();
    let xrefs = document.block_records.iter().filter(|r| r.flags.is_xref || r.flags.is_xref_overlay).count();
    if xrefs > 0 {
        out.push(format!("{xrefs} external reference(s) (Xrefs) were not loaded. Use a self-contained drawing (bind the Xrefs first)."));
    }
    let images = document
        .objects
        .values()
        .filter(|o| matches!(o, ObjectType::ImageDefinition(_) | ObjectType::UnderlayDefinition(_)))
        .count();
    if images > 0 {
        out.push(format!("{images} external image(s) or underlay(s) were not read."));
    }
    let unsupported = document.entities().filter(|e| matches!(e, EntityType::Unknown(_))).count();
    if unsupported > 0 {
        out.push(format!("{unsupported} unsupported entit(ies) are kept but not drawn."));
    }
    let fonts = crate::io::font_repo::missing_shx_fonts(document);
    if !fonts.is_empty() {
        out.push(format!("{} font(s) are missing and were substituted: {}.", fonts.len(), fonts.join(", ")));
    }
    out
}

/// Parse drawing bytes the way SecurePlan CAD opens them: the format from its
/// signature, no reference resolved, corrupt entities purged, the admission
/// limit applied. A parser panic is a refusal, not a crash.
pub fn load_drawing(_name: &str, bytes: Vec<u8>) -> Result<(acadrust::CadDocument, Report), ImportError> {
    if bytes.is_empty() {
        return Err(ImportError::new(ErrorCode::ImportFailed, "The file is empty."));
    }
    let Some(format) = Format::sniff(&bytes) else {
        return Err(ImportError::new(ErrorCode::ImportFailed, "This is not a DWG or DXF drawing SecurePlan CAD can open."));
    };
    // The reader is chosen by extension; the name carries no directory, so
    // nothing is resolved beside any file.
    let name = std::path::PathBuf::from(format!("drawing.{}", format.ext()));
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::io::load_bytes_finalized(&name, bytes)));
    let (mut document, lost_entities) = match parsed {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(message)) if message.contains("SecurePlan CAD opens") => {
            return Err(ImportError::new(ErrorCode::EntityLimit, message));
        }
        Ok(Err(_)) | Err(_) => {
            return Err(ImportError::new(ErrorCode::ImportFailed, "The drawing could not be read. It may be damaged or of an unsupported version."));
        }
    };
    document.source_path = None;
    // R13 and R14 DWG layers have no plot flag (R2000 added it) and the
    // reader leaves it off, which would publish nothing: they all plot.
    if format == Format::Dwg && document.dwg_source_version.unwrap_or(document.version) < acadrust::DxfVersion::AC1015 {
        for layer in document.layers.iter_mut() {
            layer.is_plottable = true;
        }
    }
    let entities = document.entities().count();
    if entities == 0 {
        return Err(ImportError::new(ErrorCode::ImportFailed, "The drawing is empty."));
    }
    let report = Report {
        format,
        version: match format {
            Format::Dwg => document.dwg_source_version.unwrap_or(document.version).as_str().to_string(),
            Format::Dxf => document.version.as_str().to_string(),
        },
        units: super::align::Units::declared(document.header.insertion_units),
        entities,
        extents: None,
        warnings: {
            let mut warnings = warnings(&document);
            if lost_entities > 0 {
                warnings.insert(
                    0,
                    format!(
                        "{lost_entities} damaged items could not be read and were dropped: they are not shown and not published. Every Apply asks you to publish without them; the original drawing is stored unchanged."
                    ),
                );
            }
            warnings
        },
        lost_entities,
    };
    Ok((document, report))
}

/// Why a drawing is being loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadPurpose {
    /// The survey's current drawing, from `openSession`.
    Open,
    /// A new current drawing from `planUpdate`.
    PlanUpdate,
    /// A drawing the user imported: it becomes the pending original.
    Import,
}

/// A finished load, carried back to the UI thread.
#[derive(Debug, Clone)]
pub struct LoadDone {
    pub job: u64,
    pub tab_id: u64,
    pub purpose: LoadPurpose,
    /// The bound document's generation and session when the load began.
    pub generation: u64,
    pub session: Option<super::bridge::SessionId>,
    pub drawing: Drawing,
    pub result: super::Carry<Result<(acadrust::CadDocument, Report), ImportError>>,
}

/// Read a chosen file for import: the size is checked before reading.
pub fn read_checked(path: &std::path::Path) -> Result<Vec<u8>, ImportError> {
    let too_large = |size: u64| {
        ImportError::new(
            ErrorCode::ImportFailed,
            format!("The file is {:.1} MiB; SecurePlan stores drawings up to 50 MiB.", size as f64 / 1048576.0),
        )
    };
    let size = std::fs::metadata(path).map_err(|_| ImportError::new(ErrorCode::ImportFailed, "The file could not be read."))?.len();
    if size > MAX_DRAWING_BYTES {
        return Err(too_large(size));
    }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(size as usize);
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_DRAWING_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|_| ImportError::new(ErrorCode::ImportFailed, "The file could not be read."))?;
    // It may have grown since it was measured.
    if bytes.len() as u64 > MAX_DRAWING_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    Ok(bytes)
}

/// Asks before a new drawing replaces `current` (PUB-01). Focus starts on
/// Cancel, and Escape cancels too.
fn replace_dialog(tab_id: u64, current: &str) -> super::ui::Dialog {
    use super::ui::{Action, Dialog};
    let mut dialog = Dialog::choice(
        "Replace the survey's drawing?",
        vec![
            format!("Current drawing: {current}"),
            "Opening another drawing replaces it here. SecurePlan keeps the current drawing until you Apply.".into(),
        ],
        vec![("Replace drawing".to_string(), Action::ImportReplace(tab_id)), ("Cancel".to_string(), Action::Dismiss)],
    );
    dialog.form_mut().focus = 1;
    dialog
}

/// A load waiting for the tab's running parser to finish. Only the latest
/// waits; an older one is dropped unstarted.
#[derive(Debug, Clone)]
pub struct PendingLoad {
    job: u64,
    generation: u64,
    session: Option<super::bridge::SessionId>,
    drawing: Drawing,
    purpose: LoadPurpose,
}

impl OpenCADStudio {
    /// Load drawing bytes into a bound tab. Each tab runs at most one parser
    /// at a time; a newer load of the same tab supersedes the older one (a
    /// running one's result is dropped, a waiting one is never started).
    pub(crate) fn secureplan_load(&mut self, tab_id: u64, bytes: Arc<Vec<u8>>, name: String, format: Format, purpose: LoadPurpose) -> Task<Message> {
        self.secureplan.next_job += 1;
        let job = self.secureplan.next_job;
        self.secureplan.load_jobs.insert(tab_id, job);
        let operation = if purpose == LoadPurpose::Import { "import" } else { "loading" };
        let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) else { return Task::none() };
        bound.busy = Some(super::session::Busy { operation, progress: None });
        bound.error = None;
        // The document, plan and session this load belongs to.
        let (generation, session) = (bound.generation, bound.session);
        self.secureplan_report_states();
        let drawing = Drawing { bytes, name: super::session::file_name(&name).into(), format, format_version: String::new() };
        let load = PendingLoad { job, generation, session, drawing, purpose };
        if self.secureplan.loads_running.contains(&tab_id) {
            self.secureplan.loads_pending.insert(tab_id, load);
            return Task::none();
        }
        self.secureplan_start_load(tab_id, load)
    }

    fn secureplan_start_load(&mut self, tab_id: u64, load: PendingLoad) -> Task<Message> {
        self.secureplan.loads_running.insert(tab_id);
        let PendingLoad { job, generation, session, drawing, purpose } = load;
        let failed = super::Msg::Loaded(LoadDone {
            job,
            tab_id,
            purpose,
            generation,
            session,
            drawing: drawing.clone(),
            result: super::Carry::new(Err(ImportError::new(ErrorCode::Internal, "Reading the drawing stopped unexpectedly."))),
        });
        self.secureplan_run_job(
            move || {
                let name = drawing.name.expose().clone();
                let result = load_drawing(&name, drawing.bytes.as_ref().clone());
                super::Msg::Loaded(LoadDone { job, tab_id, purpose, generation, session, drawing, result: super::Carry::new(result) })
            },
            failed,
        )
    }

    pub(crate) fn secureplan_loaded(&mut self, done: LoadDone) -> Task<Message> {
        // The tab's parser is free: start the latest waiting load, if any.
        self.secureplan.loads_running.remove(&done.tab_id);
        let next = match self.secureplan.loads_pending.remove(&done.tab_id) {
            Some(load) => self.secureplan_start_load(done.tab_id, load),
            None => Task::none(),
        };
        // Cancelled, superseded by a newer load, or made for a document,
        // plan or session that has since changed: dropped.
        let current = self.secureplan.sessions.by_tab(done.tab_id).is_some_and(|b| b.generation == done.generation && b.session == done.session);
        if self.secureplan.load_jobs.get(&done.tab_id) != Some(&done.job) || !current {
            return next;
        }
        Task::batch([next, self.secureplan_install_loaded(done)])
    }

    fn secureplan_install_loaded(&mut self, done: LoadDone) -> Task<Message> {
        self.secureplan.load_jobs.remove(&done.tab_id);
        if matches!(self.secureplan.dialog, Some(super::ui::Dialog::Progress { tab_id, .. }) if tab_id == done.tab_id) {
            self.secureplan.dialog = None;
        }
        let Some(result) = done.result.take() else { return Task::none() };
        let Some(index) = self.secureplan_tab_index(done.tab_id) else { return Task::none() };
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(done.tab_id) {
            bound.busy = None;
        }
        let replacing = done.purpose != LoadPurpose::Import;
        let (document, mut report) = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                if let Some(bound) = self.secureplan.sessions.by_tab_mut(done.tab_id) {
                    bound.error = Some(error.code);
                    // The new plan's drawing did not open: the old one stays
                    // under its old plan, and Apply waits for a reopen.
                    if replacing {
                        bound.staged = None;
                        bound.unresolved = true;
                    }
                }
                let (title, outcome) = if replacing {
                    ("The drawing could not be opened", "The previous drawing stays. Apply is blocked until the survey is opened from SecurePlan again.")
                } else {
                    ("Import failed", "Nothing was changed.")
                };
                self.secureplan.dialog = Some(super::ui::Dialog::notice(title, vec![error.message.clone(), outcome.into()]));
                super::testdriver_event(if replacing { "load-failed" } else { "import-failed" }, error.code.as_str());
                self.secureplan_report_states();
                return Task::none();
            }
        };
        let drawing = Drawing::describe(done.drawing.bytes, done.drawing.name.expose(), report.format, &document);
        // Keep the latest unapplied work (edits made while this parsed too)
        // before replacing it; if that fails, nothing is replaced.
        if self.secureplan_keep_recovery(index) == super::session::Preserve::Failed {
            if replacing {
                if let Some(bound) = self.secureplan.sessions.by_tab_mut(done.tab_id) {
                    bound.staged = None;
                    bound.unresolved = true;
                }
            }
            self.secureplan.dialog = Some(super::ui::Dialog::notice(
                if replacing { "The plan changed" } else { "Import stopped" },
                vec![
                    "Your unapplied edits could not be saved as a recovery copy, so they were not replaced.".into(),
                    if replacing {
                        "Nothing was replaced. Apply is blocked: free some disk space, then open the survey from SecurePlan again.".into()
                    } else {
                        "Nothing was changed. Free some disk space, then import again.".into()
                    },
                ],
            ));
            super::testdriver_event(if replacing { "replace-blocked" } else { "import-failed" }, "RECOVERY");
            self.secureplan_report_states();
            return Task::none();
        }
        self.secureplan_install(index, document, Some(drawing.clone()));
        if let Some((min, max)) = self.tabs[index].scene.model_space_extents() {
            report.extents = Some([min.x as f64, min.y as f64, max.x as f64, max.y as f64]);
        }
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(done.tab_id) {
            bound.error = None;
            bound.recovered_base = None;
            bound.replace_confirmed = false;
            bound.lost_entities = report.lost_entities;
            // The new plan is adopted together with its drawing (PUB-06).
            if let Some(meta) = bound.staged.take() {
                bound.adopt(meta);
            }
            if done.purpose == LoadPurpose::Import {
                // A new original: it is sent with the next Apply and must be
                // aligned before it (PUB-01, PUB-02).
                bound.pending_original = Some(drawing);
                bound.alignment = None;
                bound.realigned = false;
            } else {
                bound.pending_original = None;
            }
        }
        self.secureplan_report_states();
        match done.purpose {
            LoadPurpose::Import => {
                self.command_line.push_info("SecurePlan: drawing imported. Align it, then Apply.");
                let mut lines = report.lines();
                lines.push("Next: Align the drawing to the survey, then Apply.".into());
                self.secureplan.dialog = Some(super::ui::Dialog::notice("Drawing imported", lines));
                super::testdriver_event("imported", &format!("{} {}", report.format.ext(), report.version));
                Task::none()
            }
            LoadPurpose::Open => {
                super::testdriver_event("loaded", "");
                self.secureplan_after_open(done.tab_id)
            }
            LoadPurpose::PlanUpdate => {
                super::testdriver_event("loaded", "planUpdate");
                Task::none()
            }
        }
    }

    /// **Open drawing** in the bound tab `tab_id` (PUB-01). With no plan and
    /// no imported drawing the file dialog opens at once; otherwise the user
    /// first confirms replacing the current drawing, with focus on Cancel.
    pub(crate) fn secureplan_start_import(&mut self, tab_id: u64) -> Task<Message> {
        if let Err(message) = self.secureplan_can_edit_tab(tab_id) {
            self.command_line.push_error(&message);
            return Task::none();
        }
        match self.secureplan_current_drawing(tab_id) {
            Some(current) => {
                self.secureplan.dialog = Some(replace_dialog(tab_id, &current));
                Task::none()
            }
            None => self.secureplan_pick_import(tab_id),
        }
    }

    /// The drawing an import would replace, by name: the imported one not
    /// applied yet, a restored recovery copy, or the survey's plan. `None`
    /// when there is none of them.
    pub(crate) fn secureplan_current_drawing(&self, tab_id: u64) -> Option<String> {
        let bound = self.secureplan.sessions.by_tab(tab_id)?;
        if let Some(original) = &bound.pending_original {
            return Some(original.name.expose().clone());
        }
        // A restored recovery copy counts as an imported drawing.
        if bound.recovered_base.is_some() {
            return Some(bound.loaded.as_ref().map_or_else(|| "the recovered drawing".to_string(), |d| d.name.expose().clone()));
        }
        if !bound.has_plan {
            return None;
        }
        Some(bound.loaded.as_ref().map_or_else(|| "the survey's current drawing".to_string(), |d| d.name.expose().clone()))
    }

    /// Choose the file for an import into the bound tab `tab_id`: the
    /// picker's answer goes to that tab, whichever tab is active by then.
    pub(crate) fn secureplan_pick_import(&mut self, tab_id: u64) -> Task<Message> {
        if let Err(message) = self.secureplan_can_edit_tab(tab_id) {
            self.command_line.push_error(&message);
            return Task::none();
        }
        // The picker's answer applies only to the drawing there now.
        let generation = self.secureplan.sessions.by_tab(tab_id).map_or(0, |bound| bound.generation);
        #[cfg(test)]
        if let Some(path) = self.secureplan.test_pick.clone() {
            return self.secureplan_update(super::Msg::ImportPicked(tab_id, generation, Some(path.into())));
        }
        if !super::native_dialogs_allowed() {
            self.command_line.push_error("SecurePlan: file dialogs are disabled in this session.");
            return Task::none();
        }
        Task::perform(
            async {
                crate::sys::file_dialog()
                    .set_title("Open drawing for SecurePlan")
                    .add_filter("DWG or DXF drawing", &["dwg", "dxf", "DWG", "DXF"])
                    .pick_file()
                    .await
                    .map(|handle| crate::sys::handle_path(&handle))
            },
            move |path: Option<PathBuf>| Message::SecurePlan(super::Msg::ImportPicked(tab_id, generation, path.map(Into::into))),
        )
    }

    /// Import the file at `path` into the bound tab `tab_id`.
    pub(crate) fn secureplan_import_path(&mut self, tab_id: u64, path: &std::path::Path) -> Task<Message> {
        if self.secureplan.sessions.by_tab(tab_id).is_none() {
            return Task::none();
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "drawing".into());
        let bytes = match read_checked(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.secureplan.dialog = Some(super::ui::Dialog::notice("Import failed", vec![error.message, "Nothing was changed.".into()]));
                super::testdriver_event("import-failed", error.code.as_str());
                return Task::none();
            }
        };
        let format = Format::sniff(&bytes).unwrap_or(Format::Dxf);
        let task = self.secureplan_load(tab_id, Arc::new(bytes), name, format, LoadPurpose::Import);
        if self.secureplan.load_jobs.contains_key(&tab_id) {
            self.secureplan.dialog = Some(super::ui::Dialog::progress(tab_id, "Importing drawing", "Reading the drawing…"));
        }
        task
    }

    /// Stop waiting for tab `tab_id`'s load: a running parse's result is
    /// dropped and a waiting one never starts.
    pub(crate) fn secureplan_invalidate_loads(&mut self, tab_id: u64) {
        self.secureplan.load_jobs.remove(&tab_id);
        self.secureplan.loads_pending.remove(&tab_id);
    }

    /// Cancel the running import or load of tab `tab_id`: its result is
    /// dropped. A cancelled replacement leaves Apply blocked.
    pub(crate) fn secureplan_cancel_load(&mut self, tab_id: u64) {
        self.secureplan_invalidate_loads(tab_id);
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            if bound.busy.is_some_and(|b| b.operation == "import" || b.operation == "loading") {
                bound.busy = None;
            }
            if bound.staged.take().is_some() {
                bound.unresolved = true;
            }
        }
        self.secureplan_report_states();
        self.command_line.push_info("SecurePlan: import cancelled. Nothing was changed.");
    }

    /// Whether the active tab is a connected bound document in edit mode.
    pub(crate) fn secureplan_can_edit_survey(&self) -> Result<(), String> {
        match self.tabs.get(self.active_tab) {
            Some(tab) => self.secureplan_can_edit_tab(tab.id),
            None => Err("Open the survey from SecurePlan first (Edit in desktop).".into()),
        }
    }

    /// Whether bound tab `tab_id` is connected, in edit mode and not busy.
    pub(crate) fn secureplan_can_edit_tab(&self, tab_id: u64) -> Result<(), String> {
        let bound = self
            .secureplan
            .sessions
            .by_tab(tab_id)
            .ok_or_else(|| "Open the survey from SecurePlan first (Edit in desktop).".to_string())?;
        if !bound.connected() {
            return Err("SecurePlan is not connected. Open the survey from SecurePlan again.".into());
        }
        if bound.mode != Mode::Edit {
            return Err("SecurePlan opened this survey for viewing only.".into());
        }
        if bound.busy.is_some() || bound.apply.is_some() {
            return Err("SecurePlan CAD is busy with this survey. Wait for it to finish.".into());
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// R13/R14 DWGs carry no layer plot flag; read, every layer plots, so
    /// their content is published (and can be aligned).
    #[test]
    fn pre_r2000_dwg_layers_plot() {
        for version in [acadrust::DxfVersion::AC1012, acadrust::DxfVersion::AC1014] {
            let bytes = crate::io::save_to_bytes(&testutil::synthetic_document(), "dwg", version).unwrap();
            let (document, report) = load_drawing("old.dwg", bytes).unwrap();
            assert_eq!(report.version, version.as_str());
            assert!(document.layers.iter().all(|layer| layer.is_plottable), "{version:?}: a layer does not plot");
            let mut scene = crate::scene::Scene::new();
            scene.document = document;
            scene.rebuild_derived_caches();
            assert!(super::super::publish::visible_extents(&scene).is_some(), "{version:?}: nothing would be published");
        }
    }
    use crate::app::secureplan::testutil;

    #[test]
    fn invalid_empty_and_oversized_files_are_refused_before_parsing() {
        assert_eq!(load_drawing("x.dxf", Vec::new()).unwrap_err().message, "The file is empty.");
        assert!(load_drawing("x.dwg", b"not a drawing".to_vec()).unwrap_err().message.contains("not a DWG or DXF"));
        assert!(load_drawing("x.dwg", b"AC1032 truncated".to_vec()).is_err());
        // A drawing with no entities is empty too.
        let empty = crate::io::save_to_bytes(&acadrust::CadDocument::new(), "dxf", acadrust::DxfVersion::AC1032).unwrap();
        assert_eq!(load_drawing("x.dxf", empty).unwrap_err().message, "The drawing is empty.");

        let dir = std::env::temp_dir().join(format!("secureplan_import_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.dxf");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(MAX_DRAWING_BYTES + 1).unwrap();
        let refused = read_checked(&big).unwrap_err();
        assert!(refused.message.contains("50 MiB"), "{}", refused.message);
        let fits = dir.join("fits.dxf");
        std::fs::write(&fits, testutil::synthetic_dxf()).unwrap();
        assert_eq!(read_checked(&fits).unwrap(), testutil::synthetic_dxf());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dxf_and_a_dwg_import_with_a_report() {
        for (bytes, format) in [(testutil::synthetic_dxf(), Format::Dxf), (testutil::synthetic_dwg(), Format::Dwg)] {
            let (document, report) = load_drawing("synthetic", bytes).unwrap();
            assert_eq!(report.format, format);
            assert_eq!(report.units, Some(super::super::align::Units::Mm));
            assert!(report.entities > 5);
            assert!(report.warnings.is_empty(), "{:?}", report.warnings);
            assert!(document.source_path.is_none());
            assert!(report.lines().iter().any(|l| l.starts_with("Format:")));
        }
    }

    #[test]
    fn the_signatures_are_those_the_server_accepts() {
        assert_eq!(Format::sniff(b"AC1032\0\0"), Some(Format::Dwg));
        assert_eq!(Format::sniff(b"AC1009"), None, "R12 DWG is not accepted");
        assert_eq!(Format::sniff(b"\xef\xbb\xbf  999\ncomment\n999\nmore\n  0\nSECTION\n"), Some(Format::Dxf));
        assert_eq!(Format::sniff(b"AutoCAD Binary DXF\r\n\x1a\0rest"), Some(Format::Dxf));
        assert_eq!(Format::sniff(b"  0\nEOF\n"), None);
    }

    /// A drawing whose model space holds `inserts` references to a block of
    /// `lines` lines, plus `extra` lines of its own.
    fn blocks_drawing(lines: usize, inserts: usize, extra: usize) -> acadrust::CadDocument {
        use acadrust::entities::{Insert, Line};
        use acadrust::types::Vector3;
        use acadrust::EntityType;
        let mut doc = acadrust::CadDocument::new();
        let members = (0..lines).map(|i| EntityType::Line(Line::from_points(Vector3::new(i as f64, 0.0, 0.0), Vector3::new(i as f64, 1.0, 0.0)))).collect();
        crate::app::secureplan::snap::tests::block(&mut doc, "MANY", members);
        for i in 0..inserts {
            doc.add_entity(EntityType::Insert(Insert::new("MANY", Vector3::new(0.0, i as f64 * 2.0, 0.0)))).unwrap();
        }
        for i in 0..extra {
            doc.add_entity(EntityType::Line(Line::from_points(Vector3::new(-1.0, i as f64, 0.0), Vector3::new(-2.0, i as f64, 0.0)))).unwrap();
        }
        doc
    }

    #[test]
    fn the_entity_limit_admits_a_drawing_at_it_and_refuses_one_over_it() {
        // 1,200 inserts × (1 insert + 999 lines) = 1,200,000 expanded entities.
        let at_limit = blocks_drawing(999, 1200, 0);
        assert_eq!(admit(&at_limit), Ok(()));
        let over = blocks_drawing(999, 1200, 1);
        let refused = admit(&over).unwrap_err();
        assert!(refused.contains("1200000 entities"), "{refused}");
        // Through the loader: a clear refusal, and the one at the limit opens.
        let bytes = crate::io::save_to_bytes(&over, "dxf", acadrust::DxfVersion::AC1032).unwrap();
        let error = load_drawing("over.dxf", bytes).unwrap_err();
        assert_eq!(error.code, ErrorCode::EntityLimit);
        let bytes = crate::io::save_to_bytes(&at_limit, "dxf", acadrust::DxfVersion::AC1032).unwrap();
        assert!(load_drawing("at.dxf", bytes).is_ok());
    }

    /// Blocks B1 … Bn, each holding a line and an insert of the next; model
    /// space inserts B1: nesting depth n.
    fn nested_drawing(depth: usize) -> acadrust::CadDocument {
        use acadrust::entities::{Insert, Line};
        use acadrust::types::Vector3;
        use acadrust::EntityType;
        let mut doc = acadrust::CadDocument::new();
        for level in (1..=depth).rev() {
            let mut members = vec![EntityType::Line(Line::from_points(Vector3::new(0.0, 0.0, 0.0), Vector3::new(1.0, 1.0, 0.0)))];
            if level < depth {
                members.push(EntityType::Insert(Insert::new(format!("B{}", level + 1), Vector3::new(1.0, 0.0, 0.0))));
            }
            crate::app::secureplan::snap::tests::block(&mut doc, &format!("B{level}"), members);
        }
        doc.add_entity(EntityType::Insert(Insert::new("B1", Vector3::new(0.0, 0.0, 0.0)))).unwrap();
        doc
    }

    #[test]
    fn block_nesting_opens_at_depth_32_and_fails_cleanly_at_33() {
        assert_eq!(admit(&nested_drawing(MAX_BLOCK_DEPTH)), Ok(()));
        let refused = admit(&nested_drawing(MAX_BLOCK_DEPTH + 1)).unwrap_err();
        assert!(refused.contains("32 deep"), "{refused}");
        let bytes = crate::io::save_to_bytes(&nested_drawing(MAX_BLOCK_DEPTH + 1), "dxf", acadrust::DxfVersion::AC1032).unwrap();
        assert!(load_drawing("deep.dxf", bytes).unwrap_err().message.contains("32 deep"));
        let bytes = crate::io::save_to_bytes(&nested_drawing(MAX_BLOCK_DEPTH), "dxf", acadrust::DxfVersion::AC1032).unwrap();
        assert!(load_drawing("ok.dxf", bytes).is_ok());
    }

    /// Open drawing (PUB-01, DSK-08): the file dialog at once for a survey
    /// with no drawing; a confirmation naming the current drawing, with focus
    /// on Cancel, when there is a plan, an imported drawing or a restored
    /// recovery copy. Cancel, by
    /// Enter or Escape, changes nothing.
    #[test]
    fn open_drawing_asks_before_replacing_a_plan_or_an_imported_drawing() {
        use crate::app::secureplan::session::tests::{sample, Harness};
        use crate::app::secureplan::ui::trust_dialog::DialogKey;
        use crate::app::secureplan::ui::Dialog;
        use crate::app::secureplan::overlay;
        use serde_json::Value;
        const REPLACE: &str = "Replace the survey's drawing?";

        let pending = |h: &Harness| h.bound().pending_original.as_ref().map(|o| o.name.expose().clone());
        let pick = |h: &mut Harness, name: &str| {
            let file = h.dir().join(name);
            std::fs::write(&file, testutil::synthetic_dxf()).unwrap();
            h.app.secureplan.test_pick = Some(file);
        };
        let lines = |h: &Harness| match &h.app.secureplan.dialog {
            Some(Dialog::Choice { lines, form, .. }) => (lines.clone(), form.focus, form.buttons.iter().map(|(l, _)| l.clone()).collect::<Vec<_>>()),
            _ => panic!("no dialog"),
        };

        // No plan and nothing imported: the file dialog, no question.
        let mut h = Harness::new("open_drawing_new");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        pick(&mut h, "first.dxf");
        let _ = h.app.dispatch_command("SECUREPLANIMPORT");
        assert_eq!(h.dialog_title().as_deref(), Some("Drawing imported"), "a new survey was asked to confirm");
        assert_eq!(pending(&h).as_deref(), Some("first.dxf"));
        h.app.secureplan.dialog = None;

        // An imported drawing: asked, naming it, focus on Cancel.
        pick(&mut h, "second.dxf");
        let entities = h.entity_count();
        for cancel in [DialogKey::Activate, DialogKey::Cancel] {
            let _ = h.app.dispatch_command("SECUREPLANIMPORT");
            assert_eq!(h.dialog_title().as_deref(), Some(REPLACE));
            let (text, focus, buttons) = lines(&h);
            assert!(text.contains(&"Current drawing: first.dxf".to_string()), "{text:?}");
            assert_eq!(buttons, ["Replace drawing", "Cancel"]);
            assert_eq!(focus, 1, "focus is not on Cancel");
            h.key(cancel);
            assert!(h.app.secureplan.dialog.is_none(), "{cancel:?} left the dialog open");
            assert_eq!(pending(&h).as_deref(), Some("first.dxf"), "{cancel:?} replaced the drawing");
            assert_eq!(h.entity_count(), entities);
            assert!(h.app.secureplan.load_jobs.is_empty() && h.bound().busy.is_none());
        }
        // Replace drawing opens the file dialog, and the new file replaces it.
        let _ = h.app.dispatch_command("SECUREPLANIMPORT");
        h.key(DialogKey::Previous);
        h.key(DialogKey::Activate);
        assert_eq!(pending(&h).as_deref(), Some("second.dxf"));

        // A survey with a plan, opened for import from the web: the import
        // intent is edit, then Open drawing, so it asks first.
        let mut h = Harness::new("open_drawing_plan");
        h.open(Some(("plan.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", sample("openSession-edit")["cadPlan"].clone(), &"a".repeat(64), "import");
        pick(&mut h, "other.dxf");
        assert_eq!(h.dialog_title().as_deref(), Some(REPLACE), "the import intent did not ask");
        assert!(lines(&h).0.contains(&"Current drawing: plan.dxf".to_string()), "{:?}", lines(&h).0);
        let loaded = h.bound().loaded.clone().map(|d| d.bytes);
        h.key(DialogKey::Cancel);
        assert!(pending(&h).is_none() && h.bound().loaded.clone().map(|d| d.bytes) == loaded, "Cancel changed the plan");

        // A restored recovery copy, on a survey with no plan, counts as an
        // imported drawing.
        let mut h = Harness::new("open_drawing_recovered");
        let kept = crate::app::secureplan::session::Drawing {
            bytes: std::sync::Arc::new(testutil::synthetic_dxf()),
            name: "kept.dxf".to_string().into(),
            format: Format::Dxf,
            format_version: "AC1032".into(),
        };
        let entry = crate::app::secureplan::recovery::Entry {
            origin: crate::app::secureplan::bridge::tests::ORIGIN.into(),
            survey: "7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f".into(),
            base_identity: "none".into(),
            plan_version: None,
            saved: std::time::SystemTime::now(),
            drawing: kept,
            original: None,
            lost_entities: 0,
            alignment: None,
        };
        h.app.secureplan.recovery.save(&entry).unwrap();
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        assert_eq!(h.dialog_title().as_deref(), Some("Recovered edits"));
        h.key(DialogKey::Activate);
        pick(&mut h, "other.dxf");
        let _ = h.app.dispatch_command("SECUREPLANIMPORT");
        assert_eq!(h.dialog_title().as_deref(), Some(REPLACE), "a restored copy was replaced without asking");
        let (text, focus, _) = lines(&h);
        assert!(text.contains(&"Current drawing: kept.dxf".to_string()), "{text:?}");
        assert_eq!(focus, 1, "focus is not on Cancel");
        h.key(DialogKey::Cancel);
        assert!(pending(&h).is_none() && h.bound().recovered_base.is_some(), "Cancel changed the restored copy");
    }
}
