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

/// Two points, picked like any command's (object snaps, typed coordinates);
/// their distance in drawing units, in plan (X and Y).
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
        self.active_tab = index;
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
        if self.secureplan_tab_index(dialog.tab_id).is_none() {
            return;
        }
        if let Some(length) = length.lock().unwrap_or_else(|e| e.into_inner()).take() {
            dialog.measured(length);
        }
        self.secureplan.dialog = Some(Dialog::Align(Box::new(dialog)));
    }

    /// Open the alignment dialog for the active bound document.
    pub(crate) fn secureplan_open_align(&mut self, then_apply: bool) {
        if let Err(message) = self.secureplan_can_edit_survey() {
            self.command_line.push_error(&message);
            return;
        }
        let index = self.active_tab;
        let _ = self.cancel_active_command_for_space_change();
        self.secureplan.measuring = None;
        let tab = &self.tabs[index];
        let Some(bound) = self.secureplan.sessions.by_tab(tab.id) else { return };
        let Some(extents) = crate::app::secureplan::publish::visible_extents(&tab.scene) else {
            self.command_line.push_error("SecurePlan: the drawing has nothing to align.");
            return;
        };
        let declared = Units::declared(tab.scene.document.header.insertion_units);
        let empty = crate::app::secureplan::session::survey_is_empty(bound);
        let dialog = AlignDialog::new(tab.id, declared, bound.alignment, empty, crate::app::secureplan::publish::default_window(extents), then_apply);
        self.secureplan.dialog = Some(Dialog::Align(Box::new(dialog)));
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
        // Some choices of the user's own: rotation 90° from its dropdown, and a CAD point.
        let _ = h.app.update(Message::SecurePlan(Msg::FormSelect(ROTATION, 1)));
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
