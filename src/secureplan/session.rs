//! SecurePlan-bound documents and session modes (DSK-03, BRG-05, BRG-07,
//! BRG-08).
//!
//! `openSession` loads the survey's drawing from the transferred bytes into a
//! new tab that is *bound* to the session: it has no file path, its title is
//! the survey label, Save/Save As/PLOT/WBLOCK and the other file-writing
//! commands are refused, it gets no recent-file entry, no adjacent autosave,
//! `.bak` or thumbnail, and Xrefs and external images are never resolved from
//! disk while it is open. Its only durable outputs are Apply and (later)
//! Export. The web decides the mode (`edit` or `view`); `view` disables Apply.
//! The desktop reports `sessionState` whenever its dirty flag, active view,
//! busy state or error changes.

use std::collections::HashMap;
use std::sync::Arc;

use iced::Task;
use serde_json::{json, Value};

use super::align::Alignment;
use super::bridge::SessionId;
use super::overlay::Overlay;
use super::pairing::Intent;
use super::redact::Redacted;
use super::transfer::Completed;
use crate::app::{Message, OpenCADStudio};

/// Commands refused for a bound document: saving, plotting and every other
/// way of writing the drawing or its content to a file, and attaching or
/// reloading external references (DSK-02, DSK-03).
pub const REFUSED_COMMANDS: &[&str] = &[
    "SAVE", "QSAVE", "SAVEAS", "SAVEALL", "PLOT", "PRINT", "QPRINT", "QUICKPRINT", "PUBLISH", "PLOTTOFILE",
    "EXPORTPDF", "EXPORTDWF", "EXPORTDWFX", "EXPORTLAYOUT", "EXPORT", "WBLOCK", "DXFOUT", "STLOUT", "STLEXPORT",
    "STEPOUT", "STEPEXPORT", "OBJEXPORT", "JPGOUT", "PNGOUT", "BMPOUT", "TIFOUT", "WMFOUT", "PSOUT", "SVGOUT",
    "ACISOUT", "DATAEXTRACTION", "ETRANSMIT", "ARCHIVE", "XATTACH", "XREF", "XOPEN", "XRELOAD", "XBIND",
    "IMAGEATTACH", "ATTACH", "PDFATTACH", "DWFATTACH", "DGNATTACH", "EXTERNALREFERENCES", "EATTEXT", "ATTEXT",
    "DATALINKUPDATE",
];

/// The mode the web grants (BRG-08). Only `Edit` may Apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Edit,
    View,
}

impl Mode {
    fn parse(value: &Value) -> Option<Self> {
        match value.as_str()? {
            "edit" => Some(Mode::Edit),
            "view" => Some(Mode::View),
            _ => None,
        }
    }
}

/// A drawing file format (CON-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Dwg,
    Dxf,
}

/// DWG version codes the SecurePlan server accepts (CON-04).
const DWG_VERSIONS: [&str; 7] = ["AC1012", "AC1014", "AC1015", "AC1018", "AC1021", "AC1024", "AC1027"];

impl Format {
    /// Recognise a drawing by its signature, as the server does: a DWG starts
    /// with a known `AC10nn` code; an ASCII DXF starts (after an optional BOM,
    /// whitespace and `999` comments) with the group `0` / `SECTION`; a binary
    /// DXF with its sentinel.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if let Some(code) = bytes.get(..6) {
            let code = std::str::from_utf8(code).unwrap_or_default();
            if DWG_VERSIONS.contains(&code) || code == "AC1032" {
                return Some(Format::Dwg);
            }
        }
        if bytes.starts_with(b"AutoCAD Binary DXF\r\n\x1a\0") {
            return Some(Format::Dxf);
        }
        let text = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        let head = String::from_utf8_lossy(&text[..text.len().min(64 * 1024)]);
        let mut lines = head.lines().map(str::trim);
        loop {
            let code = lines.by_ref().find(|line| !line.is_empty())?;
            let value = lines.next()?;
            match (code, value) {
                ("999", _) => continue,
                ("0", "SECTION") => return Some(Format::Dxf),
                _ => return None,
            }
        }
    }

    pub fn from_media_type(media_type: &str) -> Option<Self> {
        match media_type {
            "image/vnd.dwg" => Some(Format::Dwg),
            "image/vnd.dxf" => Some(Format::Dxf),
            _ => None,
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Format::Dwg => "image/vnd.dwg",
            Format::Dxf => "image/vnd.dxf",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Format::Dwg => "dwg",
            Format::Dxf => "dxf",
        }
    }
}

/// Drawing bytes with what the desktop knows about them.
#[derive(Clone)]
pub struct Drawing {
    pub bytes: Arc<Vec<u8>>,
    pub name: Redacted<String>,
    pub format: Format,
    /// `AC10nn`: the DWG header code, or the DXF `$ACADVER`.
    pub format_version: String,
}

impl std::fmt::Debug for Drawing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Drawing")
            .field("format", &self.format)
            .field("format_version", &self.format_version)
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl Drawing {
    /// Describe `bytes` read as `document`.
    pub fn describe(bytes: Arc<Vec<u8>>, name: &str, format: Format, document: &acadrust::CadDocument) -> Self {
        let format_version = match format {
            Format::Dwg => String::from_utf8_lossy(&bytes[..6.min(bytes.len())]).into_owned(),
            Format::Dxf => document.version.as_str().to_string(),
        };
        Self { bytes, name: file_name(name).into(), format, format_version }
    }

    pub fn version(&self) -> acadrust::DxfVersion {
        acadrust::DxfVersion::parse(&self.format_version).unwrap_or(acadrust::DxfVersion::AC1032)
    }

    /// The file name stem, for naming the outputs derived from it.
    pub fn stem(&self) -> String {
        let name = self.name.expose();
        std::path::Path::new(name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "drawing".to_string())
    }
}

/// A transfer name the protocol accepts: no path separators or control
/// characters, 1 to 200 characters.
pub fn file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .take(200)
        .collect();
    if cleaned.trim().is_empty() {
        "drawing".to_string()
    } else {
        cleaned
    }
}

/// What the desktop is doing, reported in `sessionState.busy`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Busy {
    pub operation: &'static str,
    pub progress: Option<f64>,
}

/// A desktop error reported in `sessionState.error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    ImportFailed,
    EntityLimit,
    KnownLoss,
    WriterError,
    OutputTooLarge,
    TransferFailed,
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::ImportFailed => "IMPORT_FAILED",
            ErrorCode::EntityLimit => "ENTITY_LIMIT",
            ErrorCode::KnownLoss => "KNOWN_LOSS",
            ErrorCode::WriterError => "WRITER_ERROR",
            ErrorCode::OutputTooLarge => "OUTPUT_TOO_LARGE",
            ErrorCode::TransferFailed => "TRANSFER_FAILED",
            ErrorCode::Internal => "INTERNAL",
        }
    }
}

/// An `applyRequest` waiting for its `applyResult`.
#[derive(Debug, Clone)]
pub struct ApplyInFlight {
    pub request_id: String,
    /// The tab's edit revision when the snapshot was frozen.
    pub snapshot_revision: u64,
    /// The drawing that was sent; it becomes the current drawing on commit.
    pub drawing: Drawing,
    /// Whether it is the writer's output (not the verbatim loaded bytes).
    pub written: bool,
}

/// The survey's plan as `openSession` or `planUpdate` describe it. It is
/// adopted only together with a drawing that opened (PUB-06).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanMeta {
    pub base_identity: String,
    pub plan_version: Option<u64>,
    pub has_plan: bool,
    pub alignment: Option<Alignment>,
}

impl PlanMeta {
    fn of(plan: &Value, base_identity: &Value) -> Self {
        Self {
            base_identity: base_identity.as_str().unwrap_or("none").to_string(),
            plan_version: plan["version"]["number"].as_u64(),
            has_plan: plan.is_object(),
            alignment: Alignment::from_cad_plan(plan),
        }
    }
}

/// What happened to a document's unapplied work before it was replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preserve {
    /// There was none.
    Nothing,
    /// It is in a recovery copy.
    Saved,
    /// It could not be saved: the replacement must not go ahead.
    Failed,
}

/// A document bound to a SecurePlan survey.
#[derive(Clone)]
pub struct Bound {
    /// The live session, or `None` once the web page went away.
    pub session: Option<SessionId>,
    pub tab_id: u64,
    pub origin: String,
    pub survey: Redacted<String>,
    pub label: Redacted<String>,
    pub intent: Intent,
    pub mode: Mode,
    /// PUB-06: the survey's current plan identity, echoed by `applyRequest`.
    pub base_identity: String,
    /// The survey's current plan version, when it has one.
    pub plan_version: Option<u64>,
    /// The current `cadPlan` exists (the survey is not new to CAD).
    pub has_plan: bool,
    /// The bytes the document was loaded from: the current drawing, or the
    /// original after an import. Sent verbatim when the document is unmodified.
    pub loaded: Option<Drawing>,
    /// The tab's edit revision when `loaded` was installed.
    pub clean_revision: u64,
    /// An imported original not yet stored in SecurePlan (PUB-01).
    pub pending_original: Option<Drawing>,
    /// Restored from a recovery copy made under this base identity.
    pub recovered_base: Option<String>,
    /// The user chose "Replace current plan with recovered drawing".
    pub replace_confirmed: bool,
    pub overlay: Option<Overlay>,
    /// The web's word that the survey has no design elements and no
    /// comments (`surveyEmpty`); the overlay alone cannot tell (CON-03).
    pub survey_empty: bool,
    /// The mapping the next Apply uses (stored or newly aligned).
    pub alignment: Option<Alignment>,
    /// The user re-aligned a plan that already had a stored mapping.
    pub realigned: bool,
    pub busy: Option<Busy>,
    pub error: Option<ErrorCode>,
    pub apply: Option<ApplyInFlight>,
    /// The last `sessionState` body sent (without its request id).
    pub last_state: Option<Value>,
    /// Changes whenever the document, its plan or its session is replaced:
    /// work started before (an Apply build, a load) is stale after.
    pub generation: u64,
    /// A new plan waiting for its drawing to open (PUB-06).
    pub staged: Option<PlanMeta>,
    /// The survey's plan changed but its drawing could not be opened (or the
    /// local work kept first): Apply is blocked until the survey is reopened.
    pub unresolved: bool,
    /// Entities of the loaded drawing that could not be read (corrupt records
    /// the reader dropped). Publishing needs the user's acknowledgement.
    pub lost_entities: usize,
    /// The tab's dirty flag taken into an Apply build. While the build and
    /// its request run, the tab's own flag records only changes made after
    /// the snapshot; the commit keeps those and drops this one.
    pub held_dirty: bool,
}

impl Bound {
    fn new(tab_id: u64, pending: &PendingSession, mode: Mode) -> Self {
        Self {
            session: None,
            tab_id,
            origin: pending.origin.clone(),
            survey: pending.survey.clone(),
            label: String::new().into(),
            intent: pending.intent,
            mode,
            base_identity: "none".into(),
            plan_version: None,
            has_plan: false,
            loaded: None,
            clean_revision: 0,
            pending_original: None,
            recovered_base: None,
            replace_confirmed: false,
            overlay: None,
            survey_empty: false,
            alignment: None,
            realigned: false,
            busy: None,
            error: None,
            apply: None,
            last_state: None,
            generation: 0,
            staged: None,
            unresolved: false,
            lost_entities: 0,
            held_dirty: false,
        }
    }

    /// Adopt a plan (its drawing is in place).
    pub(crate) fn adopt(&mut self, meta: PlanMeta) {
        self.base_identity = meta.base_identity;
        self.plan_version = meta.plan_version;
        self.has_plan = meta.has_plan;
        self.alignment = meta.alignment;
        self.realigned = false;
        self.staged = None;
        self.unresolved = false;
    }

    /// Whether the document holds work SecurePlan does not have yet.
    pub fn unapplied(&self) -> bool {
        self.pending_original.is_some() || self.recovered_base.is_some()
    }

    pub fn connected(&self) -> bool {
        self.session.is_some()
    }
}

/// A session whose handshake completed and that has not opened a document yet.
#[derive(Debug, Clone)]
pub struct PendingSession {
    pub origin: String,
    pub survey: Redacted<String>,
    pub intent: Intent,
}

/// All SecurePlan session state.
#[derive(Default)]
pub struct Sessions {
    pub bound: Vec<Bound>,
    pub pending: HashMap<SessionId, PendingSession>,
    /// Completed transfers waiting for the message that references them.
    pub transfers: HashMap<(SessionId, u32), Arc<Completed>>,
    /// Messages waiting, in arrival order, for a transfer an earlier message
    /// of their session names (BRG-07: a session's messages are handled in
    /// order).
    pub deferred: Vec<(SessionId, String, String, Value)>,
    /// Transfers already used by a message; naming one again is an error.
    pub consumed: std::collections::HashSet<(SessionId, u32)>,
    /// When each session last made progress while messages waited.
    pub last_progress: HashMap<SessionId, std::time::Instant>,
    next_request: u64,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions").field("bound", &self.bound.len()).field("pending", &self.pending.len()).finish_non_exhaustive()
    }
}

impl Sessions {
    pub fn next_request_id(&mut self) -> String {
        self.next_request += 1;
        format!("a{}", self.next_request)
    }

    pub fn by_tab(&self, tab_id: u64) -> Option<&Bound> {
        self.bound.iter().find(|b| b.tab_id == tab_id)
    }

    pub fn by_tab_mut(&mut self, tab_id: u64) -> Option<&mut Bound> {
        self.bound.iter_mut().find(|b| b.tab_id == tab_id)
    }

    pub fn by_session_mut(&mut self, session: SessionId) -> Option<&mut Bound> {
        self.bound.iter_mut().find(|b| b.session == Some(session))
    }

    fn drop_session(&mut self, session: SessionId) {
        self.pending.remove(&session);
        self.transfers.retain(|(s, _), _| *s != session);
        self.deferred.retain(|(s, ..)| *s != session);
        self.consumed.retain(|(s, _)| *s != session);
        self.last_progress.remove(&session);
    }
}

/// How long a message may wait for a transfer it names with nothing arriving
/// on its session. Transfers are announced before the message (BRG-06) and a
/// stalled one fails after 30 s, so a longer silence means a transfer that
/// will never come.
pub const TRANSFER_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Completed transfers a session may hold before the messages that name them.
const MAX_WAITING_TRANSFERS: usize = 16;
/// Messages a session may have queued behind one waiting for a transfer.
const MAX_WAITING_MESSAGES: usize = 64;

/// Transfer ids a message references, which must complete before it is handled.
fn referenced_transfers(kind: &str, body: &Value) -> Vec<u32> {
    let keys: &[&str] = match kind {
        "openSession" => &["drawingTransferId", "overlayTransferId"],
        "planUpdate" => &["drawingTransferId"],
        "overlayUpdate" => &["overlayTransferId"],
        "exportRequest" => &["drawingTransferId", "payloadTransferId"],
        _ => &[],
    };
    keys.iter().filter_map(|key| body[*key].as_u64()).filter_map(|id| u32::try_from(id).ok()).collect()
}

/// The `activeView` of a tab: model space or the named paper layout.
fn active_view(scene: &crate::scene::Scene) -> Value {
    if scene.current_layout == "Model" {
        json!({ "kind": "model" })
    } else {
        let name: String = scene.current_layout.chars().take(255).collect();
        if name.is_empty() {
            json!({ "kind": "model" })
        } else {
            json!({ "kind": "layout", "layoutName": name })
        }
    }
}

impl OpenCADStudio {
    // ── Lookup ──────────────────────────────────────────────────────────────

    pub(crate) fn secureplan_tab_index(&self, tab_id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == tab_id)
    }

    /// The bound document shown in the active tab.
    pub(crate) fn secureplan_active_bound(&self) -> Option<&Bound> {
        let tab = self.tabs.get(self.active_tab)?;
        self.secureplan.sessions.by_tab(tab.id)
    }

    pub(crate) fn secureplan_is_bound(&self, index: usize) -> bool {
        self.tabs.get(index).is_some_and(|tab| self.secureplan.sessions.by_tab(tab.id).is_some())
    }

    /// Whether the bound document in tab `index` holds work SecurePlan lacks.
    pub(crate) fn secureplan_has_unapplied(&self, index: usize) -> bool {
        let Some(tab) = self.tabs.get(index) else { return false };
        self.secureplan.sessions.by_tab(tab.id).is_some_and(|b| tab.dirty || b.held_dirty || b.unapplied())
    }

    /// Whether the document differs from the bytes it was loaded from.
    pub(crate) fn secureplan_modified(&self, index: usize) -> bool {
        let tab = &self.tabs[index];
        match self.secureplan.sessions.by_tab(tab.id) {
            Some(bound) => bound.loaded.is_none() || tab.dirty || bound.held_dirty || tab.edit_revision != bound.clean_revision,
            None => tab.dirty,
        }
    }

    // ── Sending ─────────────────────────────────────────────────────────────

    pub(crate) fn secureplan_send(&mut self, session: SessionId, mut message: Value) -> bool {
        if message.get("requestId").is_none() {
            message["requestId"] = json!(self.secureplan.sessions.next_request_id());
        }
        self.secureplan_bridge().is_some_and(|bridge| bridge.send(session, message))
    }

    /// Send `sessionState` for every connected document whose state changed.
    pub(crate) fn secureplan_report_states(&mut self) {
        let mut updates = Vec::new();
        for bound in &self.secureplan.sessions.bound {
            let (Some(session), Some(index)) = (bound.session, self.secureplan_tab_index(bound.tab_id)) else { continue };
            let tab = &self.tabs[index];
            let body = json!({
                "dirty": tab.dirty || bound.unapplied(),
                "activeView": active_view(&tab.scene),
                "busy": bound.busy.map(|busy| json!({ "operation": busy.operation, "progress": busy.progress })),
                "error": bound.error.map(|code| json!({ "code": code.as_str() })),
            });
            if bound.last_state.as_ref() != Some(&body) {
                updates.push((bound.tab_id, session, body));
            }
        }
        for (tab_id, session, body) in updates {
            let mut message = body.clone();
            message["type"] = json!("sessionState");
            if self.secureplan_send(session, message) {
                if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
                    bound.last_state = Some(body);
                }
            }
        }
    }

    // ── Guards ──────────────────────────────────────────────────────────────

    /// Refuse Xrefs and external images while any bound document is open.
    pub(crate) fn secureplan_refresh_guards(&self) {
        use super::guards::{set_external_resource_refused, ExternalResource};
        let bound = !self.secureplan.sessions.bound.is_empty() || self.secureplan.workers > 0;
        set_external_resource_refused(ExternalResource::Xref, bound);
        set_external_resource_refused(ExternalResource::Image, bound);
        set_external_resource_refused(ExternalResource::DataLink, bound);
    }

    /// Hook for `update`: file-writing messages (Save, Save As, plotting,
    /// printing and exports) are refused while a bound document is active, or
    /// open at all for the ones that cover every tab.
    pub(crate) fn secureplan_blocks_message(&mut self, message: &Message) -> bool {
        let active = matches!(
            message,
            Message::SaveFile
                | Message::SaveAs
                | Message::PlotDialogOpen
                | Message::PrintToPrinter
                | Message::PlotExportPath(Some(_))
                | Message::PlotWindowExportPath(Some(_))
                | Message::WblockSaveResult(_, Some(_))
                | Message::DataExtractionSaveResult(_, Some(_))
                | Message::StlExportPath(Some(_))
                | Message::StepExportPath(Some(_))
        );
        let every_tab = matches!(message, Message::PrintAllOpen | Message::PrintAllPdfPath(Some(_)));
        let refused = (active && self.secureplan_is_bound(self.active_tab))
            || (every_tab && !self.secureplan.sessions.bound.is_empty());
        if refused {
            self.command_line.push_error("This is not available for a SecurePlan drawing. Use Apply to send it to SecurePlan.");
        }
        refused
    }

    /// For writes that cover every tab (print all): refuse while any bound
    /// document is open.
    pub(crate) fn secureplan_refuse_any_bound(&mut self) -> bool {
        if self.secureplan.sessions.bound.is_empty() {
            return false;
        }
        self.command_line.push_error("This is not available while a SecurePlan drawing is open.");
        true
    }

    /// For file-writing paths in `update/file.rs`: refuse a bound tab.
    pub(crate) fn secureplan_refuse_save(&mut self, index: usize) -> bool {
        if !self.secureplan_is_bound(index) {
            return false;
        }
        self.command_line.push_error(
            "A SecurePlan drawing is not saved to a file. Use Apply to send it to SecurePlan.",
        );
        true
    }

    // ── Bridge events ───────────────────────────────────────────────────────

    pub(crate) fn secureplan_session_opened(&mut self, session: SessionId, origin: String, survey: Redacted<String>, intent: Intent) {
        self.secureplan.sessions.pending.insert(session, PendingSession { origin, survey, intent });
    }

    pub(crate) fn secureplan_session_closed(&mut self, session: SessionId) {
        self.secureplan.sessions.drop_session(session);
        if let Some(bound) = self.secureplan.sessions.by_session_mut(session) {
            bound.session = None;
            bound.busy = None;
            bound.apply = None;
            bound.last_state = None;
            let tab_id = bound.tab_id;
            self.secureplan_release_held(tab_id);
        }
        super::testdriver_event("closed", "");
    }

    pub(crate) fn secureplan_transfer(&mut self, session: SessionId, transfer: Arc<Completed>) -> Task<Message> {
        // A message references at most two transfers; more waiting means the
        // page is not following the protocol.
        let waiting = self.secureplan.sessions.transfers.keys().filter(|(s, _)| *s == session).count();
        if waiting >= MAX_WAITING_TRANSFERS {
            self.secureplan_protocol_error(session);
            return Task::none();
        }
        if self.secureplan.sessions.consumed.contains(&(session, transfer.transfer_id)) {
            self.secureplan_protocol_error(session);
            return Task::none();
        }
        self.secureplan.sessions.last_progress.insert(session, std::time::Instant::now());
        self.secureplan.sessions.transfers.insert((session, transfer.transfer_id), transfer);
        // Handle this session's queued messages, in order, up to the first
        // that still waits for a transfer.
        let mut tasks = Vec::new();
        while let Some(position) = self.secureplan.sessions.deferred.iter().position(|(s, ..)| *s == session) {
            let (_, kind, _, body) = &self.secureplan.sessions.deferred[position];
            let ready = referenced_transfers(kind, body)
                .into_iter()
                .all(|id| self.secureplan.sessions.transfers.contains_key(&(session, id)));
            if !ready {
                break;
            }
            let (s, kind, request_id, body) = self.secureplan.sessions.deferred.remove(position);
            tasks.push(self.secureplan_handle(s, kind, request_id, body));
        }
        Task::batch(tasks)
    }

    pub(crate) fn secureplan_take_transfer(&mut self, session: SessionId, id: u32) -> Option<Arc<Completed>> {
        self.secureplan.sessions.consumed.insert((session, id));
        self.secureplan.sessions.transfers.remove(&(session, id))
    }

    /// End a session that broke the protocol, dropping what it left waiting.
    pub(crate) fn secureplan_protocol_error(&mut self, session: SessionId) {
        self.secureplan.sessions.drop_session(session);
        if let Some(bridge) = self.secureplan_bridge() {
            bridge.close(session, "protocolError");
        }
    }

    /// End sessions whose waiting messages name transfers that never came.
    pub(crate) fn secureplan_expire_waiting(&mut self) {
        let now = std::time::Instant::now();
        let wait = self.secureplan.transfer_wait;
        let mut expired: Vec<SessionId> = self.secureplan.sessions.deferred.iter().map(|(s, ..)| *s).collect();
        expired.dedup();
        expired.retain(|s| self.secureplan.sessions.last_progress.get(s).is_none_or(|t| now.duration_since(*t) >= wait));
        for session in expired {
            self.secureplan_protocol_error(session);
        }
    }

    pub(crate) fn secureplan_message(&mut self, session: SessionId, kind: String, request_id: String, body: Value) -> Task<Message> {
        // A transfer can be used once: naming a used one again is an error.
        if referenced_transfers(&kind, &body).into_iter().any(|id| self.secureplan.sessions.consumed.contains(&(session, id))) {
            self.secureplan_protocol_error(session);
            return Task::none();
        }
        // A message is handled only once every transfer it names is complete,
        // and after every earlier message of its session.
        let queued_ahead = self.secureplan.sessions.deferred.iter().any(|(s, ..)| *s == session);
        let waiting = queued_ahead
            || referenced_transfers(&kind, &body)
                .into_iter()
                .any(|id| !self.secureplan.sessions.transfers.contains_key(&(session, id)));
        if waiting {
            if self.secureplan.sessions.deferred.iter().filter(|(s, ..)| *s == session).count() >= MAX_WAITING_MESSAGES {
                self.secureplan_protocol_error(session);
                return Task::none();
            }
            if !queued_ahead {
                self.secureplan.sessions.last_progress.insert(session, std::time::Instant::now());
            }
            self.secureplan.sessions.deferred.push((session, kind, request_id, body));
            return Task::none();
        }
        self.secureplan_handle(session, kind, request_id, body)
    }

    /// Handle a message whose transfers are complete.
    fn secureplan_handle(&mut self, session: SessionId, kind: String, request_id: String, body: Value) -> Task<Message> {
        let task = match kind.as_str() {
            "openSession" => self.secureplan_open_session(session, &body),
            "sessionMode" => {
                if let (Some(mode), Some(bound)) = (Mode::parse(&body["mode"]), self.secureplan.sessions.by_session_mut(session)) {
                    bound.mode = mode;
                    let tab_id = bound.tab_id;
                    if mode == Mode::View {
                        self.command_line.push_info("SecurePlan: view only. Apply is not available.");
                        self.secureplan_close_form_dialogs(tab_id);
                    } else {
                        self.command_line.push_info("SecurePlan: editing enabled.");
                    }
                }
                Task::none()
            }
            "overlayUpdate" => {
                let id = body["overlayTransferId"].as_u64().unwrap_or_default() as u32;
                self.secureplan_overlay_update(session, id, body["surveyEmpty"].as_bool().unwrap_or(false));
                Task::none()
            }
            "applyProgress" => {
                self.secureplan_apply_progress(session, &request_id, &body);
                Task::none()
            }
            "applyResult" => self.secureplan_apply_result(session, &request_id, &body),
            "planUpdate" => self.secureplan_plan_update(session, &body),
            "exportRequest" => {
                // CAD export arrives in a later version (EXP-01..03).
                for key in ["drawingTransferId", "payloadTransferId"] {
                    if let Some(id) = body[key].as_u64() {
                        self.secureplan_take_transfer(session, id as u32);
                    }
                }
                self.command_line.push_error("SecurePlan: CAD export is not available in this version of SecurePlan CAD.");
                self.secureplan_send(
                    session,
                    json!({ "type": "exportResult", "requestId": request_id, "status": "error", "code": "INVALID" }),
                );
                Task::none()
            }
            // Conversion arrives in a later version (CNV-01..03).
            _ => Task::none(),
        };
        self.secureplan_report_states();
        task
    }

    // ── openSession ─────────────────────────────────────────────────────────

    fn secureplan_open_session(&mut self, session: SessionId, body: &Value) -> Task<Message> {
        let Some(pending) = self.secureplan.sessions.pending.remove(&session) else {
            // A second openSession for the same session.
            self.secureplan_protocol_error(session);
            return Task::none();
        };
        let overlay_id = body["overlayTransferId"].as_u64().unwrap_or_default() as u32;
        let overlay = self.secureplan_take_transfer(session, overlay_id).and_then(|transfer| {
            (transfer.media_type == "application/vnd.secureplan.overlay+json")
                .then(|| super::overlay::parse(transfer.bytes.expose()).ok())
                .flatten()
        });
        let drawing = match body["drawingTransferId"].as_u64() {
            None => None,
            Some(id) => self
                .secureplan_take_transfer(session, id as u32)
                .and_then(|transfer| Format::from_media_type(&transfer.media_type).map(|format| (transfer, format))),
        };
        let drawing_expected = body["drawingTransferId"].is_u64();
        let (Some(overlay), true) = (overlay, drawing.is_some() == drawing_expected) else {
            self.secureplan_protocol_error(session);
            return Task::none();
        };
        let mode = Mode::parse(&body["mode"]).unwrap_or(Mode::View);
        let label: String = body["surveyLabel"].as_str().unwrap_or("SecurePlan survey").to_string();
        let meta = PlanMeta::of(&body["cadPlan"], &body["baseIdentity"]);

        // Re-pairing to a document that is already open (BRG-05): reuse its tab.
        let existing = self
            .secureplan
            .sessions
            .bound
            .iter()
            .position(|b| b.origin == pending.origin && b.survey == pending.survey);
        let tab_id = match existing {
            Some(position) => {
                let old = self.secureplan.sessions.bound[position].session;
                if let (Some(old), Some(bridge)) = (old.filter(|old| *old != session), self.secureplan_bridge()) {
                    bridge.close(old, "superseded");
                }
                self.secureplan.sessions.bound[position].tab_id
            }
            None => {
                self.tab_counter += 1;
                let tab = crate::app::document::DocumentTab::new_drawing(self.tab_counter);
                let id = tab.id;
                self.tabs.push(tab);
                let index = self.tabs.len() - 1;
                self.active_tab = index;
                self.apply_display_defaults(index);
                self.tabs[index].is_start = false;
                self.secureplan.sessions.bound.push(Bound::new(id, &pending, mode));
                id
            }
        };
        let index = self.secureplan_tab_index(tab_id).unwrap_or(self.active_tab);
        let unapplied = existing.is_some() && self.secureplan_has_unapplied(index);
        // The same plan as before: the local edits stay as they are.
        let keep_local = unapplied && self.secureplan.sessions.by_tab(tab_id).is_some_and(|b| b.base_identity == meta.base_identity);
        // A changed plan replaces the document: keep the local work first,
        // under the plan it was made from, and replace nothing if that fails.
        let preserved = if unapplied && !keep_local { self.secureplan_keep_recovery(index) } else { Preserve::Nothing };
        {
            let bound = self.secureplan.sessions.by_tab_mut(tab_id).expect("bound above");
            bound.session = Some(session);
            bound.generation += 1;
            // What the page asks for now (openSession), over the launch's.
            bound.intent = body["intent"].as_str().and_then(Intent::parse).unwrap_or(pending.intent);
            bound.mode = mode;
            bound.label = label.clone().into();
            bound.overlay = Some(overlay);
            bound.survey_empty = body["surveyEmpty"].as_bool().unwrap_or(false);
            bound.busy = None;
            bound.error = None;
            bound.last_state = None;
            bound.apply = None;
            if keep_local {
                bound.plan_version = meta.plan_version;
                bound.has_plan = meta.has_plan;
            }
        }
        self.tabs[index].tab_title = label;
        self.active_tab = index;
        self.secureplan.command_guard.bind_tab(tab_id);
        self.secureplan_refresh_guards();
        if let Some(bridge) = self.secureplan_bridge() {
            bridge.set_document_open(&pending.origin, pending.survey.expose(), true);
        }
        self.command_line.push_info("SecurePlan: survey drawing opened.");
        super::testdriver_event("opened", if mode == Mode::Edit { "edit" } else { "view" });

        if keep_local {
            return Task::none();
        }
        if preserved == Preserve::Failed {
            self.secureplan_block_replacement(tab_id);
            return Task::none();
        }
        if preserved == Preserve::Saved {
            self.secureplan.dialog = Some(super::ui::Dialog::notice(
                "The plan changed",
                vec![
                    "The survey's plan changed in SecurePlan while this drawing was disconnected. Its current drawing is loaded.".into(),
                    "Your unapplied edits were kept as a recovery copy; SecurePlan CAD offers it the next time you open this survey.".into(),
                ],
            ));
        }
        self.secureplan_replace_plan(tab_id, meta, drawing, super::import::LoadPurpose::Open)
    }

    /// Replace the bound document's plan with `meta` and its drawing. The
    /// plan is adopted only once the drawing has opened; until then, and for
    /// good if it cannot open, Apply is blocked (PUB-06).
    fn secureplan_replace_plan(
        &mut self,
        tab_id: u64,
        meta: PlanMeta,
        drawing: Option<(Arc<Completed>, Format)>,
        purpose: super::import::LoadPurpose,
    ) -> Task<Message> {
        let Some(index) = self.secureplan_tab_index(tab_id) else { return Task::none() };
        // Every replacement makes loads started for an earlier one stale,
        // with or without a drawing of its own.
        self.secureplan_invalidate_loads(tab_id);
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.generation += 1;
        }
        match drawing {
            Some((transfer, format)) => {
                if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
                    bound.staged = Some(meta);
                    bound.apply = None;
                }
                let bytes = Arc::new(transfer.bytes.expose().clone());
                self.secureplan_load(tab_id, bytes, transfer.name.expose().clone(), format, purpose)
            }
            // No CAD drawing (a new survey, or the CAD link was removed).
            None => {
                self.secureplan_install(index, acadrust::CadDocument::new(), None);
                if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
                    bound.adopt(meta);
                    bound.pending_original = None;
                    bound.recovered_base = None;
                    bound.replace_confirmed = false;
                    bound.lost_entities = 0;
                    bound.apply = None;
                }
                if purpose == super::import::LoadPurpose::Open {
                    self.secureplan_after_open(tab_id)
                } else {
                    Task::none()
                }
            }
        }
    }

    /// The plan changed but the local work could not be kept: nothing is
    /// replaced and Apply stays blocked until the survey is reopened.
    fn secureplan_block_replacement(&mut self, tab_id: u64) {
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.unresolved = true;
            bound.staged = None;
            bound.apply = None;
        }
        self.secureplan.dialog = Some(super::ui::Dialog::notice(
            "The plan changed",
            vec![
                "The survey's plan changed in SecurePlan, but your unapplied edits could not be saved as a recovery copy.".into(),
                "Nothing was replaced, so your edits are still here. Apply is blocked: free some disk space, then open the survey from SecurePlan again.".into(),
            ],
        ));
        super::testdriver_event("replace-blocked", "");
    }

    /// After the survey's drawing is in place: offer a recovery copy, or
    /// start an import the web asked for.
    pub(crate) fn secureplan_after_open(&mut self, tab_id: u64) -> Task<Message> {
        if self.secureplan_offer_recovery(tab_id) {
            return Task::none();
        }
        let wants_import = self
            .secureplan
            .sessions
            .by_tab(tab_id)
            .is_some_and(|b| b.intent == Intent::Import && b.mode == Mode::Edit);
        let may_pick = super::native_dialogs_allowed() || cfg!(test);
        if wants_import && may_pick {
            return self.secureplan_start_import(tab_id);
        }
        Task::none()
    }

    /// Put `document` into tab `index` as a freshly opened drawing, loaded
    /// from `loaded` (verbatim bytes) or new when `None`.
    pub(crate) fn secureplan_install(&mut self, index: usize, document: acadrust::CadDocument, loaded: Option<Drawing>) {
        {
            let tab = &mut self.tabs[index];
            tab.scene.clear();
            tab.scene.document = document;
            tab.scene.load_named_parameters_from_document();
            tab.scene.load_parametric_constraints_from_document();
            tab.scene.material_base_dir = None;
            crate::app::style_ops::ensure_standard_styles(&mut tab.scene.document);
            crate::io::linetypes::populate_document(&mut tab.scene.document);
            tab.adopt_active_ucs_from_header();
            tab.current_path = None;
            tab.is_start = false;
            tab.active_layer = tab.scene.document.header.current_layer_name.clone();
            // The model-space annotation scale from CANNOSCALEVALUE and the
            // drawing's units, as opening a drawing does.
            let cannoscale = tab.scene.document.header.annotation_scale_value;
            let unit_factor = tab.scene.annotation_scale_unit_factor();
            tab.scene.annotation_scale =
                if cannoscale > 1e-9 { ((1.0 / cannoscale) / unit_factor) as f32 } else { (1.0 / unit_factor) as f32 };
            tab.scene.rebuild_derived_caches();
            tab.scene.current_layout = "Model".to_string();
            tab.scene.load_current_layout_state();
            tab.scene.reset_transient_visibility();
            tab.scene.restore_saved_camera();
            // The camera placed on open is not an edit of the drawing.
            tab.last_synced_camera_gen = tab.scene.camera_generation;
            tab.history = crate::app::document::HistoryState::default();
            tab.dirty = false;
            tab.recovery_save_as_required = false;
            tab.active_cmd = None;
        }
        self.adopt_header_sysvars(index);
        let tab_id = self.tabs[index].id;
        let revision = self.tabs[index].edit_revision;
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.loaded = loaded;
            bound.clean_revision = revision;
            bound.lost_entities = 0;
            bound.held_dirty = false;
            // Work started on the previous document is stale now.
            bound.generation += 1;
            bound.apply = None;
        }
        if index == self.active_tab {
            self.refresh_layer_panel();
        }
    }

    // ── Closing a bound tab ─────────────────────────────────────────────────

    /// Hook for `on_tab_close`: a bound tab with unapplied work asks the user
    /// to Apply, Discard or Keep it (DSK-03). `None` lets the close continue.
    pub(crate) fn secureplan_tab_closing(&mut self, index: usize) -> Option<Task<Message>> {
        if !self.secureplan_is_bound(index) {
            return None;
        }
        if self.secureplan_has_unapplied(index) {
            self.secureplan_open_close_dialog(self.tabs[index].id);
            return Some(Task::none());
        }
        self.secureplan_unbind(self.tabs[index].id);
        None
    }

    /// Forget a bound tab: its session ends with `documentClosed`.
    pub(crate) fn secureplan_unbind(&mut self, tab_id: u64) {
        let Some(position) = self.secureplan.sessions.bound.iter().position(|b| b.tab_id == tab_id) else { return };
        let bound = self.secureplan.sessions.bound.remove(position);
        self.secureplan.command_guard.unbind_tab(tab_id);
        if let Some(bridge) = self.secureplan_bridge() {
            bridge.set_document_open(&bound.origin, bound.survey.expose(), false);
            if let Some(session) = bound.session {
                bridge.close(session, "documentClosed");
            }
        }
        if let Some(session) = bound.session {
            self.secureplan.sessions.drop_session(session);
        }
        self.secureplan_refresh_guards();
    }

    /// Close a bound tab now, whatever its state (after the user decided).
    pub(crate) fn secureplan_close_tab(&mut self, tab_id: u64) -> Task<Message> {
        self.secureplan_unbind(tab_id);
        match self.secureplan_tab_index(tab_id) {
            Some(index) => {
                self.tabs[index].dirty = false;
                Task::batch([self.on_tab_close(index), self.continue_tab_close_queue()])
            }
            None => Task::none(),
        }
    }

    pub(crate) fn secureplan_window_title_for_bound(&self) -> Option<String> {
        let bound = self.secureplan_active_bound()?;
        let tab = self.tabs.get(self.active_tab)?;
        let dot = if tab.dirty || bound.unapplied() { "● " } else { "" };
        Some(format!("{dot}{} — SecurePlan — {} {}", bound.label.expose(), super::APP_NAME, super::VERSION))
    }
}

/// Whether the survey is empty for the first Apply (CON-03): no CAD plan yet,
/// and the web says it has no design elements and no comments. Its page then
/// goes to the survey origin.
pub fn survey_is_empty(bound: &Bound) -> bool {
    !bound.has_plan && bound.survey_empty
}

/// Whether the bound document may Apply now: a recovered drawing made from
/// another plan needs the explicit "Replace current plan with recovered
/// drawing" (DSK-03).
pub fn apply_allowed(bound: &Bound) -> Result<(), String> {
    if bound.unresolved {
        return Err("The survey's plan changed and this drawing is not up to date. Open the survey from SecurePlan again.".into());
    }
    if bound.staged.is_some() {
        return Err("The survey's new drawing is still loading.".into());
    }
    match &bound.recovered_base {
        Some(base) if *base != bound.base_identity && !bound.replace_confirmed => Err(
            "These edits were recovered from another version of the plan. Choose \"Replace current plan with recovered drawing\" to apply them."
                .into(),
        ),
        _ => Ok(()),
    }
}

/// What an Apply snapshot was taken under: its outputs may be sent only if
/// the session, plan and document are still the same (PUB-06).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ApplyOrigin {
    pub session: Option<SessionId>,
    pub base_identity: String,
    pub generation: u64,
}

/// A finished Apply build, carried back to the UI thread.
#[derive(Debug, Clone)]
pub struct ApplyBuilt {
    pub tab_id: u64,
    pub origin: ApplyOrigin,
    pub snapshot_revision: u64,
    pub plan: super::publish::ApplyPlan,
    pub alignment: Alignment,
    pub realigned: bool,
    pub result: super::Carry<Result<super::publish::ApplyOutputs, super::publish::ApplyError>>,
}

impl OpenCADStudio {
    // ── Apply (PUB-03, PUB-04) ──────────────────────────────────────────────

    /// Start Apply: finish or cancel the active command, freeze the drawing
    /// and show the Apply dialog (after alignment when there is none yet).
    pub(crate) fn secureplan_begin_apply(&mut self) -> Task<Message> {
        if let Err(message) = self.secureplan_can_edit_survey().and_then(|()| {
            self.secureplan_active_bound().map_or(Ok(()), apply_allowed)
        }) {
            self.command_line.push_error(&message);
            return Task::none();
        }
        let index = self.active_tab;
        let tab_id = self.tabs[index].id;
        let bound = self.secureplan.sessions.by_tab(tab_id).expect("checked above");
        if bound.alignment.is_none() {
            self.secureplan_open_align(true);
            return Task::none();
        }
        let cancelled = self.cancel_active_command_for_space_change();
        let Some(dialog) = self.secureplan_apply_dialog(index) else {
            self.command_line.push_error("SecurePlan: the drawing has nothing to publish.");
            return cancelled;
        };
        self.secureplan.dialog = Some(super::ui::Dialog::Apply(Box::new(dialog)));
        cancelled
    }

    /// The Apply dialog over a snapshot of tab `index`, taken now.
    pub(crate) fn secureplan_apply_dialog(&self, index: usize) -> Option<super::ui::apply_dialog::ApplyDialog> {
        let tab = &self.tabs[index];
        let bound = self.secureplan.sessions.by_tab(tab.id)?;
        let alignment = bound.alignment?;
        let extents = super::publish::visible_extents(&tab.scene)?;
        let snapshot = super::publish::Snapshot {
            document: tab.scene.document.clone(),
            annotation_scale: tab.scene.annotation_scale,
            loaded: bound.loaded.clone(),
            modified: self.secureplan_modified(index),
            pending_original: bound.pending_original.clone(),
            lost_entities: bound.lost_entities,
        };
        let mut dialog = super::ui::apply_dialog::ApplyDialog::new(
            tab.id,
            Arc::new(snapshot),
            tab.edit_revision,
            extents,
            alignment,
            survey_is_empty(bound),
            bound.realigned,
            bound.plan_version,
            super::layout::references(&tab.scene),
        );
        dialog.origin = ApplyOrigin { session: bound.session, base_identity: bound.base_identity.clone(), generation: bound.generation };
        // A paper layout the user is on starts chosen when it can be published.
        let current = &tab.scene.current_layout;
        if dialog.layouts.iter().any(|(name, found)| name == current && found.is_ok()) {
            dialog.select_view(Some(current));
        }
        Some(dialog)
    }

    /// Whether an Apply begun under `origin` still matches the document.
    fn secureplan_apply_current(&self, tab_id: u64, origin: &ApplyOrigin) -> bool {
        self.secureplan.sessions.by_tab(tab_id).is_some_and(|b| {
            b.session == origin.session
                && b.base_identity == origin.base_identity
                && b.generation == origin.generation
                && b.mode == Mode::Edit
                && apply_allowed(b).is_ok()
        })
    }

    /// An Apply begun on an earlier state of the survey: nothing is sent.
    fn secureplan_apply_stale(&mut self, tab_id: u64) -> Task<Message> {
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            if bound.busy.is_some_and(|b| b.operation == "apply") {
                bound.busy = None;
            }
        }
        self.secureplan_release_held(tab_id);
        self.secureplan.dialog = Some(super::ui::Dialog::notice(
            "Apply stopped",
            vec![
                "The survey changed in SecurePlan (its plan, the connection or the editing mode) while this Apply was being prepared.".into(),
                "Nothing was sent. Check the drawing, then Apply again.".into(),
            ],
        ));
        super::testdriver_event("apply-failed", "STALE");
        self.secureplan_report_states();
        Task::none()
    }

    /// Confirm the Apply dialog: build every output from its snapshot.
    pub(crate) fn secureplan_confirm_apply(&mut self) -> Task<Message> {
        let Some(super::ui::Dialog::Apply(dialog)) = &self.secureplan.dialog else { return Task::none() };
        let plan = match dialog.plan() {
            Ok(plan) => plan,
            Err(problem) => {
                self.command_line.push_error(&format!("SecurePlan: {problem}"));
                return Task::none();
            }
        };
        let Some(super::ui::Dialog::Apply(dialog)) = self.secureplan.dialog.take() else { return Task::none() };
        if !self.secureplan_apply_current(dialog.tab_id, &dialog.origin) {
            return self.secureplan_apply_stale(dialog.tab_id);
        }
        self.secureplan_build_apply(*dialog, plan)
    }

    pub(crate) fn secureplan_build_apply(&mut self, dialog: super::ui::apply_dialog::ApplyDialog, plan: super::publish::ApplyPlan) -> Task<Message> {
        let tab_id = dialog.tab_id;
        // From here the tab's dirty flag records only later changes (the
        // snapshot's own go into `held_dirty`), so a commit never clears a
        // change made while the Apply ran, whatever marked it.
        let Some(index) = self.secureplan_tab_index(tab_id) else { return Task::none() };
        let was_dirty = std::mem::take(&mut self.tabs[index].dirty);
        let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) else { return Task::none() };
        bound.held_dirty |= was_dirty;
        bound.busy = Some(Busy { operation: "apply", progress: None });
        bound.error = None;
        let alignment = Alignment { units: dialog.alignment.units, mapping: plan.mapping };
        let realigned = dialog.realigned;
        let snapshot = dialog.snapshot;
        let snapshot_revision = dialog.snapshot_revision;
        let origin = dialog.origin;
        self.secureplan_report_states();
        self.command_line.push_info("SecurePlan: preparing the drawing, PDF and snap file…");
        self.secureplan_run_job(move || {
            let result = super::publish::build_outputs(&snapshot, &plan);
            super::Msg::ApplyBuilt(ApplyBuilt { tab_id, origin, snapshot_revision, plan, alignment, realigned, result: super::Carry::new(result) })
        })
    }

    /// The outputs are ready: send them and `applyRequest`, or report why not.
    pub(crate) fn secureplan_apply_built(&mut self, built: ApplyBuilt) -> Task<Message> {
        let Some(result) = built.result.take() else { return Task::none() };
        // Built from a snapshot of an earlier plan, session or document: stale.
        if !self.secureplan_apply_current(built.tab_id, &built.origin) {
            return self.secureplan_apply_stale(built.tab_id);
        }
        let fail = |app: &mut Self, code: Option<ErrorCode>, message: String| {
            if let Some(bound) = app.secureplan.sessions.by_tab_mut(built.tab_id) {
                bound.busy = None;
                bound.error = code;
            }
            app.secureplan_release_held(built.tab_id);
            app.secureplan.dialog = Some(super::ui::Dialog::notice("Apply stopped", vec![message, "Nothing was sent. Your edits are kept.".into()]));
            super::testdriver_event("apply-failed", code.map_or("", ErrorCode::as_str));
            app.secureplan_report_states();
            Task::none()
        };
        let outputs = match result {
            Ok(outputs) => outputs,
            Err(error) => return fail(self, Some(error.code), error.message),
        };
        let (session, base_identity) = (built.origin.session, built.origin.base_identity.clone());
        let (Some(session), Some(bridge)) = (session, self.secureplan_bridge()) else {
            return fail(self, None, "SecurePlan is no longer connected. Open the survey from SecurePlan again, then Apply.".into());
        };
        let stem = outputs.drawing.stem();
        let send = |name: &str, media_type: &str, bytes: Arc<Vec<u8>>| bridge.send_transfer(session, name, media_type, bytes);
        let drawing_id = send(outputs.drawing.name.expose(), outputs.drawing.format.media_type(), Arc::clone(&outputs.drawing.bytes));
        let original_id = outputs
            .original
            .as_ref()
            .map(|original| (send(original.name.expose(), original.format.media_type(), Arc::clone(&original.bytes)), original.format_version.clone()));
        let pdf_id = send(&file_name(&format!("{stem}.pdf")), "application/pdf", Arc::new(outputs.pdf.clone()));
        let snap_id = send(&file_name(&format!("{stem}.spsnap")), "application/vnd.secureplan.snap", Arc::new(outputs.snap.clone()));
        let (Some(drawing_id), Some(pdf_id), Some(snap_id)) = (drawing_id, pdf_id, snap_id) else {
            return fail(self, Some(ErrorCode::TransferFailed), "The outputs could not be sent to SecurePlan.".into());
        };
        let original = match original_id {
            None => Value::Null,
            Some((Some(id), version)) => json!({ "transferId": id, "formatVersion": version }),
            Some((None, _)) => return fail(self, Some(ErrorCode::TransferFailed), "The original could not be sent to SecurePlan.".into()),
        };
        let p = outputs.transform.placement;
        let request_id = self.secureplan.sessions.next_request_id();
        let message = json!({
            "type": "applyRequest",
            "requestId": request_id,
            "baseIdentity": base_identity,
            "view": built.plan.view.to_json(),
            "cadUnits": built.alignment.units.as_str(),
            "mapping": built.alignment.mapping_json(),
            "realigned": built.realigned,
            "placement": {
                "widthPt": p.width_pt,
                "heightPt": p.height_pt,
                "centerX": p.center_x,
                "centerY": p.center_y,
                "widthMm": p.width_mm,
                "heightMm": p.height_mm,
            },
            "roundingBoundMm": p.rounding_bound_mm,
            "chordToleranceMm": p.chord_tolerance_mm,
            "drawing": { "transferId": drawing_id, "formatVersion": outputs.drawing.format_version },
            "original": original,
            "pdfTransferId": pdf_id,
            "snapTransferId": snap_id,
            "desktopVersion": super::VERSION,
            "forkCommit": super::FORK_COMMIT,
        });
        if !self.secureplan_send(session, message) {
            return fail(self, Some(ErrorCode::TransferFailed), "The request could not be sent to SecurePlan.".into());
        }
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(built.tab_id) {
            bound.apply = Some(ApplyInFlight { request_id, snapshot_revision: built.snapshot_revision, drawing: outputs.drawing, written: outputs.written });
            bound.alignment = Some(built.alignment);
        }
        let mut note = "SecurePlan: sent. Confirm the Apply in SecurePlan.".to_string();
        if outputs.omitted_images > 0 {
            note.push_str(&format!(" {} raster image(s) are not drawn in the published PDF.", outputs.omitted_images));
        }
        self.command_line.push_info(&note);
        super::testdriver_event("apply-sent", "");
        Task::none()
    }

    fn secureplan_apply_progress(&mut self, session: SessionId, request_id: &str, body: &Value) {
        let Some(bound) = self.secureplan.sessions.by_session_mut(session) else { return };
        if bound.apply.as_ref().is_none_or(|apply| apply.request_id != request_id) {
            return;
        }
        let progress = body["progress"].as_f64();
        bound.busy = Some(Busy { operation: "apply", progress });
        let step = match body["step"].as_str().unwrap_or_default() {
            "savingDraft" => "saving the design",
            "creatingRevision" => "creating a revision",
            "uploading" => "uploading",
            _ => "committing",
        };
        self.command_line.push_info(&format!("SecurePlan: {step}…"));
    }

    fn secureplan_apply_result(&mut self, session: SessionId, request_id: &str, body: &Value) -> Task<Message> {
        let Some(bound) = self.secureplan.sessions.by_session_mut(session) else { return Task::none() };
        let Some(apply) = bound.apply.take().filter(|apply| apply.request_id == request_id) else {
            return Task::none();
        };
        bound.busy = None;
        let tab_id = bound.tab_id;
        if body["status"] == "committed" {
            let version = body["planVersion"].as_u64();
            bound.base_identity = body["baseIdentity"].as_str().unwrap_or("none").to_string();
            bound.plan_version = version;
            bound.has_plan = true;
            bound.pending_original = None;
            bound.recovered_base = None;
            bound.replace_confirmed = false;
            bound.realigned = false;
            // Written by the writer, the stored drawing no longer holds the
            // damaged items the reader dropped.
            if apply.written {
                bound.lost_entities = 0;
            }
            bound.loaded = Some(apply.drawing);
            bound.held_dirty = false;
            bound.clean_revision = apply.snapshot_revision;
            let (origin, survey) = (bound.origin.clone(), bound.survey.expose().clone());
            if let Some(index) = self.secureplan_tab_index(tab_id) {
                // The tab's dirty flag now records only changes made after the
                // snapshot; an undo or redo since then changes the document too.
                if self.tabs[index].edit_revision != apply.snapshot_revision {
                    self.tabs[index].dirty = true;
                }
                // A recovery copy of later work stays.
                if !self.secureplan_has_unapplied(index) {
                    self.secureplan.recovery.delete(&origin, &survey);
                }
            }
            let version = version.map_or(String::new(), |v| format!(" as plan version {v}"));
            self.command_line.push_info(&format!("SecurePlan: applied{version}."));
            super::testdriver_event("applied", body["planVersion"].as_u64().map(|v| v.to_string()).as_deref().unwrap_or(""));
        } else {
            self.secureplan_release_held(tab_id);
            let code = body["code"].as_str().unwrap_or("SAVE_FAILED").to_string();
            let reason = match code.as_str() {
                "PLAN_CHANGED" => "The survey's plan changed in SecurePlan. The new plan is being loaded.",
                "LEASE_LOST" => "SecurePlan no longer holds the survey for editing.",
                "NOT_EDIT_MODE" => "SecurePlan opened this survey for viewing only.",
                "QUOTA" => "The survey has reached its file limit.",
                "INVALID" => "SecurePlan refused the outputs as invalid.",
                "CANCELLED" => "The Apply was cancelled in SecurePlan.",
                _ => "SecurePlan could not save the plan.",
            };
            let mut lines = vec![reason.to_string()];
            if let Some(detail) = body["detail"].as_str() {
                lines.push(detail.chars().take(500).collect());
            }
            lines.push("Your edits are kept; you can Apply again.".into());
            self.secureplan.dialog = Some(super::ui::Dialog::notice("Not applied", lines));
            super::testdriver_event("apply-failed", &code);
        }
        Task::none()
    }

    /// The survey's plan changed (PUB-06): keep unapplied work as a recovery
    /// copy under the old base identity, then load the new drawing. If the
    /// work cannot be kept, nothing is replaced; if the new drawing cannot be
    /// opened, the old one stays under its old plan and Apply is blocked.
    fn secureplan_plan_update(&mut self, session: SessionId, body: &Value) -> Task<Message> {
        let Some(tab_id) = self.secureplan.sessions.by_session_mut(session).map(|b| b.tab_id) else { return Task::none() };
        let Some(index) = self.secureplan_tab_index(tab_id) else { return Task::none() };
        let drawing = body["drawingTransferId"].as_u64().and_then(|id| self.secureplan_take_transfer(session, id as u32));
        let drawing = drawing.and_then(|t| Format::from_media_type(&t.media_type).map(|format| (t, format)));
        if body["drawingTransferId"].is_u64() && drawing.is_none() {
            self.secureplan_protocol_error(session);
            return Task::none();
        }
        let preserved = self.secureplan_keep_recovery(index);
        if preserved == Preserve::Failed {
            self.secureplan_block_replacement(tab_id);
            return Task::none();
        }
        let meta = PlanMeta::of(&body["cadPlan"], &body["baseIdentity"]);
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            // An Apply in flight belongs to the old plan.
            bound.apply = None;
            bound.busy = None;
        }
        let reason = match body["reason"].as_str().unwrap_or_default() {
            "applied" => "another Apply",
            "restored" => "a revision restore",
            "replaced" => "a replaced drawing",
            "removed" => "the CAD link was removed",
            _ => "a change",
        };
        let mut lines = vec![format!("The survey's plan changed in SecurePlan ({reason}). Its current drawing is being loaded.")];
        if preserved == Preserve::Saved {
            lines.push("Your unapplied edits were kept as a recovery copy; SecurePlan CAD offers it the next time you open this survey.".into());
        }
        self.secureplan.dialog = Some(super::ui::Dialog::notice("The plan changed", lines));
        self.secureplan_replace_plan(tab_id, meta, drawing, super::import::LoadPurpose::PlanUpdate)
    }

    // ── Dialog helpers ──────────────────────────────────────────────────────

    /// Close tab `tab_id`'s alignment or Apply dialog (its mode left `edit`).
    pub(crate) fn secureplan_close_form_dialogs(&mut self, tab_id: u64) {
        let owner = match &self.secureplan.dialog {
            Some(super::ui::Dialog::Align(dialog)) => Some(dialog.tab_id),
            Some(super::ui::Dialog::Apply(dialog)) => Some(dialog.tab_id),
            _ => None,
        };
        if owner == Some(tab_id) {
            self.secureplan.dialog = None;
        }
    }

    /// An Apply ended without a commit: changes taken into its snapshot are
    /// unapplied again.
    pub(crate) fn secureplan_release_held(&mut self, tab_id: u64) {
        let held = self.secureplan.sessions.by_tab_mut(tab_id).is_some_and(|b| std::mem::take(&mut b.held_dirty));
        if let (true, Some(index)) = (held, self.secureplan_tab_index(tab_id)) {
            self.tabs[index].dirty = true;
        }
    }

    /// Ask what to do with a bound tab's unapplied work before closing it.
    pub(crate) fn secureplan_open_close_dialog(&mut self, tab_id: u64) {
        use super::ui::{Action, Dialog};
        let can_apply = self.secureplan.sessions.by_tab(tab_id).is_some_and(|b| b.connected() && b.mode == Mode::Edit);
        let mut buttons = Vec::new();
        if can_apply {
            buttons.push(("Apply first".to_string(), Action::CloseApply(tab_id)));
        }
        buttons.push(("Discard edits".to_string(), Action::CloseDiscard(tab_id)));
        buttons.push(("Keep a recovery copy".to_string(), Action::CloseKeep(tab_id)));
        buttons.push(("Cancel".to_string(), Action::Dismiss));
        self.secureplan.dialog = Some(Dialog::choice(
            "Unapplied edits",
            vec!["This SecurePlan drawing has edits SecurePlan does not have yet.".into()],
            buttons,
        ));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::bridge::tests::{connect_web, next_event, test_bridge, Web, ORIGIN};
    use crate::app::secureplan::bridge::{self, Bridge, BridgeEvent};
    use crate::app::secureplan::channel::{self, FrameKind};
    use crate::app::secureplan::pairing::tests::launch;
    use crate::app::secureplan::ui::{Action, Dialog, DialogKey};
    use crate::app::secureplan::{overlay, publish, recovery, testutil, vectors, Msg};
    use std::sync::mpsc;
    use tungstenite::Message as Frame;

    pub(crate) const BASE: &str = "221bbc1c061a608c3c3f6dd11825b38060755cf4225abe8f419328376b538ff9";

    /// A desktop app paired with a web page over a real bridge session.
    pub(crate) struct Harness {
        pub app: OpenCADStudio,
        pub events: mpsc::Receiver<BridgeEvent>,
        pub web: Web,
        pub session: SessionId,
        next_transfer: u32,
        pub bridge: Arc<Bridge>,
        /// `surveyEmpty` for the next `openSession`.
        pub survey_empty: bool,
        /// Earlier web connections, kept open.
        old_webs: Vec<Web>,
        dir: std::path::PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    impl Harness {
        pub(crate) fn new(tag: &str) -> Self {
            let mut app = OpenCADStudio::new_for_test();
            app.automation_op(r#"{"op":"new"}"#);
            let dir = std::env::temp_dir().join(format!("secureplan_session_{tag}_{}", std::process::id()));
            app.secureplan.settings = Default::default();
            app.secureplan.settings_path = Some(dir.join("secureplan.json"));
            app.secureplan.recovery = recovery::Store::at(dir.join("recovery"));
            let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
            let events = bridge.take_events().unwrap();
            app.secureplan.bridge = Some(Arc::clone(&bridge));
            let request = launch(ORIGIN, 7);
            bridge.add_pending(request.clone());
            let web = connect_web(bridge.port(), &request).expect("pair");
            let opened = next_event(&events);
            let BridgeEvent::Opened { session, .. } = &opened else { panic!("expected Opened") };
            let session = *session;
            let _ = app.update(Message::SecurePlan(Msg::Bridge(opened)));
            Self { app, events, web, session, next_transfer: 1, bridge, survey_empty: true, old_webs: Vec::new(), dir }
        }

        pub(crate) fn dir(&self) -> &std::path::Path {
            std::fs::create_dir_all(&self.dir).unwrap();
            &self.dir
        }

        /// Deliver the next bridge event to the app.
        pub(crate) fn pump(&mut self) {
            let event = next_event(&self.events);
            let _ = self.app.update(Message::SecurePlan(Msg::Bridge(event)));
        }

        pub(crate) fn send(&mut self, message: Value) {
            self.web.send(&message);
            self.pump();
        }

        /// Send bytes from the web as a transfer; returns its id.
        pub(crate) fn transfer(&mut self, name: &str, media_type: &str, bytes: &[u8]) -> u32 {
            use sha2_011::{Digest, Sha256};
            let id = self.next_transfer;
            self.next_transfer += 1;
            let sha: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
            self.web.send(&json!({ "type": "transferStart", "requestId": format!("t{id}"), "transferId": id, "name": name, "mediaType": media_type, "byteLength": bytes.len(), "sha256": sha }));
            for (seq, piece) in bytes.chunks(super::super::transfer::MAX_CHUNK_PAYLOAD).enumerate() {
                let mut frame = id.to_le_bytes().to_vec();
                frame.extend_from_slice(&(seq as u32).to_le_bytes());
                frame.extend_from_slice(piece);
                let sealed = self.web.sealer.seal(FrameKind::Chunk, &frame).unwrap();
                self.web.send_raw(Frame::binary(sealed));
            }
            self.pump();
            id
        }

        /// Send a transfer with a chosen id.
        pub(crate) fn transfer_numbered(&mut self, id: u32, name: &str, media_type: &str, bytes: &[u8]) -> u32 {
            let next = self.next_transfer;
            self.next_transfer = id;
            let sent = self.transfer(name, media_type, bytes);
            self.next_transfer = next.max(id + 1);
            sent
        }

        /// Make the desktop send a `sessionState`, as a marker in the stream.
        pub(crate) fn send_state_probe(&mut self) {
            let index = self.app.active_tab;
            self.app.tabs[index].dirty = !self.app.tabs[index].dirty;
            self.app.secureplan_report_states();
        }

        /// `openSession` with an overlay and, optionally, the current drawing.
        pub(crate) fn open(&mut self, drawing: Option<(&str, Format, Vec<u8>)>, overlay: Vec<u8>, mode: &str, cad_plan: Value, base: &str, intent: &str) {
            let overlay_id = self.transfer("overlay.json", "application/vnd.secureplan.overlay+json", &overlay);
            let drawing_id = drawing.map(|(name, format, bytes)| self.transfer(name, format.media_type(), &bytes));
            let placement = if cad_plan.is_null() { Value::Null } else { sample("openSession-edit")["placement"].clone() };
            self.send(json!({
                "type": "openSession", "requestId": "open-1", "intent": intent, "mode": mode,
                "surveyLabel": "Synthetic survey", "cadPlan": cad_plan, "placement": placement,
                "baseIdentity": base, "drawingTransferId": drawing_id, "overlayTransferId": overlay_id,
                "surveyEmpty": self.survey_empty,
            }));
        }

        pub(crate) fn open_dxf(&mut self) {
            self.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        }

        pub(crate) fn tab_id(&self) -> u64 {
            self.app.tabs[self.app.active_tab].id
        }

        pub(crate) fn bound(&self) -> &Bound {
            self.app.secureplan.sessions.by_tab(self.tab_id()).expect("a bound document")
        }

        /// The next web-side control message of type `kind`, with every
        /// desktop transfer received before it (id → (transferStart, bytes)).
        pub(crate) fn receive(&mut self, kind: &str) -> (Value, HashMap<u64, (Value, Vec<u8>)>) {
            let mut transfers: HashMap<u64, (Value, Vec<u8>)> = HashMap::new();
            loop {
                match self.web.socket.read().expect("a frame") {
                    Frame::Text(text) => {
                        let sealed = channel::b64url_decode(text.as_str()).unwrap();
                        let message: Value = serde_json::from_slice(&self.web.opener.open(FrameKind::Control, &sealed).unwrap()).unwrap();
                        if message["type"] == "transferStart" {
                            transfers.insert(message["transferId"].as_u64().unwrap(), (message, Vec::new()));
                        } else if message["type"] == kind {
                            return (message, transfers);
                        }
                    }
                    Frame::Binary(sealed) => {
                        let chunk = self.web.opener.open(FrameKind::Chunk, &sealed).unwrap();
                        let id = u32::from_le_bytes(chunk[..4].try_into().unwrap()) as u64;
                        transfers.get_mut(&id).expect("announced").1.extend_from_slice(&chunk[8..]);
                    }
                    _ => {}
                }
            }
        }

        /// Read `sessionState` messages until one satisfies `wanted`.
        pub(crate) fn state_where(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
            loop {
                let (state, _) = self.receive("sessionState");
                if wanted(&state) {
                    return state;
                }
            }
        }

        /// Hold worker jobs (loads, Apply builds) until released.
        pub(crate) fn hold_jobs(&mut self) {
            self.app.secureplan.held_jobs = Some(Default::default());
        }

        pub(crate) fn held(&self) -> usize {
            self.app.secureplan.held_jobs.as_ref().map_or(0, |h| h.0.len())
        }

        pub(crate) fn release(&mut self, index: usize) {
            let _ = self.app.secureplan_release_job(index);
        }

        /// Pair again from the web: `survey` another survey, or `None` for a
        /// re-pair of the open one (confirmed in the desktop). The harness
        /// then talks over the new session.
        pub(crate) fn pair_again(&mut self, pairing: u8, survey: Option<&str>) {
            let mut request = launch(ORIGIN, pairing);
            if let Some(survey) = survey {
                request.survey = survey.to_string().into();
            }
            // A re-pair supersedes the open document's live session, if any.
            let superseded = if survey.is_none() { self.app.secureplan.sessions.by_tab(self.tab_id()).and_then(|b| b.session) } else { None };
            self.bridge.add_pending(request.clone());
            let web = connect_web(self.bridge.port(), &request).expect("pair again");
            self.old_webs.push(std::mem::replace(&mut self.web, web));
            loop {
                let event = next_event(&self.events);
                let opened = match &event {
                    BridgeEvent::Opened { session, needs_confirmation, .. } => Some((*session, *needs_confirmation)),
                    _ => None,
                };
                let _ = self.app.update(Message::SecurePlan(Msg::Bridge(event)));
                if let Some((session, needs_confirmation)) = opened {
                    self.session = session;
                    if needs_confirmation {
                        let _ = self.app.update(Message::SecurePlan(Msg::Action(Action::Repair(session, true))));
                    }
                    if let Some(old) = superseded {
                        self.pump_until_closed(old);
                    }
                    return;
                }
            }
        }

        /// Deliver bridge events until the app has seen `session` close.
        pub(crate) fn pump_until_closed(&mut self, session: SessionId) {
            loop {
                let event = next_event(&self.events);
                let done = matches!(&event, BridgeEvent::Closed { session: s, .. } if *s == session);
                let _ = self.app.update(Message::SecurePlan(Msg::Bridge(event)));
                if done {
                    return;
                }
            }
        }

        /// Make the recovery store unwritable (a file where its folder goes).
        pub(crate) fn break_recovery_store(&mut self) {
            let blocker = self.dir().join("blocker");
            std::fs::write(&blocker, b"not a folder").unwrap();
            self.app.secureplan.recovery = recovery::Store::at(blocker.join("recovery"));
        }

        pub(crate) fn entity_count(&self) -> usize {
            self.app.tabs[self.app.active_tab].scene.document.entities().count()
        }

        pub(crate) fn dialog_title(&self) -> Option<String> {
            match &self.app.secureplan.dialog {
                Some(Dialog::Choice { title, .. }) => Some(title.clone()),
                _ => None,
            }
        }

        pub(crate) fn key(&mut self, key: DialogKey) {
            let _ = self.app.update(Message::SecurePlan(Msg::DialogKey(key)));
        }

        /// Add a line to the drawing as an edit would.
        pub(crate) fn edit(&mut self, from: (f64, f64), to: (f64, f64)) {
            use acadrust::entities::Line;
            use acadrust::types::Vector3;
            let index = self.app.active_tab;
            let tab = &mut self.app.tabs[index];
            tab.scene.document.add_entity(acadrust::EntityType::Line(Line::from_points(Vector3::new(from.0, from.1, 0.0), Vector3::new(to.0, to.1, 0.0)))).unwrap();
            tab.scene.rebuild_derived_caches();
            tab.dirty = true;
            tab.edit_revision += 1;
        }
    }

    pub(crate) fn sample(name: &str) -> Value {
        vectors::json("messages/valid.json")["samples"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap()["message"].clone()
    }

    fn last_error(app: &OpenCADStudio) -> String {
        app.command_line.last_error.clone().unwrap_or_default()
    }

    // ── F4a ─────────────────────────────────────────────────────────────────

    #[test]
    fn open_session_loads_the_drawing_into_a_bound_document() {
        let mut h = Harness::new("open");
        h.open_dxf();
        let (state, _) = h.receive("sessionState");
        assert_eq!(state["dirty"], false);
        assert_eq!(state["activeView"]["kind"], "model");
        assert_eq!(h.bound().session, Some(h.session));
        let tab = &h.app.tabs[h.app.active_tab];
        assert!(tab.current_path.is_none(), "a bound document has no file");
        assert!(h.app.recent_files.is_empty(), "no recent-file entry");
        assert!(tab.scene.document.entities().count() >= 8);
        assert_eq!(tab.tab_display_name(), "Synthetic survey");
        let title = h.app.secureplan_window_title();
        assert!(title.contains("Synthetic survey") && title.contains("SecurePlan"), "{title}");
        assert_eq!(h.bound().loaded.as_ref().unwrap().bytes.as_slice(), testutil::synthetic_dxf().as_slice());
        assert_eq!(h.bound().loaded.as_ref().unwrap().format_version, "AC1032");
        // References are refused while it is open, and allowed again after.
        use crate::app::secureplan::guards::{external_resource_allowed, ExternalResource};
        assert!(!external_resource_allowed(ExternalResource::Xref) && !external_resource_allowed(ExternalResource::Image));
        let index = h.app.active_tab;
        let _ = h.app.update(Message::TabClose(index));
        assert!(external_resource_allowed(ExternalResource::Xref));
        let (close, _) = h.receive("close");
        assert_eq!(close["reason"], "documentClosed");
    }

    #[test]
    fn a_message_waits_for_the_transfers_it_names() {
        let mut h = Harness::new("deferred");
        h.send(json!({
            "type": "openSession", "requestId": "open-1", "intent": "edit", "mode": "edit",
            "surveyLabel": "Synthetic survey", "cadPlan": null, "placement": null,
            "baseIdentity": "none", "drawingTransferId": 2, "overlayTransferId": 1, "surveyEmpty": true,
        }));
        assert!(h.app.secureplan.sessions.bound.is_empty(), "handled before its transfers");
        h.transfer("overlay.json", "application/vnd.secureplan.overlay+json", &overlay::tests::overlay_bytes(&[]));
        assert!(h.app.secureplan.sessions.bound.is_empty());
        h.transfer("synthetic.dxf", "image/vnd.dxf", &testutil::synthetic_dxf());
        assert_eq!(h.bound().loaded.as_ref().unwrap().bytes.as_slice(), testutil::synthetic_dxf().as_slice());
    }

    #[test]
    fn a_bound_document_refuses_saving_plotting_exports_and_xref_reads() {
        let mut h = Harness::new("refuse");
        h.open_dxf();
        let index = h.app.active_tab;
        h.edit((1.0, 1.0), (2.0, 2.0));
        for message in [Message::SaveFile, Message::SaveAs, Message::PlotDialogOpen, Message::PrintAllOpen, Message::PrintToPrinter] {
            let label = format!("{message:?}");
            h.app.command_line.last_error = None;
            let _ = h.app.update(message);
            assert!(last_error(&h.app).contains("Use Apply"), "{label} was not refused");
        }
        for command in ["PLOT", "WBLOCK", "_saveas", "QSAVE", "XATTACH", "XRELOAD", "EXPORTPDF", "DXFOUT"] {
            h.app.command_line.last_error = None;
            let _ = h.app.dispatch_command(command);
            assert!(h.app.tabs[index].active_cmd.is_none(), "{command} started");
            assert!(last_error(&h.app).contains("not available for a SecurePlan drawing"), "{command}: {}", last_error(&h.app));
        }
        // No save path writes it, whatever the entry point.
        let _ = h.app.save_with_default_format(index);
        assert!(h.app.active_save_jobs.is_empty());
        let target = h.dir().join("escape.dxf");
        let _ = h.app.queue_native_save(index, target.clone(), acadrust::DxfVersion::AC1032, crate::app::SavePurpose::Manual, crate::app::SaveContinuation::None, true, false);
        assert!(!target.exists() && h.app.active_save_jobs.is_empty());
        // Xrefs are never read from disk for it.
        let inner = h.dir().join("inner.dwg");
        std::fs::write(&inner, crate::io::save_to_bytes(&acadrust::CadDocument::new(), "dwg", acadrust::DxfVersion::AC1032).unwrap()).unwrap();
        let mut host = acadrust::CadDocument::new();
        let mut record = acadrust::tables::BlockRecord::new("INNER");
        record.flags.is_xref = true;
        record.xref_path = inner.to_string_lossy().into_owned();
        host.block_records.add(record).unwrap();
        let (infos, _) = crate::io::xref::resolve_xrefs(&mut host, h.dir());
        assert!(infos.iter().all(|info| matches!(info.status, crate::io::xref::XrefStatus::NotFound)));
    }

    #[test]
    fn data_links_of_a_bound_document_are_never_read_or_written() {
        use acadrust::objects::{ClassObject, ClassObjectData, DataLink, ObjectType};
        let mut h = Harness::new("datalink");
        h.open_dxf();
        let csv = h.dir().join("linked.csv");
        std::fs::write(&csv, "a,b\nc,d\n").unwrap();
        let before = std::fs::read(&csv).unwrap();
        let index = h.app.active_tab;
        // A table linked to a local file, and one to another computer.
        for target in [csv.to_string_lossy().into_owned(), format!("//qa-host/share/{}", "linked.csv")] {
            let doc = &mut h.app.tabs[index].scene.document;
            let handle = doc.allocate_handle();
            let link = DataLink { connection_string: target, option: 1, path_option: 1, ..Default::default() };
            let mut object = ClassObject::new(ClassObjectData::DataLink(link));
            object.handle = handle;
            doc.objects.insert(handle, ObjectType::ClassObject(object));
            let mut table = acadrust::entities::Table::new(acadrust::types::Vector3::ZERO, 1, 1);
            table.rows[0].cells[0].data_link_handle = Some(handle);
            doc.add_entity(acadrust::EntityType::Table(table)).unwrap();
            assert!(crate::app::annotation_data::read_data_link(doc, handle).is_err(), "the link was read");
        }
        for command in ["DATALINKUPDATE", "DATALINKUPDATE WRITE"] {
            h.app.command_line.last_error = None;
            let _ = h.app.dispatch_command(command);
            assert!(!last_error(&h.app).is_empty(), "{command} was not refused");
        }
        // Even run past the command guard, the write finds no path.
        h.app.secureplan.command_guard.unbind_tab(h.tab_id());
        let _ = h.app.dispatch_command("DATALINKUPDATE WRITE");
        assert_eq!(std::fs::read(&csv).unwrap(), before, "the linked file was written");
    }

    #[test]
    fn extraction_and_layer_translation_never_write_a_bound_drawing_to_files() {
        let mut h = Harness::new("writes");
        h.open_dxf();
        let index = h.app.active_tab;
        let out = h.dir().join("extracted.csv");
        let settings = h.dir().join("extraction.dxex");
        // The wizard opened through an alias the command guard does not
        // list, finished with a file output and a settings file.
        h.app.open_data_extraction();
        h.app.data_extraction.output_file = true;
        h.app.data_extraction.output_path = out.to_string_lossy().into_owned();
        h.app.data_extraction.settings_path = settings.to_string_lossy().into_owned();
        h.app.data_extraction.properties =
            vec![crate::ui::window::annotation_data::ExtractionProperty { key: "Layer".into(), name: "Layer".into(), category: "General".into(), checked: true }];
        let _ = h.app.update(Message::DataExtractionFinish);
        assert!(!out.exists() && !settings.exists(), "the wizard wrote a file");
        let _ = h.app.update(Message::DataExtractionSaveResult("a,b\n".into(), Some(out.clone())));
        assert!(!out.exists());
        // LAYTRANS: its mappings file and its log (named after the survey).
        let mappings = h.dir().join("mappings.txt");
        h.app.layer_translator = Some(crate::ui::window::layer_translator::State { write_log: true, ..Default::default() });
        let _ = h.app.update(Message::LayerTranslatorMappingsPath(mappings.clone(), true));
        assert!(!mappings.exists(), "the mappings were written");
        let log = std::path::PathBuf::from(&h.app.tabs[index].tab_title).with_extension("laytrans.log");
        let _ = h.app.update(Message::LayerTranslatorTranslate);
        let written = log.exists();
        std::fs::remove_file(&log).ok();
        assert!(!written, "the translation log was written");
        // Plotting and printing paths, whichever dialog reached them.
        for message in [Message::PlotExportPath(Some(h.dir().join("plot.pdf"))), Message::PrintAllPdfPath(Some(h.dir().join("all.pdf")))] {
            let _ = h.app.update(message);
        }
        assert!(!h.dir().join("plot.pdf").exists() && !h.dir().join("all.pdf").exists());
    }

    #[test]
    fn layer_changes_mark_the_document_dirty_and_are_reported() {
        let mut h = Harness::new("layers");
        h.open_dxf();
        let index = h.app.active_tab;
        let row = h.app.tabs[index].layers.layers.iter().position(|l| l.name == "0").expect("layer 0");
        let _ = h.app.update(Message::LayerToggleVisible(row));
        assert!(h.app.tabs[index].dirty, "turning a layer off is an edit");
        let _ = h.app.update(Message::SecurePlan(Msg::Tick));
        h.state_where(|state| state["dirty"] == true);
        let _ = h.app.update(Message::Undo);
        let _ = h.app.update(Message::LayerToggleFreeze(row));
        assert!(h.app.tabs[index].dirty, "freezing a layer is an edit");
        assert!(h.app.secureplan_modified(index));
    }

    #[test]
    fn the_web_decides_the_mode() {
        let mut h = Harness::new("mode");
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "view", Value::Null, "none", "view");
        assert_eq!(h.bound().mode, Mode::View);
        let _ = h.app.dispatch_command("SECUREPLANAPPLY");
        assert!(last_error(&h.app).contains("viewing only"));
        assert!(h.app.secureplan.dialog.is_none());
        h.send(json!({ "type": "sessionMode", "requestId": "m1", "mode": "edit", "reason": "leaseAcquired" }));
        assert_eq!(h.bound().mode, Mode::Edit);
        let _ = h.app.dispatch_command("SECUREPLANAPPLY");
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "Apply starts with the alignment");
        h.send(json!({ "type": "sessionMode", "requestId": "m2", "mode": "view", "reason": "leaseLost" }));
        assert!(h.app.secureplan.dialog.is_none(), "losing edit mode closes the dialog");
    }

    #[test]
    fn the_secureplan_tab_is_a_keyboard_menu_too() {
        let mut h = Harness::new("menu");
        let _ = h.app.dispatch_command("SECUREPLAN");
        let Some(Dialog::Choice { form, .. }) = &h.app.secureplan.dialog else { panic!("no menu") };
        let labels: Vec<&str> = form.buttons.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["Import drawing", "Align", "Apply", "Design overlay", "Trusted websites", "Revoke trust", "Developer origins", "Close"]);
        assert_eq!(form.focus, 0, "focus starts on the first action");
        // Down to "Developer origins", then Enter.
        for _ in 0..6 {
            h.key(DialogKey::Next);
        }
        h.key(DialogKey::Activate);
        assert!(h.app.secureplan.dialog.is_none());
        assert!(h.app.secureplan.settings.developer_loopback_origins, "the focused action ran");
    }

    // ── F4r ─────────────────────────────────────────────────────────────────

    #[test]
    fn autosave_keeps_a_user_only_recovery_copy_and_nothing_beside_a_file() {
        let mut h = Harness::new("autosave");
        h.open_dxf();
        h.edit((100.0, 100.0), (200.0, 200.0));
        let index = h.app.active_tab;
        let _ = h.app.update(Message::AutoSave);
        let entry = h.app.secureplan.recovery.load(ORIGIN, h.bound().survey.expose()).expect("a recovery copy");
        assert_eq!(entry.base_identity, "none");
        let (document, _) = crate::app::secureplan::import::load_drawing("x", entry.drawing.bytes.as_ref().clone()).unwrap();
        assert_eq!(document.entities().count(), h.app.tabs[index].scene.document.entities().count(), "the copy holds the edit");
        assert!(!h.app.autosave_target(index).exists(), "no .sv$ file");
        assert!(h.app.active_save_jobs.is_empty(), "no save, so no .bak and no thumbnail");
    }

    #[test]
    fn closing_with_edits_asks_and_keep_or_discard_decide_the_copy() {
        for keep in [true, false] {
            let mut h = Harness::new(if keep { "close_keep" } else { "close_discard" });
            h.open_dxf();
            h.edit((1.0, 1.0), (5.0, 5.0));
            let index = h.app.active_tab;
            let tab_id = h.tab_id();
            let _ = h.app.update(Message::TabClose(index));
            let Some(Dialog::Choice { form, .. }) = &h.app.secureplan.dialog else { panic!("no close prompt") };
            let labels: Vec<&str> = form.buttons.iter().map(|(l, _)| l.as_str()).collect();
            assert_eq!(labels, ["Apply first", "Discard edits", "Keep a recovery copy", "Cancel"]);
            assert!(h.app.secureplan_tab_index(tab_id).is_some(), "nothing closed before the answer");
            let action = if keep { Action::CloseKeep(tab_id) } else { Action::CloseDiscard(tab_id) };
            let _ = h.app.update(Message::SecurePlan(Msg::Action(action)));
            assert!(h.app.secureplan_tab_index(tab_id).is_none(), "closed");
            assert_eq!(h.app.secureplan.recovery.load(ORIGIN, "7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f").is_some(), keep);
            let (close, _) = h.receive("close");
            assert_eq!(close["reason"], "documentClosed");
        }
    }

    #[test]
    fn a_recovery_copy_is_offered_on_re_pair_and_another_plan_needs_the_explicit_replace() {
        let mut h = Harness::new("offer");
        let drawing = Drawing { bytes: Arc::new(testutil::synthetic_dxf()), name: "synthetic.dxf".to_string().into(), format: Format::Dxf, format_version: "AC1032".into() };
        let entry = recovery::Entry {
            origin: ORIGIN.into(),
            survey: "7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f".into(),
            base_identity: "none".into(),
            plan_version: None,
            saved: std::time::SystemTime::now(),
            drawing: drawing.clone(),
            original: Some(drawing),
            lost_entities: 0,
            alignment: None,
        };
        h.app.secureplan.recovery.save(&entry).unwrap();
        // The survey now has a plan: the copy was made from another one.
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", sample("openSession-edit")["cadPlan"].clone(), BASE, "edit");
        let Some(Dialog::Choice { form, lines, .. }) = &h.app.secureplan.dialog else { panic!("no recovery offer") };
        assert_eq!(form.buttons[0].0, "Replace current plan with recovered drawing");
        assert!(lines.iter().any(|l| l.contains("no plan") && l.contains("plan version 1")), "both versions shown: {lines:?}");
        // Without the explicit choice Apply is refused.
        let mut refused = Bound { recovered_base: Some("none".into()), replace_confirmed: false, ..h.bound().clone() };
        assert!(apply_allowed(&refused).is_err());
        refused.replace_confirmed = true;
        assert!(apply_allowed(&refused).is_ok());
        let tab_id = h.tab_id();
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::RecoveryRestore(tab_id))));
        assert!(h.app.secureplan_has_unapplied(h.app.active_tab));
        assert!(h.bound().replace_confirmed && h.bound().pending_original.is_some());
        assert!(apply_allowed(h.bound()).is_ok());
    }

    // ── F4b ─────────────────────────────────────────────────────────────────

    #[test]
    fn import_keeps_the_original_byte_for_byte_and_reads_no_references() {
        use acadrust::objects::{ImageDefinition, ObjectType};
        let mut h = Harness::new("import");
        // A drawing referencing a local image, a remote one and an Xref.
        let dir = h.dir().to_path_buf();
        let png = dir.join("pixel.png");
        image::RgbaImage::new(1, 1).save(&png).unwrap();
        let mut doc = testutil::synthetic_document();
        for reference in [png.to_string_lossy().into_owned(), "https://example.invalid/plan.png".to_string()] {
            let handle = doc.allocate_handle();
            let mut definition = ImageDefinition::with_dimensions(&reference, 1, 1);
            definition.handle = handle;
            doc.objects.insert(handle, ObjectType::ImageDefinition(definition));
            let mut image = acadrust::entities::RasterImage::new(&reference, acadrust::types::Vector3::ZERO, 1.0, 1.0);
            image.definition_handle = Some(handle);
            doc.add_entity(acadrust::EntityType::RasterImage(image)).unwrap();
        }
        let mut record = acadrust::tables::BlockRecord::new("OUTSIDE");
        record.flags.is_xref = true;
        record.xref_path = dir.join("outside.dwg").to_string_lossy().into_owned();
        doc.block_records.add(record).unwrap();
        // Without a SecurePlan drawing open the local image would be read.
        assert!(!publish::tests::scene_images(doc.clone()).is_empty(), "the fixture image resolves");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
        let file = dir.join("referencing.dxf");
        let bytes = crate::io::save_to_bytes(&doc, "dxf", acadrust::DxfVersion::AC1032).unwrap();
        std::fs::write(&file, &bytes).unwrap();

        let tab_id = h.tab_id();
        let _ = h.app.secureplan_import_path(tab_id, &file);
        let Some(Dialog::Choice { title, lines, .. }) = &h.app.secureplan.dialog else { panic!("no import report") };
        assert_eq!(title, "Drawing imported");
        assert!(lines.iter().any(|l| l.contains("Xref")) && lines.iter().any(|l| l.contains("image")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Extents:")) && lines.iter().any(|l| l == "Declared units: mm"));
        assert_eq!(h.bound().pending_original.as_ref().unwrap().bytes.as_slice(), bytes.as_slice(), "the original byte for byte");
        assert!(h.app.tabs[h.app.active_tab].scene.images.is_empty(), "no image was read");
        assert!(h.bound().alignment.is_none(), "a new original must be aligned");
        let _ = h.app.update(Message::SecurePlan(Msg::Tick));
        h.state_where(|state| state["dirty"] == true && state["busy"].is_null());

        // An invalid file changes nothing.
        let bad = dir.join("bad.dwg");
        std::fs::write(&bad, b"AC1032 not really").unwrap();
        let before = h.app.tabs[h.app.active_tab].scene.document.entities().count();
        let _ = h.app.secureplan_import_path(tab_id, &bad);
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Choice { title, .. }) if title == "Import failed"));
        assert_eq!(h.app.tabs[h.app.active_tab].scene.document.entities().count(), before);
        assert_eq!(h.bound().pending_original.as_ref().unwrap().bytes.as_slice(), bytes.as_slice());
    }

    // ── F7 ──────────────────────────────────────────────────────────────────

    #[test]
    fn the_overlay_is_replaced_on_update_and_never_enters_the_drawing() {
        let mut h = Harness::new("overlay");
        let first = overlay::tests::overlay_bytes(&[([0.0, 0.0], [1000.0, 0.0]), ([1000.0, 0.0], [1000.0, 1000.0])]);
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), first, "edit", Value::Null, "none", "edit");
        let index = h.app.active_tab;
        let entities = h.app.tabs[index].scene.document.entities().count();
        assert_eq!(h.bound().overlay.as_ref().unwrap().walls.len(), 2);
        let second = overlay::tests::overlay_bytes(&[([5.0, 5.0], [6.0, 6.0])]);
        let id = h.transfer("overlay.json", "application/vnd.secureplan.overlay+json", &second);
        h.send(json!({ "type": "overlayUpdate", "requestId": "o1", "overlayTransferId": id, "surveyEmpty": false }));
        let walls = &h.bound().overlay.as_ref().unwrap().walls;
        assert_eq!(walls.len(), 1, "replaced, not merged");
        assert_eq!(walls[0].1, [5.0, 5.0]);
        assert_eq!(h.app.tabs[index].scene.document.entities().count(), entities, "never a drawing entity");
        let _ = h.app.dispatch_command("SELECTALL");
        let _ = h.app.dispatch_command("AI_SELALL");
        assert!(h.app.tabs[index].scene.selected.len() <= entities, "nothing of the overlay is selectable");
        // The label and the toggle.
        assert!(h.app.secureplan_viewport_overlay().is_some());
        let _ = h.app.dispatch_command("SECUREPLANOVERLAY");
        assert!(h.app.secureplan_viewport_overlay().is_none());
    }

    // ── F4c ─────────────────────────────────────────────────────────────────

    #[test]
    fn a_stored_mapping_is_reused_and_a_new_original_is_aligned_first() {
        let mut h = Harness::new("reuse");
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", sample("openSession-edit")["cadPlan"].clone(), BASE, "edit");
        let stored = crate::app::secureplan::align::Alignment::from_cad_plan(&sample("openSession-edit")["cadPlan"]).unwrap();
        assert_eq!(h.bound().alignment, Some(stored));
        let _ = h.app.dispatch_command("SECUREPLANAPPLY");
        let Some(Dialog::Apply(dialog)) = &h.app.secureplan.dialog else { panic!("Apply goes straight to its dialog") };
        assert_eq!(dialog.plan().unwrap().mapping, stored.mapping, "the stored mapping");
        assert!(!dialog.realigned);
        h.key(DialogKey::Cancel);
        // Re-aligning is disclosed.
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        let Some(Dialog::Align(_)) = &h.app.secureplan.dialog else { panic!("no alignment dialog") };
        h.app.secureplan.dialog.as_mut().unwrap().form_mut().focus = 3;
        h.key(DialogKey::Right);
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::AlignConfirm)));
        assert!(h.bound().realigned);
        assert_eq!(h.bound().alignment.unwrap().mapping.quarter_turns, (stored.mapping.quarter_turns + 1) % 4);
    }

    // ── F5 ──────────────────────────────────────────────────────────────────

    /// Align with the drawing's own units (Enter on the dialog), which then
    /// opens the Apply dialog.
    fn align_and_open_apply(h: &mut Harness) {
        let _ = h.app.dispatch_command("SECUREPLANAPPLY");
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))));
        h.key(DialogKey::Activate);
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Apply(_))), "the Apply dialog follows");
    }

    #[test]
    fn apply_sends_every_output_from_one_snapshot() {
        let mut h = Harness::new("apply");
        h.open_dxf();
        align_and_open_apply(&mut h);
        let Some(Dialog::Apply(dialog)) = &h.app.secureplan.dialog else { unreachable!() };
        // The default window: the visible extents plus 2% per side.
        let [x0, y0, x1, y1] = dialog.default_window;
        assert!((x0 + 600.0).abs() < 1.0 && (y0 + 360.0).abs() < 1.0 && (x1 - 30600.0).abs() < 1.0 && (y1 - 18360.0).abs() < 1.0, "{:?}", dialog.default_window);
        // An edit attempted after the snapshot was frozen.
        h.edit((29000.0, 17000.0), (29500.0, 17500.0));
        h.key(DialogKey::Activate);
        let (request, transfers) = h.receive("applyRequest");
        assert_eq!(request["baseIdentity"], "none");
        assert_eq!(request["original"], Value::Null);
        assert_eq!(request["view"]["kind"], "model");
        assert_eq!(request["cadUnits"], "mm");
        let placement = &request["placement"];
        // First Apply on an empty survey: the page sits at the origin.
        let (w, h_mm) = (placement["widthMm"].as_f64().unwrap(), placement["heightMm"].as_f64().unwrap());
        assert!((placement["centerX"].as_f64().unwrap() - w / 2.0).abs() < 1e-9 && (placement["centerY"].as_f64().unwrap() - h_mm / 2.0).abs() < 1e-9);
        let bytes_of = |key: &str| transfers[&request[key].as_u64().unwrap()].1.clone();
        let drawing = transfers[&request["drawing"]["transferId"].as_u64().unwrap()].clone();
        assert_eq!(drawing.1, testutil::synthetic_dxf(), "unmodified at the snapshot: the verbatim bytes");
        assert_eq!(drawing.0["mediaType"], "image/vnd.dxf");
        let pdf = bytes_of("pdfTransferId");
        let snap = bytes_of("snapTransferId");
        let (wp, hp) = (placement["widthPt"].as_u64().unwrap() as u32, placement["heightPt"].as_u64().unwrap() as u32);
        let geometry = crate::app::secureplan::snap::tests::read(&snap, (wp, hp)).expect("a valid snap file");
        // The PDF and the snaps agree, and neither has the late edit.
        let plan = publish::ApplyPlan {
            view: publish::PublishedView::Model { window_cad: [x0, y0, x1, y1] },
            mapping: crate::app::secureplan::align::Alignment::from_cad_plan(&json!({ "cadUnits": "mm", "mapping": request["mapping"] })).unwrap().mapping,
            mm_per_pt: w / wp as f64,
            damaged_acknowledged: false,
        };
        let transform = plan.transform().unwrap();
        let points = publish::tests::content_points(&pdf);
        let near = |p: (f64, f64), set: &[[f64; 2]]| set.iter().any(|q| (q[0] - p.0).hypot(q[1] - p.1) < 0.01);
        let known = transform.apply(12345.6, 7890.1);
        let late = transform.apply(29500.0, 17500.0);
        let snaps: Vec<[f64; 2]> = geometry.points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect();
        assert!(near(known, &points) && near(known, &snaps));
        assert!(!near(late, &points) && !near(late, &snaps), "the edit leaked into the outputs");
        // Committed: the edit made after the snapshot keeps the document dirty.
        let request_id = request["requestId"].clone();
        h.send(json!({ "type": "applyResult", "requestId": request_id, "status": "committed", "planVersion": 1, "baseIdentity": BASE }));
        assert_eq!(h.bound().base_identity, BASE);
        assert_eq!(h.bound().plan_version, Some(1));
        assert!(h.app.tabs[h.app.active_tab].dirty);
        assert!(h.bound().apply.is_none() && h.bound().busy.is_none());
    }

    #[test]
    fn after_an_edit_the_drawing_is_written_and_the_original_sent_separately() {
        let mut h = Harness::new("original");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
        let file = h.dir().join("synthetic.dwg");
        let original = testutil::synthetic_dwg();
        std::fs::write(&file, &original).unwrap();
        let tab_id = h.tab_id();
        let _ = h.app.secureplan_import_path(tab_id, &file);
        h.key(DialogKey::Activate); // the import report
        h.edit((100.0, 100.0), (900.0, 100.0));
        align_and_open_apply(&mut h);
        h.key(DialogKey::Activate);
        let (request, transfers) = h.receive("applyRequest");
        let drawing = &transfers[&request["drawing"]["transferId"].as_u64().unwrap()];
        let sent_original = &transfers[&request["original"]["transferId"].as_u64().unwrap()];
        assert_eq!(sent_original.1, original, "the original, byte for byte");
        assert_eq!(sent_original.0["name"], "synthetic.dwg");
        assert_eq!(request["original"]["formatVersion"], "AC1032");
        assert_ne!(drawing.1, original, "the edited drawing is written");
        assert_eq!(drawing.0["mediaType"], "image/vnd.dwg");
        assert_eq!(request["drawing"]["formatVersion"], "AC1032", "in the original format and version");
        let written = crate::io::load_bytes("x.dwg", drawing.1.clone()).unwrap();
        let expected = testutil::synthetic_document().entities().count() + 1;
        assert_eq!(written.entities().count(), expected, "the writer output holds the edit");
        // An error keeps the edits.
        h.send(json!({ "type": "applyResult", "requestId": request["requestId"], "status": "error", "code": "LEASE_LOST", "detail": null }));
        assert!(h.app.secureplan_has_unapplied(h.app.active_tab));
        assert!(h.bound().pending_original.is_some());
    }

    #[test]
    fn outputs_repeat_exactly_and_known_loss_or_size_stop_apply() {
        let scene = publish::tests::synthetic_dxf_scene();
        let loaded = Drawing { bytes: Arc::new(testutil::synthetic_dxf()), name: "synthetic.dxf".to_string().into(), format: Format::Dxf, format_version: "AC1032".into() };
        let snapshot = publish::Snapshot { document: scene.document.clone(), annotation_scale: 1.0, loaded: Some(loaded.clone()), modified: true, pending_original: None, lost_entities: 0 };
        let window = [0.0, 0.0, 30000.0, 18000.0];
        let mapping = crate::app::secureplan::align::mapping_at(window, 1.0, 0, [0.0, 0.0]);
        let plan = publish::ApplyPlan { view: publish::PublishedView::Model { window_cad: window }, mapping, mm_per_pt: publish::choose_mm_per_pt(window, &mapping).unwrap(), damaged_acknowledged: false };
        let first = publish::build_outputs(&snapshot, &plan).unwrap();
        let second = publish::build_outputs(&snapshot, &plan).unwrap();
        assert_eq!(first.drawing.bytes, second.drawing.bytes, "the writer output repeats");
        assert_eq!((first.pdf.clone(), first.snap.clone()), (second.pdf.clone(), second.snap.clone()));
        let verbatim = publish::Snapshot { modified: false, ..snapshot.clone() };
        assert_eq!(publish::build_outputs(&verbatim, &plan).unwrap().drawing.bytes, loaded.bytes);

        // Known loss: an object the DXF writer cannot write.
        let mut lossy = snapshot.clone();
        let handle = lossy.document.allocate_handle();
        lossy.document.objects.insert(
            handle,
            acadrust::objects::ObjectType::Unknown {
                type_name: "SYNTHETIC_UNKNOWN".into(),
                handle,
                owner: acadrust::Handle::NULL,
                raw_dxf_codes: None,
                raw_dwg_data: None,
                raw_dwg_handle_bits: 0,
                raw_dwg_version: None,
            },
        );
        let error = publish::build_outputs(&lossy, &plan).unwrap_err();
        assert_eq!(error.code, ErrorCode::KnownLoss);

        // Over 50 MiB: stopped with the measured size.
        let huge = Drawing { bytes: Arc::new(vec![0u8; publish::MAX_OUTPUT_BYTES + 1]), ..loaded };
        let oversized = publish::Snapshot { pending_original: Some(huge), ..verbatim };
        let error = publish::build_outputs(&oversized, &plan).unwrap_err();
        assert_eq!(error.code, ErrorCode::OutputTooLarge);
        assert!(error.message.contains("50.0 MiB"), "{}", error.message);
    }

    /// Runs only in the child process started by the test below.
    #[test]
    #[ignore = "run by apply_outputs_write_no_temporary_files in a child process"]
    fn apply_outputs_child() {
        if std::env::var_os("SECUREPLAN_NO_TEMP_CHILD").is_none() {
            return;
        }
        let scene = publish::tests::synthetic_dxf_scene();
        let snapshot = publish::Snapshot { document: scene.document.clone(), annotation_scale: 1.0, loaded: None, modified: true, pending_original: None, lost_entities: 0 };
        let window = [0.0, 0.0, 30000.0, 18000.0];
        let mapping = crate::app::secureplan::align::mapping_at(window, 1.0, 0, [0.0, 0.0]);
        let plan = publish::ApplyPlan { view: publish::PublishedView::Model { window_cad: window }, mapping, mm_per_pt: 3.0, damaged_acknowledged: false };
        publish::build_outputs(&snapshot, &plan).expect("outputs");
    }

    #[test]
    fn apply_outputs_write_no_temporary_files() {
        let temp = std::env::temp_dir().join(format!("secureplan_no_temp_{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "app::secureplan::session::tests::apply_outputs_child", "--include-ignored", "--test-threads", "1"])
            .env("SECUREPLAN_NO_TEMP_CHILD", "1")
            .env("TMPDIR", &temp)
            .env("TMP", &temp)
            .env("TEMP", &temp)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "the child build failed");
        let left: Vec<_> = std::fs::read_dir(&temp).unwrap().flatten().map(|e| e.file_name()).collect();
        std::fs::remove_dir_all(&temp).ok();
        assert!(left.is_empty(), "temporary files were written: {left:?}");
    }

    #[test]
    fn a_plan_change_keeps_local_edits_under_the_old_base() {
        let mut h = Harness::new("planupdate");
        h.open_dxf();
        h.edit((10.0, 10.0), (20.0, 20.0));
        let tab_id = h.tab_id();
        let mut update = sample("planUpdate-applied");
        let id = h.transfer("synthetic-drawing-2.dwg", "image/vnd.dwg", &testutil::synthetic_dwg());
        update["drawingTransferId"] = json!(id);
        update["baseIdentity"] = json!(BASE);
        h.send(update.clone());
        let entry = h.app.secureplan.recovery.load(ORIGIN, "7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f").expect("the edits were kept");
        assert_eq!(entry.base_identity, "none", "under the old base identity");
        let index = h.app.secureplan_tab_index(tab_id).unwrap();
        assert!(!h.app.tabs[index].dirty, "the new drawing is loaded clean");
        assert_eq!(h.bound().base_identity, BASE);
        assert_eq!(h.bound().loaded.as_ref().unwrap().format, Format::Dwg);
        assert_eq!(h.bound().alignment, crate::app::secureplan::align::Alignment::from_cad_plan(&update["cadPlan"]));
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Choice { title, .. }) if title == "The plan changed"));
    }

    const SURVEY: &str = "7d3c1f6e-2b4a-4c8d-9e0f-1a2b3c4d5e6f";

    fn plan_update(h: &mut Harness, drawing: Option<(&str, &str, Vec<u8>)>, base: &str) {
        let mut update = sample("planUpdate-applied");
        update["drawingTransferId"] = match drawing {
            Some((name, media, bytes)) => json!(h.transfer(name, media, &bytes)),
            None => Value::Null,
        };
        update["baseIdentity"] = json!(base);
        h.send(update);
    }

    // ── Fix round 1 ─────────────────────────────────────────────────────────

    #[test]
    fn an_apply_built_before_a_plan_change_or_reconnect_is_not_sent() {
        for change in ["planUpdate", "reconnect"] {
            let mut h = Harness::new(&format!("stale_{change}"));
            h.open_dxf();
            align_and_open_apply(&mut h);
            h.hold_jobs();
            h.key(DialogKey::Activate);
            assert_eq!(h.held(), 1, "the build is running");
            if change == "planUpdate" {
                plan_update(&mut h, Some(("new.dxf", "image/vnd.dxf", testutil::synthetic_dxf())), BASE);
                h.release(1); // the new drawing loads first
            } else {
                h.pair_again(9, None);
                h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
                h.release(1);
            }
            // Now the build finishes: its outputs belong to the old state.
            h.release(0);
            assert_eq!(h.dialog_title().as_deref(), Some("Apply stopped"), "{change}");
            assert!(h.bound().apply.is_none(), "{change}: an applyRequest was sent");
            assert!(h.bound().busy.is_none());
            // Nothing reached the page: the next message it sees is ours.
            h.send_state_probe();
            let (state, transfers) = h.receive("sessionState");
            assert!(transfers.is_empty(), "{change}: outputs were sent");
            assert!(state.is_object());
        }
    }

    #[test]
    fn a_plan_update_whose_drawing_does_not_open_changes_nothing_and_blocks_apply() {
        let deep = {
            use acadrust::entities::Insert;
            use acadrust::types::Vector3;
            let mut doc = testutil::synthetic_document();
            for level in (1..=33).rev() {
                let mut members = vec![];
                if level < 33 {
                    members.push(acadrust::EntityType::Insert(Insert::new(format!("B{}", level + 1), Vector3::ZERO)));
                } else {
                    members.push(acadrust::EntityType::Line(acadrust::entities::Line::from_points(Vector3::ZERO, Vector3::new(1.0, 1.0, 0.0))));
                }
                crate::app::secureplan::snap::tests::block(&mut doc, &format!("B{level}"), members);
            }
            doc.add_entity(acadrust::EntityType::Insert(Insert::new("B1", Vector3::ZERO))).unwrap();
            crate::io::save_to_bytes(&doc, "dxf", acadrust::DxfVersion::AC1032).unwrap()
        };
        for (case, bytes) in [("malformed", b"AC1032 not a drawing".to_vec()), ("over the limit", deep)] {
            let mut h = Harness::new(&format!("badupdate_{}", case.len()));
            h.open_dxf();
            let entities = h.entity_count();
            let media = if case == "malformed" { "image/vnd.dwg" } else { "image/vnd.dxf" };
            plan_update(&mut h, Some(("new", media, bytes)), BASE);
            assert_eq!(h.bound().base_identity, "none", "{case}: the new plan was adopted without its drawing");
            assert!(h.bound().loaded.as_ref().is_some_and(|l| l.format == Format::Dxf), "{case}");
            assert_eq!(h.entity_count(), entities, "{case}: the drawing changed");
            assert!(h.bound().unresolved && h.bound().staged.is_none(), "{case}");
            assert_eq!(h.dialog_title().as_deref(), Some("The drawing could not be opened"));
            h.app.secureplan.dialog = None;
            h.app.command_line.last_error = None;
            let _ = h.app.dispatch_command("SECUREPLANAPPLY");
            assert!(last_error(&h.app).contains("not up to date"), "{case}: {}", last_error(&h.app));
            assert!(h.app.secureplan.dialog.is_none());
        }
    }

    #[test]
    fn a_plan_change_while_disconnected_keeps_the_edits_before_replacing_them() {
        let mut h = Harness::new("reconnect");
        h.open_dxf();
        let first = h.session;
        h.web.send(&json!({ "type": "close", "requestId": "x1", "reason": "pageClosed" }));
        h.pump_until_closed(first);
        assert!(!h.bound().connected());
        h.edit((100.0, 100.0), (900.0, 900.0));
        let edited = h.entity_count();
        // The page comes back; meanwhile the plan changed in SecurePlan.
        h.pair_again(21, None);
        h.open(Some(("new.dwg", Format::Dwg, testutil::synthetic_dwg())), overlay::tests::overlay_bytes(&[]), "edit", sample("openSession-edit")["cadPlan"].clone(), BASE, "edit");
        let entry = h.app.secureplan.recovery.load(ORIGIN, SURVEY).expect("the edits were kept first");
        assert_eq!(entry.base_identity, "none", "under the plan they were made from");
        let (kept, _) = crate::app::secureplan::import::load_drawing("x", entry.drawing.bytes.as_ref().clone()).unwrap();
        assert_eq!(kept.entities().count(), edited);
        assert_eq!(h.bound().base_identity, BASE);
        assert_eq!(h.bound().loaded.as_ref().unwrap().format, Format::Dwg);
    }

    #[test]
    fn a_replacement_stops_when_the_edits_cannot_be_kept() {
        for path in ["planUpdate", "reconnect", "import"] {
            let mut h = Harness::new(&format!("nokeep_{path}"));
            h.open_dxf();
            h.edit((100.0, 100.0), (900.0, 900.0));
            let edited = h.entity_count();
            h.break_recovery_store();
            match path {
                "planUpdate" => plan_update(&mut h, Some(("new.dwg", "image/vnd.dwg", testutil::synthetic_dwg())), BASE),
                "reconnect" => {
                    let first = h.session;
                    h.web.send(&json!({ "type": "close", "requestId": "x1", "reason": "pageClosed" }));
                    h.pump_until_closed(first);
                    h.pair_again(22, None);
                    h.open(Some(("new.dwg", Format::Dwg, testutil::synthetic_dwg())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, BASE, "edit");
                }
                _ => {
                    let file = h.dir().join("other.dxf");
                    std::fs::write(&file, testutil::synthetic_dxf()).unwrap();
                    let tab_id = h.tab_id();
                    let _ = h.app.secureplan_import_path(tab_id, &file);
                    assert_eq!(h.dialog_title().as_deref(), Some("Import stopped"));
                    assert!(h.bound().pending_original.is_none(), "the import went ahead");
                }
            }
            assert_eq!(h.entity_count(), edited, "{path}: the edits were replaced");
            assert!(h.app.tabs[h.app.active_tab].dirty, "{path}");
            assert_eq!(h.bound().base_identity, "none", "{path}");
            if path != "import" {
                assert!(h.bound().unresolved, "{path}: Apply must wait for a reopen");
                assert_eq!(h.dialog_title().as_deref(), Some("The plan changed"));
            }
        }
    }

    #[test]
    fn a_restored_copy_applies_in_its_own_format_and_version() {
        for (bytes, format, version) in [
            (testutil::synthetic_dwg(), Format::Dwg, "AC1032"),
            (crate::io::save_to_bytes(&testutil::synthetic_document(), "dxf", acadrust::DxfVersion::AC1015).unwrap(), Format::Dxf, "AC1015"),
        ] {
            let mut h = Harness::new(&format!("restore_{version}_{}", format.ext()));
            let drawing = Drawing { bytes: Arc::new(bytes.clone()), name: format!("kept.{}", format.ext()).into(), format, format_version: version.into() };
            let entry = recovery::Entry { origin: ORIGIN.into(), survey: SURVEY.into(), base_identity: "none".into(), plan_version: None, saved: std::time::SystemTime::now(), drawing, original: None, lost_entities: 0, alignment: None };
            h.app.secureplan.recovery.save(&entry).unwrap();
            h.open_dxf();
            let tab_id = h.tab_id();
            let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::RecoveryRestore(tab_id))));
            assert!(h.app.secureplan_has_unapplied(h.app.active_tab));
            align_and_open_apply(&mut h);
            h.key(DialogKey::Activate);
            let (request, transfers) = h.receive("applyRequest");
            let sent = &transfers[&request["drawing"]["transferId"].as_u64().unwrap()];
            assert_eq!(sent.0["mediaType"], format.media_type(), "{version}");
            assert_eq!(request["drawing"]["formatVersion"], version);
            assert_eq!(sent.1, bytes, "{version}: the recovered drawing itself");
        }
    }

    #[test]
    fn autosave_keeps_an_import_that_has_no_edit_yet() {
        let mut h = Harness::new("autosave_import");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
        let file = h.dir().join("synthetic.dwg");
        std::fs::write(&file, testutil::synthetic_dwg()).unwrap();
        let tab_id = h.tab_id();
        let _ = h.app.secureplan_import_path(tab_id, &file);
        assert!(!h.app.tabs[h.app.active_tab].dirty);
        let _ = h.app.update(Message::AutoSave);
        let entry = h.app.secureplan.recovery.load(ORIGIN, SURVEY).expect("the import was kept");
        assert_eq!(entry.original.unwrap().bytes.as_slice(), testutil::synthetic_dwg().as_slice());
    }

    #[test]
    fn quitting_decides_each_secureplan_drawing_first() {
        for choice in ["keep", "discard", "keep-fails", "cancel"] {
            let mut h = Harness::new(&format!("quit_{choice}"));
            let main = iced::window::Id::unique();
            h.app.main_window = Some(main);
            h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
            let file = h.dir().join("synthetic.dxf");
            std::fs::write(&file, testutil::synthetic_dxf()).unwrap();
            let tab_id = h.tab_id();
            let _ = h.app.secureplan_import_path(tab_id, &file);
            h.app.secureplan.dialog = None;
            // An imported, unedited drawing still asks.
            let _ = h.app.update(Message::WindowCloseRequested(main));
            let Some(Dialog::Choice { title, .. }) = &h.app.secureplan.dialog else { panic!("{choice}: no prompt") };
            assert_eq!(title, "Unapplied edits");
            assert!(h.app.pending_close.is_none(), "the upstream Save prompt showed");
            if choice == "keep-fails" {
                h.break_recovery_store();
            }
            let action = match choice {
                "keep" | "keep-fails" => Action::CloseKeep(tab_id),
                "discard" => Action::CloseDiscard(tab_id),
                _ => Action::Dismiss,
            };
            h.app.secureplan.recovery.save(&h.app.secureplan_recovery_entry(h.app.active_tab, "none").unwrap()).ok();
            let _ = h.app.update(Message::SecurePlan(Msg::Action(action)));
            let open = h.app.secureplan_tab_index(tab_id).is_some();
            let copy = h.app.secureplan.recovery.load(ORIGIN, SURVEY).is_some();
            match choice {
                "keep" => assert!(!open && copy && h.app.secureplan.quitting, "keep"),
                "discard" => assert!(!open && !copy && h.app.secureplan.quitting, "discard deletes the copy"),
                "keep-fails" => {
                    assert!(open && !h.app.secureplan.quitting, "closed without its copy");
                    assert_eq!(h.dialog_title().as_deref(), Some("Not closed"));
                }
                _ => assert!(open && !h.app.secureplan.quitting),
            }
            if !open {
                // The quit goes on: nothing is left to decide, so the app exits.
                assert!(h.app.secureplan_window_close_requested(main).is_none());
            }
        }
    }

    #[test]
    fn two_surveys_load_at_once_and_each_gets_its_own_drawing() {
        let mut h = Harness::new("two");
        h.hold_jobs();
        h.open(Some(("a.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        let a = h.tab_id();
        h.pair_again(23, Some("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"));
        h.open(Some(("b.dwg", Format::Dwg, testutil::synthetic_dwg())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        let b = h.tab_id();
        assert_eq!(h.app.secureplan.sessions.bound.len(), 2);
        assert_ne!(a, b);
        assert_eq!(h.held(), 2);
        // B finishes first, then A: neither is dropped.
        h.release(1);
        h.release(0);
        let format = |tab: u64| h.app.secureplan.sessions.by_tab(tab).and_then(|b| b.loaded.as_ref().map(|l| l.format));
        assert_eq!(format(a), Some(Format::Dxf));
        assert_eq!(format(b), Some(Format::Dwg));
        assert!(h.app.secureplan.load_jobs.is_empty());
    }

    #[test]
    fn messages_after_a_waiting_open_session_are_handled_in_order() {
        let mut h = Harness::new("ordered");
        // openSession names transfers the page has not sent yet; a mode
        // change and an overlay update follow it.
        h.web.send(&json!({
            "type": "openSession", "requestId": "open-1", "intent": "edit", "mode": "edit",
            "surveyLabel": "Synthetic survey", "cadPlan": null, "placement": null,
            "baseIdentity": "none", "drawingTransferId": 2, "overlayTransferId": 1, "surveyEmpty": true,
        }));
        h.pump();
        h.web.send(&json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "leaseLost" }));
        h.pump();
        h.transfer("overlay.json", "application/vnd.secureplan.overlay+json", &overlay::tests::overlay_bytes(&[]));
        // Transfer 3 (the update's) completes before the open's drawing.
        let _ = h.transfer_numbered(3, "overlay.json", "application/vnd.secureplan.overlay+json", &overlay::tests::overlay_bytes(&[([7.0, 7.0], [8.0, 8.0])]));
        h.web.send(&json!({ "type": "overlayUpdate", "requestId": "o1", "overlayTransferId": 3, "surveyEmpty": false }));
        h.pump();
        assert!(h.app.secureplan.sessions.bound.is_empty(), "handled before its drawing arrived");
        let _ = h.transfer_numbered(2, "synthetic.dxf", "image/vnd.dxf", &testutil::synthetic_dxf());
        assert_eq!(h.bound().mode, Mode::View, "the later mode change was lost");
        let walls = &h.bound().overlay.as_ref().unwrap().walls;
        assert_eq!(walls.len(), 1, "the later overlay was lost");
        assert_eq!(walls[0].1, [7.0, 7.0]);
    }

    #[test]
    fn a_survey_with_only_comments_is_not_placed_at_the_origin() {
        // No design elements, so the overlay is empty; but it has comments.
        let mut h = Harness::new("comments");
        h.survey_empty = false;
        h.open_dxf();
        assert!(h.bound().overlay.as_ref().unwrap().is_empty());
        assert!(!survey_is_empty(h.bound()));
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        let Some(Dialog::Align(dialog)) = &h.app.secureplan.dialog else { panic!("no alignment dialog") };
        assert!(!dialog.empty_survey, "the translation must be the user's to choose");
        assert!(dialog.form.fields.iter().any(|f| f.label == "…lands at survey X (mm)" && f.enabled));
        h.app.secureplan.dialog = None;
        // The web says it became empty (comments deleted): the origin rule applies.
        let id = h.transfer("overlay.json", "application/vnd.secureplan.overlay+json", &overlay::tests::overlay_bytes(&[]));
        h.send(json!({ "type": "overlayUpdate", "requestId": "o2", "overlayTransferId": id, "surveyEmpty": true }));
        assert!(survey_is_empty(h.bound()));
    }

    // ── Fix round 2 ─────────────────────────────────────────────────────────

    #[cfg(feature = "secureplan-test")]
    fn driver(h: &mut Harness, line: &str) {
        let _ = h.app.secureplan_driver(crate::app::secureplan::testdriver::parse(line).unwrap());
    }

    #[cfg(feature = "secureplan-test")]
    fn tick(h: &mut Harness) {
        let _ = h.app.update(Message::Tick(iced::time::Instant::now()));
    }

    /// The synthetic plan with one damaged polyline the reader drops.
    #[cfg(feature = "secureplan-test")]
    fn damaged_dxf() -> Vec<u8> {
        use acadrust::types::Vector2;
        let mut doc = testutil::synthetic_document();
        let mut damaged = acadrust::entities::LwPolyline::from_points(vec![Vector2::new(0.0, 0.0), Vector2::new(1.0, 1.0)]);
        damaged.elevation = 1.0e11;
        doc.add_entity(acadrust::EntityType::LwPolyline(damaged)).unwrap();
        crate::io::save_to_bytes(&doc, "dxf", acadrust::DxfVersion::AC1032).unwrap()
    }

    #[cfg(feature = "secureplan-test")]
    fn import_file(h: &mut Harness, name: &str, bytes: &[u8]) {
        let file = h.dir().join(name);
        std::fs::write(&file, bytes).unwrap();
        driver(h, &format!("import {}", file.display()));
    }

    #[cfg(feature = "secureplan-test")]
    const MINIMAL_DXF: &str = "0\nSECTION\n2\nENTITIES\n0\nLINE\n8\n0\n10\n0.0\n20\n0.0\n30\n0.0\n11\n1000.0\n21\n500.0\n31\n0.0\n0\nENDSEC\n0\nEOF\n";

    #[test]
    #[cfg(feature = "secureplan-test")]
    fn an_unedited_import_is_applied_byte_for_byte_through_the_driver() {
        for (name, bytes) in [("minimal.dxf", MINIMAL_DXF.as_bytes().to_vec()), ("resaved.dxf", testutil::synthetic_dxf())] {
            let mut h = Harness::new(&format!("verbatim_{name}"));
            h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
            import_file(&mut h, name, &bytes);
            h.app.secureplan.dialog = None;
            // Frames run, and the user zooms: neither is an edit.
            tick(&mut h);
            let index = h.app.active_tab;
            h.app.tabs[index].scene.fit_all();
            tick(&mut h);
            assert!(!h.app.secureplan_modified(index), "{name}: the view counted as an edit");
            driver(&mut h, "align units=mm");
            driver(&mut h, "apply");
            let (request, transfers) = h.receive("applyRequest");
            let sent = &transfers[&request["drawing"]["transferId"].as_u64().unwrap()].1;
            let original = &transfers[&request["original"]["transferId"].as_u64().unwrap()].1;
            assert_eq!(original, &bytes, "{name}: the original");
            assert_eq!(sent, &bytes, "{name}: an unedited import is sent byte for byte");
        }
    }

    #[test]
    #[cfg(feature = "secureplan-test")]
    fn damaged_items_are_published_only_with_an_acknowledgement_every_time() {
        let bytes = damaged_dxf();
        let mut h = Harness::new("damaged");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
        import_file(&mut h, "damaged.dxf", &bytes);
        let Some(Dialog::Choice { lines, .. }) = &h.app.secureplan.dialog else { panic!("no report") };
        assert!(lines.iter().any(|l| l.contains("1 damaged items could not be read")), "{lines:?}");
        h.app.secureplan.dialog = None;
        driver(&mut h, "align units=mm");
        // Unedited: refused without the acknowledgement, applied with it.
        driver(&mut h, "apply");
        assert!(h.bound().apply.is_none(), "applied without the acknowledgement");
        driver(&mut h, "apply damaged=publish");
        let (request, transfers) = h.receive("applyRequest");
        assert_eq!(transfers[&request["drawing"]["transferId"].as_u64().unwrap()].1, bytes, "unedited: the original bytes");
        assert_eq!(transfers[&request["original"]["transferId"].as_u64().unwrap()].1, bytes, "the original is stored unchanged");
        h.send(json!({ "type": "applyResult", "requestId": request["requestId"], "status": "error", "code": "LEASE_LOST", "detail": null }));
        // Edited: still asked, every time, through the dialog too.
        h.edit((1.0, 1.0), (2.0, 2.0));
        let _ = h.app.dispatch_command("SECUREPLANAPPLY");
        h.key(DialogKey::Activate);
        assert!(h.bound().apply.is_none() && matches!(h.app.secureplan.dialog, Some(Dialog::Apply(_))), "applied without the acknowledgement");
        h.app.secureplan.dialog.as_mut().unwrap().form_mut().focus = crate::app::secureplan::ui::apply_dialog::ACKNOWLEDGE;
        h.key(DialogKey::Right);
        h.key(DialogKey::Previous);
        h.key(DialogKey::Activate);
        let (request, _) = h.receive("applyRequest");
        assert_eq!(request["type"], "applyRequest");
    }

    /// Runs only in the child process started by the test below, alone, so
    /// the refusals can be process-wide (as in the app), where code on other
    /// threads (the scene's parallel cache builds) sees them too.
    #[test]
    #[ignore = "run by a_worker_keeps_references_refused_after_the_last_drawing_closes in a child process"]
    fn worker_guard_child() {
        use crate::app::secureplan::guards::{external_resource_allowed, ExternalResource, TEST_PROCESS_WIDE};
        if std::env::var_os("SECUREPLAN_WORKER_GUARD_CHILD").is_none() {
            return;
        }
        TEST_PROCESS_WIDE.store(true, std::sync::atomic::Ordering::SeqCst);
        use acadrust::objects::{ImageDefinition, ObjectType};
        let mut h = Harness::new("worker_guard");
        let png = h.dir().join("worker.png");
        image::RgbaImage::new(1, 1).save(&png).unwrap();
        let reference = png.to_string_lossy().into_owned();
        let mut doc = testutil::synthetic_document();
        let handle = doc.allocate_handle();
        let mut definition = ImageDefinition::with_dimensions(&reference, 1, 1);
        definition.handle = handle;
        doc.objects.insert(handle, ObjectType::ImageDefinition(definition));
        let mut raster = acadrust::entities::RasterImage::new(&reference, acadrust::types::Vector3::new(100.0, 100.0, 0.0), 1.0, 1.0);
        raster.definition_handle = Some(handle);
        doc.add_entity(acadrust::EntityType::RasterImage(raster)).unwrap();
        let bytes = crate::io::save_to_bytes(&doc, "dxf", acadrust::DxfVersion::AC1032).unwrap();
        h.open(Some(("images.dxf", Format::Dxf, bytes)), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        assert!(!crate::scene::model::image_model::tests::cached(&reference), "read on open");
        align_and_open_apply(&mut h);
        h.hold_jobs();
        h.key(DialogKey::Activate);
        assert_eq!(h.held(), 1);
        // The last SecurePlan drawing closes while its Apply build runs.
        let index = h.app.active_tab;
        let _ = h.app.update(Message::TabClose(index));
        assert!(h.app.secureplan.sessions.bound.is_empty());
        assert!(!external_resource_allowed(ExternalResource::Image), "references allowed while the worker runs");
        h.release(0);
        assert!(!crate::scene::model::image_model::tests::cached(&reference), "the worker read the drawing's image");
        assert!(external_resource_allowed(ExternalResource::Image), "allowed again once no worker runs");
        // Control: with references allowed, building the same scene reads it.
        let _ = crate::app::secureplan::snap::tests::scene_of(doc);
        assert!(crate::scene::model::image_model::tests::cached(&reference), "the control read nothing");
    }

    #[test]
    fn a_worker_keeps_references_refused_after_the_last_drawing_closes() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "app::secureplan::session::tests::worker_guard_child", "--include-ignored", "--test-threads", "1", "--nocapture"])
            .env("SECUREPLAN_WORKER_GUARD_CHILD", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success() && text.contains("1 passed"), "{text}\n{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn an_older_load_never_lands_after_a_newer_removal() {
        let mut h = Harness::new("older_load");
        h.open_dxf();
        h.hold_jobs();
        plan_update(&mut h, Some(("new.dwg", "image/vnd.dwg", testutil::synthetic_dwg())), BASE);
        assert_eq!(h.held(), 1);
        let mut removed = sample("planUpdate-removed");
        removed["requestId"] = json!("plan-9");
        h.send(removed);
        assert!(h.bound().loaded.is_none() && h.entity_count() == 0, "the removal installs at once");
        h.release(0);
        assert!(h.bound().loaded.is_none(), "the older drawing landed");
        assert_eq!(h.entity_count(), 0);
        assert_eq!(h.bound().base_identity, "none");
    }

    #[test]
    fn edits_made_while_a_replacement_parses_are_kept() {
        let mut h = Harness::new("edit_during_parse");
        h.open_dxf();
        let before = h.entity_count();
        h.hold_jobs();
        plan_update(&mut h, Some(("new.dwg", "image/vnd.dwg", testutil::synthetic_dwg())), BASE);
        h.edit((5.0, 5.0), (6.0, 6.0));
        h.release(0);
        let entry = h.app.secureplan.recovery.load(ORIGIN, SURVEY).expect("the edit was kept");
        let (kept, _) = crate::app::secureplan::import::load_drawing("x", entry.drawing.bytes.as_ref().clone()).unwrap();
        assert_eq!(kept.entities().count(), before + 1, "the copy lacks the edit made during the parse");
        assert_eq!(h.bound().base_identity, BASE);
        // And if it cannot be kept, nothing is replaced.
        let mut h = Harness::new("edit_during_parse_fail");
        h.open_dxf();
        h.hold_jobs();
        plan_update(&mut h, Some(("new.dwg", "image/vnd.dwg", testutil::synthetic_dwg())), BASE);
        h.edit((5.0, 5.0), (6.0, 6.0));
        h.break_recovery_store();
        h.release(0);
        assert_eq!(h.bound().loaded.as_ref().unwrap().format, Format::Dxf, "replaced over unkept edits");
        assert!(h.bound().unresolved);
    }

    #[test]
    #[cfg(feature = "secureplan-test")]
    fn a_recovered_copy_keeps_its_damaged_items_and_needs_the_acknowledgement() {
        let mut h = Harness::new("recovered_loss");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
        import_file(&mut h, "damaged.dxf", &damaged_dxf());
        h.app.secureplan.dialog = None;
        driver(&mut h, "align units=mm");
        h.edit((1.0, 1.0), (2.0, 2.0));
        driver(&mut h, "apply");
        assert!(h.bound().apply.is_none());
        let tab_id = h.tab_id();
        let session = h.session;
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::CloseKeep(tab_id))));
        h.pump_until_closed(session);
        let kept = h.app.secureplan.recovery.load(ORIGIN, SURVEY).expect("kept");
        assert_eq!(kept.lost_entities, 1);
        assert!(kept.alignment.is_some(), "the copy has no alignment");
        // Reopened and restored: still needs the acknowledgement.
        h.pair_again(31, None);
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        let tab_id = h.tab_id();
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::RecoveryRestore(tab_id))));
        assert_eq!(h.bound().lost_entities, 1);
        assert!(h.bound().alignment.is_some(), "the copy's alignment came back");
        driver(&mut h, "apply");
        assert!(h.bound().apply.is_none(), "the purged copy was applied without the acknowledgement");
    }

    #[test]
    fn a_restored_copy_brings_its_own_alignment() {
        use crate::app::secureplan::align::{Alignment, Units};
        let current = Alignment::from_cad_plan(&sample("openSession-edit")["cadPlan"]).unwrap();
        let theirs = Alignment { units: Units::M, mapping: crate::app::secureplan::publish::Mapping { cad_origin: [1.0, 2.0], anchor_mm: [500.0, 500.0], scale_mm_per_cad_unit: 1000.0, quarter_turns: 3 } };
        for stored in [Some(theirs), None] {
            let mut h = Harness::new(&format!("restore_mapping_{}", stored.is_some()));
            let drawing = Drawing { bytes: Arc::new(testutil::synthetic_dxf()), name: "a.dxf".to_string().into(), format: Format::Dxf, format_version: "AC1032".into() };
            let entry = recovery::Entry { origin: ORIGIN.into(), survey: SURVEY.into(), base_identity: BASE.into(), plan_version: Some(1), saved: std::time::SystemTime::now(), drawing, original: None, lost_entities: 0, alignment: stored };
            h.app.secureplan.recovery.save(&entry).unwrap();
            h.open(Some(("b.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", sample("openSession-edit")["cadPlan"].clone(), BASE, "edit");
            assert_eq!(h.bound().alignment, Some(current));
            let tab_id = h.tab_id();
            let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::RecoveryRestore(tab_id))));
            assert_eq!(h.bound().alignment, stored, "the current plan's mapping was kept for another drawing");
            assert_eq!(h.bound().realigned, stored.is_some());
            if stored.is_none() {
                let _ = h.app.dispatch_command("SECUREPLANAPPLY");
                assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "align again first");
            }
        }
    }

    #[test]
    fn a_commit_keeps_changes_made_while_the_apply_ran_whatever_marked_them() {
        for control in ["visibility", "scale"] {
            let mut h = Harness::new(&format!("held_dirty_{control}"));
            h.open_dxf();
            h.edit((1.0, 1.0), (2.0, 2.0));
            align_and_open_apply(&mut h);
            h.hold_jobs();
            h.key(DialogKey::Activate);
            let index = h.app.active_tab;
            let revision = h.app.tabs[index].edit_revision;
            match control {
                "visibility" => {
                    let _ = h.app.update(Message::ToggleAnnotationVisibility);
                }
                _ => {
                    h.app.tabs[index].scene.set_annotation_all_visible(true);
                    let _ = h.app.update(Message::SyncViewportAnnotationScale);
                    h.app.tabs[index].dirty = true; // what the scale control does
                }
            }
            assert_eq!(h.app.tabs[index].edit_revision, revision, "{control}: the control bumped the revision (test premise)");
            h.release(0);
            let (request, _) = h.receive("applyRequest");
            h.send(json!({ "type": "applyResult", "requestId": request["requestId"], "status": "committed", "planVersion": 1, "baseIdentity": BASE }));
            assert!(h.app.tabs[index].dirty, "{control}: the commit cleared a later change");
            assert!(h.app.secureplan_has_unapplied(index));
        }
    }

    #[test]
    fn a_bound_drawing_gets_the_annotation_scale_it_declares() {
        // The DXF writer does not write $CANNOSCALEVALUE: add it to the header.
        let bytes = |scale: f64| {
            let text = String::from_utf8(testutil::synthetic_dxf()).unwrap();
            let at = text.find("HEADER").unwrap() + "HEADER".len();
            let at = at + text[at..].find('\n').unwrap() + 1;
            format!("{}  9\n$CANNOSCALEVALUE\n 40\n{scale}\n{}", &text[..at], &text[at..]).into_bytes()
        };
        let mut h = Harness::new("annoscale");
        h.open(Some(("a.dxf", Format::Dxf, bytes(0.01))), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        let index = h.app.active_tab;
        let factor = h.app.tabs[index].scene.annotation_scale_unit_factor();
        assert!((h.app.tabs[index].scene.annotation_scale as f64 - 100.0 / factor).abs() < 1e-3, "{}", h.app.tabs[index].scene.annotation_scale);
        plan_update(&mut h, Some(("b.dxf", "image/vnd.dxf", bytes(0.02))), BASE);
        let factor = h.app.tabs[index].scene.annotation_scale_unit_factor();
        assert!((h.app.tabs[index].scene.annotation_scale as f64 - 50.0 / factor).abs() < 1e-3, "{}", h.app.tabs[index].scene.annotation_scale);
    }

    #[test]
    fn an_import_asked_for_at_open_goes_to_the_survey_that_asked() {
        let mut h = Harness::new("import_target");
        let file = h.dir().join("picked.dxf");
        std::fs::write(&file, testutil::synthetic_dxf()).unwrap();
        h.hold_jobs();
        h.open(Some(("a.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "import");
        let a = h.tab_id();
        h.pair_again(41, Some("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"));
        h.open(Some(("b.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", Value::Null, "none", "edit");
        let b = h.tab_id();
        h.release(1);
        h.app.secureplan.test_pick = Some(file);
        // A finishes loading while B is active: the picker's file goes to A.
        h.release(0);
        while h.held() > 0 {
            h.release(0);
        }
        let pending = |h: &Harness, tab: u64| h.app.secureplan.sessions.by_tab(tab).is_some_and(|b| b.pending_original.is_some());
        assert!(pending(&h, a), "A did not get its import");
        assert!(!pending(&h, b), "the import went to B");
        // A pick for a survey that can no longer import is refused.
        h.app.secureplan.sessions.by_tab_mut(b).unwrap().mode = Mode::View;
        let file = h.dir().join("picked.dxf");
        let _ = h.app.update(Message::SecurePlan(Msg::ImportPicked(b, Some(file.into()))));
        assert!(!pending(&h, b));
    }

    #[test]
    fn a_burst_of_drawing_updates_runs_one_parser_at_a_time_and_lands_the_latest() {
        let mut h = Harness::new("burst");
        h.open_dxf();
        h.hold_jobs();
        for i in 0..5 {
            let base = format!("{:064x}", i + 1);
            let (name, media, bytes) = if i == 4 { ("last.dwg", "image/vnd.dwg", testutil::synthetic_dwg()) } else { ("mid.dxf", "image/vnd.dxf", testutil::synthetic_dxf()) };
            plan_update(&mut h, Some((name, media, bytes)), &base);
            assert_eq!(h.held(), 1, "update {i}: more than one parser started");
        }
        h.release(0); // the first, superseded; the latest starts
        assert_eq!(h.held(), 1, "only the latest waiting load starts");
        h.release(0);
        assert_eq!(h.held(), 0);
        assert_eq!(h.bound().loaded.as_ref().unwrap().format, Format::Dwg);
        assert_eq!(h.bound().base_identity, format!("{:064x}", 5));
    }

    #[test]
    fn a_transfer_that_never_comes_or_is_reused_ends_the_session() {
        let mut h = Harness::new("never");
        h.app.secureplan.transfer_wait = std::time::Duration::ZERO;
        h.web.send(&json!({
            "type": "openSession", "requestId": "open-1", "intent": "edit", "mode": "edit",
            "surveyLabel": "Synthetic survey", "cadPlan": null, "placement": null,
            "baseIdentity": "none", "drawingTransferId": null, "overlayTransferId": 99, "surveyEmpty": true,
        }));
        h.pump();
        let _ = h.app.update(Message::SecurePlan(Msg::Tick));
        assert!(h.app.secureplan.sessions.deferred.is_empty(), "the waiting message was kept");
        let (close, _) = h.receive("close");
        assert_eq!(close["reason"], "protocolError");
        // A used transfer named again.
        let mut h = Harness::new("reused");
        h.open_dxf();
        h.send(json!({ "type": "overlayUpdate", "requestId": "o1", "overlayTransferId": 1, "surveyEmpty": true }));
        let (close, _) = h.receive("close");
        assert_eq!(close["reason"], "protocolError");
    }

    #[test]
    fn a_build_finishing_after_view_mode_sends_nothing() {
        let mut h = Harness::new("view_during_build");
        h.open_dxf();
        align_and_open_apply(&mut h);
        h.hold_jobs();
        h.key(DialogKey::Activate);
        h.send(json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "leaseLost" }));
        h.release(0);
        assert!(h.bound().apply.is_none(), "sent in view mode");
        assert_eq!(h.dialog_title().as_deref(), Some("Apply stopped"));
    }

    #[test]
    fn view_mode_for_one_survey_leaves_another_surveys_dialog_open() {
        let mut h = Harness::new("scoped_close");
        h.open_dxf();
        let a_session = h.session;
        h.pair_again(51, Some("0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d"));
        h.open_dxf();
        // Back on A, its alignment dialog opens.
        let a = h.app.secureplan.sessions.by_session_mut(a_session).unwrap().tab_id;
        h.app.active_tab = h.app.secureplan_tab_index(a).unwrap();
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))));
        h.send(json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "leaseLost" }));
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "B's view mode closed A's dialog");
    }

    #[test]
    fn cancelling_close_all_forgets_the_rest_of_it() {
        let mut h = Harness::new("close_all");
        h.open_dxf();
        h.edit((1.0, 1.0), (2.0, 2.0));
        let a = h.tab_id();
        let _ = h.app.update(Message::TabNew);
        let other = h.app.tabs[h.app.active_tab].id;
        let _ = h.app.update(Message::DocTabCloseAll);
        assert_eq!(h.dialog_title().as_deref(), Some("Unapplied edits"));
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::Dismiss)));
        // Later, just the SecurePlan tab is closed and kept.
        let index = h.app.secureplan_tab_index(a).unwrap();
        let _ = h.app.update(Message::TabClose(index));
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::CloseKeep(a))));
        assert!(h.app.secureplan_tab_index(a).is_none());
        assert!(h.app.tabs.iter().any(|t| t.id == other), "the abandoned Close All closed another tab");
    }
}
