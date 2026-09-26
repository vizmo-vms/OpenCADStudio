//! The Apply dialog (PUB-03 model space, PUB-04, DSK-06).
//!
//! Apply has already finished or cancelled the active command and frozen the
//! drawing. The dialog shows the published view: a model-space window in CAD
//! coordinates, defaulting to the visible extents with a 2% margin, which the
//! user can shrink to leave out stray far-away entities; the resulting page
//! size, millimetres per point and float32 rounding bound; re-alignment and a
//! pending original. Confirming builds every output from the frozen drawing.

use std::sync::Arc;

use iced::widget::{column, text};
use iced::{Element, Length};

use super::{Action, Field, Form};
use crate::app::secureplan::align::{check_placement, mapping_at, trim_margin_to_canvas, Alignment};
use crate::app::secureplan::publish::{choose_mm_per_pt, place_page, ApplyPlan, Mapping, Snapshot};
use crate::app::Message;

const X0: usize = 0;
const Y0: usize = 1;
const X1: usize = 2;
const Y1: usize = 3;

#[derive(Debug, Clone)]
pub struct ApplyDialog {
    pub tab_id: u64,
    pub form: Form,
    pub snapshot: Arc<Snapshot>,
    /// The tab's edit revision when the snapshot was frozen.
    pub snapshot_revision: u64,
    pub default_window: [f64; 4],
    pub alignment: Alignment,
    /// The survey has no design: the page goes to the origin (CON-03).
    pub empty_survey: bool,
    pub realigned: bool,
    pub plan_version: Option<u64>,
    /// The session, plan and document generation the snapshot belongs to.
    pub origin: crate::app::secureplan::session::ApplyOrigin,
}

impl ApplyDialog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tab_id: u64,
        snapshot: Arc<Snapshot>,
        snapshot_revision: u64,
        extents: [f64; 4],
        alignment: Alignment,
        empty_survey: bool,
        realigned: bool,
        plan_version: Option<u64>,
    ) -> Self {
        // The visible extents plus the margin. With a fixed mapping the margin
        // (never the content) is trimmed where it would start the page
        // before the canvas.
        let default_window = crate::app::secureplan::publish::default_window(extents);
        let default_window = if empty_survey { default_window } else { trim_margin_to_canvas(default_window, extents, &alignment.mapping) };
        let [x0, y0, x1, y1] = default_window;
        let fields = vec![
            Field::number("Window left (CAD X)", x0),
            Field::number("Window bottom (CAD Y)", y0),
            Field::number("Window right (CAD X)", x1),
            Field::number("Window top (CAD Y)", y1),
        ];
        let buttons = vec![
            ("Apply to SecurePlan".to_string(), Action::ApplyConfirm),
            ("Reset window".to_string(), Action::ApplyReset),
            ("Cancel".to_string(), Action::Dismiss),
        ];
        Self {
            tab_id,
            form: Form::new(fields, buttons, Action::Dismiss),
            snapshot,
            snapshot_revision,
            default_window,
            alignment,
            empty_survey,
            realigned,
            plan_version,
            origin: Default::default(),
        }
    }

    pub fn reset_window(&mut self) {
        for (index, value) in [X0, Y0, X1, Y1].into_iter().zip(self.default_window) {
            self.form.set_number(index, value);
        }
    }

    pub fn window(&self) -> Result<[f64; 4], String> {
        let read = |index: usize| self.form.number(index).ok_or_else(|| "Enter all four window coordinates.".to_string());
        let window = [read(X0)?, read(Y0)?, read(X1)?, read(Y1)?];
        if window[2] <= window[0] || window[3] <= window[1] {
            return Err("The window's right must be past its left and its top above its bottom.".into());
        }
        Ok(window)
    }

    /// The mapping for `window`: the stored or aligned one, or on an empty
    /// survey one that puts the window's top-left at the origin.
    fn mapping(&self, window: [f64; 4]) -> Mapping {
        let m = self.alignment.mapping;
        if self.empty_survey {
            mapping_at(window, m.scale_mm_per_cad_unit, m.quarter_turns, [0.0, 0.0])
        } else {
            m
        }
    }

    /// The plan the fields describe, or why it cannot be published.
    pub fn plan(&self) -> Result<ApplyPlan, String> {
        let window = self.window()?;
        let mapping = self.mapping(window);
        let mm_per_pt = choose_mm_per_pt(window, &mapping).map_err(|_| "The window is too large or too small to publish.".to_string())?;
        let placement = place_page(window, &mapping, mm_per_pt).map_err(|_| "The window is too large to publish.".to_string())?;
        check_placement(&placement).map_err(|block| block.to_string())?;
        Ok(ApplyPlan { window_cad: window, mapping, mm_per_pt })
    }

    pub fn summary(&self) -> Vec<String> {
        let mut lines = vec!["Published view: model space".to_string()];
        let next = self.plan_version.map_or(1, |v| v + 1);
        lines.push(match self.plan_version {
            Some(version) => format!("Plan version {version} → {next}"),
            None => "Plan version 1 (first Apply)".to_string(),
        });
        match self.plan() {
            Ok(plan) => {
                if let Ok(transform) = plan.transform() {
                    let p = transform.placement;
                    lines.push(format!("Page: {} × {} pt, {} mm per pt", p.width_pt, p.height_pt, super::format_number(p.mm_per_pt)));
                    lines.push(format!(
                        "Placed at survey ({}, {}) mm, {} × {} mm",
                        super::format_number(p.page_world_min[0]),
                        super::format_number(p.page_world_min[1]),
                        super::format_number(p.width_mm),
                        super::format_number(p.height_mm)
                    ));
                    let within = if p.rounding_bound_mm <= 0.09 { "within 0.1 mm" } else { "larger than the 0.09 mm budget" };
                    lines.push(format!("Precision: rounding bound {} mm ({within})", super::format_number(p.rounding_bound_mm)));
                }
            }
            Err(problem) => lines.push(format!("Cannot apply yet: {problem}")),
        }
        if self.realigned {
            lines.push("The alignment changed: SecurePlan shows this re-alignment before you confirm there.".into());
        }
        if self.snapshot.pending_original.is_some() {
            lines.push("The imported original is sent too, and stored unchanged.".into());
        }
        if self.snapshot.modified {
            lines.push("The drawing has edits: it is written in its original format and version.".into());
        }
        lines
    }
}

pub fn view(dialog: &ApplyDialog) -> Element<'_, Message> {
    let mut content = column![].spacing(10).padding(8);
    content = content.push(
        text("Choose the model-space window to publish. Shrink it to leave out stray far-away entities.").size(13).width(Length::Fixed(520.0)),
    );
    content = content.push(super::form_view(&dialog.form));
    for line in dialog.summary() {
        content = content.push(text(line).size(12));
    }
    content.push(super::keys_hint()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::align::Units;
    use crate::app::secureplan::ui::DialogKey;

    fn dialog(empty: bool, anchor: [f64; 2]) -> ApplyDialog {
        let snapshot = Arc::new(Snapshot {
            document: acadrust::CadDocument::new(),
            annotation_scale: 1.0,
            loaded: None,
            modified: true,
            pending_original: None,
            lost_entities: 0,
        });
        let extents = [0.0, 0.0, 30000.0, 18000.0];
        let alignment = Alignment { units: Units::Mm, mapping: mapping_at(extents, 1.0, 0, anchor) };
        ApplyDialog::new(1, snapshot, 0, extents, alignment, empty, false, None)
    }

    #[test]
    fn the_window_can_be_shrunk_and_reset_by_keyboard() {
        let mut dialog = dialog(true, [0.0, 0.0]);
        let full = dialog.plan().unwrap();
        assert_eq!(full.window_cad, [-600.0, -360.0, 30600.0, 18360.0], "the extents plus 2%");
        // Right edge: 30600 → 15000.
        dialog.form.focus = X1;
        for _ in 0..5 {
            dialog.form.key(DialogKey::Backspace);
        }
        for c in "15000".chars() {
            dialog.form.key(DialogKey::Char(c));
        }
        let shrunk = dialog.plan().unwrap();
        assert_eq!(shrunk.window_cad, [-600.0, -360.0, 15000.0, 18360.0]);
        // On an empty survey the shrunk window still starts at the origin.
        assert_eq!(shrunk.transform().unwrap().placement.page_world_min, [0.0, 0.0]);
        dialog.reset_window();
        assert_eq!(dialog.plan().unwrap().window_cad, full.window_cad);
        assert!(dialog.summary().iter().any(|l| l.starts_with("Page:")));
    }

    #[test]
    fn the_margin_is_trimmed_to_the_canvas_but_content_outside_it_blocks_apply() {
        // The drawing's top-left sits on the canvas origin: only the margin
        // before it is trimmed.
        let at_origin = dialog(false, [0.0, 0.0]);
        assert_eq!(at_origin.plan().unwrap().window_cad, [0.0, -360.0, 30600.0, 18000.0]);
        // Anchored at x = −100 mm, content lies outside the canvas: it is
        // not trimmed away, and Apply is refused.
        let mut dialog = dialog(false, [-100.0, 0.0]);
        assert_eq!(dialog.plan().unwrap_err(), "Move the plan so it starts inside the canvas.");
        assert!(dialog.summary().iter().any(|l| l.contains("Move the plan")));
        // Shrinking the window to the part inside the canvas is the user's choice.
        dialog.form.set_number(X0, 100.0);
        dialog.form.set_number(Y1, 18000.0);
        assert!(dialog.plan().is_ok());
    }
}
