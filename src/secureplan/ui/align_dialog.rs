//! The alignment dialog (PUB-02, DSK-06).
//!
//! Units come from the drawing; a unitless or ambiguous drawing needs chosen
//! units or a known-length calibration. Rotation is a quarter turn and
//! translation a CAD point and the survey position it lands on, previewed on
//! the design overlay as they change. On an empty survey the page goes to the
//! survey origin, so the translation fields are fixed. Re-aligning a stored
//! mapping shows the current and new alignment, and the overlay draws both.
//!
//! **Measure in drawing** hides the dialog while two points are picked (with
//! object snaps, or typed), fills the known length with their distance and
//! switches to calibration. Escape brings the dialog back unchanged.

use std::sync::{Arc, Mutex};

use glam::DVec3;
use iced::widget::{column, text};
use iced::{Element, Length};

use super::{Action, Dialog, Field, Form};
use crate::app::secureplan::align::{calibrated_scale, mapping_at, top_left_corner, Alignment, Units};
use crate::app::secureplan::publish::Mapping;
use crate::app::{Message, OpenCADStudio};
use crate::command::{CadCommand, CmdResult};
use crate::scene::model::wire_model::WireModel;

const UNITS: usize = 0;
const CAD_LENGTH: usize = 1;
const REAL_LENGTH: usize = 2;
const ROTATION: usize = 3;
const CAD_X: usize = 4;
const CAD_Y: usize = 5;
const SURVEY_X: usize = 6;
const SURVEY_Y: usize = 7;

const CHOOSE: &str = "Choose…";
const CALIBRATE: &str = "Calibrate from a known length";

#[derive(Debug, Clone)]
pub struct AlignDialog {
    pub tab_id: u64,
    pub form: Form,
    /// The survey has no design and no comments yet: the page is placed at the origin.
    pub empty_survey: bool,
    /// The drawing's visible extents, for the default translation.
    pub extents: [f64; 4],
    /// The alignment in use before this dialog, if any.
    pub before: Option<Alignment>,
    /// Continue to Apply once the alignment is confirmed.
    pub then_apply: bool,
    /// The session and drawing generation the dialog was opened for: a
    /// replaced drawing or a reconnect makes it stale.
    pub opened_for: (Option<crate::app::secureplan::bridge::SessionId>, u64),
    /// Why **Measure in drawing** could not start, shown in the dialog.
    pub measure_problem: Option<String>,
}

/// The alignment dialog but for the drawing's extents, which are checked
/// on a worker.
#[derive(Debug)]
pub struct AlignStart {
    declared: Option<Units>,
    before: Option<Alignment>,
    empty_survey: bool,
    then_apply: bool,
    opened_for: (Option<crate::app::secureplan::bridge::SessionId>, u64),
}

impl AlignStart {
    /// The dialog for tab `tab_id`, whose drawing has the visible `extents`.
    pub fn dialog(self, tab_id: u64, extents: [f64; 4]) -> AlignDialog {
        let window = crate::app::secureplan::publish::default_window(extents);
        let mut dialog = AlignDialog::new(tab_id, self.declared, self.before, self.empty_survey, window, self.then_apply);
        dialog.opened_for = self.opened_for;
        dialog
    }
}

fn unit_options() -> Vec<String> {
    let mut options = vec![CHOOSE.to_string()];
    options.extend(Units::CHOICES.iter().map(|u| u.as_str().to_string()));
    options.push(CALIBRATE.to_string());
    options
}

impl AlignDialog {
    pub fn new(tab_id: u64, declared: Option<Units>, before: Option<Alignment>, empty_survey: bool, extents: [f64; 4], then_apply: bool) -> Self {
        let options = unit_options();
        let units = before.map(|a| a.units).or(declared);
        let selected = match units {
            Some(Units::Unitless) => options.len() - 1,
            Some(units) => options.iter().position(|o| o == units.as_str()).unwrap_or(0),
            None => 0,
        };
        let turns = before.map_or(0, |a| a.mapping.quarter_turns);
        let origin = before.map_or(top_left_corner(extents, turns), |a| a.mapping.cad_origin);
        let anchor = before.map_or([0.0, 0.0], |a| a.mapping.anchor_mm);
        let calibrated_scale = before.filter(|a| a.units == Units::Unitless).map_or(1.0, |a| a.mapping.scale_mm_per_cad_unit);
        let mut fields = vec![
            Field::choice("Drawing units", options, selected),
            Field::number("Known length in the drawing", 1.0),
            Field::number("Its real length (mm)", calibrated_scale),
            Field::choice("Rotation", ["0°", "90°", "180°", "270°"].map(String::from).to_vec(), usize::from(turns)),
            Field::number("CAD point X", origin[0]),
            Field::number("CAD point Y", origin[1]),
            Field::number("…lands at survey X (mm)", anchor[0]),
            Field::number("…lands at survey Y (mm)", anchor[1]),
        ];
        for index in [CAD_X, CAD_Y, SURVEY_X, SURVEY_Y] {
            fields[index].enabled = !empty_survey;
        }
        let mut dialog = Self {
            tab_id,
            form: Form::new(
                fields,
                vec![
                    ("Confirm alignment".into(), Action::AlignConfirm),
                    ("Measure in drawing".into(), Action::AlignMeasure),
                    ("Cancel".into(), Action::Dismiss),
                ],
                Action::Dismiss,
            ),
            empty_survey,
            extents,
            before,
            then_apply,
            opened_for: (None, 0),
            measure_problem: None,
        };
        dialog.refresh();
        dialog
    }

    fn units(&self) -> Option<Units> {
        let options = unit_options();
        match options.get(self.form.selected(UNITS)).map(String::as_str) {
            Some(CALIBRATE) => Some(Units::Unitless),
            Some(name) => Units::parse(name).filter(|u| *u != Units::Unitless),
            None => None,
        }
    }

    /// Enable the calibration fields only when calibrating.
    pub fn refresh(&mut self) {
        let calibrating = self.units() == Some(Units::Unitless);
        self.form.fields[CAD_LENGTH].enabled = calibrating;
        self.form.fields[REAL_LENGTH].enabled = calibrating;
    }

    /// A length measured in the drawing: calibrate from it, then ask for its
    /// real length.
    pub fn measured(&mut self, length: f64) {
        self.form.select(UNITS, unit_options().len() - 1);
        self.refresh();
        self.form.set_number(CAD_LENGTH, length);
        self.form.focus = REAL_LENGTH;
    }

    /// The alignment the fields describe.
    pub fn proposed(&self) -> Result<Alignment, String> {
        let units = self.units().ok_or("Choose the drawing's units, or calibrate from a known length.")?;
        let scale = match units.mm_per_unit() {
            Some(scale) => scale,
            None => {
                let cad = self.form.number(CAD_LENGTH).ok_or("Enter the known length in drawing units.")?;
                let real = self.form.number(REAL_LENGTH).ok_or("Enter its real length in millimetres.")?;
                calibrated_scale(cad, real).ok_or("The calibration lengths must be positive.")?
            }
        };
        let turns = self.form.selected(ROTATION) as u8;
        let mapping = if self.empty_survey {
            mapping_at(self.extents, scale, turns, [0.0, 0.0])
        } else {
            let number = |index: usize, what: &str| self.form.number(index).ok_or(format!("Enter the {what}."));
            Mapping {
                cad_origin: [number(CAD_X, "CAD point X")?, number(CAD_Y, "CAD point Y")?],
                anchor_mm: [number(SURVEY_X, "survey X")?, number(SURVEY_Y, "survey Y")?],
                scale_mm_per_cad_unit: scale,
                quarter_turns: turns,
            }
        };
        Ok(Alignment { units, mapping })
    }
}

fn describe(alignment: &Alignment) -> String {
    let m = &alignment.mapping;
    format!(
        "{} ({} mm per unit), rotation {}°, CAD ({}, {}) at survey ({}, {}) mm",
        alignment.units.as_str(),
        super::format_number(m.scale_mm_per_cad_unit),
        u16::from(m.quarter_turns) * 90,
        super::format_number(m.cad_origin[0]),
        super::format_number(m.cad_origin[1]),
        super::format_number(m.anchor_mm[0]),
        super::format_number(m.anchor_mm[1]),
    )
}

pub fn view(dialog: &AlignDialog) -> Element<'_, Message> {
    let mut content = column![].spacing(10).padding(8);
    let intro = if dialog.empty_survey {
        "The survey has no design and no comments yet, so the plan's top-left corner goes to the survey origin (0, 0). Choose the units and rotation."
    } else {
        "Choose the units and rotation, and which drawing point lands where in the survey. The SecurePlan design overlay moves as you change them."
    };
    content = content.push(text(intro).size(13).width(Length::Fixed(520.0)));
    content = content.push(super::form_view(&dialog.form));
    if let Some(before) = &dialog.before {
        content = content.push(text(format!("Current: {}", describe(before))).size(12));
    }
    match dialog.proposed() {
        Ok(proposed) => {
            let label = if dialog.before.is_some() { "New" } else { "Alignment" };
            content = content.push(text(format!("{label}: {}", describe(&proposed))).size(12));
        }
        Err(problem) => content = content.push(text(format!("To do: {problem}")).size(12)),
    }
    if let Some(problem) = &dialog.measure_problem {
        content = content.push(text(format!("Measure in drawing: {problem}")).size(12).width(Length::Fixed(520.0)));
    }
    content.push(super::keys_hint()).into()
}

/// The command that measures for the dialog.
const MEASURE: &str = "SECUREPLANMEASURE";

/// The alignment dialog, hidden while [`MeasureCommand`] runs.
#[derive(Debug)]
pub struct Measuring {
    dialog: AlignDialog,
    length: Arc<Mutex<Option<f64>>>,
}

impl Measuring {
    pub fn tab_id(&self) -> u64 {
        self.dialog.tab_id
    }
}

/// Why the active space cannot be measured for the alignment: only model
/// coordinates are drawing units (a paper layout outside a model viewport is
/// in sheet units, and the block editor in the block's own units).
fn measure_space_problem(scene: &crate::scene::Scene) -> Option<&'static str> {
    if scene.block_edit_block.is_some() {
        Some("it measures the drawing, not a block. Cancel, close the block editor, then open Align again.")
    } else if scene.current_layout != "Model" && scene.active_viewport.is_none() {
        Some("it measures model space. Cancel, switch to the Model tab or into a layout viewport, then open Align again.")
    } else {
        None
    }
}

/// Two points, picked like any command's (object snaps, typed coordinates),
/// in the world coordinate system whatever the UCS; their distance in
/// drawing units, in plan (X and Y), which is what the alignment scales.
struct MeasureCommand {
    first: Option<DVec3>,
    length: Arc<Mutex<Option<f64>>>,
}

impl CadCommand for MeasureCommand {
    fn name(&self) -> &'static str {
        MEASURE
    }

    fn prompt(&self) -> String {
        match self.first {
            None => "Measure in drawing  Specify first point:".into(),
            Some(_) => "Measure in drawing  Specify second point:".into(),
        }
    }

    fn on_point(&mut self, point: DVec3) -> CmdResult {
        let Some(first) = self.first else {
            self.first = Some(point);
            return CmdResult::NeedPoint;
        };
        let length = (point - first).truncate().length();
        if !(length.is_finite() && length > 0.0) {
            return CmdResult::ReportError("SecurePlan: the two points are the same place. Pick two different points.".into());
        }
        *self.length.lock().unwrap_or_else(|e| e.into_inner()) = Some(length);
        CmdResult::Measurement(format!("SecurePlan: measured {} drawing units.", super::format_number(length)))
    }

    fn on_enter(&mut self) -> CmdResult {
        CmdResult::Cancel
    }

    fn on_mouse_move(&mut self, point: DVec3) -> Option<WireModel> {
        let first = self.first?;
        Some(WireModel::solid_f64("secureplan_measure".into(), vec![first.to_array(), point.to_array()], WireModel::CYAN, false))
    }
}

impl OpenCADStudio {
    /// **Measure in drawing**: hide the alignment dialog and ask for two points.
    pub(crate) fn secureplan_measure_start(&mut self) {
        let Some(Dialog::Align(dialog)) = self.secureplan.dialog.take() else { return };
        let index = self.secureplan_tab_index(dialog.tab_id);
        let editable = self.secureplan_can_edit_tab(dialog.tab_id);
        let (Some(index), Ok(())) = (index, editable.clone()) else {
            if let Err(message) = editable {
                self.command_line.push_error(&message);
            }
            self.secureplan.dialog = Some(Dialog::Align(dialog));
            return;
        };
        if let Some(problem) = measure_space_problem(&self.tabs[index].scene) {
            let mut dialog = dialog;
            dialog.measure_problem = Some(problem.to_string());
            self.command_line.push_error(&format!("SecurePlan: Measure in drawing: {problem}"));
            self.secureplan.dialog = Some(Dialog::Align(dialog));
            return;
        }
        self.active_tab = index;
        // Measure starts as any command does: no previous point for Ortho or
        // Polar, and no snap or dynamic input left from an earlier pick.
        self.reset_command_start_state(index);
        let length = Arc::new(Mutex::new(None));
        let command = MeasureCommand { first: None, length: length.clone() };
        self.command_line.push_info(&command.prompt());
        self.tabs[index].active_cmd = Some(Box::new(command));
        self.secureplan.measuring = Some(Measuring { dialog: *dialog, length });
    }

    /// Once the measuring command has ended (a length, Escape or anything
    /// else), bring the alignment dialog back.
    pub(crate) fn secureplan_settle_measure(&mut self) {
        let Some(measuring) = &self.secureplan.measuring else { return };
        let running = self.tabs.iter().find(|tab| tab.id == measuring.tab_id()).is_some_and(|tab| {
            [&tab.active_cmd, &tab.suspended_cmd].iter().any(|command| command.as_ref().is_some_and(|c| c.name() == MEASURE))
        });
        // Another dialog showing is answered first.
        if running || self.secureplan.dialog.is_some() {
            return;
        }
        let Some(Measuring { mut dialog, length }) = self.secureplan.measuring.take() else { return };
        let Some(index) = self.secureplan_tab_index(dialog.tab_id) else { return };
        // The drawing was replaced or reconnected meanwhile: the form
        // describes a drawing that is gone.
        if !self.secureplan_align_current(&dialog) {
            self.command_line.push_info("SecurePlan: the drawing changed while measuring. Open Align again.");
            return;
        }
        // Nor does it come back while the survey cannot be edited (busy,
        // disconnected or view-only).
        if let Err(reason) = self.secureplan_can_edit_tab(dialog.tab_id) {
            self.command_line.push_error(&format!("{reason} Then open Align again."));
            return;
        }
        let length = length.lock().unwrap_or_else(|e| e.into_inner()).take();
        dialog.measure_problem = None;
        match (length, measure_space_problem(&self.tabs[index].scene)) {
            (Some(length), None) => dialog.measured(length),
            (Some(_), Some(problem)) => dialog.measure_problem = Some(problem.to_string()),
            (None, _) => {}
        }
        self.secureplan.dialog = Some(Dialog::Align(Box::new(dialog)));
    }

    /// Whether the alignment dialog still describes its tab's drawing.
    fn secureplan_align_current(&self, dialog: &AlignDialog) -> bool {
        self.secureplan.sessions.by_tab(dialog.tab_id).is_some_and(|bound| (bound.session, bound.generation) == dialog.opened_for)
    }

    /// Open the alignment dialog for the active bound document.
    /// Open the alignment dialog once the drawing's extents are checked (on
    /// a worker: see [`OpenCADStudio::secureplan_check_drawing`]).
    pub(crate) fn secureplan_open_align(&mut self, then_apply: bool) -> iced::Task<Message> {
        if let Err(message) = self.secureplan_can_edit_survey() {
            self.command_line.push_error(&message);
            return iced::Task::none();
        }
        let index = self.active_tab;
        let cancelled = self.cancel_active_command_for_space_change();
        self.secureplan.measuring = None;
        let tab = &self.tabs[index];
        let Some(bound) = self.secureplan.sessions.by_tab(tab.id) else { return cancelled };
        let start = AlignStart {
            declared: Units::declared(tab.scene.document.header.insertion_units),
            before: bound.alignment,
            empty_survey: crate::app::secureplan::session::survey_is_empty(bound),
            then_apply,
            opened_for: (bound.session, bound.generation),
        };
        let check = self.secureplan_check_drawing(index, crate::app::secureplan::session::CheckThen::Align(start));
        iced::Task::batch([cancelled, check])
    }

    /// The mapping the alignment dialog currently proposes, for the overlay.
    pub(crate) fn secureplan_alignment_preview(&self) -> Option<Mapping> {
        match &self.secureplan.dialog {
            Some(Dialog::Align(dialog)) => dialog.proposed().ok().map(|a| a.mapping),
            _ => None,
        }
    }

    /// Confirm the dialog's alignment. Errors keep the dialog open.
    pub(crate) fn secureplan_confirm_align(&mut self) -> iced::Task<Message> {
        let Some(Dialog::Align(dialog)) = &self.secureplan.dialog else { return iced::Task::none() };
        let (tab_id, then_apply) = (dialog.tab_id, dialog.then_apply);
        if !self.secureplan_align_current(dialog) {
            self.secureplan.dialog = None;
            self.command_line.push_error("SecurePlan: the drawing changed. Open Align again.");
            return iced::Task::none();
        }
        if let Err(reason) = self.secureplan_can_edit_tab(tab_id) {
            self.command_line.push_error(&reason);
            return iced::Task::none();
        }
        let proposed = match dialog.proposed() {
            Ok(proposed) => proposed,
            Err(problem) => {
                self.command_line.push_error(&format!("SecurePlan: {problem}"));
                return iced::Task::none();
            }
        };
        self.secureplan.dialog = None;
        self.secureplan_set_alignment(tab_id, proposed);
        if then_apply {
            return self.secureplan_begin_apply();
        }
        iced::Task::none()
    }

    /// Use `alignment` for the next Apply of the bound tab `tab_id`.
    pub(crate) fn secureplan_set_alignment(&mut self, tab_id: u64, alignment: Alignment) {
        if let Some(bound) = self.secureplan.sessions.by_tab_mut(tab_id) {
            // Re-aligning a stored mapping is disclosed in the Apply summary.
            if bound.has_plan && bound.alignment != Some(alignment) {
                bound.realigned = true;
            }
            bound.alignment = Some(alignment);
        }
        self.command_line.push_info("SecurePlan: alignment set. Apply sends it with the drawing.");
        crate::app::secureplan::testdriver_event("aligned", alignment.units.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::ui::DialogKey;

    #[test]
    fn a_unitless_drawing_needs_units_or_a_calibration() {
        let mut dialog = AlignDialog::new(1, None, None, false, [0.0, 0.0, 100.0, 50.0], false);
        assert!(dialog.proposed().unwrap_err().contains("units"));
        // Choose feet with the keyboard.
        for _ in 0..5 {
            dialog.form.key(DialogKey::Right);
        }
        dialog.refresh();
        let feet = dialog.proposed().unwrap();
        assert_eq!(feet.units, Units::Ft);
        assert_eq!(feet.mapping.scale_mm_per_cad_unit, 304.8);
        assert!(!dialog.form.fields[CAD_LENGTH].enabled);
        // Or calibrate: 250 units measure 5,000 mm.
        dialog.form.key(DialogKey::Right);
        dialog.refresh();
        assert!(dialog.form.fields[CAD_LENGTH].enabled);
        dialog.form.set_text(CAD_LENGTH, "250".into());
        dialog.form.set_text(REAL_LENGTH, "5000".into());
        let calibrated = dialog.proposed().unwrap();
        assert_eq!((calibrated.units, calibrated.mapping.scale_mm_per_cad_unit), (Units::Unitless, 20.0));
    }

    /// PUB-02: Measure in drawing, through the dialog's keys, the dropdown
    /// message and typed points at the command line.
    #[test]
    fn measure_in_drawing_fills_the_known_length_and_escape_changes_nothing() {
        use crate::app::secureplan::session::tests::Harness;
        use crate::app::secureplan::ui::{Dialog, FieldKind, Msg};
        let mut h = Harness::new("measure");
        h.survey_empty = false;
        h.open_dxf();
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        let form = |h: &Harness| match &h.app.secureplan.dialog {
            Some(Dialog::Align(dialog)) => dialog.form.clone(),
            _ => panic!("no alignment dialog"),
        };
        let text = |form: &Form, index: usize| match &form.fields[index].kind {
            FieldKind::Number { text } => text.clone(),
            FieldKind::Choice { .. } => unreachable!(),
        };
        // Some choices of the user's own: rotation 90° from its rendered
        // dropdown, and a CAD point.
        let Some(Dialog::Align(dialog)) = &h.app.secureplan.dialog else { panic!("no alignment dialog") };
        let picked = crate::app::secureplan::ui::tests::pick_rendered(view(dialog), "Rotation", 1);
        assert!(matches!(picked.as_slice(), [Message::SecurePlan(Msg::FormSelect(ROTATION, 1))]), "{picked:?}");
        for message in picked {
            let _ = h.app.update(message);
        }
        assert_eq!((form(&h).selected(ROTATION), form(&h).focus), (1, ROTATION));
        if let Some(Dialog::Align(dialog)) = &h.app.secureplan.dialog {
            assert!(crate::app::secureplan::ui::tests::shows_focus(view(dialog), "Rotation"));
        }
        let _ = h.app.update(Message::SecurePlan(Msg::FormInput(CAD_X, "12.5".into())));
        let before = form(&h);
        if let Some(Dialog::Align(dialog)) = &h.app.secureplan.dialog {
            let mut ui = iced_test::simulator(view(dialog));
            assert!(ui.find("  Measure in drawing").is_ok(), "offered whatever the units");
            assert!(ui.find("‹ 90° ›").is_err(), "choices are dropdowns, not click-to-cycle buttons");
        }
        let measuring = |h: &Harness| h.app.tabs[h.app.active_tab].active_cmd.as_ref().is_some_and(|c| c.name() == MEASURE);
        let point = |h: &mut Harness, typed: &str| {
            let _ = h.app.update(Message::CommandInput(typed.into()));
            let _ = h.app.update(Message::CommandSubmit);
        };
        // Shift+Tab from the first field reaches Cancel, then Measure in drawing.
        h.app.secureplan.dialog.as_mut().unwrap().form_mut().focus = 0;
        h.key(DialogKey::Previous);
        h.key(DialogKey::Previous);
        h.key(DialogKey::Activate);
        assert!(h.app.secureplan.dialog.is_none() && measuring(&h), "the dialog hides while points are picked");
        // Escape while picking: the dialog comes back as it was.
        point(&mut h, "0,0");
        let _ = h.app.update(Message::CommandEscape);
        assert!(!measuring(&h));
        let mut back = form(&h);
        back.focus = before.focus;
        assert_eq!(back, before, "Escape changes nothing");
        // A zero length is refused and the second point asked again.
        h.key(DialogKey::Activate);
        assert!(measuring(&h));
        point(&mut h, "10,10");
        point(&mut h, "10,10");
        assert!(h.app.command_line.last_error.clone().unwrap_or_default().contains("same place"));
        assert!(measuring(&h) && h.app.secureplan.dialog.is_none());
        point(&mut h, "40,50");
        let after = form(&h);
        assert_eq!(after.selected(UNITS), unit_options().len() - 1, "calibrating");
        assert!(after.fields[CAD_LENGTH].enabled && after.fields[REAL_LENGTH].enabled);
        assert_eq!(after.number(CAD_LENGTH), Some(50.0), "the distance in drawing units");
        assert_eq!(after.focus, REAL_LENGTH);
        assert_eq!(after.selected(ROTATION), 1, "the other fields are kept");
        for index in [CAD_X, CAD_Y, SURVEY_X, SURVEY_Y] {
            assert_eq!(text(&after, index), text(&before, index));
        }
        // Its real length, typed: 50 units are 5,000 mm, and the overlay previews it.
        h.key(DialogKey::Backspace);
        for c in "5000".chars() {
            h.key(DialogKey::Char(c));
        }
        assert_eq!(h.app.secureplan_alignment_preview().map(|m| m.scale_mm_per_cad_unit), Some(100.0));
    }

    fn measuring(h: &crate::app::secureplan::session::tests::Harness) -> bool {
        h.app.tabs.iter().any(|tab| tab.active_cmd.as_ref().is_some_and(|c| c.name() == MEASURE))
    }

    fn start_measuring(h: &mut crate::app::secureplan::session::tests::Harness) {
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        let _ = h.app.update(Message::SecurePlan(crate::app::secureplan::ui::Msg::Action(Action::AlignMeasure)));
    }

    fn typed_point(h: &mut crate::app::secureplan::session::tests::Harness, typed: &str) {
        let _ = h.app.update(Message::CommandInput(typed.into()));
        let _ = h.app.update(Message::CommandSubmit);
    }

    /// Only model coordinates are drawing units: a paper layout outside a
    /// model viewport and the block editor are refused, with the reason in
    /// the dialog.
    #[test]
    fn measure_refuses_paper_space_and_the_block_editor() {
        use crate::app::secureplan::session::tests::Harness;
        let mut h = Harness::new("measure_space");
        h.open_dxf();
        for case in ["paper", "block editor"] {
            let index = h.app.active_tab;
            let scene = &mut h.app.tabs[index].scene;
            if case == "paper" {
                scene.current_layout = "Layout1".into();
                scene.active_viewport = None;
            } else {
                scene.block_edit_block = Some(scene.current_layout_block_handle_pub());
            }
            start_measuring(&mut h);
            assert!(!measuring(&h), "{case}: measuring started");
            let Some(Dialog::Align(dialog)) = &h.app.secureplan.dialog else { panic!("{case}: the dialog closed") };
            let problem = dialog.measure_problem.clone().unwrap_or_default();
            assert!(problem.contains(if case == "paper" { "model space" } else { "not a block" }), "{case}: {problem}");
            assert!(iced_test::simulator(view(dialog)).find(format!("Measure in drawing: {problem}")).is_ok(), "{case}: not shown");
            let scene = &mut h.app.tabs[index].scene;
            scene.current_layout = "Model".into();
            scene.block_edit_block = None;
            h.key(DialogKey::Cancel);
        }
        start_measuring(&mut h);
        assert!(measuring(&h), "model space measures");
    }

    /// Typed points follow the UCS; the length is the world one.
    #[test]
    fn measure_takes_world_lengths_whatever_the_ucs() {
        use crate::app::secureplan::session::tests::Harness;
        let mut h = Harness::new("measure_ucs");
        h.open_dxf();
        assert_eq!(h.app.automation_op(r#"{"op":"run","cmd":"UCS Z 30"}"#)["ok"], true);
        let index = h.app.active_tab;
        assert!(h.app.tabs[index].active_ucs.is_some(), "the UCS is rotated");
        start_measuring(&mut h);
        typed_point(&mut h, "100,200");
        typed_point(&mut h, "130,240");
        let Some(Dialog::Align(dialog)) = &h.app.secureplan.dialog else { panic!("no alignment dialog") };
        assert_eq!(dialog.form.number(CAD_LENGTH), Some(50.0));
    }

    /// Pointer picks start afresh on every Measure: with Ortho on, the first
    /// click after a cancelled or a completed measure is not bent toward the
    /// earlier measure's point.
    #[test]
    fn a_new_measure_does_not_constrain_its_first_click_to_the_last_one() {
        use crate::app::secureplan::session::tests::Harness;
        let mut h = Harness::new("measure_pointer");
        h.open_dxf();
        let index = h.app.active_tab;
        h.app.tabs[index].scene.selection.borrow_mut().vp_size = (1600.0, 900.0);
        h.app.tabs[index].scene.sync_tiles_from_panes(1600.0, 900.0);
        h.app.snapper.snap_enabled = false;
        h.app.snapper.enabled.clear();
        h.app.polar_mode = false;
        h.app.ortho_mode = false;
        let click = |h: &mut Harness, x: f32, y: f32| {
            let _ = h.app.update(Message::ViewportMove(iced::Point::new(x, y)));
            let _ = h.app.update(Message::ViewportLeftPress);
            let _ = h.app.update(Message::ViewportLeftRelease);
        };
        let measured = |h: &Harness| match &h.app.secureplan.dialog {
            Some(Dialog::Align(dialog)) => dialog.form.number(CAD_LENGTH),
            _ => panic!("no alignment dialog"),
        };
        // The length between two points one above the other, Ortho off.
        start_measuring(&mut h);
        click(&mut h, 300.0, 200.0);
        click(&mut h, 300.0, 400.0);
        let expected = measured(&h).expect("a length");
        h.key(DialogKey::Cancel);
        h.app.ortho_mode = true;
        for ending in ["cancelled", "completed"] {
            start_measuring(&mut h);
            click(&mut h, 100.0, 100.0);
            if ending == "cancelled" {
                let _ = h.app.update(Message::CommandEscape);
            } else {
                click(&mut h, 100.0, 150.0);
            }
            assert!(!measuring(&h), "{ending}: still measuring");
            // Measure again from the dialog that came back.
            let _ = h.app.update(Message::SecurePlan(crate::app::secureplan::ui::Msg::Action(Action::AlignMeasure)));
            assert!(measuring(&h), "{ending}: measuring again");
            click(&mut h, 300.0, 200.0);
            click(&mut h, 300.0, 400.0);
            assert_eq!(measured(&h), Some(expected), "after a {ending} measure");
            h.key(DialogKey::Cancel);
        }
    }

    /// PUB-02/PUB-04: Apply chosen while a length is measured supersedes the
    /// hidden Align form. It does not come back while Apply runs, and an
    /// alignment form cannot be confirmed while the survey is busy.
    #[test]
    fn apply_during_a_measure_discards_the_hidden_alignment_form() {
        use crate::app::secureplan::session::tests::Harness;
        let mut h = Harness::new("measure_apply");
        h.open_dxf();
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        h.key(DialogKey::Activate);
        let aligned = h.bound().alignment.expect("aligned with the drawing's units");
        start_measuring(&mut h);
        assert!(measuring(&h));
        let _ = h.app.dispatch_command("SECUREPLANAPPLY");
        assert!(matches!(h.app.secureplan.dialog, Some(Dialog::Apply(_))), "the Apply dialog");
        h.key(DialogKey::Activate);
        assert!(h.bound().apply.is_some(), "Apply is running");
        assert!(h.app.secureplan.measuring.is_none(), "the measurement was kept");
        assert!(!matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "the Align form came back during Apply");
        // A form left from before cannot change the alignment while Apply runs.
        let tab_id = h.tab_id();
        let bound = h.bound();
        let mut dialog = AlignDialog::new(tab_id, Some(Units::Mm), Some(aligned), false, [0.0, 0.0, 100.0, 50.0], false);
        dialog.opened_for = (bound.session, bound.generation);
        dialog.form.focus = ROTATION;
        dialog.form.key(DialogKey::Right);
        dialog.refresh();
        h.app.secureplan.dialog = Some(Dialog::Align(Box::new(dialog)));
        let _ = h.app.update(Message::SecurePlan(crate::app::secureplan::ui::Msg::Action(Action::AlignConfirm)));
        assert!(h.app.command_line.last_error.clone().unwrap_or_default().contains("busy"));
        assert_eq!(h.bound().alignment, Some(aligned), "the alignment changed during Apply");
    }

    /// PUB-02: a drawing replaced or reconnected while its length is picked
    /// takes the hidden form with it, also when another document is active;
    /// and a stale form cannot be confirmed.
    #[test]
    fn a_drawing_replaced_or_reconnected_while_measuring_does_not_bring_its_form_back() {
        use crate::app::secureplan::session::tests::{plan_update, Harness, BASE};
        use crate::app::secureplan::session::Format;
        use crate::app::secureplan::{overlay, testutil};
        for change in ["planUpdate", "reconnect", "planUpdate elsewhere"] {
            let mut h = Harness::new(&format!("measure_{}", change.len()));
            h.open_dxf();
            let bound_id = h.tab_id();
            let index_of = |h: &Harness| h.app.tabs.iter().position(|tab| tab.id == bound_id).unwrap();
            if change.ends_with("elsewhere") {
                // Another document: a Start tab before the survey's.
                h.app.tabs.insert(0, crate::app::document::DocumentTab::new_start());
                h.app.active_tab = index_of(&h);
            }
            start_measuring(&mut h);
            assert!(measuring(&h), "{change}");
            if change.ends_with("elsewhere") {
                let other = (0..h.app.tabs.len()).find(|&i| i != index_of(&h)).unwrap();
                let _ = h.app.update(Message::TabSwitch(other));
            }
            if change == "reconnect" {
                h.pair_again(9, None);
                h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", serde_json::Value::Null, "none", "edit");
            } else {
                plan_update(&mut h, Some(("new.dxf", "image/vnd.dxf", testutil::synthetic_dxf())), BASE);
            }
            let alignment = |h: &Harness| h.app.secureplan.sessions.by_tab(bound_id).unwrap().alignment;
            let before = alignment(&h);
            // Answer what the page showed, and end any picking where it runs.
            for _ in 0..3 {
                if h.app.secureplan.dialog.is_some() && !matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))) {
                    h.key(DialogKey::Cancel);
                }
            }
            let bound = index_of(&h);
            let _ = h.app.update(Message::TabSwitch(bound));
            let _ = h.app.update(Message::CommandEscape);
            assert!(!matches!(h.app.secureplan.dialog, Some(Dialog::Align(_))), "{change}: the old form came back");
            assert!(h.app.secureplan.measuring.is_none(), "{change}");
            assert_eq!(alignment(&h), before, "{change}");
        }
        // Confirming a form whose drawing changed changes nothing.
        let mut h = Harness::new("measure_confirm");
        h.open_dxf();
        let _ = h.app.dispatch_command("SECUREPLANALIGN");
        let before = h.bound().alignment;
        let tab = h.tab_id();
        h.app.secureplan.sessions.by_tab_mut(tab).unwrap().generation += 1;
        let _ = h.app.update(Message::SecurePlan(crate::app::secureplan::ui::Msg::Action(Action::AlignConfirm)));
        assert!(h.app.secureplan.dialog.is_none());
        assert_eq!(h.bound().alignment, before, "a stale form was confirmed");
    }

    #[test]
    fn an_empty_survey_fixes_the_page_at_the_origin() {
        let dialog = AlignDialog::new(1, Some(Units::Mm), None, true, [100.0, 200.0, 1100.0, 700.0], false);
        assert!(!dialog.form.fields[CAD_X].enabled && !dialog.form.fields[SURVEY_Y].enabled);
        let mapping = dialog.proposed().unwrap().mapping;
        assert_eq!(mapping.cad_origin, [100.0, 700.0], "the extents' top-left corner");
        assert_eq!(mapping.anchor_mm, [0.0, 0.0]);
        assert_eq!(mapping.cad_to_world([100.0, 700.0]), [0.0, 0.0]);
    }

    #[test]
    fn a_stored_alignment_is_the_default_and_shown_as_current() {
        let stored = Alignment {
            units: Units::M,
            mapping: Mapping { cad_origin: [5.0, 6.0], anchor_mm: [1000.0, 2000.0], scale_mm_per_cad_unit: 1000.0, quarter_turns: 2 },
        };
        let mut dialog = AlignDialog::new(1, Some(Units::Mm), Some(stored), false, [0.0, 0.0, 10.0, 10.0], false);
        assert_eq!(dialog.proposed().unwrap(), stored, "reused unless the user changes it");
        dialog.form.focus = ROTATION;
        dialog.form.key(DialogKey::Right);
        assert_eq!(dialog.proposed().unwrap().mapping.quarter_turns, 3);
    }
}
