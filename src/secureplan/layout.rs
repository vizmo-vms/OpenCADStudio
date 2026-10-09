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

use codec::types::Handle;
use codec::EntityType;

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
    /// The other viewports that show the drawing on the sheet, in the
    /// layout's order; the reference goes at `reference_position` in it.
    pub others: Vec<SheetView>,
    pub reference_position: usize,
    /// The layout shows annotative objects of every scale (its setting).
    pub annotation_all_visible: bool,
}

/// Another viewport on a published sheet: a plan view (any twist), drawn
/// exactly from model space but giving no snaps.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetView {
    pub viewport: Handle,
    /// `[x0, y0, x1, y1]` in paper units.
    pub rect: [f64; 4],
    /// The boundary it is clipped to, if not its rectangle.
    pub boundary: Option<Handle>,
    /// Model → paper, `[a, b, c, d, e, f]`: `(a·x + b·y + e, c·x + d·y + f)`.
    pub to_paper: [f64; 6],
    /// Model units per paper unit.
    pub model_per_paper: f64,
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
    DepthClipped,
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
            Problem::DepthClipped => "uses front or back clipping, which SecurePlan cannot publish",
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

fn rect_of(viewport: &codec::entities::Viewport) -> [f64; 4] {
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

/// Whether the viewport looks straight down on the plan (+Z).
fn looks_down(viewport: &codec::entities::Viewport) -> bool {
    let direction = viewport.view_direction;
    let length = (direction.x * direction.x + direction.y * direction.y + direction.z * direction.z).sqrt();
    length > 0.0 && direction.z > 0.0 && direction.x.abs() <= 1e-9 * length && direction.y.abs() <= 1e-9 * length
}

/// Whether a point is inside the rings (even-odd).
fn inside(rings: &[Vec<[f64; 2]>], [x, y]: [f64; 2]) -> bool {
    let mut inside = false;
    for ring in rings {
        for (i, a) in ring.iter().enumerate() {
            let b = ring[(i + 1) % ring.len()];
            if (a[1] > y) != (b[1] > y) && x < a[0] + (y - a[1]) * (b[0] - a[0]) / (b[1] - a[1]) {
                inside = !inside;
            }
        }
    }
    inside
}

/// Whether the viewport shows any visible model geometry (its own frozen
/// layers excluded), measured through the renderer's own viewport frame:
/// a drawn line, or a fill (a solid hatch has no lines).
fn shows_geometry(
    model: &crate::scene::Scene,
    viewport: Handle,
    frame: &crate::scene::viewport_ref::ViewportFrame,
    rect: [f64; 4],
    all_visible: bool,
) -> bool {
    let to_paper = |x: f64, y: f64| {
        let paper = frame.model_to_paper(glam::DVec3::new(x, y, 0.0));
        [paper.x, paper.y]
    };
    let wires = model.model_wires_for_viewport_arc(viewport, 0.0);
    let lines = wires.iter().filter(|wire| wire.plot_visible).any(|wire| {
        let mut previous: Option<[f64; 2]> = None;
        (0..wire.points.len()).any(|index| {
            let [x, y, z] = wire.points[index];
            if x.is_nan() || y.is_nan() || z.is_nan() {
                previous = None;
                return false;
            }
            let point = wire.point_world(index, 0.0);
            let here = to_paper(point.x, point.y);
            let hit = crosses(previous.unwrap_or(here), here, rect);
            previous = Some(here);
            hit
        })
    });
    if lines {
        return true;
    }
    let (hatches, wipeouts) = model.secureplan_viewport_fills(viewport, all_visible);
    let centre = [(rect[0] + rect[2]) / 2.0, (rect[1] + rect[3]) / 2.0];
    hatches.iter().chain(&wipeouts).any(|fill| {
        let mut rings = vec![Vec::new()];
        for point in fill.boundary.iter() {
            if point[0].is_finite() && point[1].is_finite() {
                let at = to_paper(fill.world_origin[0] + point[0] as f64, fill.world_origin[1] + point[1] as f64);
                rings.last_mut().expect("a ring").push(at);
            } else {
                rings.push(Vec::new());
            }
        }
        rings.retain(|ring| ring.len() >= 3);
        // An edge crosses the viewport, or the fill covers it.
        rings.iter().any(|ring| (0..ring.len()).any(|i| crosses(ring[i], ring[(i + 1) % ring.len()], rect))) || inside(&rings, centre)
    })
}

/// A viewport's model → paper map as it saves it, exactly, when the renderer
/// shows that saved view (it fits a stale view to the drawing instead):
/// `(to_paper, model_per_paper, twist)`.
fn saved_view(viewport: &codec::entities::Viewport, frame: &crate::scene::viewport_ref::ViewportFrame) -> Option<([f64; 6], f64, f64)> {
    use std::f64::consts::TAU;
    // The renderer turns the model by ±twist; take the sign it uses.
    let apart = |a: f64, b: f64| ((a - b).rem_euclid(TAU)).min((b - a).rem_euclid(TAU));
    let twist = viewport.twist_angle;
    let theta = [twist, -twist].into_iter().find(|t| apart(frame.twist, *t) <= 1e-4)?;
    let k = viewport.view_height.abs() / viewport.height.abs();
    let (sin, cos) = theta.sin_cos();
    let (cx, cy) = (viewport.view_center.x, viewport.view_center.y);
    // The model point at the centre: target plus the view centre along the
    // model directions of paper +x (cos, −sin) and +y (sin, cos).
    let target = [viewport.view_target.x + cos * cx + sin * cy, viewport.view_target.y - sin * cx + cos * cy];
    let slack = 1e-5 * (1.0 + cx.abs() + cy.abs() + viewport.view_height.abs()) + 1e-9 * (target[0].abs() + target[1].abs());
    let same = (frame.model_target.x - target[0]).abs() <= slack
        && (frame.model_target.y - target[1]).abs() <= slack
        && (frame.scale * k - 1.0).abs() <= 1e-5;
    if !same {
        return None;
    }
    let (a, b, c, d) = (cos / k, -sin / k, sin / k, cos / k);
    let (px, py) = (viewport.center.x, viewport.center.y);
    Some(([a, b, c, d, px - (a * target[0] + b * target[1]), py - (c * target[0] + d * target[1])], k, theta))
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
    let viewports: Vec<&codec::entities::Viewport> = paper
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
    let all_visible = paper.annotation_all_visible();
    let mut judged = Vec::new();
    for viewport in &viewports {
        let handle = viewport.common.handle;
        let rect = rect_of(viewport);
        let frame = model.viewport_frame(handle);
        // Depth clipping first: no enabled viewport may use it, whatever else.
        let problem = if viewport.status.front_clipping || viewport.status.back_clipping {
            Some(Problem::DepthClipped)
        } else if !(viewport.width.abs() > 0.0 && viewport.height.abs() > 0.0 && viewport.view_height.abs() > 0.0)
            || !rect.iter().all(|v| v.is_finite())
            || !viewport.view_height.is_finite()
        {
            Some(Problem::NoView)
        } else if !viewport.clip_boundary_handle.is_null() {
            Some(Problem::Clipped)
        } else if viewport.status.perspective {
            Some(Problem::Perspective)
        } else if !looks_down(viewport)
            // The renderer's own frame: a mirrored or oblique view has none.
            || frame.is_none()
        {
            Some(Problem::NotPlan)
        } else if viewports.iter().any(|other| other.common.handle != handle && overlap(rect, rect_of(other))) {
            Some(Problem::Overlapped)
        } else if !frame.as_ref().is_some_and(|frame| shows_geometry(model, handle, frame, rect, all_visible)) {
            Some(Problem::NoGeometry)
        } else {
            None
        };
        judged.push((*viewport, rect, frame, problem));
    }

    let candidates: Vec<_> = judged.iter().filter(|(_, _, _, problem)| problem.is_none()).collect();
    let (viewport, rect, frame) = match candidates.as_slice() {
        [(viewport, rect, Some(frame), _)] => (*viewport, *rect, *frame),
        [(viewport, ..)] => return refuse(format!("viewport {} {}.", hex(viewport.common.handle), Problem::NotPlan.text())),
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

    // The other viewports that show the drawing are drawn on the sheet as
    // exactly as the reference: plan views only, without depth clipping.
    let mut others = Vec::new();
    let mut reference_position = 0;
    for (other, other_rect, other_frame, problem) in &judged {
        let handle = other.common.handle;
        if handle == viewport.common.handle {
            reference_position = others.len();
            continue;
        }
        if other.status.front_clipping || other.status.back_clipping {
            return refuse(format!("viewport {} {}.", hex(handle), Problem::DepthClipped.text()));
        }
        if *problem == Some(Problem::NoView) {
            continue; // Nothing drawn.
        }
        let Some(frame) = other_frame.filter(|_| !other.status.perspective && looks_down(other)) else {
            return refuse(format!(
                "viewport {} shows a 3D view, which SecurePlan cannot draw on a published sheet. Turn it off to publish this layout.",
                hex(handle)
            ));
        };
        if !shows_geometry(model, handle, &frame, *other_rect, all_visible) {
            continue; // Nothing drawn.
        }
        let Some((to_paper, model_per_paper, _)) = saved_view(other, &frame) else {
            return refuse(format!("viewport {} {}.", hex(handle), Problem::NoGeometry.text()));
        };
        let boundary = (!other.clip_boundary_handle.is_null()).then_some(other.clip_boundary_handle);
        if boundary.is_some_and(|b| !matches!(paper.document.get_entity(b), Some(EntityType::Circle(_) | EntityType::LwPolyline(_) | EntityType::Polyline2D(_) | EntityType::Ellipse(_) | EntityType::Spline(_)))) {
            return refuse(format!("viewport {} is clipped by a boundary SecurePlan cannot read.", hex(handle)));
        }
        others.push(SheetView { viewport: handle, rect: *other_rect, boundary, to_paper, model_per_paper });
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
        others,
        reference_position,
        annotation_all_visible: all_visible,
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
    use codec::entities::{Line, Viewport};
    use codec::types::Vector3;
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

        // Depth clipping would publish and snap other storeys (PUB-03).
        for (front, back) in [(true, false), (false, true)] {
            let mut clipped = plan_viewport((420.0, 300.0), 0.0);
            clipped.status.front_clipping = front;
            clipped.status.back_clipping = back;
            let refused = refusal(vec![clipped]);
            assert!(refused.ends_with("uses front or back clipping, which SecurePlan cannot publish."), "{refused}");
        }

        // Another viewport can be drawn on the sheet only as a plan view
        // without depth clipping.
        let mut iso = plan_viewport((740.0, 480.0), 0.0);
        iso.width = 100.0;
        iso.height = 100.0;
        iso.view_direction = Vector3::new(1.0, -1.0, 1.0);
        let refused = refusal(vec![plan_viewport((300.0, 300.0), 0.0), iso.clone()]);
        assert!(refused.ends_with("shows a 3D view, which SecurePlan cannot draw on a published sheet. Turn it off to publish this layout."), "{refused}");
        iso.status.back_clipping = true;
        let refused = refusal(vec![plan_viewport((300.0, 300.0), 0.0), iso]);
        assert!(refused.ends_with("uses front or back clipping, which SecurePlan cannot publish."), "{refused}");
        // Even a viewport with no size may not use it.
        let mut empty = plan_viewport((740.0, 480.0), 0.0);
        (empty.width, empty.height) = (0.0, 0.0);
        empty.status.front_clipping = true;
        let refused = refusal(vec![plan_viewport((300.0, 300.0), 0.0), empty]);
        assert!(refused.ends_with("uses front or back clipping, which SecurePlan cannot publish."), "{refused}");
    }

    #[test]
    fn a_viewport_showing_only_a_solid_fill_shows_the_drawing() {
        use codec::entities::{BoundaryEdge, BoundaryPath, Hatch, PolylineEdge};
        use codec::types::Vector2;
        // A 1:10 viewport onto a solid hatch far from everything else.
        let mut viewport = plan_viewport((420.0, 300.0), 0.0);
        viewport.view_target = Vector3::new(61000.0, 1000.0, 0.0);
        viewport.view_height = 1800.0;
        let (mut scene, _) = layout_scene(vec![viewport]);
        assert!(found(&scene).unwrap_err().ends_with("shows none of the drawing's model geometry."));
        let mut path = BoundaryPath::new();
        let corners = [[59000.0, -1000.0], [63000.0, -1000.0], [63000.0, 3000.0], [59000.0, 3000.0]].map(|[x, y]| Vector2::new(x, y));
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(corners.to_vec(), true)));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        scene.add_entity(EntityType::Hatch(hatch));
        scene.rebuild_derived_caches();
        // The fill covers the whole viewport: no edge is in view.
        assert!(found(&scene).is_ok(), "{:?}", found(&scene));
    }

    #[test]
    fn a_sheet_beyond_the_page_limits_is_refused() {
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        for object in scene.document.objects.values_mut() {
            if let codec::objects::ObjectType::Layout(layout) = object {
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
        let mut furniture = codec::tables::Layer::new("FURNITURE");
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
        // The model view is drawn inside a q … Q scope clipped exactly to the
        // viewport, and the wall crossing its edge stays inside that scope.
        let ops = publish::tests::operations(&pdf);
        let (open, close, depths) = clip_scope(&ops, viewport_page);
        let wall = op_at(&ops, model.apply(-5000.0, 5000.0));
        assert!(open < wall && wall < close, "the crossing wall escapes the viewport clip");
        assert_eq!(depths[close + 1], depths[open], "the state is restored after the viewport");

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

    /// The q/Q depth before each operation.
    fn depths(ops: &[lopdf::content::Operation]) -> Vec<i32> {
        let mut depth = 0;
        ops.iter()
            .map(|op| {
                let before = depth;
                depth += match op.operator.as_str() {
                    "q" => 1,
                    "Q" => -1,
                    _ => 0,
                };
                before
            })
            .collect()
    }

    /// The `q` opening the scope clipped to the page rectangle `rect` and the
    /// `Q` closing it (the clip path is exactly the rectangle's corners).
    fn clip_scope(ops: &[lopdf::content::Operation], rect: [f64; 4]) -> (usize, usize, Vec<i32>) {
        let depths = depths(ops);
        let number = |o: &lopdf::Object| o.as_float().map(f64::from).or_else(|_| o.as_i64().map(|v| v as f64)).unwrap();
        for (w, _) in ops.iter().enumerate().filter(|(_, op)| op.operator == "W") {
            let mut start = w;
            let mut corners = Vec::new();
            while start > 0 && matches!(ops[start - 1].operator.as_str(), "m" | "l" | "h") {
                start -= 1;
                if ops[start].operator != "h" {
                    corners.push([number(&ops[start].operands[0]), number(&ops[start].operands[1])]);
                }
            }
            let expected = [[rect[0], rect[1]], [rect[2], rect[1]], [rect[2], rect[3]], [rect[0], rect[3]]];
            let exact = corners.len() == 4 && expected.iter().all(|e| corners.iter().any(|c| (c[0] - e[0]).abs() < 1e-3 && (c[1] - e[1]).abs() < 1e-3));
            if !exact {
                continue;
            }
            assert_eq!(ops[start - 1].operator, "q", "the viewport clip is not scoped");
            let close = (w..ops.len()).find(|&i| ops[i].operator == "Q" && depths[i] == depths[w]).expect("the clip scope closes");
            return (start - 1, close, depths);
        }
        panic!("no clip path is the viewport rectangle {rect:?}");
    }

    /// The index of the first path operation at `point` (page points).
    fn op_at(ops: &[lopdf::content::Operation], (x, y): (f64, f64)) -> usize {
        let number = |o: &lopdf::Object| o.as_float().map(f64::from).or_else(|_| o.as_i64().map(|v| v as f64)).unwrap();
        ops.iter()
            .position(|op| (op.operator == "m" || op.operator == "l") && (number(&op.operands[0]) - x).abs() < 1e-3 && (number(&op.operands[1]) - y).abs() < 1e-3)
            .unwrap_or_else(|| panic!("nothing is drawn at ({x}, {y})"))
    }

    #[test]
    fn paper_space_drawn_last_is_outside_the_viewport_clip() {
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        for object in scene.document.objects.values_mut() {
            if let codec::objects::ObjectType::Layout(layout) = object {
                if layout.name == LAYOUT {
                    layout.plot_flags.draw_viewports_first = true;
                }
            }
        }
        scene.rebuild_derived_caches();
        let reference = found(&scene).unwrap();
        let mapping = mm_mapping();
        let (pdf, _, _) = publish_layout(&scene, &reference, &mapping);
        let (_, paper) = reference.transforms(&mapping).unwrap();
        let ops = publish::tests::operations(&pdf);
        let (open, close, depths) = clip_scope(&ops, reference.viewport_on_page(&paper));
        let title = op_at(&ops, paper.apply(20.0, 20.0));
        assert!(title > close, "the title block is drawn inside the viewport clip");
        assert_eq!(depths[close + 1], depths[open], "the state is restored after the viewport");
    }

    #[test]
    fn the_viewport_annotation_scale_places_annotative_blocks() {
        use codec::entities::Insert;
        use codec::xdata::ExtendedDataRecord;
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        // The drawing's own annotation scale is 1:1; the 1:100 viewport's is 1:100.
        scene.set_annotation_scale_named("1:100").unwrap();
        scene.set_annotation_scale_named("1:1").unwrap();
        // A 10-unit annotative symbol at (15000, 3000): 1000 units at 1:100.
        snap::tests::block(&mut scene.document, "SYMBOL", vec![line(0.0, 0.0, 10.0, 0.0)]);
        let mut insert = Insert::new("SYMBOL", Vector3::new(15000.0, 3000.0, 0.0));
        insert.common.extended_data.add_record(ExtendedDataRecord::new("AcAnnotativeData"));
        scene.document.add_entity(EntityType::Insert(insert)).unwrap();
        scene.rebuild_derived_caches();
        let reference = found(&scene).unwrap();
        let mapping = mm_mapping();
        let (pdf, _, geometry) = publish_layout(&scene, &reference, &mapping);
        let (model, _) = reference.transforms(&mapping).unwrap();
        let drawn = publish::tests::content_points(&pdf);
        let ends: Vec<[f64; 2]> = geometry.segments.iter().flat_map(|s| [[s[0] as f64, s[1] as f64], [s[2] as f64, s[3] as f64]]).collect();
        let (scaled, unscaled) = (model.apply(16000.0, 3000.0), model.apply(15010.0, 3000.0));
        assert!(near(&drawn, scaled, 1e-3) && near(&ends, scaled, 1e-3), "the symbol is not at the viewport's 1:100 size");
        assert!(!near(&drawn, unscaled, 1e-3) && !near(&ends, unscaled, 1e-3), "the symbol is at the drawing's 1:1 size");
    }

    /// A 1:100 viewport on a drawing whose own annotation scale is 1:1.
    fn annotation_scene() -> Scene {
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        scene.set_annotation_scale_named("1:100").unwrap();
        scene.set_annotation_scale_named("1:1").unwrap();
        scene
    }

    #[test]
    fn annotative_fills_follow_the_viewport_annotation_scale() {
        use codec::entities::{BoundaryEdge, BoundaryPath, Hatch, Insert, PolylineEdge, Wipeout};
        use codec::types::Vector2;
        use codec::xdata::ExtendedDataRecord;
        let mut scene = annotation_scene();
        // A symbol: a line, a 10 × 10 solid hatch and a 10 × 10 wipeout below it.
        let mut path = BoundaryPath::new();
        let square = |y: f64| [[0.0, y], [10.0, y], [10.0, y + 10.0], [0.0, y + 10.0]].map(|[x, y]| Vector2::new(x, y)).to_vec();
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(square(0.0), true)));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        let wipeout = Wipeout::polygonal(&square(-20.0), 0.0);
        snap::tests::block(&mut scene.document, "SYMBOL", vec![line(0.0, 0.0, 10.0, 0.0), EntityType::Hatch(hatch), EntityType::Wipeout(wipeout)]);
        let mut insert = Insert::new("SYMBOL", Vector3::new(15000.0, 3000.0, 0.0));
        insert.common.extended_data.add_record(ExtendedDataRecord::new("AcAnnotativeData"));
        scene.document.add_entity(EntityType::Insert(insert)).unwrap();
        scene.rebuild_derived_caches();
        let reference = found(&scene).unwrap();
        let mapping = mm_mapping();
        let (pdf, _, _) = publish_layout(&scene, &reference, &mapping);
        let (model, _) = reference.transforms(&mapping).unwrap();
        let drawn = publish::tests::content_points(&pdf);
        // At 1:100 the symbol is 1000 units: the hatch reaches (16000, 4000),
        // the wipeout's far corner (16000, 1000).
        for (what, scaled, unscaled) in [("hatch", (16000.0, 4000.0), (15010.0, 3010.0)), ("wipeout", (16000.0, 1000.0), (15010.0, 2980.0))] {
            assert!(near(&drawn, model.apply(scaled.0, scaled.1), 1e-3), "the {what} is not at the viewport's 1:100 size");
            assert!(!near(&drawn, model.apply(unscaled.0, unscaled.1), 1e-3), "the {what} is at the drawing's 1:1 size");
        }
    }

    #[test]
    fn objects_of_other_annotation_scales_are_tessellated_and_snapped_as_drawn() {
        use codec::entities::{Circle, Insert};
        let mut scene = annotation_scene();
        // A block of the 1:50 scale only, with a circle: the layout shows
        // every scale's objects, so it is drawn through the 1:100 viewport.
        let fifty = scene.set_annotation_scale_named("1:50").unwrap();
        scene.set_annotation_scale_named("1:1").unwrap();
        let mut circle = Circle::new();
        circle.radius = 750.0;
        snap::tests::block(&mut scene.document, "ROUND", vec![EntityType::Circle(circle)]);
        let insert = scene.document.add_entity(EntityType::Insert(Insert::new("ROUND", Vector3::new(22000.0, 14000.0, 0.0)))).unwrap();
        assert!(crate::scene::annotative::create_annotation_context(&mut scene.document, insert, fifty));
        scene.rebuild_derived_caches();
        let reference = found(&scene).unwrap();
        assert!(reference.annotation_all_visible);
        let mapping = mm_mapping();
        let (pdf, _, geometry) = publish_layout(&scene, &reference, &mapping);
        let (model, _) = reference.transforms(&mapping).unwrap();
        let (cx, cy) = model.apply(22000.0, 14000.0);
        let radius = 750.0 / model.placement.mm_per_pt;
        let tolerance = model.placement.chord_tolerance_mm / model.placement.mm_per_pt;
        let r_pt = model.placement.rounding_bound_mm / model.placement.mm_per_pt;
        let (error, chords) = chord_error(&publish::tests::content_points(&pdf), (cx, cy), radius);
        assert!(chords > 48 && error <= tolerance + 2.0 * r_pt, "{chords} chords, error {error} pt > {tolerance} pt");
        let ends: Vec<[f64; 2]> = geometry.segments.iter().flat_map(|s| [[s[0] as f64, s[1] as f64], [s[2] as f64, s[3] as f64]]).collect();
        assert!(chord_error(&ends, (cx, cy), radius).1 > 48, "the drawn circle gives no snaps");
    }

    #[test]
    fn paper_space_is_tessellated_at_its_own_annotation_scale() {
        use codec::entities::{Circle, Insert};
        use codec::xdata::ExtendedDataRecord;
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        // The drawing's scale is 10:1, but paper space draws at 1:1: an
        // annotative paper symbol keeps its size.
        scene.set_annotation_scale_named("10:1").unwrap();
        let mut circle = Circle::new();
        circle.radius = 20.0;
        snap::tests::block(&mut scene.document, "STAMP", vec![EntityType::Circle(circle)]);
        scene.set_current_layout(LAYOUT.into());
        let mut insert = Insert::new("STAMP", Vector3::new(100.0, 500.0, 0.0));
        insert.common.extended_data.add_record(ExtendedDataRecord::new("AcAnnotativeData"));
        scene.add_entity(EntityType::Insert(insert));
        scene.set_current_layout("Model".into());
        scene.rebuild_derived_caches();
        let reference = found(&scene).unwrap();
        let mapping = mm_mapping();
        let (pdf, _, _) = publish_layout(&scene, &reference, &mapping);
        let (model, paper) = reference.transforms(&mapping).unwrap();
        let (cx, cy) = paper.apply(100.0, 500.0);
        let radius = paper.apply(120.0, 500.0).0 - cx;
        let tolerance = model.placement.chord_tolerance_mm / model.placement.mm_per_pt;
        let r_pt = model.placement.rounding_bound_mm / model.placement.mm_per_pt;
        let (error, chords) = chord_error(&publish::tests::content_points(&pdf), (cx, cy), radius);
        assert!(chords > 48 && error <= tolerance + 2.0 * r_pt, "{chords} chords, error {error} pt > {tolerance} pt");
    }

    /// Fill operators in a page's content.
    fn fills(pdf: &[u8]) -> usize {
        publish::tests::operations(pdf).iter().filter(|op| matches!(op.operator.as_str(), "f" | "f*" | "F" | "B" | "B*" | "b" | "b*")).count()
    }

    #[test]
    fn the_viewport_render_mode_decides_whether_3d_faces_are_filled() {
        use codec::entities::{Face3D, ViewportRenderMode};
        let publish_with = |face: bool, mode: ViewportRenderMode| {
            let mut viewport = plan_viewport((420.0, 300.0), 0.0);
            viewport.render_mode = mode;
            let (mut scene, _) = layout_scene(vec![viewport]);
            if face {
                let corner = |x: f64, y: f64| Vector3::new(x, y, 0.0);
                let face = Face3D::new(corner(15000.0, 5000.0), corner(16000.0, 5000.0), corner(16000.0, 6000.0), corner(15000.0, 6000.0));
                scene.document.add_entity(EntityType::Face3D(face)).unwrap();
                scene.rebuild_derived_caches();
            }
            let reference = found(&scene).unwrap();
            fills(&publish_layout(&scene, &reference, &mm_mapping()).0)
        };
        let plain = publish_with(false, ViewportRenderMode::Wireframe2D);
        assert_eq!(publish_with(true, ViewportRenderMode::Wireframe2D), plain, "a wireframe viewport fills a 3D face");
        assert!(publish_with(true, ViewportRenderMode::FlatShaded) > plain, "a shaded viewport does not fill a 3D face");
    }

    #[test]
    fn psltscale_keeps_dashes_the_same_on_paper_in_every_viewport() {
        use codec::tables::LineType;
        let dashes = |psltscale: bool| {
            let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
            scene.document.line_types.add(LineType::dashed()).unwrap();
            // PSLTSCALE is the layout's own setting.
            for object in scene.document.objects.values_mut() {
                if let codec::objects::ObjectType::Layout(layout) = object {
                    if layout.name == LAYOUT {
                        layout.flags = if psltscale { layout.flags | 1 } else { layout.flags & !1 };
                    }
                }
            }
            scene.document.header.paper_space_linetype_scaling = !psltscale;
            // A dashed wall seen by the 1:100 plan and the 1:10 detail.
            let mut wall = line(4800.0, 5100.0, 5200.0, 5100.0);
            wall.common_mut().linetype = "Dashed".into();
            wall.common_mut().linetype_scale = 200.0;
            scene.document.add_entity(wall).unwrap();
            with_detail(&mut scene);
            let reference = found(&scene).unwrap();
            let pdf = publish_layout(&scene, &reference, &mm_mapping()).0;
            let number = |o: &lopdf::Object| o.as_float().map(f64::from).or_else(|_| o.as_i64().map(|v| v as f64)).unwrap();
            let mut arrays: Vec<Vec<f64>> = publish::tests::operations(&pdf)
                .iter()
                .filter(|op| op.operator == "d")
                .map(|op| op.operands[0].as_array().unwrap().iter().map(number).collect::<Vec<f64>>())
                .filter(|dash| !dash.is_empty())
                .collect();
            arrays.sort_by(|a, b| a[0].total_cmp(&b[0]));
            arrays.dedup();
            arrays
        };
        // PSLTSCALE: 0.5 × 200 = 100 paper mm dashes in both viewports.
        let on = dashes(true);
        assert_eq!(on.len(), 1, "{on:?}");
        assert!((on[0][0] - (100.0 * 72.0 / 25.4_f64).round()).abs() <= 1.0, "{on:?}");
        // Without it, dashes scale with each viewport: 1 mm at 1:100, 10 mm at 1:10.
        let off = dashes(false);
        assert_eq!(off.len(), 2, "{off:?}");
        assert!(off[1][0] > 5.0 * off[0][0], "{off:?}");
    }

    /// A circular 1:10 detail viewport onto the column at (5000, 5000), beside
    /// the plan: clipped to a circle on the sheet, so not a second plan.
    fn with_detail(scene: &mut Scene) -> Handle {
        detail_at(scene, (5000.0, 5000.0), 2000.0)
    }

    /// A circular detail viewport beside the plan showing `target` with
    /// `view_height` model units across its 200 mm height.
    fn detail_at(scene: &mut Scene, target: (f64, f64), view_height: f64) -> Handle {
        scene.set_current_layout(LAYOUT.into());
        let mut outline = codec::entities::Circle::new();
        outline.center = Vector3::new(720.0, 470.0, 0.0);
        outline.radius = 100.0;
        let outline = scene.add_entity(EntityType::Circle(outline));
        let mut detail = plan_viewport((720.0, 470.0), 0.0);
        detail.id = 3;
        (detail.width, detail.height, detail.view_height) = (200.0, 200.0, view_height);
        detail.view_target = Vector3::new(target.0, target.1, 0.0);
        detail.clip_boundary_handle = outline;
        let detail = scene.add_entity(EntityType::Viewport(detail));
        scene.set_current_layout("Model".into());
        scene.rebuild_derived_caches();
        detail
    }

    /// A circular solid hatch of `radius` about `(x, y)` in model space.
    fn round_fill(scene: &mut Scene, (x, y): (f64, f64), radius: f64) {
        use codec::entities::{BoundaryEdge, BoundaryPath, CircularArcEdge, Hatch};
        let mut path = BoundaryPath::new();
        path.add_edge(BoundaryEdge::CircularArc(CircularArcEdge {
            center: codec::types::Vector2::new(x, y),
            radius,
            start_angle: 0.0,
            end_angle: std::f64::consts::TAU,
            counter_clockwise: true,
        }));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        scene.add_entity(EntityType::Hatch(hatch));
        scene.rebuild_derived_caches();
    }

    /// The largest chord error (page points) of the page points `ring` lying
    /// on the circle of `radius` about `centre`, and how many chords.
    fn chord_error(points: &[[f64; 2]], (cx, cy): (f64, f64), radius: f64) -> (f64, usize) {
        let ring: Vec<[f64; 2]> = points.iter().copied().filter(|p| ((p[0] - cx).hypot(p[1] - cy) - radius).abs() < 0.01).collect();
        let mut worst = 0.0_f64;
        let mut chords = 0;
        for pair in ring.windows(2) {
            if (pair[0][0] - pair[1][0]).hypot(pair[0][1] - pair[1][1]) < radius {
                let middle = [(pair[0][0] + pair[1][0]) / 2.0, (pair[0][1] + pair[1][1]) / 2.0];
                worst = worst.max(radius - (middle[0] - cx).hypot(middle[1] - cy));
                chords += 1;
            }
        }
        (worst, chords)
    }

    #[test]
    fn an_enlarged_detail_viewport_is_drawn_exactly_within_the_page_precision() {
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        // A round solid fill (radius 200) inside the column (radius 300).
        round_fill(&mut scene, (5000.0, 5000.0), 200.0);
        let detail = with_detail(&mut scene);
        let reference = found(&scene).unwrap();
        assert_eq!(reference.others.len(), 1);
        assert_eq!((reference.others[0].viewport, reference.others[0].model_per_paper), (detail, 10.0));
        let mapping = mm_mapping();
        let (pdf, publication, geometry) = publish_layout(&scene, &reference, &mapping);
        let (model, paper) = reference.transforms(&mapping).unwrap();
        let place = |x: f64, y: f64| {
            let [a, b, c, d, e, f] = reference.others[0].to_paper;
            paper.apply(a * x + b * y + e, c * x + d * y + f)
        };
        let (cx, cy) = place(5000.0, 5000.0);
        assert!((cx - paper.apply(720.0, 470.0).0).abs() < 1e-9 && (cy - paper.apply(720.0, 470.0).1).abs() < 1e-9);
        // Every chord of the column, 10× enlarged, within the page's chord
        // tolerance, and every vertex where the f64 transform puts it.
        let radius = place(5300.0, 5000.0).0 - cx;
        let tolerance = model.placement.chord_tolerance_mm / model.placement.mm_per_pt;
        let r_pt = model.placement.rounding_bound_mm / model.placement.mm_per_pt;
        let drawn = publish::tests::content_points(&pdf);
        let column: Vec<[f64; 2]> = drawn.iter().copied().filter(|p| ((p[0] - cx).hypot(p[1] - cy) - radius).abs() < 0.01).collect();
        assert!(column.len() > 300, "{} vertices", column.len());
        let mut chords = 0;
        for pair in column.windows(2) {
            if (pair[0][0] - pair[1][0]).hypot(pair[0][1] - pair[1][1]) < 5.0 {
                let middle = [(pair[0][0] + pair[1][0]) / 2.0, (pair[0][1] + pair[1][1]) / 2.0];
                assert!(radius - (middle[0] - cx).hypot(middle[1] - cy) <= tolerance + 2.0 * r_pt, "a chord misses the page precision");
                chords += 1;
            }
        }
        assert!(chords > 300);
        let column_polyline = publication
            .scene
            .document
            .entities()
            .find_map(|entity| match entity {
                EntityType::LwPolyline(p) if p.vertices.iter().all(|v| ((v.location.x - 5000.0).hypot(v.location.y - 5000.0) - 300.0).abs() < 1e-6) => Some(p.clone()),
                _ => None,
            })
            .expect("the column is re-tessellated");
        let worst = column_polyline
            .vertices
            .iter()
            .map(|vertex| {
                let (x, y) = place(vertex.location.x, vertex.location.y);
                drawn.iter().map(|p| (p[0] - x).hypot(p[1] - y)).fold(f64::INFINITY, f64::min)
            })
            .fold(0.0, f64::max);
        // One f32 rounding per coordinate: within √2·r.
        assert!(worst <= r_pt * std::f64::consts::SQRT_2, "the detail is {worst} pt from where its f64 transform puts it (r = {r_pt} pt)");
        // The round fill too: every boundary chord within the page precision
        // (the fill renderer alone cuts at 7.5°), from exact vertices.
        let fill_radius = place(5200.0, 5000.0).0 - cx;
        let (fill_error, fill_chords) = chord_error(&drawn, (cx, cy), fill_radius);
        assert!(fill_chords > 200 && fill_error <= tolerance + 2.0 * r_pt, "round fill: {fill_chords} chords, error {fill_error} pt > {tolerance} pt");
        let fill_boundary: Vec<[f64; 2]> = publication
            .scene
            .document
            .entities()
            .find_map(|entity| match entity {
                EntityType::Hatch(hatch) => Some(hatch.paths[0].edges.iter().flat_map(|edge| match edge {
                    codec::entities::BoundaryEdge::Polyline(p) => p.vertices.iter().map(|v| [v.x, v.y]).collect(),
                    _ => Vec::new(),
                }).collect()),
                _ => None,
            })
            .expect("the fill is re-tessellated");
        assert!(fill_boundary.len() > 200);
        let fill_worst = fill_boundary
            .iter()
            .map(|&[x, y]| {
                let (x, y) = place(x, y);
                drawn.iter().map(|p| (p[0] - x).hypot(p[1] - y)).fold(f64::INFINITY, f64::min)
            })
            .fold(0.0, f64::max);
        assert!(fill_worst <= r_pt * std::f64::consts::SQRT_2, "the fill is {fill_worst} pt from its exact vertices");
        // Clipped to its circle on the sheet.
        let ops = publish::tests::operations(&pdf);
        let paths: Vec<usize> = ops
            .iter()
            .enumerate()
            .filter(|(_, op)| op.operator == "W")
            .map(|(w, _)| ops[..w].iter().rev().take_while(|op| matches!(op.operator.as_str(), "m" | "l" | "h")).filter(|op| op.operator == "l").count())
            .collect();
        assert!(paths.iter().any(|&edges| edges > 32), "no circular clip: {paths:?}");
        let snapped: Vec<[f64; 2]> = geometry.points.iter().map(|p| [p[0] as f64, p[1] as f64]).collect();
        assert!(!near(&snapped, (cx, cy), radius * 1.5), "the detail gives snaps");
    }

    #[test]
    fn fill_vertices_far_from_the_fills_centre_stay_exact() {
        use codec::entities::{BoundaryEdge, BoundaryPath, CircularArcEdge, Hatch, PolylineEdge};
        use codec::types::Vector2;
        let (mut scene, _) = layout_scene(vec![plan_viewport((420.0, 300.0), 0.0)]);
        // A slab over the whole plan with a round hole (radius 100) at
        // (5000, 5000): the hole is ~10 m from the fill's centre, where an
        // f32 offset is ½ ulp ≈ 0.0005 units.
        let mut outer = BoundaryPath::new();
        let corners = [[0.0, 0.0], [30000.0, 0.0], [30000.0, 18000.0], [0.0, 18000.0]].map(|[x, y]| Vector2::new(x, y));
        outer.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(corners.to_vec(), true)));
        let mut hole = BoundaryPath::new();
        hole.add_edge(BoundaryEdge::CircularArc(CircularArcEdge {
            center: Vector2::new(5000.0, 5000.0),
            radius: 100.0,
            start_angle: 0.0,
            end_angle: std::f64::consts::TAU,
            counter_clockwise: true,
        }));
        let mut slab = Hatch::new();
        slab.is_solid = true;
        slab.paths = vec![outer, hole];
        scene.add_entity(EntityType::Hatch(slab));
        // A 1:1 detail onto the hole's edge.
        detail_at(&mut scene, (5100.0, 5000.0), 200.0);
        let reference = found(&scene).unwrap();
        assert_eq!(reference.others[0].model_per_paper, 1.0);
        let mapping = mm_mapping();
        let (pdf, publication, _) = publish_layout(&scene, &reference, &mapping);
        let (model, paper) = reference.transforms(&mapping).unwrap();
        let r_pt = model.placement.rounding_bound_mm / model.placement.mm_per_pt;
        let place = |x: f64, y: f64| {
            let [a, b, c, d, e, f] = reference.others[0].to_paper;
            paper.apply(a * x + b * y + e, c * x + d * y + f)
        };
        let hole: Vec<[f64; 2]> = publication
            .scene
            .document
            .entities()
            .find_map(|entity| match entity {
                EntityType::Hatch(hatch) => Some(hatch.paths[1].edges.iter().flat_map(|edge| match edge {
                    BoundaryEdge::Polyline(p) => p.vertices.iter().map(|v| [v.x, v.y]).collect(),
                    _ => Vec::new(),
                }).collect()),
                _ => None,
            })
            .expect("the hole is re-tessellated");
        let drawn = publish::tests::content_points(&pdf);
        let worst = hole
            .iter()
            .map(|&[x, y]| {
                let (x, y) = place(x, y);
                drawn.iter().map(|p| (p[0] - x).hypot(p[1] - y)).fold(f64::INFINITY, f64::min)
            })
            .fold(0.0, f64::max);
        assert!(worst <= r_pt * std::f64::consts::SQRT_2, "the hole is drawn {worst} pt from its exact vertices (r = {r_pt} pt)");
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
            let bytes = crate::io::save_to_bytes(&scene.document, format, codec::DxfVersion::AC1032).unwrap();
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
