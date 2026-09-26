//! Paper-layout publishing (PUB-03 layouts).
//!
//! A paper layout can be the published view when exactly one of its enabled
//! viewports is rectangular, orthographic, looks straight down on the plan,
//! shows model geometry and is not overlapped by another enabled viewport.
//! That viewport is the semantic reference: its in-plane twist must be a
//! quarter turn (within 1e-9 rad), so the whole sheet maps to the survey world
//! as a similarity that keeps the page axis-aligned (CON-01).
//!
//! The whole sheet is published at its sheet size (one sheet millimetre is
//! 72/25.4 points). Through the viewport, a sheet point `p` shows the model
//! point `m = c + k·B·(p − v)`: `v` is the viewport centre, `c` the model point
//! it shows, `k` model units per paper unit and `B` the quarter-turn basis
//! (the model directions of paper +x and +y). The CAD → world mapping then
//! places the sheet: paper → world is the mapping applied through the
//! viewport, a similarity of `s·k` mm per paper unit.
//!
//! Model geometry is published through the CAD → world mapping itself, with
//! the model-space precision rules (curves re-tessellated within the chord
//! tolerance, largest-singular-value stretch, one f32 rounding), clipped to the
//! viewport. Paper-space entities (the title block) and any other viewports
//! are drawn through the paper → world mapping; they contribute no snaps.

use acadrust::types::Handle;
use acadrust::EntityType;

use super::publish::{place_page, Mapping, PageTransform, PlacementError, MAX_AREA_PT2, MAX_SIDE_PT};

/// Points per sheet millimetre.
const PT_PER_MM: f64 = 72.0 / 25.4;
/// A twist within this of a quarter turn is one (PUB-03).
const TWIST_TOLERANCE: f64 = 1e-9;
/// The longest layout name the contract carries (`view.layoutName`).
const MAX_NAME_CHARS: usize = 255;

/// A paper layout's reference viewport: how its sheet maps to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutReference {
    pub layout: String,
    /// The layout's paper-space block.
    pub paper_block: Handle,
    pub viewport: Handle,
    /// The sheet `[x0, y0, x1, y1]` in paper units.
    pub sheet: [f64; 4],
    /// Paper units per sheet millimetre.
    pub paper_units_per_mm: f64,
    /// The viewport rectangle `[x0, y0, x1, y1]` in paper units.
    pub viewport_rect: [f64; 4],
    /// Model units per paper unit.
    pub model_per_paper: f64,
    /// The model point shown at the viewport's centre.
    pub model_center: [f64; 2],
    /// The model directions of paper +x and +y: a quarter turn, exactly.
    pub basis: [[f64; 2]; 2],
    /// Layers frozen in the viewport.
    pub frozen_layers: Vec<Handle>,
}

impl LayoutReference {
    /// The `view` of CON-02 for this layout.
    pub fn view_json(&self) -> serde_json::Value {
        serde_json::json!({ "kind": "layout", "layoutName": self.layout, "viewportHandle": format!("{:X}", self.viewport.value()) })
    }

    fn paper_center(&self) -> [f64; 2] {
        let [x0, y0, x1, y1] = self.viewport_rect;
        [(x0 + x1) / 2.0, (y0 + y1) / 2.0]
    }

    /// The model point a sheet point shows through the viewport.
    pub fn paper_to_model(&self, [x, y]: [f64; 2]) -> [f64; 2] {
        let [vx, vy] = self.paper_center();
        let (dx, dy) = ((x - vx) * self.model_per_paper, (y - vy) * self.model_per_paper);
        let [right, up] = self.basis;
        [self.model_center[0] + right[0] * dx + up[0] * dy, self.model_center[1] + right[1] * dx + up[1] * dy]
    }

    /// Paper → world: `mapping` applied through the viewport. `rot(q')·F`
    /// = `rot(q)·F·B`, with `F` the y flip, is again a quarter turn.
    pub fn paper_mapping(&self, mapping: &Mapping) -> Mapping {
        let [right, up] = self.basis;
        // rot(q)·F·B·F as integers, matched against the four quarter turns.
        let rot = |q: u8| match q % 4 {
            0 => [[1, 0], [0, 1]],
            1 => [[0, -1], [1, 0]],
            2 => [[-1, 0], [0, -1]],
            _ => [[0, 1], [-1, 0]],
        };
        let b = [[right[0] as i32, up[0] as i32], [-(right[1] as i32), -(up[1] as i32)]];
        let r = rot(mapping.quarter_turns);
        let product = |i: usize, j: usize| r[i][0] * b[0][j] + r[i][1] * b[1][j];
        // (rot(q)·F·B)·F: the second column changes sign.
        let target = [[product(0, 0), -product(0, 1)], [product(1, 0), -product(1, 1)]];
        let turns = (0..4u8).find(|q| rot(*q) == target).unwrap_or(0);
        let scale = mapping.scale_mm_per_cad_unit * self.model_per_paper;
        // Measured from the sheet corner that lands top-left in the world,
        // so a page placed at the origin is there exactly.
        let [x0, y0, x1, y1] = self.sheet;
        let provisional = Mapping { cad_origin: [x0, y0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: scale, quarter_turns: turns };
        let corner = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
            .into_iter()
            .min_by(|a, b| {
                let (a, b) = (provisional.cad_to_world(*a), provisional.cad_to_world(*b));
                (a[0] + a[1]).total_cmp(&(b[0] + b[1]))
            })
            .unwrap_or([x0, y0]);
        Mapping { cad_origin: corner, anchor_mm: mapping.cad_to_world(self.paper_to_model(corner)), scale_mm_per_cad_unit: scale, quarter_turns: turns }
    }

    /// World millimetres per page point: the sheet at its own size.
    pub fn mm_per_pt(&self, mapping: &Mapping) -> f64 {
        mapping.scale_mm_per_cad_unit * self.model_per_paper * self.paper_units_per_mm / PT_PER_MM
    }

    /// The sheet's size in millimetres.
    pub fn sheet_mm(&self) -> [f64; 2] {
        let [x0, y0, x1, y1] = self.sheet;
        [(x1 - x0) / self.paper_units_per_mm, (y1 - y0) / self.paper_units_per_mm]
    }

    /// The page for `mapping`: the model transform (CAD → page) and the
    /// paper transform (sheet → page), sharing one placement.
    pub fn transforms(&self, mapping: &Mapping) -> Result<(PageTransform, PageTransform), PlacementError> {
        let paper = self.paper_mapping(mapping);
        let placement = place_page(self.sheet, &paper, self.mm_per_pt(mapping))?;
        Ok((PageTransform { mapping: *mapping, placement }, PageTransform { mapping: paper, placement }))
    }

    /// The same mapping, translated so the page's top-left corner is at the
    /// survey origin (an empty survey, CON-03). The sheet corner that lands
    /// there becomes the mapping's own origin, so it lands there exactly.
    pub fn at_origin(&self, mapping: &Mapping) -> Mapping {
        let corner = self.paper_mapping(mapping).cad_origin;
        Mapping { cad_origin: self.paper_to_model(corner), anchor_mm: [0.0, 0.0], ..*mapping }
    }

    /// The viewport's rectangle in page points `[x0, y0, x1, y1]`.
    pub fn viewport_on_page(&self, paper: &PageTransform) -> [f64; 4] {
        let [x0, y0, x1, y1] = self.viewport_rect;
        let corners = [[x0, y0], [x1, y1]].map(|[x, y]| paper.apply(x, y));
        [
            corners[0].0.min(corners[1].0),
            corners[0].1.min(corners[1].1),
            corners[0].0.max(corners[1].0),
            corners[0].1.max(corners[1].1),
        ]
    }
}

/// The published-page limits as a reason.
pub fn sheet_too_large(sheet_mm: [f64; 2]) -> String {
    format!(
        "The sheet is {} × {} mm, larger than a SecurePlan page ({} pt per side and {} pt² in all).",
        super::ui::format_number(sheet_mm[0]),
        super::ui::format_number(sheet_mm[1]),
        MAX_SIDE_PT,
        MAX_AREA_PT2
    )
}

/// Why one viewport cannot be the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Problem {
    NoView,
    Clipped,
    Perspective,
    NotPlan,
    Overlapped,
    NoGeometry,
}

impl Problem {
    fn text(self) -> &'static str {
        match self {
            Problem::NoView => "has no valid size or view",
            Problem::Clipped => "is clipped to a non-rectangular boundary",
            Problem::Perspective => "shows a perspective view",
            Problem::NotPlan => "does not look straight down on the plan",
            Problem::Overlapped => "is overlapped by another viewport",
            Problem::NoGeometry => "shows none of the drawing's model geometry",
        }
    }
}

fn hex(handle: Handle) -> String {
    format!("{:X}", handle.value())
}

fn rect_of(viewport: &acadrust::entities::Viewport) -> [f64; 4] {
    let (hw, hh) = (viewport.width.abs() / 2.0, viewport.height.abs() / 2.0);
    [viewport.center.x - hw, viewport.center.y - hh, viewport.center.x + hw, viewport.center.y + hh]
}

fn overlap(a: [f64; 4], b: [f64; 4]) -> bool {
    let size = (a[2] - a[0]).max(a[3] - a[1]).max(b[2] - b[0]).max(b[3] - b[1]);
    let slack = size * 1e-9;
    a[0] < b[2] - slack && b[0] < a[2] - slack && a[1] < b[3] - slack && b[1] < a[3] - slack
}

/// Whether the segment `a`–`b` crosses the rectangle (Liang–Barsky).
fn crosses([x0, y0]: [f64; 2], [x1, y1]: [f64; 2], rect: [f64; 4]) -> bool {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (p, q) in [(-dx, x0 - rect[0]), (dx, rect[2] - x0), (-dy, y0 - rect[1]), (dy, rect[3] - y0)] {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else if p < 0.0 {
            t0 = t0.max(q / p);
        } else {
            t1 = t1.min(q / p);
        }
    }
    t0 <= t1
}

/// Whether the viewport shows any visible model geometry (its own frozen
/// layers excluded), measured through the renderer's own viewport frame.
fn shows_geometry(model: &crate::scene::Scene, viewport: Handle, frame: &crate::scene::viewport_ref::ViewportFrame, rect: [f64; 4]) -> bool {
    let wires = model.model_wires_for_viewport_arc(viewport, 0.0);
    wires.iter().filter(|wire| wire.plot_visible).any(|wire| {
        let mut previous: Option<[f64; 2]> = None;
        (0..wire.points.len()).any(|index| {
            let [x, y, z] = wire.points[index];
            if x.is_nan() || y.is_nan() || z.is_nan() {
                previous = None;
                return false;
            }
            let point = wire.point_world(index, 0.0);
            let paper = frame.model_to_paper(point);
            let here = [paper.x, paper.y];
            let hit = match previous {
                Some(before) => crosses(before, here, rect),
                None => crosses(here, here, rect),
            };
            previous = Some(here);
            hit
        })
    })
}

/// The paper layouts of the drawing, each with its reference viewport or
/// the reason it cannot be published. `scene` holds the drawing with its
/// derived caches (any layout may be current).
pub fn references(scene: &crate::scene::Scene) -> Vec<(String, Result<LayoutReference, String>)> {
    let names: Vec<String> = scene.layout_names().into_iter().filter(|name| name != "Model").collect();
    if names.is_empty() {
        return Vec::new();
    }
    // The layout-dependent reads (its block, sheet and viewports) need a scene
    // on that layout; the model is read from `scene` itself.
    let mut paper = crate::scene::Scene::new();
    paper.document = scene.document.clone();
    names
        .into_iter()
        .map(|name| {
            paper.current_layout = name.clone();
            let found = reference(&paper, scene, &name);
            (name, found)
        })
        .collect()
}

/// The reference viewport of the paper layout `paper` is on, or why the
/// layout cannot be published. `model` has the same drawing with derived caches.
pub fn reference(paper: &crate::scene::Scene, model: &crate::scene::Scene, name: &str) -> Result<LayoutReference, String> {
    let refuse = |reason: String| Err(format!("Layout \"{name}\": {reason}"));
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("Layout names longer than {MAX_NAME_CHARS} characters cannot be published."));
    }
    let viewports: Vec<&acadrust::entities::Viewport> = paper
        .layout_content_viewports()
        .iter()
        .filter_map(|handle| match paper.document.get_entity(*handle) {
            Some(EntityType::Viewport(viewport)) if viewport.status.is_on => Some(viewport),
            _ => None,
        })
        .collect();
    if viewports.is_empty() {
        return refuse("it has no viewport that is turned on.".into());
    }

    // Each viewport's problem, if it has one, and its renderer frame.
    let mut judged = Vec::new();
    for viewport in &viewports {
        let handle = viewport.common.handle;
        let rect = rect_of(viewport);
        let frame = model.viewport_frame(handle);
        let direction = viewport.view_direction;
        let length = (direction.x * direction.x + direction.y * direction.y + direction.z * direction.z).sqrt();
        let problem = if !(viewport.width.abs() > 0.0 && viewport.height.abs() > 0.0 && viewport.view_height.abs() > 0.0)
            || !rect.iter().all(|v| v.is_finite())
            || !viewport.view_height.is_finite()
        {
            Some(Problem::NoView)
        } else if !viewport.clip_boundary_handle.is_null() {
            Some(Problem::Clipped)
        } else if viewport.status.perspective {
            Some(Problem::Perspective)
        } else if !(length > 0.0 && direction.z > 0.0 && direction.x.abs() <= 1e-9 * length && direction.y.abs() <= 1e-9 * length)
            // The renderer's own frame: a mirrored or oblique view has none.
            || frame.is_none()
        {
            Some(Problem::NotPlan)
        } else if viewports.iter().any(|other| other.common.handle != handle && overlap(rect, rect_of(other))) {
            Some(Problem::Overlapped)
        } else if !frame.as_ref().is_some_and(|frame| shows_geometry(model, handle, frame, rect)) {
            Some(Problem::NoGeometry)
        } else {
            None
        };
        judged.push((*viewport, rect, frame, problem));
    }

    let candidates: Vec<_> = judged.iter().filter(|(_, _, _, problem)| problem.is_none()).collect();
    let (viewport, rect, frame) = match candidates.as_slice() {
        [(viewport, rect, Some(frame), _)] => (*viewport, *rect, *frame),
        [] => {
            let reasons: Vec<String> =
                judged.iter().filter_map(|(viewport, _, _, problem)| problem.map(|p| format!("viewport {} {}", hex(viewport.common.handle), p.text()))).collect();
            return if reasons.len() == 1 {
                refuse(format!("{}.", reasons[0]))
            } else {
                refuse(format!("no viewport can be the plan: {}.", reasons.join("; ")))
            };
        }
        several => {
            let handles: Vec<String> = several.iter().map(|(viewport, _, _, _)| hex(viewport.common.handle)).collect();
            return refuse(format!(
                "it has {} plan viewports ({}); SecurePlan needs exactly one. Turn the others off, or publish model space.",
                several.len(),
                handles.join(", ")
            ));
        }
    };

    // The twist must be a quarter turn, as the mapping is.
    let quarter = std::f64::consts::FRAC_PI_2;
    let twist = viewport.twist_angle;
    let turns = (twist / quarter).round();
    let frame_turns = (frame.twist / quarter).round();
    if !twist.is_finite() || (twist - turns * quarter).abs() > TWIST_TOLERANCE || (frame.twist - frame_turns * quarter).abs() > 1e-4 {
        return refuse(format!(
            "viewport {} is twisted by {}°; SecurePlan needs 0°, 90°, 180° or 270°.",
            hex(viewport.common.handle),
            super::ui::format_number(twist.to_degrees())
        ));
    }
    // The renderer maps model to paper by a rotation of `frame.twist`: the
    // model directions of paper +x and +y are that rotation's inverse.
    let (sin, cos) = match (frame_turns as i64).rem_euclid(4) {
        0 => (0.0, 1.0),
        1 => (1.0, 0.0),
        2 => (0.0, -1.0),
        _ => (-1.0, 0.0),
    };
    let basis = [[cos, -sin], [sin, cos]];
    let model_per_paper = viewport.view_height.abs() / viewport.height.abs();
    let (cx, cy) = (viewport.view_center.x, viewport.view_center.y);
    let model_center = [
        viewport.view_target.x + basis[0][0] * cx + basis[1][0] * cy,
        viewport.view_target.y + basis[0][1] * cx + basis[1][1] * cy,
    ];
    // The renderer shows the saved view only when it frames the drawing;
    // otherwise it fits the view to the drawing, which is not what was saved.
    let slack = 1e-5 * (1.0 + cx.abs() + cy.abs() + viewport.view_height.abs()) + 1e-9 * (model_center[0].abs() + model_center[1].abs());
    let saved = (frame.model_target.x - model_center[0]).abs() <= slack
        && (frame.model_target.y - model_center[1]).abs() <= slack
        && (frame.scale * model_per_paper - 1.0).abs() <= 1e-5;
    if !saved {
        return refuse(format!("viewport {} {}.", hex(viewport.common.handle), Problem::NoGeometry.text()));
    }

    let Some(((x0, y0), (x1, y1))) = paper.paper_limits() else { return refuse("its sheet has no size.".into()) };
    let paper_units_per_mm = paper.paper_space_unit_factor();
    let reference = LayoutReference {
        layout: name.to_string(),
        paper_block: paper.current_layout_block_handle_pub(),
        viewport: viewport.common.handle,
        sheet: [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)],
        paper_units_per_mm,
        viewport_rect: rect,
        model_per_paper,
        model_center,
        basis,
        frozen_layers: viewport.frozen_layers.clone(),
    };
    let [width_mm, height_mm] = reference.sheet_mm();
    let side = |mm: f64| (mm * PT_PER_MM - 1e-9).ceil();
    let (width_pt, height_pt) = (side(width_mm), side(height_mm));
    if !(paper_units_per_mm.is_finite() && paper_units_per_mm > 0.0 && width_pt >= 1.0 && height_pt >= 1.0) {
        return refuse("its sheet has no size.".into());
    }
    if width_pt > MAX_SIDE_PT as f64 || height_pt > MAX_SIDE_PT as f64 || width_pt * height_pt > MAX_AREA_PT2 as f64 {
        return refuse(sheet_too_large([width_mm, height_mm]));
    }
    Ok(reference)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::publish::{self, Mapping};
    use crate::app::secureplan::snap::{self, SnapGeometry};
    use acadrust::entities::{Line, Viewport};
    use acadrust::types::Vector3;
    use crate::scene::Scene;

    use crate::app::secureplan::testutil::{self, plan_viewport, LAYOUT};

    fn line(x0: f64, y0: f64, x1: f64, y1: f64) -> EntityType {
        EntityType::Line(Line::from_points(Vector3::new(x0, y0, 0.0), Vector3::new(x1, y1, 0.0)))
    }

    /// The synthetic floor plan, a stray line far outside the viewport, and
    /// the paper layout of [`testutil::add_layout`] with `viewports`.
    pub(crate) fn layout_scene(viewports: Vec<Viewport>) -> (Scene, Vec<Handle>) {
        let mut scene = Scene::new();
        scene.document = testutil::synthetic_document();
        scene.document.add_entity(line(100000.0, 100000.0, 101000.0, 100000.0)).unwrap();
        let handles = testutil::add_layout(&mut scene, viewports);
        (scene, handles)
    }

    fn found(scene: &Scene) -> Result<LayoutReference, String> {
        references(scene).into_iter().find(|(name, _)| name == LAYOUT).expect("the layout is listed").1
    }

    pub(crate) fn mm_mapping() -> Mapping {
        Mapping { cad_origin: [0.0, 18000.0], anchor_mm: [1000.0, 1000.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 }
    }

    /// Model points inside the viewport land on the page where the renderer's
    /// own viewport frame puts them, through both transforms.
    fn assert_agrees_with_the_renderer(scene: &Scene, reference: &LayoutReference, mapping: &Mapping) {
        let frame = scene.viewport_frame(reference.viewport).unwrap();
        let (model, paper) = reference.transforms(mapping).unwrap();
        for cad in [[12345.6, 7890.1], [20000.0, 10000.0], [15000.0, 9000.0], [5000.0, 5000.0]] {
            let on_sheet = frame.model_to_paper(glam::DVec3::new(cad[0], cad[1], 0.0));
            let (px, py) = paper.apply(on_sheet.x, on_sheet.y);
            let (mx, my) = model.apply(cad[0], cad[1]);
            assert!((px - mx).abs() < 1e-3 && (py - my).abs() < 1e-3, "{cad:?}: sheet ({px}, {py}) vs model ({mx}, {my})");
            let back = reference.paper_to_model([on_sheet.x, on_sheet.y]);
            assert!((back[0] - cad[0]).abs() < 1e-3 && (back[1] - cad[1]).abs() < 1e-3, "{cad:?} → {back:?}");
        }
    }

    #[test]
    fn a_plan_viewport_is_the_reference_and_the_sheet_is_the_page() {
        let (scene, handles) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        let reference = found(&scene).expect("eligible");
        assert_eq!(reference.viewport, handles[0]);
        assert_eq!(reference.view_json()["viewportHandle"], hex(handles[0]));
        assert_eq!(reference.view_json()["layoutName"], LAYOUT);
        assert!((reference.model_per_paper - 100.0).abs() < 1e-12);
        assert_eq!(reference.model_center, [15000.0, 9000.0]);
        assert_eq!(reference.sheet_mm(), [841.0, 594.0]);
        let mapping = mm_mapping();
        let (model, paper) = reference.transforms(&mapping).unwrap();
        let placement = model.placement;
        // The whole A1 sheet at its own size: 841 mm = 2383.94 pt.
        assert_eq!((placement.width_pt, placement.height_pt), (2384, 1684));
        assert!((placement.mm_per_pt - 100.0 * 25.4 / 72.0).abs() < 1e-12);
        assert_eq!(paper.placement, placement);
        assert_agrees_with_the_renderer(&scene, &reference, &mapping);
        // The viewport's centre shows (15000, 9000), which the mapping puts
        // at world (16000, 10000).
        let world = mapping.cad_to_world([15000.0, 9000.0]);
        assert_eq!(world, [16000.0, 10000.0]);
        let sheet_world = reference.paper_mapping(&mapping).cad_to_world([420.0, 300.0]);
        assert!((sheet_world[0] - 16000.0).abs() < 1e-9 && (sheet_world[1] - 10000.0).abs() < 1e-9);
    }

    #[test]
    fn quarter_turned_viewports_and_mappings_keep_the_page_axis_aligned() {
        for twist in [90.0, 180.0, 270.0, -90.0] {
            let (scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), twist)]);
            let reference = found(&scene).unwrap_or_else(|reason| panic!("{twist}°: {reason}"));
            for turns in 0..4 {
                let mapping = Mapping { quarter_turns: turns, ..mm_mapping() };
                assert_agrees_with_the_renderer(&scene, &reference, &mapping);
                let placement = reference.transforms(&mapping).unwrap().0.placement;
                // An odd total quarter turn stands the sheet on its side.
                let upright = (placement.width_pt, placement.height_pt) == (2384, 1684);
                let sideways = (placement.width_pt, placement.height_pt) == (1684, 2384);
                assert!(upright || sideways, "{twist}° q{turns}: {placement:?}");
            }
        }
    }

    #[test]
    fn an_empty_survey_puts_the_sheet_at_the_origin_exactly() {
        for twist in [0.0, 90.0] {
            let (scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), twist)]);
            let reference = found(&scene).unwrap();
            for turns in 0..4 {
                let mapping = reference.at_origin(&Mapping { quarter_turns: turns, ..mm_mapping() });
                let placement = reference.transforms(&mapping).unwrap().0.placement;
                assert_eq!(placement.page_world_min, [0.0, 0.0], "{twist}° q{turns}");
            }
        }
    }

    fn refusal(viewports: Vec<Viewport>) -> String {
        found(&layout_scene(viewports).0).expect_err("refused")
    }

    #[test]
    fn ineligible_layouts_are_refused_with_the_reason() {
        let twisted = refusal(vec![plan_viewport((420.0, 300.0), 30.0)]);
        assert!(twisted.starts_with("Layout \"Sheet A1\": viewport "), "{twisted}");
        assert!(twisted.ends_with(" is twisted by 30°; SecurePlan needs 0°, 90°, 180° or 270°."), "{twisted}");
        assert!(refusal(vec![plan_viewport((420.0, 300.0), 90.0 + 1e-6)]).contains("is twisted by"), "a twist just off a quarter turn");

        let mut perspective = plan_viewport((420.0, 300.0), 0.0);
        perspective.status.perspective = true;
        perspective.lens_length = 50.0;
        assert!(refusal(vec![perspective]).ends_with("shows a perspective view."));

        let mut oblique = plan_viewport((420.0, 300.0), 0.0);
        oblique.view_direction = Vector3::new(1.0, -1.0, 1.0);
        assert!(refusal(vec![oblique]).ends_with("does not look straight down on the plan."));
        let mut below = plan_viewport((420.0, 300.0), 0.0);
        below.view_direction = Vector3::new(0.0, 0.0, -1.0);
        assert!(refusal(vec![below]).ends_with("does not look straight down on the plan."));

        let mut clipped = plan_viewport((420.0, 300.0), 0.0);
        clipped.clip_boundary_handle = Handle::new(0x7777);
        assert!(refusal(vec![clipped]).ends_with("is clipped to a non-rectangular boundary."));

        let mut empty = plan_viewport((420.0, 300.0), 0.0);
        empty.view_target = Vector3::new(500000.0, 500000.0, 0.0);
        assert!(refusal(vec![empty]).ends_with("shows none of the drawing's model geometry."));

        let overlapped = refusal(vec![plan_viewport((420.0, 300.0), 0.0), plan_viewport((500.0, 300.0), 0.0)]);
        assert!(overlapped.contains("no viewport can be the plan") && overlapped.matches("is overlapped by another viewport").count() == 2, "{overlapped}");

        let several = refusal(vec![plan_viewport((200.0, 300.0), 0.0), plan_viewport((600.0, 300.0), 0.0)]);
        assert!(several.contains("it has 2 plan viewports") && several.contains("needs exactly one"), "{several}");

        let mut off = plan_viewport((420.0, 300.0), 0.0);
        off.status.is_on = false;
        assert_eq!(refusal(vec![off]), "Layout \"Sheet A1\": it has no viewport that is turned on.");

        // A second, non-plan viewport beside the plan does not stop it.
        let mut iso = plan_viewport((740.0, 480.0), 0.0);
        iso.width = 100.0;
        iso.height = 100.0;
        iso.view_direction = Vector3::new(1.0, -1.0, 1.0);
        assert!(found(&layout_scene(vec![plan_viewport((300.0, 300.0), 0.0), iso]).0).is_ok());
    }

    #[test]
    fn a_sheet_beyond_the_page_limits_is_refused() {
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        for object in scene.document.objects.values_mut() {
            if let acadrust::objects::ObjectType::Layout(layout) = object {
                if layout.name == LAYOUT {
                    layout.paper_width = 6000.0;
                }
            }
        }
        let refused = found(&scene).expect_err("too large");
        assert!(refused.starts_with("Layout \"Sheet A1\": The sheet is 6000 × 594 mm, larger than a SecurePlan page"), "{refused}");
    }

    /// The layout published through its viewport: the PDF, the publication
    /// and its snaps.
    fn publish_layout(scene: &Scene, reference: &LayoutReference, mapping: &Mapping) -> (Vec<u8>, publish::Publication, SnapGeometry) {
        let (model, _) = reference.transforms(mapping).unwrap();
        let publication = publish::prepare_layout(scene, reference, model).unwrap();
        let pdf = publish::page_pdf(&publication).unwrap().bytes;
        let snap = publish::page_snap(&publication);
        let (w, h) = (model.placement.width_pt, model.placement.height_pt);
        let geometry = snap::tests::read(&snap, (w, h)).expect("a valid snap file");
        (pdf, publication, geometry)
    }

    fn near(points: &[[f64; 2]], (x, y): (f64, f64), within: f64) -> bool {
        points.iter().any(|p| (p[0] - x).hypot(p[1] - y) <= within)
    }

    #[test]
    fn a_layout_publishes_its_sheet_and_snaps_only_to_model_geometry_in_its_viewport() {
        let mut viewport = plan_viewport((420.0, 300.0), 0.0);
        // Furniture is frozen in this viewport only.
        let mut furniture = acadrust::tables::Layer::new("FURNITURE");
        furniture.handle = Handle::new(0x9000);
        viewport.frozen_layers = vec![furniture.handle];
        let (mut scene, _) = layout_scene(vec![viewport]);
        scene.document.layers.add(furniture).unwrap();
        let mut chair = line(9000.0, 3000.0, 9500.0, 3000.0);
        chair.common_mut().layer = "FURNITURE".into();
        scene.document.add_entity(chair).unwrap();
        // A wall running out of the viewport's left edge.
        scene.document.add_entity(line(-5000.0, 5000.0, 5000.0, 5000.0)).unwrap();
        scene.rebuild_derived_caches();
        let reference = found(&scene).unwrap();
        let mapping = reference.at_origin(&mm_mapping());
        let (pdf, publication, geometry) = publish_layout(&scene, &reference, &mapping);
        let (model, paper) = reference.transforms(&mapping).unwrap();

        // The whole A1 sheet is the page (CON-01), placed at the origin.
        let doc = lopdf::Document::load_mem(&pdf).unwrap();
        let page = publish::tests::page_dict(&doc);
        let media: Vec<i64> = page.get(b"MediaBox").unwrap().as_array().unwrap().iter().map(|o| o.as_i64().unwrap()).collect();
        assert_eq!(media, vec![0, 0, 2384, 1684]);
        assert!(page.get(b"UserUnit").is_err() && page.get(b"CropBox").is_err());
        assert_eq!(model.placement.page_world_min, [0.0, 0.0]);
        let (again, _, _) = publish_layout(&scene, &reference, &mapping);
        assert_eq!(pdf, again, "deterministic");

        // Model geometry lands where the CAD → world mapping puts it; the
        // title block where the sheet does.
        let drawn = publish::tests::content_points(&pdf);
        let r_pt = model.placement.rounding_bound_mm / model.placement.mm_per_pt;
        for cad in [[12345.6, 7890.1], [20000.0, 10000.0], [30000.0, 18000.0]] {
            assert!(near(&drawn, model.apply(cad[0], cad[1]), r_pt * 2.0 + 1e-6), "PDF misses {cad:?}");
        }
        for sheet in [[20.0, 20.0], [821.0, 20.0]] {
            assert!(near(&drawn, paper.apply(sheet[0], sheet[1]), 1e-3), "PDF misses the title block at {sheet:?}");
        }
        // Only once: the sheet does not draw the viewport's content again
        // through the renderer's own (float32) projection.
        let known = model.apply(12345.6, 7890.1);
        assert_eq!(drawn.iter().filter(|p| (p[0] - known.0).hypot(p[1] - known.1) < 0.05).count(), 1, "the viewport is drawn twice");
        // The model view is clipped to the viewport on the page.
        let viewport_page = reference.viewport_on_page(&paper);
        assert_eq!(publication.clip, viewport_page);
        let expected = [paper.apply(270.0, 210.0), paper.apply(570.0, 390.0)];
        // Paper and page are both y up.
        assert_eq!(viewport_page, [expected[0].0, expected[0].1, expected[1].0, expected[1].1]);
        let ops = publish::tests::operations(&pdf);
        assert!(ops.iter().filter(|op| op.operator == "W").count() >= 2, "no viewport clip");

        // Snaps: model geometry inside the viewport only.
        let snapped: Vec<[f64; 2]> = geometry.points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect();
        let ends: Vec<[f64; 2]> = geometry.segments.iter().flat_map(|s| [[s[0] as f64, s[1] as f64], [s[2] as f64, s[3] as f64]]).collect();
        let inside = |p: &[f64; 2]| {
            p[0] >= viewport_page[0] - 1e-3 && p[0] <= viewport_page[2] + 1e-3 && p[1] >= viewport_page[1] - 1e-3 && p[1] <= viewport_page[3] + 1e-3
        };
        assert!(snapped.iter().chain(&ends).all(inside), "a snap lies outside the viewport");
        assert!(near(&snapped, model.apply(12345.6, 7890.1), 1e-3), "the known line's end");
        assert!(!near(&ends, paper.apply(20.0, 20.0), 1.0) && !near(&ends, paper.apply(821.0, 20.0), 1.0), "a paper-space entity snaps");
        assert!(!near(&ends, model.apply(9000.0, 3000.0), 1e-3), "a layer frozen in the viewport snaps");
        assert!(!near(&drawn, model.apply(9500.0, 3000.0), 1e-3), "a layer frozen in the viewport is drawn");
        // The wall is cut at the viewport's edge (model x = 0).
        assert!(near(&ends, model.apply(0.0, 5000.0), 1e-3) && !near(&snapped, model.apply(-5000.0, 5000.0), 1.0));
        // The column is re-tessellated within the chord tolerance.
        let (cx, cy) = model.apply(5000.0, 5000.0);
        let radius = 300.0 / model.placement.mm_per_pt;
        let tolerance = model.placement.chord_tolerance_mm / model.placement.mm_per_pt;
        let column: Vec<_> = geometry
            .segments
            .iter()
            .filter(|s| [[s[0], s[1]], [s[2], s[3]]].iter().all(|p| ((p[0] as f64 - cx).hypot(p[1] as f64 - cy) - radius).abs() < 1e-3))
            .collect();
        assert!(column.len() > 48, "{} chords", column.len());
        for s in column {
            let middle = ((s[0] + s[2]) as f64 / 2.0, (s[1] + s[3]) as f64 / 2.0);
            assert!(radius - (middle.0 - cx).hypot(middle.1 - cy) <= tolerance + 2.0 * r_pt);
        }
    }

    #[test]
    fn a_quarter_turned_viewport_publishes_the_sheet_on_its_side() {
        let (scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 90.0)]);
        let reference = found(&scene).unwrap();
        let mapping = mm_mapping();
        let (pdf, _, geometry) = publish_layout(&scene, &reference, &mapping);
        let (model, paper) = reference.transforms(&mapping).unwrap();
        assert_eq!((model.placement.width_pt, model.placement.height_pt), (1684, 2384));
        let drawn = publish::tests::content_points(&pdf);
        let snapped: Vec<[f64; 2]> = geometry.points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect();
        for cad in [[12345.6, 7890.1], [20000.0, 10000.0]] {
            let at = model.apply(cad[0], cad[1]);
            assert!(near(&drawn, at, 1e-3) && near(&snapped, at, 1e-3), "{cad:?}");
        }
        assert!(near(&drawn, paper.apply(20.0, 20.0), 1e-3));
    }

    #[test]
    fn the_renderer_plots_the_viewport_where_the_publication_draws_it() {
        for twist in [0.0, 270.0] {
            let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), twist)]);
            let reference = found(&scene).unwrap();
            let (model, paper) = reference.transforms(&mm_mapping()).unwrap();
            scene.set_current_layout(LAYOUT.into());
            let (_, projected) = scene.plot_wire_groups(None);
            let plotted: Vec<[f64; 2]> = projected
                .iter()
                .flat_map(|wire| wire.points.iter().filter(|p| p[0].is_finite()).map(|p| paper.apply(p[0] as f64, p[1] as f64)))
                .map(|(x, y)| [x, y])
                .collect();
            for cad in [[12345.6, 7890.1], [20000.0, 10000.0]] {
                assert!(near(&plotted, model.apply(cad[0], cad[1]), 0.01), "{twist}°: the layout plot puts {cad:?} elsewhere");
            }
        }
    }

    #[test]
    fn a_layout_survives_the_dxf_and_dwg_round_trip() {
        let (scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 90.0)]);
        let expected = found(&scene).unwrap();
        for format in ["dxf", "dwg"] {
            let bytes = crate::io::save_to_bytes(&scene.document, format, acadrust::DxfVersion::AC1032).unwrap();
            let mut loaded = Scene::new();
            loaded.document = crate::io::load_bytes(&format!("layout.{format}"), bytes).unwrap();
            loaded.rebuild_derived_caches();
            let reference = found(&loaded).unwrap_or_else(|reason| panic!("{format}: {reason}"));
            assert_eq!((reference.sheet, reference.basis, reference.model_center), (expected.sheet, expected.basis, expected.model_center), "{format}");
            assert_eq!((reference.model_per_paper, reference.viewport_rect), (expected.model_per_paper, expected.viewport_rect), "{format}");
        }
    }

    #[test]
    fn model_space_only_drawings_have_no_layouts() {
        let mut scene = Scene::new();
        scene.document = crate::app::secureplan::testutil::synthetic_document();
        scene.rebuild_derived_caches();
        let layouts = references(&scene);
        assert!(layouts.iter().all(|(_, found)| found.is_err()), "{layouts:?}");
    }
}
