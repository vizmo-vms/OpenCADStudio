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
use crate::app::secureplan::export::{self, ComposeDone, Composed, ExportJob, JobKey, SaveDone, Stage, WriteDone, WriteError};
use crate::app::secureplan::session::{Drawing, Format};
use crate::app::secureplan::{Carry, Msg};
use crate::app::{Message, OpenCADStudio};

const FORMAT: usize = 0;
const ACKNOWLEDGE: usize = 1;

#[derive(Debug, Clone)]
pub struct ExportDialog {
    pub tab_id: u64,
    /// The export this dialog answers.
    pub key: JobKey,
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
    /// Said when the writer cannot write the applied drawing's own version
    /// (R13), so the default is the nearest version it can (EXP-03).
    pub version_note: Option<String>,
    /// Written and waiting: further confirmations are ignored.
    pub writing: bool,
}

impl ExportDialog {
    pub fn new(tab_id: u64, key: JobKey, request_id: String, composed: Composed) -> Self {
        let default = export::default_choice(composed.format, &composed.version);
        let version_note = acadrust::DxfVersion::parse(&composed.version)
            .filter(|&version| !export::writable(composed.format, version))
            .map(|version| {
                let (format, nearest) = export::choice(default);
                format!(
                    "SecurePlan CAD cannot write {}, the applied drawing's version, so the export defaults to {}, the nearest version it can write.",
                    export::version_label(composed.format, version),
                    export::version_label(format, nearest)
                )
            });
        let formats = export::choices().iter().map(|choice| choice.to_string()).collect();
        let mut acknowledge = Field::choice("Export without the objects listed", vec!["No".into(), "Yes".into()], 0);
        acknowledge.enabled = false;
        let fields = vec![Field::choice("Format and version", formats, default), acknowledge];
        let buttons = vec![("Export…".to_string(), Action::ExportSave), ("Cancel".to_string(), Action::ExportCancel)];
        let mut dialog = Self {
            tab_id,
            key,
            request_id,
            form: Form::new(fields, buttons, Action::ExportCancel),
            summary: composed.composition.summary(),
            document: Arc::new(composed.document),
            lost_entities: composed.lost_entities,
            stem: composed.stem,
            loss: (usize::MAX, 0),
            version_note,
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
        lines.extend(self.version_note.clone());
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

impl OpenCADStudio {
    /// `exportRequest` (EXP-01): read the snapshot's drawing and add the
    /// design on a worker, with the snapshot's own mapping, then show the
    /// export dialog.
    pub(crate) fn secureplan_export_request(&mut self, session: SessionId, request_id: &str, body: &serde_json::Value) -> Task<Message> {
        let take = |app: &mut Self, key: &str| body[key].as_u64().and_then(|id| app.secureplan_take_transfer(session, id as u32));
        let (drawing, payload) = (take(self, "drawingTransferId"), take(self, "payloadTransferId"));
        let refuse = |app: &mut Self, message: &str| {
            app.secureplan_send(session, json!({ "type": "exportResult", "requestId": request_id, "status": "error", "code": "INVALID" }));
            app.command_line.push_error(&format!("SecurePlan: {message}"));
            crate::app::secureplan::testdriver_event("export-failed", "INVALID");
            Task::none()
        };
        // The mapping of the snapshot the drawing and the design come from,
        // never the desktop's own stored one.
        let Some(mapping) = crate::app::secureplan::align::mapping_from_json(&body["mapping"]) else {
            return refuse(self, "the export request has no usable mapping.");
        };
        self.secureplan.next_job += 1;
        let key = JobKey { session, serial: self.secureplan.next_job };
        let Some(bound) = self.secureplan.sessions.by_session_mut(session) else {
            return refuse(self, "open the survey from SecurePlan before exporting.");
        };
        if bound.export.is_some() {
            return refuse(self, "an export is already under way for this survey.");
        }
        let drawing = drawing.and_then(|t| Format::from_media_type(&t.media_type).map(|format| (t, format)));
        let (Some((drawing, format)), Some(payload)) = (drawing, payload.filter(|t| t.media_type == export::MEDIA_TYPE)) else {
            return refuse(self, "the export request was incomplete.");
        };
        let tab_id = bound.tab_id;
        bound.export = Some(ExportJob { key, request_id: request_id.to_string(), stage: Stage::Composing, waiting: None, shown: None, target: None });
        let drawing = Drawing {
            bytes: Arc::new(drawing.bytes.expose().clone()),
            name: crate::app::secureplan::session::file_name(drawing.name.expose()).into(),
            format,
            format_version: String::new(),
        };
        let payload = payload.bytes.expose().clone();
        let snapshot = body["snapshot"].clone();
        self.secureplan_report_states();
        self.command_line.push_info("SecurePlan: preparing the CAD export…");
        let failed = Msg::ExportComposed(ComposeDone { tab_id, key, result: Carry::new(Err("Preparing the export stopped unexpectedly.".into())) });
        self.secureplan_run_job(
            move || {
                let result = export::prepare(&drawing, &payload, &snapshot, &mapping);
                Msg::ExportComposed(ComposeDone { tab_id, key, result: Carry::new(result) })
            },
            failed,
        )
    }

    /// Tab `tab_id`'s export job `key`, if it is still wanted: the tab is
    /// still bound to the session that asked for it.
    fn secureplan_export_job(&mut self, tab_id: u64, key: JobKey) -> Option<&mut ExportJob> {
        let bound = self.secureplan.sessions.by_tab_mut(tab_id).filter(|b| b.session == Some(key.session))?;
        bound.export.as_mut().filter(|job| job.key == key)
    }

    /// Answer export `key` with `message` (its request id is added) and
    /// forget it.
    fn secureplan_finish_export(&mut self, tab_id: u64, key: JobKey, mut message: serde_json::Value) {
        let Some(job) = self.secureplan_export_job(tab_id, key) else { return };
        message["type"] = json!("exportResult");
        message["requestId"] = json!(job.request_id);
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.export = None;
        }
        self.secureplan_send(key.session, message);
        self.secureplan_close_export_dialog(tab_id);
        self.secureplan_report_states();
    }

    fn secureplan_close_export_dialog(&mut self, tab_id: u64) {
        if matches!(&self.secureplan.dialog, Some(Dialog::Export(dialog)) if dialog.tab_id == tab_id) {
            self.secureplan.dialog = None;
        }
    }

    /// The session ended, the tab closed or is rebinding to another session:
    /// the export is dropped unanswered, and its workers' results are ignored.
    pub(crate) fn secureplan_drop_export(&mut self, tab_id: u64) {
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.export = None;
        }
        self.secureplan_close_export_dialog(tab_id);
    }

    fn secureplan_export_failed(&mut self, tab_id: u64, key: JobKey, code: &str, lines: Vec<String>) {
        self.secureplan_finish_export(tab_id, key, json!({ "status": "error", "code": code }));
        self.secureplan.dialog = Some(Dialog::notice("Export failed", lines));
        crate::app::secureplan::testdriver_event("export-failed", code);
    }

    pub(crate) fn secureplan_export_composed(&mut self, done: ComposeDone) -> Task<Message> {
        let Some(job) = self.secureplan_export_job(done.tab_id, done.key) else { return Task::none() };
        if !matches!(job.stage, Stage::Composing) {
            return Task::none();
        }
        let request_id = job.request_id.clone();
        let Some(result) = done.result.take() else { return Task::none() };
        let composed = match result {
            Ok(composed) => composed,
            Err(message) => {
                self.secureplan_export_failed(done.tab_id, done.key, "INVALID", vec![message, "Nothing was written.".into()]);
                return Task::none();
            }
        };
        let dialog = Box::new(ExportDialog::new(done.tab_id, done.key, request_id, composed));
        let job = self.secureplan_export_job(done.tab_id, done.key).expect("found above");
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
            Some(Dialog::Export(dialog)) => Some(dialog.key),
            _ => None,
        };
        for bound in &mut self.secureplan.sessions.bound {
            if let Some(job) = bound.export.as_mut() {
                if matches!(job.stage, Stage::Choosing) && job.waiting.is_none() && showing != Some(job.key) {
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
        let (tab_id, key, document) = (dialog.tab_id, dialog.key, Arc::clone(&dialog.document));
        let Some(job) = self.secureplan_export_job(tab_id, key) else { return Task::none() };
        job.stage = Stage::Writing;
        let failed = Msg::ExportWritten(WriteDone { tab_id, key, format, version, result: Carry::new(Err(WriteError::Writer)) });
        self.secureplan_run_job(
            move || {
                let result = export::write(&document, format, version);
                Msg::ExportWritten(WriteDone { tab_id, key, format, version, result: Carry::new(result) })
            },
            failed,
        )
    }

    /// Cancel: nothing is written.
    pub(crate) fn secureplan_export_cancel(&mut self) {
        let Some(Dialog::Export(dialog)) = &self.secureplan.dialog else { return };
        let (tab_id, key) = (dialog.tab_id, dialog.key);
        self.secureplan_finish_export(tab_id, key, json!({ "status": "cancelled" }));
        self.secureplan_close_export_dialog(tab_id);
        self.command_line.push_info("SecurePlan: export cancelled. Nothing was written.");
        crate::app::secureplan::testdriver_event("export-cancelled", "");
    }

    pub(crate) fn secureplan_export_written(&mut self, done: WriteDone) -> Task<Message> {
        let Some(job) = self.secureplan_export_job(done.tab_id, done.key) else { return Task::none() };
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
                self.secureplan_export_failed(done.tab_id, done.key, error.code(), vec![reason.into(), "Nothing was saved.".into()]);
                return Task::none();
            }
        };
        let target = job.target.clone();
        job.stage = Stage::Saving { bytes, format: done.format, version: done.version };
        let stem = match &self.secureplan.dialog {
            Some(Dialog::Export(dialog)) if dialog.key == done.key => dialog.stem.clone(),
            _ => "drawing".into(),
        };
        self.secureplan_close_export_dialog(done.tab_id);
        let (tab_id, key) = (done.tab_id, done.key);
        if let Some(path) = target {
            return self.secureplan_export_picked(tab_id, key, Some(path));
        }
        #[cfg(test)]
        if self.secureplan.test_hold_save {
            return Task::none();
        }
        if !crate::app::secureplan::native_dialogs_allowed() {
            self.secureplan_finish_export(tab_id, key, json!({ "status": "cancelled" }));
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
            move |path: Option<std::path::PathBuf>| Message::SecurePlan(Msg::ExportPicked(tab_id, key, path.map(Into::into))),
        )
    }

    /// The Save dialog closed: write the file there on a worker, the
    /// export's only write.
    pub(crate) fn secureplan_export_picked(&mut self, tab_id: u64, key: JobKey, path: Option<std::path::PathBuf>) -> Task<Message> {
        let Some(job) = self.secureplan_export_job(tab_id, key) else { return Task::none() };
        let Stage::Saving { bytes, format, version } = job.stage.clone() else { return Task::none() };
        let Some(path) = path else {
            self.secureplan_finish_export(tab_id, key, json!({ "status": "cancelled" }));
            self.command_line.push_info("SecurePlan: export cancelled. Nothing was written.");
            crate::app::secureplan::testdriver_event("export-cancelled", "");
            return Task::none();
        };
        job.stage = Stage::Storing;
        let name = export::result_name(&path);
        let failed = Msg::ExportSaved(SaveDone { tab_id, key, name: name.clone(), format, version, saved: false });
        self.secureplan_run_job(
            move || {
                let saved = std::fs::write(&path, bytes.as_ref()).is_ok();
                Msg::ExportSaved(SaveDone { tab_id, key, name, format, version, saved })
            },
            failed,
        )
    }

    /// The file was written, or could not be.
    pub(crate) fn secureplan_export_saved(&mut self, done: SaveDone) {
        let Some(job) = self.secureplan_export_job(done.tab_id, done.key) else { return };
        if !matches!(job.stage, Stage::Storing) {
            return;
        }
        if !done.saved {
            self.secureplan_export_failed(
                done.tab_id,
                done.key,
                "WRITER_ERROR",
                vec!["The file could not be saved there. Check the folder and try the export again.".into()],
            );
            return;
        }
        let (format, version) = (done.format, done.version);
        let message = json!({ "status": "written", "fileName": done.name, "format": format.ext(), "formatVersion": version.as_str() });
        self.secureplan_finish_export(done.tab_id, done.key, message);
        self.command_line.push_info(&format!("SecurePlan: exported as {} {}.", format.ext().to_ascii_uppercase(), version.as_str()));
        crate::app::secureplan::testdriver_event("exported", &format!("{} {}", format.ext(), version.as_str()));
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
        let (tab_id, key) = (dialog.tab_id, dialog.key);
        if let Some(job) = self.secureplan_export_job(tab_id, key) {
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

    /// The survey's stored mapping in [`exporting`].
    fn stored() -> serde_json::Value {
        json!({ "cadOrigin": [0.0, 18000.0], "anchorMm": [0.0, 0.0], "scaleMmPerCadUnit": 1.0, "quarterTurns": 0 })
    }

    fn request(h: &mut Harness, drawing: &[u8], payload: &serde_json::Value) {
        request_with(h, "x1", drawing, payload, stored());
    }

    fn request_with(h: &mut Harness, id: &str, drawing: &[u8], payload: &serde_json::Value, mapping: serde_json::Value) {
        let drawing_id = h.transfer("plan.dxf", "image/vnd.dxf", drawing);
        let payload_id = h.transfer("export.json", export::MEDIA_TYPE, payload.to_string().as_bytes());
        h.send(json!({
            "type": "exportRequest", "requestId": id,
            "snapshot": payload["snapshot"], "mapping": mapping,
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

    /// Press **Export…** from the keyboard: to the button, then Enter.
    fn press_export(h: &mut Harness) {
        let Some(Dialog::Export(dialog)) = &h.app.secureplan.dialog else { panic!("no export dialog") };
        let steps = dialog.form.fields.iter().filter(|f| f.enabled).count() - usize::from(dialog.form.focus > 0);
        for _ in 0..steps {
            h.key(DialogKey::Next);
        }
        h.key(DialogKey::Activate);
    }

    #[test]
    fn damaged_items_the_reader_dropped_need_the_acknowledgement() {
        let mut h = exporting("export_damage", "edit");
        // The applied drawing itself has a damaged item: reading it drops it.
        request(&mut h, &crate::app::secureplan::session::tests::damaged_dxf(), &sample_payload());
        let Some(Dialog::Export(dialog)) = &h.app.secureplan.dialog else { panic!("no export dialog") };
        assert_eq!(dialog.lost_entities, 1, "the reader's count reaches the dialog");
        assert_eq!(dialog.known_loss(), 1);
        assert!(dialog.form.fields[ACKNOWLEDGE].enabled);
        assert!(dialog.lines().iter().any(|line| line.contains("1 damaged item(s)")));
        // Export… without Yes writes nothing.
        let file = h.dir().join("damaged.dxf");
        if let Some(job) = h.app.secureplan.sessions.bound[0].export.as_mut() {
            job.target = Some(file.clone());
        }
        press_export(&mut h);
        assert!(!file.exists());
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Export(d)) if !d.writing), "still choosing");
        assert!(h.app.command_line.last_error.clone().unwrap_or_default().contains("Choose Yes"));
        // Yes, then Export…: written.
        let _ = h.app.secureplan_export_to(file.clone(), None, None, true).unwrap();
        let (result, _) = h.receive("exportResult");
        assert_eq!(result["status"], "written");
        assert!(file.exists());
    }

    /// The synthetic plan as DXF with an object of a type the reader does
    /// not know: kept as DXF codes, it can be written back to DXF, not DWG.
    fn dxf_with_unknown_object() -> Vec<u8> {
        let text = String::from_utf8(testutil::synthetic_dxf()).unwrap();
        // Before the group-code line of the ENTITIES section's ENDSEC.
        let entities = text.find("ENTITIES").unwrap();
        let end = entities + text[entities..].find("ENDSEC").unwrap();
        let cut = text[..text[..end].rfind('\n').unwrap()].rfind('\n').unwrap() + 1;
        let unknown = "  0\nSECUREPLANTESTOBJECT\n  5\nFFF0\n100\nAcDbEntity\n  8\n0\n";
        format!("{}{unknown}{}", &text[..cut], &text[cut..]).into_bytes()
    }

    /// EXP-03: an R13 drawing exports as R14 by default, the dialog says why,
    /// and anything R14 cannot hold is known loss needing the acknowledgement.
    #[test]
    fn an_r13_drawing_defaults_to_r14_says_so_and_counts_what_r14_loses() {
        let mut document = testutil::synthetic_document();
        let handle = document.allocate_handle();
        document.objects.insert(
            handle,
            acadrust::objects::ObjectType::Unknown {
                type_name: "SYNTHETIC_R13_OBJECT".into(),
                handle,
                owner: acadrust::Handle::NULL,
                raw_dxf_codes: None,
                raw_dwg_data: Some(vec![0u8; 4]),
                raw_dwg_handle_bits: 0,
                raw_dwg_version: Some(acadrust::DxfVersion::AC1012),
            },
        );
        let composed = export::Composed {
            document,
            composition: Default::default(),
            format: Format::Dwg,
            version: "AC1012".into(),
            lost_entities: 0,
            stem: "r13".into(),
        };
        let key = JobKey { session: 1, serial: 1 };
        let dialog = ExportDialog::new(1, key, "x1".into(), composed);
        assert_eq!(dialog.choice(), (Format::Dwg, acadrust::DxfVersion::AC1014), "the nearest version the writer offers");
        let lines = dialog.lines();
        assert!(
            lines.iter().any(|line| line.contains("cannot write DWG R13 (AC1012)") && line.contains("defaults to DWG R14 (AC1014)")),
            "{lines:?}"
        );
        assert_eq!(dialog.known_loss(), 1, "R14 cannot hold the R13-only object");
        assert!(dialog.form.fields[ACKNOWLEDGE].enabled);
        assert!(dialog.plan().is_err(), "not without the acknowledgement");
        // A drawing in a version the writer offers gets no such line.
        let composed = export::Composed {
            document: testutil::synthetic_document(),
            composition: Default::default(),
            format: Format::Dwg,
            version: "AC1018".into(),
            lost_entities: 0,
            stem: "r2004".into(),
        };
        assert!(ExportDialog::new(1, key, "x2".into(), composed).version_note.is_none());
    }

    /// DSK-06: the rendered dropdowns choose the format and the
    /// acknowledgement; a fixed acknowledgement says so and cannot change.
    #[test]
    fn the_dropdowns_choose_the_format_and_the_acknowledgement() {
        use crate::app::secureplan::ui::tests::{pick_rendered, shows_focus};
        use crate::app::secureplan::ui::{Dialog, Msg};
        let composed = |version: &str| export::Composed {
            document: testutil::synthetic_document(),
            composition: Default::default(),
            format: Format::Dwg,
            version: version.into(),
            lost_entities: 1,
            stem: "synthetic".into(),
        };
        let key = JobKey { session: 1, serial: 1 };
        let dialog = ExportDialog::new(1, key, "x1".into(), composed("AC1032"));
        assert!(dialog.form.fields[ACKNOWLEDGE].enabled, "a lost object needs the acknowledgement");
        let mut app = crate::app::OpenCADStudio::new_for_test();
        let format = pick_rendered(view(&dialog), "Format and version", 1);
        let acknowledge = pick_rendered(view(&dialog), "Export without the objects listed", 1);
        app.secureplan.dialog = Some(Dialog::Export(Box::new(dialog)));
        for (messages, field) in [(format, FORMAT), (acknowledge, ACKNOWLEDGE)] {
            let [Message::SecurePlan(chosen @ Msg::FormSelect(index, 1))] = messages.as_slice() else { panic!("{messages:?}") };
            assert_eq!(*index, field);
            let _ = app.update(Message::SecurePlan(chosen.clone()));
            let Some(Dialog::Export(dialog)) = &app.secureplan.dialog else { panic!("the dialog closed") };
            assert_eq!((dialog.form.selected(field), dialog.form.focus), (1, field));
            assert!(shows_focus(view(dialog), dialog.form.fields[field].label));
        }
        let Some(Dialog::Export(dialog)) = &app.secureplan.dialog else { panic!("the dialog closed") };
        assert_eq!(dialog.choice(), export::choice(1));
        // Nothing lost: the acknowledgement is fixed.
        let dialog = ExportDialog::new(1, key, "x2".into(), export::Composed { lost_entities: 0, ..composed("AC1032") });
        assert!(!dialog.form.fields[ACKNOWLEDGE].enabled);
        assert!(pick_rendered(view(&dialog), "Export without the objects listed", 1).is_empty(), "a fixed choice changed");
    }

    #[test]
    fn a_format_that_cannot_hold_the_drawing_needs_the_acknowledgement() {
        let mut h = exporting("export_format_loss", "edit");
        request(&mut h, &dxf_with_unknown_object(), &sample_payload());
        let Some(Dialog::Export(dialog)) = h.app.secureplan.dialog.as_mut() else { panic!("no export dialog") };
        assert_eq!(dialog.lost_entities, 0);
        // DXF keeps the object: nothing lost, nothing to acknowledge.
        assert_eq!(dialog.choice().0, Format::Dxf);
        assert_eq!(dialog.known_loss(), 0);
        assert!(!dialog.form.fields[ACKNOWLEDGE].enabled);
        // DWG 2018 cannot hold it: the loss is counted and must be acknowledged.
        assert!(dialog.select(Format::Dwg, Some(acadrust::DxfVersion::AC1032)));
        assert_eq!(dialog.known_loss(), 1);
        assert!(dialog.lines().iter().any(|line| line.contains("1 object(s) DWG AC1032 cannot hold")), "{:?}", dialog.lines());
        let file = h.dir().join("lossy.dwg");
        if let Some(job) = h.app.secureplan.sessions.bound[0].export.as_mut() {
            job.target = Some(file.clone());
        }
        press_export(&mut h);
        assert!(!file.exists(), "not saved before the acknowledgement");
        // Back to DXF and to DWG again: the acknowledgement is asked again.
        let Some(Dialog::Export(dialog)) = h.app.secureplan.dialog.as_mut() else { panic!("no export dialog") };
        dialog.acknowledge();
        assert!(dialog.select(Format::Dxf, None));
        assert!(dialog.select(Format::Dwg, Some(acadrust::DxfVersion::AC1032)));
        assert!(dialog.plan().is_err(), "a new choice needs its own Yes");
        let _ = h.app.secureplan_export_to(file.clone(), Some(Format::Dwg), Some(acadrust::DxfVersion::AC1032), true).unwrap();
        let (result, _) = h.receive("exportResult");
        assert_eq!((result["status"].as_str(), result["format"].as_str()), (Some("written"), Some("dwg")));
        assert!(file.exists());
    }

    #[test]
    fn the_export_uses_the_requests_mapping_not_the_stored_one() {
        let mut h = exporting("export_mapping", "edit");
        // The snapshot was applied with another alignment than the one the
        // desktop holds: a quarter turn, centimetres and an anchor.
        let snapshot = json!({ "cadOrigin": [100.0, 500.0], "anchorMm": [1000.0, 2000.0], "scaleMmPerCadUnit": 10.0, "quarterTurns": 1 });
        request_with(&mut h, "x1", &testutil::synthetic_dxf(), &sample_payload(), snapshot.clone());
        let Some(Dialog::Export(dialog)) = &h.app.secureplan.dialog else { panic!("no export dialog") };
        let mapping = crate::app::secureplan::align::mapping_from_json(&snapshot).unwrap();
        let camera = dialog
            .document
            .entities()
            .find_map(|e| match e {
                acadrust::EntityType::Insert(i) if i.common.layer == "SECUREPLAN-CAMERA" => Some(i.insert_point),
                _ => None,
            })
            .expect("the camera");
        let expected = crate::app::secureplan::align::world_to_cad(&mapping, [6000.0, 3000.0]);
        assert!((camera.x - expected[0]).abs() < 1e-9 && (camera.y - expected[1]).abs() < 1e-9, "{camera:?} {expected:?}");
        assert_ne!(h.bound().plan_alignment.map(|a| a.mapping), Some(mapping), "the stored mapping differs");
        // A request without a usable mapping never reaches the desktop: the schema requires it.
        assert!(crate::app::secureplan::protocol::validate_message(
            crate::app::secureplan::protocol::Direction::WebToDesktop,
            &json!({ "type": "exportRequest", "requestId": "x9", "snapshot": sample_payload()["snapshot"], "drawingTransferId": 1, "payloadTransferId": 2 })
        )
        .is_err());
    }

    /// Re-pair the survey from a new page and open it there, before the old
    /// session's close arrives; the reopened drawing's load is run at once.
    fn re_pair(h: &mut Harness, pairing: u8) {
        let mut plan = crate::app::secureplan::session::tests::sample("openSession-edit")["cadPlan"].clone();
        plan["mapping"] = stored();
        plan["cadUnits"] = json!("mm");
        let old = h.session;
        h.pair_again_overtaking(pairing);
        assert_eq!(h.bound().session, Some(old), "the old session's close has not been handled");
        let held = h.held();
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", plan, BASE, "export");
        if h.held() > held {
            h.release(h.held() - 1);
        }
    }

    #[test]
    fn a_re_pair_ends_the_old_export_at_every_stage() {
        for (pairing, stage) in [(40u8, "composing"), (41, "writing"), (42, "saving")] {
            let mut h = exporting(&format!("export_repair_{stage}"), "edit");
            h.hold_jobs();
            let old_file = h.dir().join("old.dxf");
            request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
            let old_key = h.bound().export.as_ref().unwrap().key;
            if stage != "composing" {
                h.release(0);
                if stage == "writing" {
                    let _ = h.app.secureplan_export_to(old_file.clone(), None, None, false).unwrap();
                } else {
                    // Written, and its Save dialog stands open.
                    h.app.secureplan.test_hold_save = true;
                    press_export(&mut h);
                    h.release(0);
                    assert!(matches!(h.bound().export.as_ref().unwrap().stage, Stage::Saving { .. }), "{stage}");
                }
            }
            // A new page opens the survey and asks for an export with the same request id.
            re_pair(&mut h, pairing);
            assert!(h.bound().export.is_none(), "{stage}: the old export ended with the rebinding");
            assert!(!matches!(h.app.secureplan.dialog, Some(Dialog::Export(_))), "{stage}: its dialog closed");
            request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
            let new_key = h.bound().export.as_ref().expect("the new export started").key;
            assert_ne!(new_key, old_key);
            // The old worker or Save dialog finishes now: nothing happens.
            match stage {
                "saving" => {
                    let _ = h.app.update(Message::SecurePlan(Msg::ExportPicked(h.tab_id(), old_key, Some(old_file.clone().into()))));
                }
                _ => h.release(0),
            }
            assert!(!old_file.exists(), "{stage}: the old export wrote a file");
            assert_eq!(h.bound().export.as_ref().map(|job| job.key), Some(new_key), "{stage}");
            assert!(matches!(h.bound().export.as_ref().unwrap().stage, Stage::Composing), "{stage}: the new export is untouched");
            h.send_state_probe();
            let seen = h.types_until("sessionState");
            assert!(!seen.iter().any(|t| t == "exportResult"), "{stage}: the new page got an answer for the old export: {seen:?}");
            // The new export goes on and answers the new page.
            h.release(h.held() - 1);
            h.app.secureplan.test_hold_save = false;
            let new_file = h.dir().join("new.dxf");
            let _ = h.app.secureplan_export_to(new_file.clone(), None, None, false).unwrap();
            h.release(0);
            h.release(0);
            let (result, _) = h.receive("exportResult");
            assert_eq!((result["requestId"].as_str(), result["status"].as_str()), (Some("x1"), Some("written")), "{stage}");
            assert!(new_file.exists() && !old_file.exists(), "{stage}");
            // The old session's close, late, changes nothing for the new one.
            h.deliver_held_close();
            assert_eq!(h.bound().session, Some(h.session), "{stage}");
        }
    }

    #[test]
    fn the_file_is_written_on_a_worker() {
        let mut h = exporting("export_worker_write", "edit");
        request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
        h.hold_jobs();
        let file = h.dir().join("slow target.dxf");
        let _ = h.app.secureplan_export_to(file.clone(), None, None, false).unwrap();
        h.release(0);
        // Written and checked in memory; the file write itself waits for its worker.
        assert!(matches!(h.bound().export.as_ref().unwrap().stage, Stage::Storing));
        assert_eq!(h.held(), 1);
        assert!(!file.exists(), "not written on the UI thread");
        h.release(0);
        assert!(file.exists());
        let (result, _) = h.receive("exportResult");
        assert_eq!((result["status"].as_str(), result["fileName"].as_str()), (Some("written"), Some("slow target.dxf")));
    }

    #[test]
    fn a_failed_export_worker_answers_and_ends_the_export() {
        for stage in ["compose", "write", "save"] {
            let mut h = exporting(&format!("export_panic_{stage}"), "edit");
            let file = h.dir().join("panic.dxf");
            match stage {
                "compose" => {
                    h.app.secureplan.test_panic_next_job = true;
                    request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
                }
                "write" => {
                    request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
                    h.app.secureplan.test_panic_next_job = true;
                    let _ = h.app.secureplan_export_to(file.clone(), None, None, false).unwrap();
                }
                _ => {
                    request(&mut h, &testutil::synthetic_dxf(), &sample_payload());
                    h.hold_jobs();
                    let _ = h.app.secureplan_export_to(file.clone(), None, None, false).unwrap();
                    // The write finishes; the file write's worker (started now) fails.
                    h.app.secureplan.test_panic_next_job = true;
                    h.release(0);
                    h.release(0);
                }
            }
            let (result, _) = h.receive("exportResult");
            assert_eq!(result["status"], "error", "{stage}");
            assert!(h.bound().export.is_none(), "{stage}: the export ended");
            assert_eq!(h.app.secureplan.workers, 0, "{stage}");
            let _ = h.state_where(|s| s["busy"].is_null());
        }
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
        h.send(json!({ "type": "exportRequest", "requestId": "x2", "snapshot": sample_payload()["snapshot"], "mapping": stored(), "drawingTransferId": drawing_id, "payloadTransferId": payload_id }));
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
