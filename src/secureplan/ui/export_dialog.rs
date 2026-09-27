//! The export dialog (EXP-03, DSK-06).
//!
//! It opens when an `exportRequest`'s drawing has been read and the design
//! added (EXP-02). The user picks the format and version (the applied
//! drawing's own by default), sees what the export adds, and confirms with
//! **Export…**, which writes the file in memory, checks it and opens the
//! native Save dialog, the only place SecurePlan CAD writes it. When the
//! chosen format cannot hold some objects, or the reader dropped damaged
//! items, "Export without N objects" must be set to Yes first. Keyboard: as
//! every SecurePlan dialog (Tab, the arrows, Enter, Space, Escape, "›").

use std::sync::Arc;

use iced::widget::{column, text};
use iced::{Element, Length, Task};
use serde_json::json;

use super::{Action, Dialog, Field, FieldKind, Form};
use crate::app::secureplan::bridge::SessionId;
use crate::app::secureplan::export::{self, Composed, ComposeDone, ExportJob, Stage, WriteDone, WriteError};
use crate::app::secureplan::session::{Drawing, Format};
use crate::app::secureplan::{Carry, Msg};
use crate::app::{Message, OpenCADStudio};

const FORMAT: usize = 0;
const ACKNOWLEDGE: usize = 1;

#[derive(Debug, Clone)]
pub struct ExportDialog {
    pub tab_id: u64,
    pub request_id: String,
    pub form: Form,
    /// The applied drawing with the design added.
    pub document: Arc<acadrust::CadDocument>,
    pub summary: Vec<String>,
    /// Damaged items the reader dropped from the applied drawing.
    pub lost_entities: usize,
    /// The applied drawing's name without its extension.
    pub stem: String,
    /// The known loss for the chosen format: (choice, objects lost).
    loss: (usize, usize),
    /// Written and waiting: further confirmations are ignored.
    pub writing: bool,
}

impl ExportDialog {
    pub fn new(tab_id: u64, request_id: String, composed: Composed) -> Self {
        let default = export::default_choice(composed.format, &composed.version);
        let formats = export::choices().iter().map(|choice| choice.to_string()).collect();
        let mut acknowledge = Field::choice("Export without the objects listed", vec!["No".into(), "Yes".into()], 0);
        acknowledge.enabled = false;
        let fields = vec![Field::choice("Format and version", formats, default), acknowledge];
        let buttons = vec![("Export…".to_string(), Action::ExportSave), ("Cancel".to_string(), Action::ExportCancel)];
        let mut dialog = Self {
            tab_id,
            request_id,
            form: Form::new(fields, buttons, Action::ExportCancel),
            summary: composed.composition.summary(),
            document: Arc::new(composed.document),
            lost_entities: composed.lost_entities,
            stem: composed.stem,
            loss: (usize::MAX, 0),
            writing: false,
        };
        dialog.refresh();
        dialog
    }

    /// The chosen format and version.
    pub fn choice(&self) -> (Format, acadrust::DxfVersion) {
        export::choice(self.form.selected(FORMAT))
    }

    /// Choose `format` (and `version`, else the newest); `false` if the
    /// writer offers no such choice.
    pub fn select(&mut self, format: Format, version: Option<acadrust::DxfVersion>) -> bool {
        let found = (0..export::choices().len())
            .filter(|&i| export::choice(i).0 == format && version.is_none_or(|v| export::choice(i).1 == v))
            .max_by_key(|&i| export::choice(i).1);
        let Some(index) = found else { return false };
        if let Some(Field { kind: FieldKind::Choice { selected, .. }, .. }) = self.form.fields.get_mut(FORMAT) {
            *selected = index;
        }
        self.refresh();
        true
    }

    /// Objects the chosen format loses: what it cannot hold, and the damaged
    /// items the reader dropped.
    pub fn known_loss(&self) -> usize {
        self.loss.1
    }

    /// Recount the loss when the choice changed; its acknowledgement is asked
    /// again for every new choice.
    pub fn refresh(&mut self) {
        let chosen = self.form.selected(FORMAT);
        if self.loss.0 != chosen {
            let (format, version) = self.choice();
            self.loss = (chosen, export::unwritable(&self.document, format, version) + self.lost_entities);
            if let Some(Field { kind: FieldKind::Choice { selected, .. }, enabled, .. }) = self.form.fields.get_mut(ACKNOWLEDGE) {
                *selected = 0;
                *enabled = self.loss.1 > 0;
            }
        }
    }

    /// Set "Export without the objects listed" to Yes (the driver's flag).
    pub fn acknowledge(&mut self) {
        if self.loss.1 > 0 && self.form.selected(ACKNOWLEDGE) == 0 {
            self.form.cycle(ACKNOWLEDGE, true);
        }
    }

    /// The format and version to write, or why not yet.
    pub fn plan(&self) -> Result<(Format, acadrust::DxfVersion), String> {
        if self.loss.1 > 0 && self.form.selected(ACKNOWLEDGE) != 1 {
            return Err(format!("Choose Yes for \"Export without the objects listed\" to export without {} object(s).", self.loss.1));
        }
        Ok(self.choice())
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = self.summary.clone();
        if self.loss.1 > 0 {
            let (format, version) = self.choice();
            let unwritable = self.loss.1 - self.lost_entities;
            let mut parts = Vec::new();
            if unwritable > 0 {
                parts.push(format!("{unwritable} object(s) {} {} cannot hold", format.ext().to_ascii_uppercase(), version.as_str()));
            }
            if self.lost_entities > 0 {
                parts.push(format!("{} damaged item(s) the applied drawing lost when it was read", self.lost_entities));
            }
            lines.push(format!("Known loss: the export leaves out {}.", parts.join(" and ")));
        }
        if self.writing {
            lines.push("Writing the drawing…".into());
        }
        lines
    }
}

pub fn view(dialog: &ExportDialog) -> Element<'_, Message> {
    let mut content = column![].spacing(10).padding(8);
    content = content.push(
        text("Exports the drawing SecurePlan holds with the design added on SECUREPLAN layers. Nothing changes in SecurePlan.")
            .size(13)
            .width(Length::Fixed(520.0)),
    );
    content = content.push(super::form_view(&dialog.form));
    for line in dialog.lines() {
        content = content.push(text(line).size(12).width(Length::Fixed(520.0)));
    }
    content.push(super::keys_hint()).into()
}

fn export_result(request_id: &str, status: &str) -> serde_json::Value {
    json!({ "type": "exportResult", "requestId": request_id, "status": status })
}

impl OpenCADStudio {
    /// `exportRequest` (EXP-01): read the snapshot's drawing and add the
    /// design on a worker, then show the export dialog.
    pub(crate) fn secureplan_export_request(&mut self, session: SessionId, request_id: &str, body: &serde_json::Value) -> Task<Message> {
        let take = |app: &mut Self, key: &str| body[key].as_u64().and_then(|id| app.secureplan_take_transfer(session, id as u32));
        let (drawing, payload) = (take(self, "drawingTransferId"), take(self, "payloadTransferId"));
        let refuse = |app: &mut Self, message: &str| {
            app.secureplan_send(session, json!({ "type": "exportResult", "requestId": request_id, "status": "error", "code": "INVALID" }));
            app.command_line.push_error(&format!("SecurePlan: {message}"));
            crate::app::secureplan::testdriver_event("export-failed", "INVALID");
            Task::none()
        };
        let Some(bound) = self.secureplan.sessions.by_session_mut(session) else {
            return refuse(self, "open the survey from SecurePlan before exporting.");
        };
        if bound.export.is_some() {
            return refuse(self, "an export is already under way for this survey.");
        }
        let Some(alignment) = bound.plan_alignment else {
            return refuse(self, "the survey has no applied CAD plan to export.");
        };
        let drawing = drawing.and_then(|t| Format::from_media_type(&t.media_type).map(|format| (t, format)));
        let (Some((drawing, format)), Some(payload)) = (drawing, payload.filter(|t| t.media_type == export::MEDIA_TYPE)) else {
            return refuse(self, "the export request was incomplete.");
        };
        let tab_id = bound.tab_id;
        bound.export = Some(ExportJob { request_id: request_id.to_string(), stage: Stage::Composing, waiting: None, shown: None, target: None });
        let drawing = Drawing {
            bytes: Arc::new(drawing.bytes.expose().clone()),
            name: crate::app::secureplan::session::file_name(drawing.name.expose()).into(),
            format,
            format_version: String::new(),
        };
        let payload = payload.bytes.expose().clone();
        let snapshot = body["snapshot"].clone();
        let request_id = request_id.to_string();
        self.secureplan_report_states();
        self.command_line.push_info("SecurePlan: preparing the CAD export…");
        self.secureplan_run_job(move || {
            let result = export::prepare(&drawing, &payload, &snapshot, &alignment.mapping);
            Msg::ExportComposed(ComposeDone { tab_id, request_id, result: Carry::new(result) })
        })
    }

    /// The export job of tab `tab_id` for `request_id`, if it is still wanted.
    fn secureplan_export_job(&mut self, tab_id: u64, request_id: &str) -> Option<&mut ExportJob> {
        self.secureplan.sessions.by_tab_mut(tab_id)?.export.as_mut().filter(|job| job.request_id == request_id)
    }

    /// Answer the export with `message` and forget it.
    fn secureplan_finish_export(&mut self, tab_id: u64, message: serde_json::Value) {
        let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) else { return };
        let session = bound.session;
        bound.export = None;
        if let Some(session) = session {
            self.secureplan_send(session, message);
        }
        self.secureplan_close_export_dialog(tab_id);
        self.secureplan_report_states();
    }

    fn secureplan_close_export_dialog(&mut self, tab_id: u64) {
        if matches!(&self.secureplan.dialog, Some(Dialog::Export(dialog)) if dialog.tab_id == tab_id) {
            self.secureplan.dialog = None;
        }
    }

    /// The session ended or the tab closed: the export is dropped unanswered.
    pub(crate) fn secureplan_drop_export(&mut self, tab_id: u64) {
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.export = None;
        }
        self.secureplan_close_export_dialog(tab_id);
    }

    fn secureplan_export_failed(&mut self, tab_id: u64, request_id: &str, code: &str, lines: Vec<String>) {
        let mut message = export_result(request_id, "error");
        message["code"] = json!(code);
        self.secureplan_finish_export(tab_id, message);
        self.secureplan.dialog = Some(Dialog::notice("Export failed", lines));
        crate::app::secureplan::testdriver_event("export-failed", code);
    }

    pub(crate) fn secureplan_export_composed(&mut self, done: ComposeDone) -> Task<Message> {
        let Some(job) = self.secureplan_export_job(done.tab_id, &done.request_id) else { return Task::none() };
        if !matches!(job.stage, Stage::Composing) {
            return Task::none();
        }
        let Some(result) = done.result.take() else { return Task::none() };
        let composed = match result {
            Ok(composed) => composed,
            Err(message) => {
                self.secureplan_export_failed(done.tab_id, &done.request_id, "INVALID", vec![message, "Nothing was written.".into()]);
                return Task::none();
            }
        };
        let dialog = Box::new(ExportDialog::new(done.tab_id, done.request_id, composed));
        let job = self.secureplan_export_job(done.tab_id, &dialog.request_id).expect("found above");
        job.stage = Stage::Choosing;
        job.waiting = Some(dialog);
        self.secureplan_show_waiting_export();
        crate::app::secureplan::testdriver_event("export-ready", "");
        Task::none()
    }

    /// Show an export dialog that waited for another SecurePlan dialog, or
    /// that another dialog replaced before the user answered it.
    pub(crate) fn secureplan_show_waiting_export(&mut self) {
        let showing = match &self.secureplan.dialog {
            Some(Dialog::Export(dialog)) => Some(dialog.tab_id),
            _ => None,
        };
        for bound in &mut self.secureplan.sessions.bound {
            if let Some(job) = bound.export.as_mut() {
                if matches!(job.stage, Stage::Choosing) && job.waiting.is_none() && showing != Some(bound.tab_id) {
                    job.waiting = job.shown.take();
                }
            }
        }
        if self.secureplan.dialog.is_some() || self.secureplan.trust.prompt().is_some() {
            return;
        }
        let waiting = self.secureplan.sessions.bound.iter_mut().find_map(|b| {
            let job = b.export.as_mut()?;
            let dialog = job.waiting.take()?;
            job.shown = Some(dialog.clone());
            Some(dialog)
        });
        if let Some(dialog) = waiting {
            self.secureplan.dialog = Some(Dialog::Export(dialog));
        }
    }

    /// **Export…**: write the chosen format in memory and check it.
    pub(crate) fn secureplan_export_save(&mut self) -> Task<Message> {
        let Some(Dialog::Export(dialog)) = self.secureplan.dialog.as_mut() else { return Task::none() };
        if dialog.writing {
            return Task::none();
        }
        let (format, version) = match dialog.plan() {
            Ok(choice) => choice,
            Err(problem) => {
                self.command_line.push_error(&format!("SecurePlan: {problem}"));
                crate::app::secureplan::testdriver_event("export-loss", &dialog.known_loss().to_string());
                return Task::none();
            }
        };
        dialog.writing = true;
        let (tab_id, request_id, document) = (dialog.tab_id, dialog.request_id.clone(), Arc::clone(&dialog.document));
        let Some(job) = self.secureplan_export_job(tab_id, &request_id) else { return Task::none() };
        job.stage = Stage::Writing;
        self.secureplan_run_job(move || {
            let result = export::write(&document, format, version);
            Msg::ExportWritten(WriteDone { tab_id, request_id, format, version, result: Carry::new(result) })
        })
    }

    /// Cancel: nothing is written.
    pub(crate) fn secureplan_export_cancel(&mut self) {
        let Some(Dialog::Export(dialog)) = &self.secureplan.dialog else { return };
        let (tab_id, request_id) = (dialog.tab_id, dialog.request_id.clone());
        self.secureplan_finish_export(tab_id, export_result(&request_id, "cancelled"));
        self.command_line.push_info("SecurePlan: export cancelled. Nothing was written.");
        crate::app::secureplan::testdriver_event("export-cancelled", "");
    }

    pub(crate) fn secureplan_export_written(&mut self, done: WriteDone) -> Task<Message> {
        let Some(job) = self.secureplan_export_job(done.tab_id, &done.request_id) else { return Task::none() };
        if !matches!(job.stage, Stage::Writing) {
            return Task::none();
        }
        let Some(result) = done.result.take() else { return Task::none() };
        let bytes = match result {
            Ok(bytes) => Arc::new(bytes),
            Err(error) => {
                let reason = match error {
                    WriteError::Writer => "The drawing could not be written in that format and version.",
                    WriteError::Header => "The written drawing does not have the format and version that were chosen.",
                };
                self.secureplan_export_failed(done.tab_id, &done.request_id, error.code(), vec![reason.into(), "Nothing was saved.".into()]);
                return Task::none();
            }
        };
        let target = job.target.clone();
        job.stage = Stage::Saving { bytes, format: done.format, version: done.version };
        let stem = match &self.secureplan.dialog {
            Some(Dialog::Export(dialog)) if dialog.tab_id == done.tab_id => dialog.stem.clone(),
            _ => "drawing".into(),
        };
        self.secureplan_close_export_dialog(done.tab_id);
        let (tab_id, request_id) = (done.tab_id, done.request_id);
        if let Some(path) = target {
            return self.secureplan_export_picked(tab_id, &request_id, Some(path));
        }
        if !crate::app::secureplan::native_dialogs_allowed() {
            self.secureplan_finish_export(tab_id, export_result(&request_id, "cancelled"));
            self.command_line.push_error("SecurePlan: file dialogs are disabled in this session.");
            crate::app::secureplan::testdriver_event("export-cancelled", "");
            return Task::none();
        }
        let ext = done.format.ext();
        let name = crate::app::secureplan::session::file_name(&format!("{stem}-secureplan.{ext}"));
        Task::perform(
            async move {
                crate::sys::file_dialog()
                    .set_title("Export CAD with the SecurePlan design")
                    .set_file_name(name)
                    .add_filter(ext.to_ascii_uppercase(), &[ext])
                    .save_file()
                    .await
                    .map(|handle| crate::sys::handle_path(&handle))
            },
            move |path: Option<std::path::PathBuf>| Message::SecurePlan(Msg::ExportPicked(tab_id, request_id.clone(), path.map(Into::into))),
        )
    }

    /// The Save dialog closed: write the file there, the export's only write.
    pub(crate) fn secureplan_export_picked(&mut self, tab_id: u64, request_id: &str, path: Option<std::path::PathBuf>) -> Task<Message> {
        let Some(job) = self.secureplan_export_job(tab_id, request_id) else { return Task::none() };
        let Stage::Saving { bytes, format, version } = job.stage.clone() else { return Task::none() };
        let Some(path) = path else {
            self.secureplan_finish_export(tab_id, export_result(request_id, "cancelled"));
            self.command_line.push_info("SecurePlan: export cancelled. Nothing was written.");
            crate::app::secureplan::testdriver_event("export-cancelled", "");
            return Task::none();
        };
        if std::fs::write(&path, bytes.as_ref()).is_err() {
            self.secureplan_export_failed(
                tab_id,
                request_id,
                "WRITER_ERROR",
                vec!["The file could not be saved there. Check the folder and try the export again.".into()],
            );
            return Task::none();
        }
        let mut message = export_result(request_id, "written");
        message["fileName"] = json!(export::result_name(&path));
        message["format"] = json!(format.ext());
        message["formatVersion"] = json!(version.as_str());
        self.secureplan_finish_export(tab_id, message);
        self.command_line.push_info(&format!("SecurePlan: exported as {} {}.", format.ext().to_ascii_uppercase(), version.as_str()));
        crate::app::secureplan::testdriver_event("exported", &format!("{} {}", format.ext(), version.as_str()));
        Task::none()
    }

    /// Confirm the export dialog without the Save dialog, saving to `path`
    /// (tests and the `secureplan-test` driver).
    #[cfg(any(test, feature = "secureplan-test"))]
    pub(crate) fn secureplan_export_to(
        &mut self,
        path: std::path::PathBuf,
        format: Option<Format>,
        version: Option<acadrust::DxfVersion>,
        accept_loss: bool,
    ) -> Result<Task<Message>, String> {
        self.secureplan_show_waiting_export();
        let Some(Dialog::Export(dialog)) = self.secureplan.dialog.as_mut() else { return Err("no export is waiting".into()) };
        if let Some(format) = format {
            if !dialog.select(format, version) {
                return Err("the writer offers no such format and version".into());
            }
        }
        if accept_loss {
            dialog.acknowledge();
        }
        let (tab_id, request_id) = (dialog.tab_id, dialog.request_id.clone());
        if let Some(job) = self.secureplan_export_job(tab_id, &request_id) {
            job.target = Some(path);
        }
        Ok(self.secureplan_export_save())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::export::tests::sample_payload;
    use crate::app::secureplan::session::tests::{Harness, BASE};
    use crate::app::secureplan::ui::DialogKey;
    use crate::app::secureplan::{overlay, testutil};

    /// A survey whose current plan is the synthetic DXF, mapped 1:1 with CAD
    /// (0, 18000) at world (0, 0), in `mode`.
    fn exporting(tag: &str, mode: &str) -> Harness {
        let mut h = Harness::new(tag);
        h.survey_empty = false;
        let mut plan = crate::app::secureplan::session::tests::sample("openSession-edit")["cadPlan"].clone();
        plan["mapping"] = json!({ "cadOrigin": [0.0, 18000.0], "anchorMm": [0.0, 0.0], "scaleMmPerCadUnit": 1.0, "quarterTurns": 0 });
        plan["cadUnits"] = json!("mm");
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), mode, plan, BASE, "export");
        let _ = h.receive("sessionState");
        h
    }

    fn request(h: &mut Harness, drawing: &[u8], payload: &serde_json::Value) {
        let drawing_id = h.transfer("plan.dxf", "image/vnd.dxf", drawing);
        let payload_id = h.transfer("export.json", export::MEDIA_TYPE, payload.to_string().as_bytes());
        h.send(json!({
            "type": "exportRequest", "requestId": "x1",
            "snapshot": payload["snapshot"],
            "drawingTransferId": drawing_id, "payloadTransferId": payload_id,
        }));
    }

    fn full() -> serde_json::Value {
        let mut value = sample_payload();
        value["options"]["includeCoverage"] = json!(true);
        value
    }

    #[test]
    fn a_viewer_exports_through_the_dialog_and_the_file_reopens() {
        let mut h = exporting("export_view", "view");
        request(&mut h, &testutil::synthetic_dxf(), &full());
        let Some(Dialog::Export(dialog)) = &h.app.secureplan.dialog else { panic!("no export dialog") };
        assert_eq!(dialog.choice(), (Format::Dxf, acadrust::DxfVersion::AC1032), "the applied drawing's format by default");
        assert!(dialog.lines().iter().any(|line| line.contains("may overlap")));
        let state = h.state_where(|s| s["busy"]["operation"] == "export");
        assert_eq!(state["busy"]["operation"], "export");
        // Keyboard: Left moves to the previous choice (DWG R14), Right back to DXF 2018.
        h.key(DialogKey::Left);
        h.key(DialogKey::Right);
        let file = h.dir().join("exported.dxf");
        let _ = h.app.secureplan_export_to(file.clone(), None, None, false).unwrap();
        let (result, transfers) = h.receive("exportResult");
        assert!(transfers.is_empty(), "no bytes go back to the web");
        assert_eq!(result, json!({ "type": "exportResult", "requestId": "x1", "status": "written", "fileName": "exported.dxf", "format": "dxf", "formatVersion": "AC1032" }));
        let reread = crate::io::load_bytes("exported.dxf", std::fs::read(&file).unwrap()).unwrap();
        assert!(reread.layers.contains("SECUREPLAN-CAMERA") && reread.layers.contains("SECUREPLAN-ROUTE"));
        assert!(h.bound().export.is_none());
        assert!(h.app.secureplan.dialog.is_none());
        let _ = h.state_where(|s| s["busy"].is_null());
        // The open drawing is untouched: export works on the snapshot's copy.
        assert!(h.app.tabs[h.app.active_tab].scene.document.layers.iter().all(|l| !l.name.starts_with("SECUREPLAN")));
    }

    #[test]
    fn known_loss_needs_the_acknowledgement() {
        let mut h = exporting("export_loss", "edit");
        request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
        // The applied drawing lost two damaged items when it was read.
        let Some(Dialog::Export(dialog)) = h.app.secureplan.dialog.as_mut() else { panic!("no export dialog") };
        dialog.lost_entities = 2;
        dialog.loss.0 = usize::MAX;
        dialog.refresh();
        assert_eq!(dialog.known_loss(), 2);
        assert!(dialog.form.fields[ACKNOWLEDGE].enabled);
        assert!(dialog.lines().iter().any(|line| line.contains("2 damaged item(s)")));
        // Export… without Yes writes nothing.
        let file = h.dir().join("lossy.dxf");
        h.key(DialogKey::Next);
        h.key(DialogKey::Next);
        h.key(DialogKey::Activate);
        assert!(!file.exists());
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Export(d)) if !d.writing), "still choosing");
        assert!(h.app.command_line.last_error.clone().unwrap_or_default().contains("Choose Yes"));
        // Yes, then Export…: written.
        let _ = h.app.secureplan_export_to(file.clone(), None, None, true).unwrap();
        let (result, _) = h.receive("exportResult");
        assert_eq!(result["status"], "written");
        assert!(file.exists());
    }

    #[test]
    fn cancel_and_bad_requests_are_answered() {
        let mut h = exporting("export_cancel", "edit");
        request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
        h.key(DialogKey::Cancel);
        let (result, _) = h.receive("exportResult");
        assert_eq!(result, json!({ "type": "exportResult", "requestId": "x1", "status": "cancelled" }));
        assert!(h.bound().export.is_none());
        // A payload of another snapshot, or a drawing that does not read.
        let mut other = sample_payload();
        other["snapshot"]["version"] = json!(13);
        let drawing_id = h.transfer("plan.dxf", "image/vnd.dxf", &testutil::synthetic_dxf());
        let payload_id = h.transfer("export.json", export::MEDIA_TYPE, other.to_string().as_bytes());
        h.send(json!({ "type": "exportRequest", "requestId": "x2", "snapshot": sample_payload()["snapshot"], "drawingTransferId": drawing_id, "payloadTransferId": payload_id }));
        let (result, _) = h.receive("exportResult");
        assert_eq!((result["requestId"].as_str(), result["code"].as_str()), (Some("x2"), Some("INVALID")));
        request(&mut h, b"0\nSECTION\n2\nHEADER\n", &sample_payload());
        let (result, _) = h.receive("exportResult");
        assert_eq!(result["code"], "INVALID");
    }

    #[test]
    fn the_export_waits_for_an_open_dialog_and_ends_with_its_session() {
        let mut h = exporting("export_wait", "edit");
        h.app.secureplan.dialog = Some(Dialog::notice("Something else", vec![]));
        request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Choice { .. })), "the other dialog stays");
        h.key(DialogKey::Activate);
        let _ = h.app.update(Message::SecurePlan(Msg::Tick));
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Export(_))), "shown once the other closed");
        // Another dialog replaces it (a plan change, say): it comes back after.
        h.app.secureplan.dialog = Some(Dialog::notice("The plan changed", vec![]));
        let _ = h.app.update(Message::SecurePlan(Msg::Tick));
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Choice { .. })));
        h.key(DialogKey::Activate);
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Export(_))), "the export is still asked");
        let session = h.session;
        h.web.send(&json!({ "type": "close", "requestId": "c1", "reason": "pageClosed" }));
        h.pump_until_closed(session);
        assert!(h.bound().export.is_none());
        assert!(h.app.secureplan.dialog.is_none() || !matches!(&h.app.secureplan.dialog, Some(Dialog::Export(_))));
    }
}
