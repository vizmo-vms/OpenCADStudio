//! The conversion menu (CNV-01, DSK-06).
//!
//! `SECUREPLANCONVERT` (the ribbon's **Convert selection**) opens a keyboard
//! menu over the current selection: **Create SecurePlan walls**, **Create
//! cable route** or Cancel. It is a choice dialog, so Tab, the arrows, Enter,
//! Space and Escape work as in every SecurePlan dialog, with the "›" focus
//! marker. `SECUREPLANCONVERT WALLS` or `ROUTE` skips the menu.
//!
//! Conversion needs a connected session in edit mode and a clean drawing whose
//! plan is the survey's current one: after an edit, Apply comes first (the
//! refusal offers it) and the user selects again. The candidates go to the web
//! in `convertRequest`, which shows its own preview and adds the objects;
//! `convertResult` reports what happened.

use serde_json::json;

use super::{Action, Dialog};
use crate::app::secureplan::convert::{self, Kind};
use crate::app::secureplan::session::Mode;
use crate::app::OpenCADStudio;

/// Why conversion cannot start, and whether Apply would help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub message: String,
    pub apply_first: bool,
}

impl Blocked {
    fn new(message: &str) -> Self {
        Self { message: message.into(), apply_first: false }
    }
}

/// The menu for `count` selected objects of tab `tab_id`.
pub fn menu(tab_id: u64, count: usize) -> Dialog {
    Dialog::choice(
        "Convert to SecurePlan",
        vec![
            format!("{count} object(s) selected. Choose what SecurePlan creates from their lines and curves."),
            "SecurePlan then asks for the layer, status and material (or the cable), and adds the objects to the design.".into(),
        ],
        vec![
            ("Create SecurePlan walls".to_string(), Action::Convert(tab_id, Kind::Walls)),
            ("Create cable route".to_string(), Action::Convert(tab_id, Kind::Route)),
            ("Cancel".to_string(), Action::Dismiss),
        ],
    )
}

/// A CAD point for the user, from world mm.
fn cad_place(mapping: &crate::app::secureplan::publish::Mapping, at_mm: [f64; 2]) -> String {
    let [x, y] = crate::app::secureplan::align::world_to_cad(mapping, at_mm);
    format!("({}, {})", super::format_number((x * 1000.0).round() / 1000.0), super::format_number((y * 1000.0).round() / 1000.0))
}

impl OpenCADStudio {
    /// Whether the active tab's selection may be converted now (CNV-01).
    pub(crate) fn secureplan_can_convert(&self) -> Result<(), Blocked> {
        let index = self.active_tab;
        let tab_id = self.tabs.get(index).map(|tab| tab.id).ok_or_else(|| Blocked::new("Open the survey from SecurePlan first."))?;
        self.secureplan_can_edit_tab(tab_id).map_err(|message| Blocked::new(&message))?;
        let bound = self.secureplan.sessions.by_tab(tab_id).expect("checked above");
        if !bound.has_plan || bound.plan_alignment.is_none() {
            return Err(Blocked { message: "Apply the drawing to SecurePlan first; conversion works on the applied plan.".into(), apply_first: true });
        }
        if bound.unresolved || bound.staged.is_some() || bound.recovered_base.is_some() {
            return Err(Blocked::new("This drawing is not the survey's current plan. Open the survey from SecurePlan again."));
        }
        if self.secureplan_has_unapplied(index) || self.secureplan_modified(index) {
            return Err(Blocked {
                message: "The drawing has changes SecurePlan does not have. Apply first, then select again.".into(),
                apply_first: true,
            });
        }
        if self.tabs[index].scene.current_layout != "Model" {
            return Err(Blocked::new("Switch to model space and select the lines to convert."));
        }
        if self.tabs[index].scene.selected.is_empty() {
            return Err(Blocked::new("Select the lines or curves to convert first."));
        }
        Ok(())
    }

    /// Say why conversion cannot start, offering Apply when that is the way.
    fn secureplan_convert_blocked(&mut self, blocked: Blocked) {
        super::super::testdriver_event("convert-refused", &blocked.message);
        if blocked.apply_first {
            self.secureplan.dialog = Some(Dialog::choice(
                "Apply first",
                vec![blocked.message],
                vec![("Apply first".to_string(), Action::Command("SECUREPLANAPPLY")), ("Cancel".to_string(), Action::Dismiss)],
            ));
        } else {
            self.command_line.push_error(&format!("SecurePlan: {}", blocked.message));
        }
    }

    /// `SECUREPLANCONVERT [WALLS|ROUTE]`: the menu, or straight to one kind.
    pub(crate) fn secureplan_open_convert(&mut self, kind: Option<Kind>) -> iced::Task<crate::app::Message> {
        if let Err(blocked) = self.secureplan_can_convert() {
            self.secureplan_convert_blocked(blocked);
            return iced::Task::none();
        }
        let tab = &self.tabs[self.active_tab];
        match kind {
            Some(kind) => self.secureplan_convert(tab.id, kind),
            None => {
                self.secureplan.dialog = Some(menu(tab.id, tab.scene.selected.len()));
                iced::Task::none()
            }
        }
    }

    /// Convert tab `tab_id`'s selection to `kind` and send `convertRequest`.
    pub(crate) fn secureplan_convert(&mut self, tab_id: u64, kind: Kind) -> iced::Task<crate::app::Message> {
        // Checked again: the menu may have been open while things changed.
        if self.tabs.get(self.active_tab).map(|tab| tab.id) != Some(tab_id) {
            self.command_line.push_error("SecurePlan: the drawing to convert is no longer the active one.");
            return iced::Task::none();
        }
        if let Err(blocked) = self.secureplan_can_convert() {
            self.secureplan_convert_blocked(blocked);
            return iced::Task::none();
        }
        let bound = self.secureplan.sessions.by_tab(tab_id).expect("checked above");
        let (session, base, mapping) = (bound.session, bound.base_identity.clone(), bound.plan_alignment.expect("checked above").mapping);
        let scene = &self.tabs[self.active_tab].scene;
        let found = match convert::candidates(scene, &scene.selected, &mapping, kind) {
            Ok(found) => found,
            Err(refusal) => {
                let mut lines = vec![refusal.message.clone()];
                if !refusal.at_mm.is_empty() {
                    let places: Vec<String> = refusal.at_mm.iter().take(5).map(|at| cad_place(&mapping, *at)).collect();
                    let more = refusal.at_mm.len().saturating_sub(5);
                    let more = if more > 0 { format!(" and {more} more") } else { String::new() };
                    lines.push(format!("Look at {}{more} (drawing coordinates).", places.join(", ")));
                }
                lines.push("Nothing was sent to SecurePlan.".into());
                self.secureplan.dialog = Some(Dialog::notice("Not converted", lines));
                super::super::testdriver_event("convert-refused", &refusal.message);
                return iced::Task::none();
            }
        };
        let request_id = self.secureplan.sessions.next_request_id();
        let message = json!({
            "type": "convertRequest",
            "requestId": request_id,
            "baseIdentity": base,
            "candidates": found.to_json(),
        });
        // Checked as the bridge will check it: a message it refuses would end
        // the session.
        if crate::app::secureplan::protocol::encode_outbound(&message).is_err() {
            self.secureplan.dialog = Some(Dialog::notice(
                "Not converted",
                vec!["The selection is too large to send in one conversion. Select fewer objects.".into(), "Nothing was sent to SecurePlan.".into()],
            ));
            super::super::testdriver_event("convert-refused", "too large");
            return iced::Task::none();
        }
        let sent = session.is_some_and(|session| self.secureplan_send(session, message));
        if !sent {
            self.command_line.push_error("SecurePlan: the conversion could not be sent. Open the survey from SecurePlan again.");
            super::super::testdriver_event("convert-refused", "not connected");
            return iced::Task::none();
        }
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            bound.converts.push(request_id);
        }
        self.command_line.push_info(&found.summary());
        for line in convert::rejected_lines(&found) {
            self.command_line.push_info(&format!("SecurePlan: {line}."));
        }
        let count = match kind {
            Kind::Walls => found.walls.len(),
            Kind::Route => found.points.len(),
        };
        super::super::testdriver_event("convert-sent", &format!("{} {count} rejected={}", kind.as_str(), found.rejected_total));
        iced::Task::none()
    }

    /// `convertResult` (CNV-03): report what the web did with a conversion.
    pub(crate) fn secureplan_convert_result(&mut self, session: crate::app::secureplan::bridge::SessionId, request_id: &str, body: &serde_json::Value) {
        let Some(bound) = self.secureplan.sessions.by_session_mut(session) else { return };
        let Some(position) = bound.converts.iter().position(|id| id == request_id) else { return };
        bound.converts.remove(position);
        let view_only = bound.mode == Mode::View;
        match body["status"].as_str().unwrap_or_default() {
            "added" => {
                let count = body["elementIds"].as_array().map_or(0, Vec::len);
                self.command_line.push_info(&format!(
                    "SecurePlan: added {count} object(s) to the design. SecurePlan saves them with the design; they are independent of this drawing."
                ));
                super::super::testdriver_event("converted", &count.to_string());
            }
            "cancelled" => {
                self.command_line.push_info("SecurePlan: the conversion was cancelled in SecurePlan.");
                super::super::testdriver_event("convert-cancelled", "");
            }
            _ => {
                let code = body["code"].as_str().unwrap_or("INVALID");
                let reason = match code {
                    "PLAN_CHANGED" => "The survey's plan changed in SecurePlan. Select again once the new plan is loaded.",
                    "LEASE_LOST" => "SecurePlan no longer holds the survey for editing.",
                    "NOT_EDIT_MODE" => "SecurePlan opened this survey for viewing only.",
                    "LIMIT" => "The design would be over SecurePlan's limits. Convert fewer objects.",
                    _ if view_only => "SecurePlan opened this survey for viewing only.",
                    _ => "SecurePlan refused the candidates as invalid: for example, part of them lies outside the design canvas. Check the selection and the alignment, then convert again.",
                };
                self.secureplan.dialog = Some(Dialog::notice("Not converted", vec![reason.into(), "Nothing was added to the design.".into()]));
                super::super::testdriver_event("convert-failed", code);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::secureplan::session::tests::{Harness, BASE};
    use crate::app::secureplan::ui::{Action, Dialog, DialogKey};
    use crate::app::secureplan::{overlay, testutil, Msg};
    use crate::app::Message;
    use serde_json::json;

    /// A survey whose current plan is the synthetic drawing, mapped 1:1 with
    /// CAD (0, 18000) at world (0, 0).
    fn applied(tag: &str) -> Harness {
        let mut h = Harness::new(tag);
        h.survey_empty = false;
        let mut plan = crate::app::secureplan::session::tests::sample("openSession-edit")["cadPlan"].clone();
        plan["mapping"] = json!({ "cadOrigin": [0.0, 18000.0], "anchorMm": [0.0, 0.0], "scaleMmPerCadUnit": 1.0, "quarterTurns": 0 });
        plan["cadUnits"] = json!("mm");
        h.open(Some(("synthetic.dxf", crate::app::secureplan::session::Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", plan, BASE, "edit");
        let _ = h.receive("sessionState");
        h
    }

    /// Select the synthetic plan's lines along y = 0 and x = 30000 (the
    /// outline's first two sides).
    fn select_corner(h: &mut Harness) {
        let index = h.app.active_tab;
        let scene = &mut h.app.tabs[index].scene;
        let handles: Vec<_> = scene
            .document
            .entities()
            .filter(|entity| match entity {
                acadrust::EntityType::Line(line) => {
                    (line.start.y == 0.0 && line.end.y == 0.0) || (line.start.x == 30000.0 && line.end.x == 30000.0)
                }
                _ => false,
            })
            .map(|entity| entity.common().handle)
            .collect();
        assert_eq!(handles.len(), 2);
        scene.select_entities(&handles);
    }

    #[test]
    fn the_keyboard_menu_sends_wall_candidates_with_the_base_identity() {
        let mut h = applied("convert_walls");
        select_corner(&mut h);
        let _ = h.app.dispatch_command("SECUREPLANCONVERT");
        let Some(Dialog::Choice { form, .. }) = &h.app.secureplan.dialog else { panic!("no menu") };
        let labels: Vec<&str> = form.buttons.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["Create SecurePlan walls", "Create cable route", "Cancel"]);
        assert_eq!(form.focus, 0, "focus starts on walls");
        h.key(DialogKey::Activate);
        assert!(h.app.secureplan.dialog.is_none());
        let (request, _) = h.receive("convertRequest");
        assert_eq!(request["baseIdentity"], BASE);
        assert_eq!(request["candidates"]["kind"], "walls");
        let walls = request["candidates"]["walls"].as_array().unwrap();
        assert_eq!(walls.len(), 2);
        // CAD (0, 0)–(30000, 0) is world (0, 18000)–(30000, 18000).
        assert_eq!(walls[0], json!({ "start": [0.0, 18000.0], "end": [30000.0, 18000.0] }));
        let id = request["requestId"].as_str().unwrap().to_string();
        assert_eq!(h.bound().converts, vec![id.clone()]);
        h.send(json!({ "type": "convertResult", "requestId": id, "status": "added", "elementIds": ["w1", "w2"] }));
        assert!(h.bound().converts.is_empty());
        assert!(h.app.command_line.history.iter().any(|line| line.text.contains("added 2 object(s)")));
    }

    #[test]
    fn a_route_goes_through_the_menu_and_an_error_result_is_reported() {
        let mut h = applied("convert_route");
        select_corner(&mut h);
        let _ = h.app.dispatch_command("SECUREPLANCONVERT");
        h.key(DialogKey::Next);
        h.key(DialogKey::Activate);
        let (request, _) = h.receive("convertRequest");
        assert_eq!(request["candidates"]["kind"], "route");
        let points = request["candidates"]["points"].as_array().unwrap();
        assert_eq!(points, &vec![json!([0.0, 18000.0]), json!([30000.0, 18000.0]), json!([30000.0, 0.0])]);
        let id = request["requestId"].clone();
        h.send(json!({ "type": "convertResult", "requestId": id, "status": "error", "code": "PLAN_CHANGED" }));
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Choice { title, .. }) if title == "Not converted"));
        // The web refuses candidates outside the canvas as INVALID: the user is told why.
        h.app.secureplan.dialog = None;
        let _ = h.app.dispatch_command("SECUREPLANCONVERT ROUTE");
        let (request, _) = h.receive("convertRequest");
        h.send(json!({ "type": "convertResult", "requestId": request["requestId"], "status": "error", "code": "INVALID" }));
        let Some(Dialog::Choice { lines, .. }) = &h.app.secureplan.dialog else { panic!("no notice") };
        assert!(lines[0].contains("outside the design canvas"), "{lines:?}");
    }

    #[test]
    fn conversion_needs_a_clean_applied_drawing_in_edit_mode() {
        // Edited since it was applied: Apply comes first.
        let mut h = applied("convert_dirty");
        select_corner(&mut h);
        h.edit((0.0, 0.0), (1000.0, 1000.0));
        let _ = h.app.dispatch_command("SECUREPLANCONVERT WALLS");
        let Some(Dialog::Choice { title, form, .. }) = &h.app.secureplan.dialog else { panic!("no refusal") };
        assert_eq!(title, "Apply first");
        assert_eq!(form.buttons[0].1, Action::Command("SECUREPLANAPPLY"));
        assert!(h.bound().converts.is_empty(), "nothing sent");

        // View only: refused.
        let mut h = applied("convert_view");
        select_corner(&mut h);
        h.send(json!({ "type": "sessionMode", "requestId": "m1", "mode": "view", "reason": "roleChanged" }));
        let _ = h.app.dispatch_command("SECUREPLANCONVERT WALLS");
        assert!(h.app.command_line.last_error.clone().unwrap_or_default().contains("viewing only"));
        assert!(h.bound().converts.is_empty());

        // No plan yet (a new survey): Apply first.
        let mut h = Harness::new("convert_new");
        h.open_dxf();
        let _ = h.receive("sessionState");
        select_corner(&mut h);
        let _ = h.app.dispatch_command("SECUREPLANCONVERT ROUTE");
        assert!(matches!(&h.app.secureplan.dialog, Some(Dialog::Choice { title, .. }) if title == "Apply first"));

        // Nothing selected.
        let mut h = applied("convert_empty");
        let _ = h.app.dispatch_command("SECUREPLANCONVERT");
        assert!(h.app.secureplan.dialog.is_none());
        assert!(h.app.command_line.last_error.clone().unwrap_or_default().contains("Select"));
    }

    #[test]
    fn a_re_alignment_not_yet_applied_does_not_move_the_candidates() {
        let mut h = applied("convert_realigned");
        select_corner(&mut h);
        // Re-aligned but not applied: the survey's stored mapping still holds.
        let stored = h.bound().plan_alignment.expect("stored");
        let mut moved = stored;
        moved.mapping.anchor_mm = [5000.0, 5000.0];
        moved.mapping.quarter_turns = 1;
        let tab_id = h.tab_id();
        h.app.secureplan_set_alignment(tab_id, moved);
        assert!(h.bound().realigned);
        let _ = h.app.dispatch_command("SECUREPLANCONVERT WALLS");
        let (request, _) = h.receive("convertRequest");
        assert_eq!(request["candidates"]["walls"][0], json!({ "start": [0.0, 18000.0], "end": [30000.0, 18000.0] }));
    }

    #[test]
    fn a_refused_route_says_where_and_sends_nothing() {
        let mut h = applied("convert_gap");
        // The outline's bottom side and the diagonal: they do not meet.
        let index = h.app.active_tab;
        let scene = &mut h.app.tabs[index].scene;
        let handles: Vec<_> = scene
            .document
            .entities()
            .filter(|e| matches!(e, acadrust::EntityType::Line(l) if (l.start.y == 0.0 && l.end.y == 0.0) || l.start.x == 12345.6))
            .map(|e| e.common().handle)
            .collect();
        scene.select_entities(&handles);
        let _ = h.app.update(Message::SecurePlan(Msg::Action(Action::Convert(h.tab_id(), crate::app::secureplan::convert::Kind::Route))));
        let Some(Dialog::Choice { title, lines, .. }) = &h.app.secureplan.dialog else { panic!("no notice") };
        assert_eq!(title, "Not converted");
        assert!(lines.iter().any(|line| line.contains("drawing coordinates")), "{lines:?}");
        assert!(h.bound().converts.is_empty());
    }
}
