//! The Apply dialog (PUB-03, PUB-04, DSK-06).
//!
//! Apply has already finished or cancelled the active command and frozen the
//! drawing. The dialog shows the published view, which the user picks: model
//! space, or one of the drawing's paper layouts (a layout that cannot be
//! published says why). Model space is a window in CAD coordinates,
//! defaulting to the visible extents with a 2% margin, which the user can
//! shrink to leave out stray far-away entities; a layout is its whole sheet
//! through its reference viewport. The dialog shows the resulting page size,
//! millimetres per point and float32 rounding bound, re-alignment and a
//! pending original. Confirming builds every output from the frozen drawing.

use std::sync::Arc;

use iced::widget::{column, text};
use iced::{Element, Length};

use super::{Action, Field, FieldKind, Form};
use crate::app::secureplan::align::{check_placement, mapping_at, trim_margin_to_canvas, Alignment};
use crate::app::secureplan::layout::{sheet_too_large, LayoutReference};
use crate::app::secureplan::publish::{choose_mm_per_pt, place_page, ApplyPlan, Mapping, PublishedView, Snapshot};
use crate::app::Message;

/// Model space, or one of the paper layouts.
const VIEW: usize = 0;
const X0: usize = 1;
const Y0: usize = 2;
const X1: usize = 3;
const Y1: usize = 4;
/// "Publish without N damaged items", shown only when the reader dropped some.
pub(crate) const ACKNOWLEDGE: usize = 5;
const MODEL_SPACE: &str = "Model space";

/// A paper layout, with its reference viewport or why it cannot be published.
pub type LayoutChoice = (String, Result<LayoutReference, String>);

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
    /// The drawing's paper layouts, in the order the view choice lists them.
    pub layouts: Vec<LayoutChoice>,
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
        layouts: Vec<LayoutChoice>,
    ) -> Self {
        // The visible extents plus the margin. With a fixed mapping the margin
        // (never the content) is trimmed where it would start the page
        // before the canvas.
        let default_window = crate::app::secureplan::publish::default_window(extents);
        let default_window = if empty_survey { default_window } else { trim_margin_to_canvas(default_window, extents, &alignment.mapping) };
        let [x0, y0, x1, y1] = default_window;
        let views = std::iter::once(MODEL_SPACE.to_string()).chain(layouts.iter().map(|(name, _)| format!("Layout: {name}"))).collect();
        let mut view = Field::choice("Published view", views, 0);
        view.enabled = !layouts.is_empty();
        let mut fields = vec![
            view,
            Field::number("Window left (CAD X)", x0),
            Field::number("Window bottom (CAD Y)", y0),
            Field::number("Window right (CAD X)", x1),
            Field::number("Window top (CAD Y)", y1),
        ];
        // Damaged items the reader dropped: the user acknowledges publishing
        // without them, every time (unticked by default).
        if snapshot.lost_entities > 0 {
            fields.push(Field::choice("Publish without the damaged items", vec!["No".into(), "Yes".into()], 0));
        }
        let buttons = vec![
            ("Apply to SecurePlan".to_string(), Action::ApplyConfirm),
            ("Reset window".to_string(), Action::ApplyReset),
            ("Cancel".to_string(), Action::Dismiss),
        ];
        let mut dialog = Self {
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
            layouts,
        };
        dialog.refresh();
        dialog
    }

    /// The chosen paper layout; `None` for model space.
    pub fn layout(&self) -> Option<&LayoutChoice> {
        self.form.selected(VIEW).checked_sub(1).and_then(|index| self.layouts.get(index))
    }

    /// Choose model space (`None`) or the paper layout `name`; `false` if
    /// the drawing has no layout by that name.
    pub fn select_view(&mut self, layout: Option<&str>) -> bool {
        let choice = match layout {
            None => 0,
            Some(name) => match self.layouts.iter().position(|(layout, _)| layout == name) {
                Some(index) => index + 1,
                None => return false,
            },
        };
        if let Some(Field { kind: FieldKind::Choice { selected, .. }, .. }) = self.form.fields.get_mut(VIEW) {
            *selected = choice;
        }
        self.refresh();
        true
    }

    /// The window applies to model space only.
    pub fn refresh(&mut self) {
        let model = self.layout().is_none();
        for index in [X0, Y0, X1, Y1] {
            self.form.fields[index].enabled = model;
        }
    }

    /// Set the model-space window (the stdin driver's `window=`).
    pub fn set_window(&mut self, window: [f64; 4]) {
        for (index, value) in [X0, Y0, X1, Y1].into_iter().zip(window) {
            self.form.set_number(index, value);
        }
    }

    /// Tick "Publish without N damaged items" (the stdin driver's flag).
    pub fn acknowledge_damaged(&mut self) {
        if self.snapshot.lost_entities > 0 && self.form.selected(ACKNOWLEDGE) == 0 {
            self.form.cycle(ACKNOWLEDGE, true);
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
        let damaged_acknowledged = self.snapshot.lost_entities == 0 || self.form.selected(ACKNOWLEDGE) == 1;
        let unacknowledged = || format!("Choose Yes for \"Publish without {} damaged items\" to apply.", self.snapshot.lost_entities);
        if let Some((_, found)) = self.layout() {
            let reference = found.as_ref().map_err(Clone::clone)?;
            // On an empty survey the sheet's top-left goes to the origin.
            let mapping = if self.empty_survey { reference.at_origin(&self.alignment.mapping) } else { self.alignment.mapping };
            let (transform, _) = reference.transforms(&mapping).map_err(|_| sheet_too_large(reference.sheet_mm()))?;
            check_placement(&transform.placement).map_err(|block| block.to_string())?;
            if !damaged_acknowledged {
                return Err(unacknowledged());
            }
            let view = PublishedView::Layout(Box::new(reference.clone()));
            return Ok(ApplyPlan { view, mapping, mm_per_pt: transform.placement.mm_per_pt, damaged_acknowledged });
        }
        let window = self.window()?;
        let mapping = self.mapping(window);
        let mm_per_pt = choose_mm_per_pt(window, &mapping).map_err(|_| "The window is too large or too small to publish.".to_string())?;
        let placement = place_page(window, &mapping, mm_per_pt).map_err(|_| "The window is too large to publish.".to_string())?;
        check_placement(&placement).map_err(|block| block.to_string())?;
        if !damaged_acknowledged {
            return Err(unacknowledged());
        }
        Ok(ApplyPlan { view: PublishedView::Model { window_cad: window }, mapping, mm_per_pt, damaged_acknowledged })
    }

    pub fn summary(&self) -> Vec<String> {
        let mut lines = vec![match self.layout() {
            None => "Published view: model space".to_string(),
            Some((name, Ok(reference))) => {
                let [width, height] = reference.sheet_mm();
                format!(
                    "Published view: layout \"{name}\", the whole {} × {} mm sheet. Viewport {:X} is the plan SecurePlan snaps to.",
                    super::format_number(width),
                    super::format_number(height),
                    reference.viewport.value()
                )
            }
            Some((name, Err(_))) => format!("Published view: layout \"{name}\""),
        }];
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
        if self.snapshot.lost_entities > 0 {
            lines.push(format!(
                "{} damaged items could not be read: they are not in the published plan. The original drawing is stored unchanged.",
                self.snapshot.lost_entities
            ));
        }
        lines
    }
}

pub fn view(dialog: &ApplyDialog) -> Element<'_, Message> {
    let mut content = column![].spacing(10).padding(8);
    let intro = match dialog.layout() {
        None => "Choose the view to publish. For model space, choose the window: shrink it to leave out stray far-away entities.",
        Some(_) => "A layout publishes its whole sheet at its size. Its viewport is the plan: SecurePlan snaps only to model geometry inside it.",
    };
    content = content.push(text(intro).size(13).width(Length::Fixed(520.0)));
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
        ApplyDialog::new(1, snapshot, 0, extents, alignment, empty, false, None, Vec::new())
    }

    /// The Apply dialog over the synthetic layout drawing, its viewport
    /// twisted by `twist` degrees.
    fn layout_dialog(twist: f64, empty: bool) -> ApplyDialog {
        use crate::app::secureplan::layout;
        let (scene, _) = layout::tests::layout_scene(vec![crate::app::secureplan::testutil::plan_viewport((420.0, 300.0), twist)]);
        // Anchored so the whole sheet lies inside the canvas.
        let base = dialog(empty, [30000.0, 30000.0]);
        ApplyDialog::new(1, base.snapshot, 0, [0.0, 0.0, 30000.0, 18000.0], base.alignment, empty, false, Some(3), layout::references(&scene))
    }

    #[test]
    fn a_layout_is_chosen_by_keyboard_and_publishes_its_sheet() {
        let mut dialog = layout_dialog(0.0, false);
        assert_eq!(dialog.form.focus, VIEW, "the view choice comes first");
        assert!(matches!(dialog.plan().unwrap().view, PublishedView::Model { .. }), "model space by default");
        // Right moves through the layouts (the dialog refreshes after every
        // key): a new drawing's empty "Layout1", then "Sheet A1".
        let names: Vec<String> = dialog.layouts.iter().map(|(name, _)| name.clone()).collect();
        assert_eq!(names, ["Layout1", "Sheet A1"]);
        dialog.form.key(DialogKey::Right);
        dialog.refresh();
        assert!(dialog.plan().unwrap_err().ends_with("it has no viewport that is turned on."));
        dialog.form.key(DialogKey::Right);
        dialog.refresh();
        let (name, _) = dialog.layout().expect("a layout is chosen");
        assert_eq!(name, "Sheet A1");
        assert!([X0, Y0, X1, Y1].iter().all(|&field| !dialog.form.fields[field].enabled), "the window is model space only");
        dialog.form.key(DialogKey::Next);
        assert_eq!(dialog.form.focus, dialog.form.fields.len(), "Tab skips the window fields to Apply");
        let plan = dialog.plan().unwrap();
        let PublishedView::Layout(reference) = &plan.view else { panic!("a layout plan") };
        assert_eq!(plan.view.to_json()["kind"], "layout");
        assert_eq!(plan.mapping, dialog.alignment.mapping, "a survey with content keeps its mapping");
        assert_eq!(plan.mm_per_pt, reference.mm_per_pt(&plan.mapping));
        let summary = dialog.summary();
        assert!(summary[0].starts_with("Published view: layout \"Sheet A1\", the whole 841 × 594 mm sheet."), "{summary:?}");
        assert!(summary.iter().any(|line| line == "Page: 2384 × 1684 pt, 35.277778 mm per pt"), "{summary:?}");
        // Left goes back through "Layout1" to model space and its window.
        dialog.form.focus = VIEW;
        for _ in 0..2 {
            dialog.form.key(DialogKey::Left);
            dialog.refresh();
        }
        assert!(dialog.layout().is_none() && dialog.form.fields[X0].enabled);
        assert!(matches!(dialog.plan().unwrap().view, PublishedView::Model { .. }));

        // On an empty survey the sheet's top-left goes to the origin.
        let mut empty = layout_dialog(0.0, true);
        assert!(empty.select_view(Some("Sheet A1")));
        assert!(!empty.select_view(Some("No such layout")));
        assert_eq!(empty.plan().unwrap().transform().unwrap().placement.page_world_min, [0.0, 0.0]);
    }

    #[test]
    fn a_layout_that_cannot_be_published_says_why() {
        let mut dialog = layout_dialog(30.0, false);
        assert!(dialog.select_view(Some("Sheet A1")));
        let reason = dialog.plan().unwrap_err();
        assert!(reason.ends_with("is twisted by 30°; SecurePlan needs 0°, 90°, 180° or 270°."), "{reason}");
        assert!(dialog.summary().iter().any(|line| line == &format!("Cannot apply yet: {reason}")));
    }

    fn window_of(plan: &ApplyPlan) -> [f64; 4] {
        match plan.view {
            PublishedView::Model { window_cad } => window_cad,
            PublishedView::Layout(_) => panic!("a layout, not model space"),
        }
    }

    #[test]
    fn damaged_items_need_a_keyboard_acknowledgement_on_every_apply() {
        let base = dialog(true, [0.0, 0.0]);
        let snapshot = Snapshot { lost_entities: 3, ..(*base.snapshot).clone() };
        let mut dialog = ApplyDialog::new(1, Arc::new(snapshot), 0, [0.0, 0.0, 30000.0, 18000.0], base.alignment, true, false, None, Vec::new());
        assert_eq!(dialog.plan().unwrap_err(), "Choose Yes for \"Publish without 3 damaged items\" to apply.");
        dialog.form.focus = ACKNOWLEDGE;
        dialog.form.key(DialogKey::Right);
        let plan = dialog.plan().unwrap();
        assert!(plan.damaged_acknowledged);
        assert!(dialog.summary().iter().any(|l| l.starts_with("3 damaged items")));
    }

    #[test]
    fn the_window_can_be_shrunk_and_reset_by_keyboard() {
        let mut dialog = dialog(true, [0.0, 0.0]);
        let full = dialog.plan().unwrap();
        assert_eq!(window_of(&full), [-600.0, -360.0, 30600.0, 18360.0], "the extents plus 2%");
        // Right edge: 30600 → 15000.
        dialog.form.focus = X1;
        for _ in 0..5 {
            dialog.form.key(DialogKey::Backspace);
        }
        for c in "15000".chars() {
            dialog.form.key(DialogKey::Char(c));
        }
        let shrunk = dialog.plan().unwrap();
        assert_eq!(window_of(&shrunk), [-600.0, -360.0, 15000.0, 18360.0]);
        // On an empty survey the shrunk window still starts at the origin.
        assert_eq!(shrunk.transform().unwrap().placement.page_world_min, [0.0, 0.0]);
        dialog.reset_window();
        assert_eq!(window_of(&dialog.plan().unwrap()), window_of(&full));
        assert!(dialog.summary().iter().any(|l| l.starts_with("Page:")));
    }

    #[test]
    fn the_margin_is_trimmed_to_the_canvas_but_content_outside_it_blocks_apply() {
        // The drawing's top-left sits on the canvas origin: only the margin
        // before it is trimmed.
        let at_origin = dialog(false, [0.0, 0.0]);
        assert_eq!(window_of(&at_origin.plan().unwrap()), [0.0, -360.0, 30600.0, 18000.0]);
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
