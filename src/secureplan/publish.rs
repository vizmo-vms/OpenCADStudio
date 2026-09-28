//! CON-01 page placement and the published vector PDF.
//!
//! A model-space window is mapped CAD → world mm (the stored mapping) → page
//! points, exactly as `secureplan-vectors/placement.json` defines:
//!
//! - CAD → world: `d = ((x − ox)·s, −(y − oy)·s)`, `world = anchor + rot(q, d)`;
//! - the window's world box `[wx0, wy0, wx1, wy1]` gives
//!   `W = ceil((wx1 − wx0)/m − 1e-9)` and `H` likewise, for `m` mm per point;
//! - world → page: `x = (xw − wx0)/m`, `y = H − (yw − wy0)/m`.
//!
//! The PDF writer emits page coordinates computed this way in f64 and rounded
//! once to f32, so every drawn point lies within the float32 bound
//! `r = ½·ulp32(max(W, H))·m` of its CAD source in world space.
//!
//! A paper layout (PUB-03) publishes its whole sheet: its reference viewport's
//! model geometry goes through the same CAD → page transform, clipped to the
//! viewport, and the rest of the sheet through the paper → world mapping of
//! [`super::layout::LayoutReference`].

use super::snap;

pub const MAX_SIDE_PT: u32 = 14_400;
pub const MAX_AREA_PT2: u64 = 64_000_000;
/// Precision budget in world mm, and the floor for the chord tolerance.
const PRECISION_MM: f64 = 0.1;
const MIN_CHORD_TOLERANCE_MM: f64 = 0.01;

/// CAD model → world mm (the `cadPlan.mapping` of CON-02).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    pub cad_origin: [f64; 2],
    pub anchor_mm: [f64; 2],
    pub scale_mm_per_cad_unit: f64,
    pub quarter_turns: u8,
}

impl Mapping {
    pub fn cad_to_world(&self, [x, y]: [f64; 2]) -> [f64; 2] {
        let s = self.scale_mm_per_cad_unit;
        let (dx, dy) = ((x - self.cad_origin[0]) * s, -(y - self.cad_origin[1]) * s);
        let (rx, ry) = match self.quarter_turns % 4 {
            0 => (dx, dy),
            1 => (-dy, dx),
            2 => (-dx, -dy),
            _ => (dy, -dx),
        };
        [self.anchor_mm[0] + rx, self.anchor_mm[1] + ry]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    InvalidWindow,
    InvalidScale,
    EmptyPage,
    SideTooLarge,
    AreaTooLarge,
}

/// Where the published page sits in the survey world (CON-01).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub width_pt: u32,
    pub height_pt: u32,
    pub mm_per_pt: f64,
    /// The world position of the page's top-left corner.
    pub page_world_min: [f64; 2],
    pub center_x: f64,
    pub center_y: f64,
    pub width_mm: f64,
    pub height_mm: f64,
    /// `r`: the float32 rounding bound in world mm.
    pub rounding_bound_mm: f64,
    /// `max(0.1 mm − r, 0.01 mm)`.
    pub chord_tolerance_mm: f64,
}

/// Half the spacing of float32 values around `value`'s binade, times nothing:
/// `ulp32(v) = 2^(floor(log2 v) − 23)` for normal positive `v`.
pub fn ulp32(value: f64) -> f64 {
    let bits = (value as f32).to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127;
    2f64.powi(exponent - 23)
}

/// Place the page for a model-space window at `mm_per_pt`.
pub fn place_page(window_cad: [f64; 4], mapping: &Mapping, mm_per_pt: f64) -> Result<Placement, PlacementError> {
    let [x0, y0, x1, y1] = window_cad;
    if !(window_cad.iter().all(|v| v.is_finite()) && x1 > x0 && y1 > y0) {
        return Err(PlacementError::InvalidWindow);
    }
    if !(mm_per_pt.is_finite() && mm_per_pt > 0.0) {
        return Err(PlacementError::InvalidScale);
    }
    let corners = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]].map(|corner| mapping.cad_to_world(corner));
    let fold = |f: fn(f64, f64) -> f64, axis: usize| corners.iter().map(|c| c[axis]).fold(corners[0][axis], f);
    let (wx0, wy0, wx1, wy1) = (fold(f64::min, 0), fold(f64::min, 1), fold(f64::max, 0), fold(f64::max, 1));
    let width = ((wx1 - wx0) / mm_per_pt - 1e-9).ceil();
    let height = ((wy1 - wy0) / mm_per_pt - 1e-9).ceil();
    if !(width.is_finite() && height.is_finite()) || width < 1.0 || height < 1.0 {
        return Err(PlacementError::EmptyPage);
    }
    if width > MAX_SIDE_PT as f64 || height > MAX_SIDE_PT as f64 {
        return Err(PlacementError::SideTooLarge);
    }
    let (width_pt, height_pt) = (width as u32, height as u32);
    if width_pt as u64 * height_pt as u64 > MAX_AREA_PT2 {
        return Err(PlacementError::AreaTooLarge);
    }
    let width_mm = width_pt as f64 * mm_per_pt;
    let height_mm = height_pt as f64 * mm_per_pt;
    let rounding_bound_mm = 0.5 * ulp32(width_pt.max(height_pt) as f64) * mm_per_pt;
    Ok(Placement {
        width_pt,
        height_pt,
        mm_per_pt,
        page_world_min: [wx0, wy0],
        center_x: wx0 + width_mm / 2.0,
        center_y: wy0 + height_mm / 2.0,
        width_mm,
        height_mm,
        rounding_bound_mm,
        chord_tolerance_mm: (PRECISION_MM - rounding_bound_mm).max(MIN_CHORD_TOLERANCE_MM),
    })
}

impl Placement {
    pub fn world_to_page(&self, [x, y]: [f64; 2]) -> [f64; 2] {
        [
            (x - self.page_world_min[0]) / self.mm_per_pt,
            self.height_pt as f64 - (y - self.page_world_min[1]) / self.mm_per_pt,
        ]
    }

    /// On a survey with content the page must start inside the canvas.
    pub fn starts_inside_canvas(&self) -> bool {
        self.page_world_min[0] >= 0.0 && self.page_world_min[1] >= 0.0
    }
}

/// CAD coordinates → page points for one published page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageTransform {
    pub mapping: Mapping,
    pub placement: Placement,
}

impl PageTransform {
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let [px, py] = self.placement.world_to_page(self.mapping.cad_to_world([x, y]));
        (px, py)
    }

    /// Page points per CAD unit (the mapping is a similarity).
    pub fn points_per_cad_unit(&self) -> f64 {
        self.mapping.scale_mm_per_cad_unit / self.placement.mm_per_pt
    }
}

/// A layer's drawing coordinates → page points: a page transform, after an
/// optional exact affine map into its coordinates (a paper layout's other
/// viewports: model → paper, `[a, b, c, d, e, f]` for
/// `(a·x + b·y + e, c·x + d·y + f)`), all in f64 and rounded once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerTransform {
    pub page: PageTransform,
    pub to_page_space: Option<[f64; 6]>,
}

impl LayerTransform {
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let (x, y) = match self.to_page_space {
            Some([a, b, c, d, e, f]) => (a * x + b * y + e, c * x + d * y + f),
            None => (x, y),
        };
        self.page.apply(x, y)
    }

    /// Page points per drawing unit (the maps are similarities).
    pub fn points_per_cad_unit(&self) -> f64 {
        let stretch = self.to_page_space.map_or(1.0, |[a, b, c, d, _, _]| (a * d - b * c).abs().sqrt());
        self.page.points_per_cad_unit() * stretch
    }
}

impl From<PageTransform> for LayerTransform {
    fn from(page: PageTransform) -> Self {
        Self { page, to_page_space: None }
    }
}

/// The published view, ready to write: a copy of the drawing in which every
/// drawn curve (circle, arc, ellipse, spline, bulged or tilted polyline) is
/// replaced by a polyline within the placement's chord tolerance, built from
/// the renderer's own curve definitions (OCS normals included). The PDF and
/// the SPSNAP file are both derived from this copy, so they agree with each
/// other and never depend on the editor's zoom-level tessellation (CON-05).
pub struct Publication {
    /// The prepared drawing: in model space, or on the published layout.
    pub scene: crate::scene::Scene,
    /// CAD model coordinates → page points.
    pub transform: PageTransform,
    /// The snap end points of each replaced curve (arc and chain ends), in
    /// its block's coordinates, by entity handle.
    key_points: rustc_hash::FxHashMap<u64, Vec<[f64; 3]>>,
    /// Where on the page snaps are taken, `[x0, y0, x1, y1]` in points: the
    /// whole page, or a paper layout's reference viewport.
    pub clip: [f64; 4],
    /// A paper layout: its sheet and the viewports drawn on it.
    pub layout: Option<LayoutPublication>,
}

/// A published paper layout: the sheet in paper coordinates, and each
/// viewport's model content in the layout's draw order, placed exactly and
/// clipped to the viewport. Only the reference viewport gives snaps.
pub struct LayoutPublication {
    /// Paper coordinates → page points.
    pub paper: PageTransform,
    /// Model space as the reference viewport shows it.
    pub reference: ViewContext,
    pub views: Vec<ViewLayer>,
}

/// One viewport's model content on the sheet.
pub struct ViewLayer {
    pub viewport: acadrust::types::Handle,
    /// Model units per paper unit.
    pub model_per_paper: f64,
    pub transform: LayerTransform,
    /// Page-space polygons the content is clipped to (all of them).
    pub clips: Vec<Vec<[f64; 2]>>,
}

/// Model space as a paper viewport shows it, as native paper rendering
/// draws it: the viewport's frozen layers and annotation scale.
#[derive(Debug, Clone)]
pub struct ViewContext {
    /// The block drawn: model space, or a layout's paper space.
    pub model_block: acadrust::types::Handle,
    pub frozen: rustc_hash::FxHashSet<acadrust::types::Handle>,
    pub annotation_scale: Option<acadrust::types::Handle>,
    pub annotation_multiplier: f32,
    /// Annotative objects of other scales are shown (the layout's setting).
    pub all_visible: bool,
}

impl ViewContext {
    /// Model space through `viewport` on a layout whose "show all
    /// annotative objects" setting is `all_visible`.
    pub fn of(scene: &crate::scene::Scene, viewport: acadrust::types::Handle, all_visible: bool) -> Self {
        let (model_block, frozen, annotation_scale, annotation_multiplier) = scene.secureplan_viewport_context(viewport);
        Self { model_block, frozen, annotation_scale, annotation_multiplier, all_visible }
    }

    /// A layout's paper space, as native paper rendering draws it: at 1:1.
    pub fn paper(scene: &crate::scene::Scene, block: acadrust::types::Handle, all_visible: bool) -> Self {
        let annotation_scale = scene.paper_annotation_scale_handle();
        Self { model_block: block, frozen: Default::default(), annotation_scale, annotation_multiplier: 1.0, all_visible }
    }
}

/// Walk the model-space entities drawn in `scene` — through block references
/// and array instances, with the renderer's own transforms and visibility
/// (off, frozen, invisible) — calling `leaf` for each drawn entity of a block
/// reference or of model space itself. Content that belongs to dimensions,
/// tables and leaders is skipped. `plot_only` also skips non-plotting layers.
pub(crate) fn walk_model<F>(scene: &crate::scene::Scene, plot_only: bool, leaf: F)
where
    F: FnMut(&acadrust::EntityType, &crate::scene::render_graph::InstanceContext),
{
    walk_block(scene, scene.current_layout_block_handle_pub(), None, plot_only, leaf);
}

/// [`walk_model`] for the entities of `block` (model or a paper space), in
/// the drawing's own annotation context or, for model space seen through a
/// paper viewport, in that viewport's (`view`).
pub(crate) fn walk_block<F>(scene: &crate::scene::Scene, block: acadrust::types::Handle, view: Option<&ViewContext>, plot_only: bool, leaf: F)
where
    F: FnMut(&acadrust::EntityType, &crate::scene::render_graph::InstanceContext),
{
    walk_block_where(scene, block, view, |entity, context| !plot_only || scene.layer_plottable_in_context(entity, context), leaf);
}

/// [`walk_block`] that descends only into what `keep` accepts (a block
/// reference it refuses is not walked at all).
pub(crate) fn walk_block_where<K, F>(scene: &crate::scene::Scene, block: acadrust::types::Handle, view: Option<&ViewContext>, keep: K, mut leaf: F)
where
    K: FnMut(&acadrust::EntityType, &crate::scene::render_graph::InstanceContext) -> bool,
    F: FnMut(&acadrust::EntityType, &crate::scene::render_graph::InstanceContext),
{
    use crate::scene::render_graph::{BlockRoot, BlockRootRole, RenderSceneGraph, SceneRoot};
    let document = &scene.document;
    let depths = scene.draw_depth_map();
    // The annotation context the published view is drawn in, so the walk
    // sees exactly what is drawn (including other scales' annotative
    // objects when the space shows them all).
    let (frozen, annotation, multiplier, all_visible) = match view {
        Some(view) => (Some(&view.frozen), view.annotation_scale, view.annotation_multiplier, view.all_visible),
        None => (
            None,
            crate::scene::annotative::scale_handle_by_name(document, &document.header.current_annotation_scale),
            scene.annotation_scale,
            scene.annotation_all_visible(),
        ),
    };
    let graph = RenderSceneGraph::new(document, frozen, annotation, all_visible, depths.as_ref()).with_annotation_scale(multiplier);
    let root = SceneRoot::Block(BlockRoot { record: block, role: BlockRootRole::ModelSpace });
    graph.walk_root(
        root,
        keep,
        |entity, context| {
            let owned_content = !context.root_handle.is_null()
                && !matches!(document.get_entity(context.root_handle), Some(acadrust::EntityType::Insert(_)));
            if !owned_content {
                leaf(entity, context);
            }
        },
    );
}

/// The largest factor by which `transform` can stretch a length on its way
/// to the plan: the largest singular value of its linear part followed by the
/// projection to world XY. Tessellating with `tolerance / plan_scale` keeps
/// every instance of a curve within `tolerance` in the plan, whatever the
/// nesting, rotation, non-uniform scale or tilt of its block references.
pub(crate) fn plan_scale(transform: &acadrust::types::Transform) -> f64 {
    use acadrust::types::Vector3;
    let columns = [Vector3::new(1.0, 0.0, 0.0), Vector3::new(0.0, 1.0, 0.0), Vector3::new(0.0, 0.0, 1.0)]
        .map(|axis| transform.apply_rotation(axis));
    // A·Aᵀ for the 2×3 plan matrix A whose columns are the images of the axes.
    let (mut a, mut b, mut d) = (0.0, 0.0, 0.0);
    for column in columns {
        a += column.x * column.x;
        b += column.x * column.y;
        d += column.y * column.y;
    }
    let largest = (a + d) / 2.0 + (((a - d) / 2.0).powi(2) + b * b).sqrt();
    largest.sqrt().max(f64::MIN_POSITIVE)
}

/// Whether the drawn geometry of `entity` is curved and needs replacing by a
/// polyline within the chord tolerance.
fn needs_flattening(entity: &acadrust::EntityType) -> bool {
    use acadrust::EntityType;
    match entity {
        EntityType::Circle(_) | EntityType::Arc(_) | EntityType::Ellipse(_) | EntityType::Spline(_) | EntityType::Polyline2D(_) => true,
        EntityType::LwPolyline(polyline) => polyline.vertices.iter().any(|vertex| vertex.bulge.abs() > 1e-12),
        EntityType::Hatch(hatch) => hatch.paths.iter().flat_map(|path| &path.edges).any(curved_edge),
        _ => false,
    }
}

/// Whether a hatch boundary edge is curved (an arc, ellipse, spline or
/// bulged polyline): the fill renderer cuts those at a fixed angle.
fn curved_edge(edge: &acadrust::entities::BoundaryEdge) -> bool {
    use acadrust::entities::BoundaryEdge;
    match edge {
        BoundaryEdge::Line(_) => false,
        BoundaryEdge::Polyline(polyline) => polyline.vertices.iter().any(|vertex| vertex.z.abs() > 1e-12),
        _ => true,
    }
}

/// `hatch` with every curved boundary edge replaced by a straight polyline
/// edge within `tolerance` (in its own plane), so the fill is drawn within
/// the publication's precision from exact vertices.
fn flatten_hatch(hatch: &acadrust::entities::Hatch, tolerance: f64) -> Result<acadrust::entities::Hatch, String> {
    use acadrust::entities::{BoundaryEdge, PolylineEdge};
    use acadrust::types::Vector2;
    let mut flat = hatch.clone();
    for edge in flat.paths.iter_mut().flat_map(|path| path.edges.iter_mut()) {
        if !curved_edge(edge) {
            continue;
        }
        let Some(curve) = crate::entities::hatch::edge_curve(edge) else { continue };
        let points = bounded_points(&curve, tolerance, MAX_CURVE_SEGMENTS)?;
        if points.len() < 2 {
            return Err(TOO_LARGE.into());
        }
        *edge = BoundaryEdge::Polyline(PolylineEdge::new(points.into_iter().map(|[x, y]| Vector2::new(x, y)).collect(), false));
    }
    Ok(flat)
}

/// Give each fill the exact f64 positions of its boundary vertices
/// (`boundary_wcs`, aligned with `boundary`), recovered from its boundary
/// paths: the fill model keeps them only as f32 offsets. A vertex that is
/// not a boundary-path vertex (cut by a block clip) keeps its f32 offset.
fn exact_fill_boundaries(fills: &mut [crate::scene::model::hatch_model::HatchModel]) {
    use acadrust::entities::BoundaryEdge;
    for fill in fills {
        let (Some(paths), Some(plane)) = (&fill.boundary_paths, &fill.fill_plane) else { continue };
        let origin = fill.world_origin;
        let mut exact: rustc_hash::FxHashMap<(u32, u32), [f64; 2]> = Default::default();
        for path in paths.iter().filter(|path| path.flags.bits() & 8 == 0) {
            for edge in &path.edges {
                let points: Vec<[f64; 2]> = match edge {
                    BoundaryEdge::Line(line) => vec![[line.start.x, line.start.y], [line.end.x, line.end.y]],
                    BoundaryEdge::Polyline(polyline) => polyline.vertices.iter().map(|v| [v.x, v.y]).collect(),
                    _ => Vec::new(),
                };
                for [x, y] in points {
                    let at = [0, 1].map(|i| plane.origin[i] + x * plane.x_axis[i] + y * plane.y_axis[i]);
                    exact.insert((((at[0] - origin[0]) as f32).to_bits(), ((at[1] - origin[1]) as f32).to_bits()), at);
                }
            }
        }
        if exact.is_empty() {
            continue;
        }
        let boundary = fill
            .boundary
            .iter()
            .map(|&[x, y]| {
                if x.is_nan() || y.is_nan() {
                    [f64::NAN, f64::NAN]
                } else {
                    exact.get(&(x.to_bits(), y.to_bits())).copied().unwrap_or([origin[0] + x as f64, origin[1] + y as f64])
                }
            })
            .collect();
        fill.boundary_wcs = Some(std::sync::Arc::new(boundary));
    }
}

/// Points along `curve`, first to last, whose chords depart from it by at
/// most `tolerance` anywhere along their span, not only at their middles:
///
/// - an elliptic arc is cut in equal parameter steps `Δ ≤ √(8·tol/a)`: its
///   second derivative is at most the major radius `a`, and a chord of a
///   curve departs from it by at most `Δ²/8·max|C''|`;
/// - a NURBS curve is split into its Bézier pieces by knot insertion, and each
///   piece is halved (de Casteljau) until every control point lies within the
///   tolerance of the chord between its ends. A piece lies in the convex hull
///   of its control points (positive weights), so its distance from that
///   chord is at most theirs;
/// - circles, circular arcs and polylines of lines and bulges (hatch
///   boundary edges): cut analytically by [`arc_points`] and
///   [`tessellate_chain`], with no cap short of [`MAX_CURVE_SEGMENTS`]. The
///   kernel's own cut stops at 16,384 chords, runs a clockwise bulge
///   backwards between its vertices and reads a bulge sweeping under 1e-9 rad
///   as a full circle;
/// - anything else (straight): the kernel's cut, checked at each chord's
///   middle.
///
/// A curve that needs more than [`MAX_CURVE_SEGMENTS`] chords is refused.
pub(crate) fn bounded_points(curve: &cadkernel::geom2d::Curve, tolerance: f64, limit: usize) -> Result<Vec<[f64; 2]>, String> {
    use cadkernel::geom2d::Curve;
    if !(tolerance.is_finite() && tolerance > 0.0) {
        return Err(TOO_LARGE.into());
    }
    match curve {
        Curve::Ellipse(arc) => {
            let a = arc.ellipse.major_radius.abs().max(arc.ellipse.minor_radius.abs());
            let sweep = arc.sweep();
            let count = if a <= 0.0 { 1.0 } else { (sweep / (8.0 * tolerance / a).sqrt()).ceil().max(1.0) };
            if !count.is_finite() || count > MAX_CURVE_SEGMENTS as f64 {
                return Err(TOO_LARGE.into());
            }
            if count >= limit as f64 {
                return Err(OVER_LIMIT.into());
            }
            let count = count as usize;
            Ok((0..=count).map(|i| arc.ellipse.point_at(arc.start_parameter + sweep * i as f64 / count as f64)).collect())
        }
        Curve::Nurbs(nurbs) => nurbs_points(nurbs, tolerance, limit),
        Curve::Circle(circle) => Ok(arc_points((circle.centre[0], circle.centre[1]), circle.radius, 0.0, std::f64::consts::TAU, tolerance, limit)?
            .into_iter()
            .map(|(x, y)| [x, y])
            .collect()),
        Curve::Arc(arc) => Ok(arc_points((arc.centre[0], arc.centre[1]), arc.radius, arc.start_angle, arc.sweep(), tolerance, limit)?
            .into_iter()
            .map(|(x, y)| [x, y])
            .collect()),
        Curve::Polyline(polyline) => {
            let chain: Vec<WideVertex> = polyline
                .vertices
                .iter()
                .map(|v| WideVertex { at: (v.position[0], v.position[1]), bulge: v.bulge, start_width: 0.0, end_width: 0.0 })
                .collect();
            Ok(tessellate_chain(&chain, polyline.closed, tolerance, limit)?.into_iter().map(|v| [v.location.x, v.location.y]).collect())
        }
        _ => {
            let points = curve.tessellate_within(tolerance);
            if points.len() > limit {
                return Err(OVER_LIMIT.into());
            }
            let met = points.windows(2).all(|pair| {
                let middle = [(pair[0][0] + pair[1][0]) / 2.0, (pair[0][1] + pair[1][1]) / 2.0];
                let on = curve.point_at(curve.parameter_at(middle));
                (on[0] - middle[0]).hypot(on[1] - middle[1]) <= tolerance * (1.0 + 1e-6)
            });
            if met { Ok(points) } else { Err(TOO_LARGE.into()) }
        }
    }
}

/// Degrees past this are refused rather than cut (drawings use 1 to 3,
/// rarely more; the work per span grows with the cube of the degree).
const MAX_NURBS_DEGREE: usize = 32;

/// [`bounded_points`] for a NURBS curve, in homogeneous coordinates, with at
/// most `limit` points. Each non-empty knot span of the domain is turned
/// into its Bézier piece by blossoming: control point `j` of the span
/// `[a, b]` is the blossom `f(a, …, a, b, …, b)` with `j` arguments `b`,
/// evaluated by de Boor's recurrence on the span's own `p + 1` controls. That
/// works for clamped, unclamped and periodic knot vectors alike (its
/// denominators are at least `b − a > 0`), needs no knot insertion and no
/// multiplicity counts, and costs O(p³) per span.
fn nurbs_points(nurbs: &cadkernel::geom2d::NurbsCurve, tolerance: f64, limit: usize) -> Result<Vec<[f64; 2]>, String> {
    let p = nurbs.degree();
    let knots = nurbs.knots();
    let control: Vec<[f64; 3]> = nurbs.control_points().iter().zip(nurbs.weights()).map(|(c, w)| [c[0] * w, c[1] * w, *w]).collect();
    let n = control.len();
    if p == 0 || p > MAX_NURBS_DEGREE || n <= p || knots.len() != n + p + 1 {
        return Err(UNSUPPORTED_SPLINE.into());
    }
    let finite = knots.iter().all(|k| k.is_finite()) && control.iter().flatten().all(|v| v.is_finite());
    if !finite || knots.windows(2).any(|w| w[0] > w[1]) || control.iter().any(|h| h[2] <= 0.0) || knots[p] >= knots[n] {
        return Err(UNSUPPORTED_SPLINE.into());
    }
    let euclid = |h: [f64; 3]| [h[0] / h[2], h[1] / h[2]];
    // The blossom of span k (U[k] < U[k+1]) at the arguments `t`.
    let blossom = |k: usize, t: &[f64]| -> [f64; 3] {
        let mut d: Vec<[f64; 3]> = control[k - p..=k].to_vec();
        for (r, &tr) in t.iter().enumerate().map(|(r, t)| (r + 1, t)) {
            for i in (r..=p).rev() {
                let g = k - p + i;
                let alpha = (tr - knots[g]) / (knots[g + p + 1 - r] - knots[g]);
                d[i] = [0, 1, 2].map(|c| (1.0 - alpha) * d[i - 1][c] + alpha * d[i][c]);
            }
        }
        d[p]
    };
    let mut out: Vec<[f64; 2]> = Vec::new();
    for k in p..n {
        let (a, b) = (knots[k], knots[k + 1]);
        if a >= b {
            continue;
        }
        let piece: Vec<[f64; 3]> = (0..=p)
            .map(|j| {
                let t: Vec<f64> = std::iter::repeat_n(a, p - j).chain(std::iter::repeat_n(b, j)).collect();
                blossom(k, &t)
            })
            .collect();
        if piece.iter().any(|h| h[2] <= 0.0 || !h.iter().all(|v| v.is_finite())) {
            return Err(UNSUPPORTED_SPLINE.into());
        }
        if out.is_empty() {
            out.push(euclid(piece[0]));
        }
        // Halve until flat; a stack keeps the pieces in order.
        let mut stack = vec![(piece, 0u32)];
        while let Some((piece, depth)) = stack.pop() {
            let (start, end) = (euclid(piece[0]), euclid(piece[p]));
            if piece.iter().all(|h| segment_distance(euclid(*h), start, end) <= tolerance) {
                out.push(end);
                if out.len() > limit {
                    return Err(if limit < MAX_CURVE_SEGMENTS { OVER_LIMIT.into() } else { TOO_LARGE.into() });
                }
                continue;
            }
            if depth >= 60 {
                return Err(TOO_LARGE.into());
            }
            // de Casteljau at the middle.
            let (mut left, mut right) = (Vec::with_capacity(p + 1), vec![[0.0; 3]; p + 1]);
            let mut row = piece;
            for level in 0..=p {
                left.push(row[0]);
                right[p - level] = row[row.len() - 1];
                row = row.windows(2).map(|w| [0, 1, 2].map(|j| (w[0][j] + w[1][j]) / 2.0)).collect();
            }
            stack.push((right, depth + 1));
            stack.push((left, depth + 1));
        }
    }
    if out.len() < 2 {
        return Err(UNSUPPORTED_SPLINE.into());
    }
    Ok(out)
}

/// The distance from `point` to the segment `a`–`b`.
fn segment_distance(point: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let length2 = dx * dx + dy * dy;
    let t = if length2 == 0.0 { 0.0 } else { (((point[0] - a[0]) * dx + (point[1] - a[1]) * dy) / length2).clamp(0.0, 1.0) };
    (point[0] - a[0] - t * dx).hypot(point[1] - a[1] - t * dy)
}

/// More segments than this for one curve means a curve far larger than any
/// plan (a circle thousands of kilometres across at 0.1 mm): refuse it.
const MAX_CURVE_SEGMENTS: usize = 2_000_000;

/// An arc of radius `radius` about `center` from `start` through `sweep`
/// radians, cut so no chord departs from it by more than `tolerance`, with
/// no upper limit short of [`MAX_CURVE_SEGMENTS`] (unlike the kernel).
fn arc_points(center: (f64, f64), radius: f64, start: f64, sweep: f64, tolerance: f64, limit: usize) -> Result<Vec<(f64, f64)>, String> {
    let step = if tolerance >= radius { std::f64::consts::FRAC_PI_2 } else { (2.0 * (1.0 - tolerance / radius).acos()).min(std::f64::consts::FRAC_PI_2) };
    let count = (sweep.abs() / step).ceil().max(1.0);
    if !count.is_finite() || count > MAX_CURVE_SEGMENTS as f64 {
        return Err(TOO_LARGE.into());
    }
    if count >= limit as f64 {
        return Err(OVER_LIMIT.into());
    }
    let count = count as usize;
    Ok((0..=count)
        .map(|i| {
            let angle = start + sweep * i as f64 / count as f64;
            (center.0 + radius * angle.cos(), center.1 + radius * angle.sin())
        })
        .collect())
}

/// The arc of sweep `theta` (bulge = tan(θ/4)) from `a` to `b`, cut so no
/// chord departs from it by more than `tolerance`, like [`arc_points`].
///
/// A nearly straight bulge has a radius of up to ~1e16 drawing units, so it
/// is measured and drawn from its chord, never from its far-away centre:
/// - its step `4·asin(√(tol / 2r))` equals `2·acos(1 − tol/r)` (a chord of
///   that sweep sags by exactly `tol`), without `1 − tol/r` rounding to 1;
///   an arc whose whole sagitta is within the tolerance is its chord alone;
/// - the point at sweep `φ` is `a + sin(φ/2)/sin(θ/2) · rot(b − a, (φ − θ)/2)`
///   (the chord from `a` through `φ` is `2r·sin(φ/2)` long and leaves the
///   tangent at `a`, turned `θ/2` from `b − a`, by `φ/2`), exact to the
///   rounding of the chord whatever the radius, and the ends are the stored
///   vertices.
fn bulge_points(a: (f64, f64), b: (f64, f64), theta: f64, tolerance: f64, limit: usize) -> Result<Vec<(f64, f64)>, String> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let half = (theta / 2.0).sin();
    let radius = dx.hypot(dy) / (2.0 * half.abs());
    let step = if tolerance >= radius { std::f64::consts::FRAC_PI_2 } else { (4.0 * (tolerance / (2.0 * radius)).sqrt().asin()).min(std::f64::consts::FRAC_PI_2) };
    let count = (theta.abs() / step).ceil().max(1.0);
    if !count.is_finite() || count > MAX_CURVE_SEGMENTS as f64 {
        return Err(TOO_LARGE.into());
    }
    if count >= limit as f64 {
        return Err(OVER_LIMIT.into());
    }
    let count = count as usize;
    Ok((0..=count)
        .map(|i| {
            if i == count {
                return b;
            }
            let phi = theta * i as f64 / count as f64;
            let (sin, cos) = ((phi - theta) / 2.0).sin_cos();
            let k = (phi / 2.0).sin() / half;
            (a.0 + k * (dx * cos - dy * sin), a.1 + k * (dx * sin + dy * cos))
        })
        .collect())
}

/// A curve needs more points than the caller's budget allows.
pub(crate) const OVER_LIMIT: &str = "The curve needs more points than this operation allows.";
const UNSUPPORTED_SPLINE: &str = "A spline in the published view has a knot structure SecurePlan CAD cannot cut within the publication's precision.";
/// A curve that would need more than [`MAX_CURVE_SEGMENTS`] chords. The user
/// sees [`too_large`] instead, which names the curve and where it is.
const TOO_LARGE: &str = "A curve is too large to draw within the publication's precision.";

/// The refusal for a curve too large to publish: its kind and a point on it
/// in drawing units, shown only in the local Apply dialog. Every curve in the
/// drawing is cut, inside the published window or not, so it has to be
/// corrected or erased in the drawing.
fn too_large(entity: &acadrust::EntityType, [x, y]: [f64; 2]) -> String {
    use acadrust::EntityType;
    let kind = match entity {
        EntityType::Circle(_) => "A circle",
        EntityType::Arc(_) => "An arc",
        EntityType::Ellipse(_) => "An ellipse",
        EntityType::Spline(_) => "A spline",
        EntityType::Hatch(_) => "A hatch boundary",
        _ => "A polyline",
    };
    format!("{kind} near ({x:.2}, {y:.2}) is too large to draw within the publication's precision. Find it in the drawing, then correct or erase it.")
}

/// A point on `entity`'s curve (or its hatch boundary), in its block's
/// coordinates, for [`too_large`].
fn curve_point(entity: &acadrust::EntityType) -> acadrust::types::Vector3 {
    use acadrust::entities::BoundaryEdge;
    use acadrust::types::Vector3;
    use acadrust::EntityType;
    let ocs = |normal: Vector3, (x, y): (f64, f64), elevation: f64| {
        let [x, y, z] = ocs_to_wcs(normal, (x, y), elevation);
        Vector3::new(x, y, z)
    };
    match entity {
        EntityType::Circle(c) => ocs(c.normal, (c.center.x + c.radius, c.center.y), c.center.z),
        EntityType::Arc(a) => ocs(a.normal, (a.center.x + a.radius * a.start_angle.cos(), a.center.y + a.radius * a.start_angle.sin()), a.center.z),
        EntityType::Ellipse(e) => e.center + e.major_axis,
        EntityType::Spline(s) => s.fit_points.first().or(s.control_points.first()).copied().unwrap_or(Vector3::ZERO),
        EntityType::LwPolyline(p) => p.vertices.first().map_or(Vector3::ZERO, |v| ocs(p.normal, (v.location.x, v.location.y), p.elevation)),
        EntityType::Polyline2D(p) => p.vertices.first().map_or(Vector3::ZERO, |v| ocs(p.normal, (v.location.x, v.location.y), p.elevation)),
        EntityType::Hatch(h) => {
            let start = h.paths.iter().flat_map(|path| &path.edges).find_map(|edge| match edge {
                BoundaryEdge::Line(l) => Some((l.start.x, l.start.y)),
                BoundaryEdge::CircularArc(a) => Some((a.center.x + a.radius * a.start_angle.cos(), a.center.y + a.radius * a.start_angle.sin())),
                BoundaryEdge::EllipticArc(e) => Some((e.center.x + e.major_axis_endpoint.x, e.center.y + e.major_axis_endpoint.y)),
                BoundaryEdge::Spline(s) => s.control_points.first().map(|p| (p.x, p.y)),
                BoundaryEdge::Polyline(p) => p.vertices.first().map(|v| (v.x, v.y)),
            });
            start.map_or(Vector3::ZERO, |at| ocs(h.normal, at, h.elevation))
        }
        _ => Vector3::ZERO,
    }
}

/// One polyline vertex in its OCS with the widths at the start and end of
/// its outgoing segment.
struct WideVertex {
    at: (f64, f64),
    bulge: f64,
    start_width: f64,
    end_width: f64,
}

/// Tessellate a (possibly bulged, possibly tapered) OCS polyline. Widths are
/// interpolated linearly along each segment, as they are drawn.
fn tessellate_chain(vertices: &[WideVertex], closed: bool, tolerance: f64, limit: usize) -> Result<Vec<acadrust::entities::LwVertex>, String> {
    if vertices.len() > limit {
        return Err(OVER_LIMIT.into());
    }
    use acadrust::entities::LwVertex;
    use acadrust::types::Vector2;
    let mut out: Vec<LwVertex> = Vec::new();
    let count = if closed { vertices.len() } else { vertices.len().saturating_sub(1) };
    let push = |out: &mut Vec<LwVertex>, x: f64, y: f64, start_width: f64, end_width: f64| {
        let mut vertex = LwVertex::new(Vector2::new(x, y));
        vertex.start_width = start_width;
        vertex.end_width = end_width;
        out.push(vertex);
    };
    for i in 0..count {
        let (from, to) = (&vertices[i], &vertices[(i + 1) % vertices.len()]);
        let (a, b) = (from.at, to.at);
        let chord = (b.0 - a.0).hypot(b.1 - a.1);
        let width_at = |t: f64| from.start_width + (from.end_width - from.start_width) * t;
        if from.bulge.abs() < 1e-12 || chord == 0.0 {
            push(&mut out, a.0, a.1, from.start_width, from.end_width);
            continue;
        }
        let points = bulge_points(a, b, 4.0 * from.bulge.atan(), tolerance, limit.saturating_sub(out.len()))?;
        let pieces = points.len() - 1;
        for (k, point) in points[..pieces].iter().enumerate() {
            push(&mut out, point.0, point.1, width_at(k as f64 / pieces as f64), width_at((k + 1) as f64 / pieces as f64));
        }
    }
    if let Some(last) = vertices.get(if closed { 0 } else { vertices.len().saturating_sub(1) }) {
        push(&mut out, last.at.0, last.at.1, 0.0, 0.0);
    }
    if out.len() > limit {
        return Err(OVER_LIMIT.into());
    }
    Ok(out)
}

/// OCS → WCS for a point with the given extrusion normal.
pub(crate) fn ocs_to_wcs(normal: acadrust::types::Vector3, (x, y): (f64, f64), elevation: f64) -> [f64; 3] {
    let (wx, wy, wz) = crate::scene::view::transform::ocs_point_to_wcs((x, y, elevation), (normal.x, normal.y, normal.z));
    [wx, wy, wz]
}

/// The replacement polyline for a curved entity, in the entity's own plane
/// (normal and elevation kept, so every enclosing transform still applies
/// to its true 3D position), and its snap end points in block coordinates.
/// A replacement polyline and its snap end points in block coordinates.
pub(crate) type Flattened = (acadrust::entities::LwPolyline, Vec<[f64; 3]>);

pub(crate) fn flatten(entity: &acadrust::EntityType, tolerance: f64) -> Result<Option<Flattened>, String> {
    flatten_within(entity, tolerance, MAX_CURVE_SEGMENTS)
}

/// [`flatten`] with at most `limit` points: a curve that needs more is
/// [`OVER_LIMIT`] (conversion's work budget) before it is cut any further.
pub(crate) fn flatten_within(entity: &acadrust::EntityType, tolerance: f64, limit: usize) -> Result<Option<Flattened>, String> {
    use crate::scene::model::wire_model::SnapHint;
    use acadrust::entities::{LwPolyline, LwVertex};
    use acadrust::types::{Vector2, Vector3};
    use acadrust::EntityType;
    let plain = |points: Vec<(f64, f64)>| points.into_iter().map(|(x, y)| LwVertex::new(Vector2::new(x, y))).collect::<Vec<_>>();
    let (vertices, normal, elevation, keys) = match entity {
        EntityType::Circle(circle) => {
            let points = arc_points((circle.center.x, circle.center.y), circle.radius, 0.0, std::f64::consts::TAU, tolerance, limit)?;
            (plain(points), circle.normal, circle.center.z, Vec::new())
        }
        EntityType::Arc(arc) => {
            let mut sweep = arc.end_angle - arc.start_angle;
            if sweep <= 0.0 {
                sweep += std::f64::consts::TAU;
            }
            let points = arc_points((arc.center.x, arc.center.y), arc.radius, arc.start_angle, sweep, tolerance, limit)?;
            let keys = [points[0], points[points.len() - 1]].map(|p| ocs_to_wcs(arc.normal, p, arc.center.z)).to_vec();
            (plain(points), arc.normal, arc.center.z, keys)
        }
        EntityType::LwPolyline(polyline) => {
            // Widths as the renderer reads them: a vertex's own when set,
            // otherwise the constant width.
            let chain: Vec<WideVertex> = polyline
                .vertices
                .iter()
                .map(|v| {
                    let (start_width, end_width) = if v.start_width > 1e-9 || v.end_width > 1e-9 {
                        (v.start_width, v.end_width)
                    } else {
                        (polyline.constant_width, polyline.constant_width)
                    };
                    WideVertex { at: (v.location.x, v.location.y), bulge: v.bulge, start_width, end_width }
                })
                .collect();
            let keys = chain.iter().map(|v| ocs_to_wcs(polyline.normal, v.at, polyline.elevation)).collect();
            (tessellate_chain(&chain, polyline.is_closed, tolerance, limit)?, polyline.normal, polyline.elevation, keys)
        }
        EntityType::Polyline2D(polyline) => {
            // The vertices it draws (fit points, not the spline frame), with
            // their own widths when set, otherwise the polyline's defaults.
            let drawn = crate::entities::polyline::drawn_vertices2d(polyline);
            let chain: Vec<WideVertex> = drawn
                .as_deref()
                .unwrap_or(&polyline.vertices)
                .iter()
                .map(|v| {
                    let (start_width, end_width) = if v.start_width > 1e-9 || v.end_width > 1e-9 {
                        (v.start_width, v.end_width)
                    } else {
                        (polyline.start_width, polyline.end_width)
                    };
                    WideVertex { at: (v.location.x, v.location.y), bulge: v.bulge, start_width, end_width }
                })
                .collect();
            let keys = chain.iter().map(|v| ocs_to_wcs(polyline.normal, v.at, polyline.elevation)).collect();
            (tessellate_chain(&chain, polyline.flags.is_closed(), tolerance, limit)?, polyline.normal, polyline.elevation, keys)
        }
        EntityType::Ellipse(_) | EntityType::Spline(_) => {
            // Planar curves only; a curve through space stays as the renderer draws it.
            let Some(curve) = crate::entities::curve::entity_curve(entity) else { return Ok(None) };
            // Cut with a bound that holds along each whole chord, in the
            // curve's plane (an isometry of space, so the bound carries over).
            let points: Vec<[f64; 3]> = bounded_points(&curve.curve, tolerance, limit)?.into_iter().map(|uv| curve.plane.point_at(uv)).collect();
            if points.len() < 2 {
                return Ok(None);
            }
            let [ux, uy, uz] = curve.plane.x_axis;
            let [vx, vy, vz] = curve.plane.y_axis;
            let normal = Vector3::new(uy * vz - uz * vy, uz * vx - ux * vz, ux * vy - uy * vx);
            let length = (normal.x * normal.x + normal.y * normal.y + normal.z * normal.z).sqrt();
            if length == 0.0 {
                return Ok(None);
            }
            let normal = Vector3::new(normal.x / length, normal.y / length, normal.z / length);
            let (ax, ay) = crate::scene::view::transform::ocs_axes((normal.x, normal.y, normal.z));
            let dot = |p: &[f64; 3], a: (f64, f64, f64)| p[0] * a.0 + p[1] * a.1 + p[2] * a.2;
            let elevation = dot(&points[0], (normal.x, normal.y, normal.z));
            let ocs = points.iter().map(|p| (dot(p, ax), dot(p, ay))).collect();
            let snap = crate::entities::curve::snap_from(&curve);
            let keys = snap.snap_pts.iter().filter(|(_, hint)| matches!(hint, SnapHint::Endpoint)).map(|(p, _)| p.to_array()).collect();
            (plain(ocs), normal, elevation, keys)
        }
        _ => return Ok(None),
    };
    if vertices.len() < 2 {
        return Ok(None);
    }
    let mut polyline = LwPolyline::new();
    polyline.vertices = vertices;
    polyline.common = entity.common().clone();
    polyline.normal = normal;
    polyline.elevation = elevation;
    // Linetypes run along the whole curve, as they did on the original.
    polyline.plinegen = true;
    Ok(Some((polyline, keys)))
}

/// Record, for every curve `block` draws, the finest local tolerance any of
/// its instances needs: `tolerance` (in the block's space units) divided by
/// the instance's largest stretch to the plan, and where that instance is
/// drawn (a point on it in the space's drawing units, for a refusal).
fn curve_tolerances(
    scene: &crate::scene::Scene,
    block: acadrust::types::Handle,
    view: Option<&ViewContext>,
    tolerance: f64,
    out: &mut rustc_hash::FxHashMap<u64, (f64, [f64; 2])>,
) {
    walk_block(scene, block, view, false, |entity, context| {
        if needs_flattening(entity) {
            let local = tolerance / plan_scale(&context.transform);
            let at = || {
                let at = context.transform.apply(curve_point(entity));
                [at.x, at.y]
            };
            let slot = out.entry(entity.common().handle.value()).or_insert_with(|| (local, at()));
            if local < slot.0 {
                *slot = (local, at());
            }
        }
    });
}

/// A copy of `source`'s drawing with each listed curve replaced by its
/// polyline within its tolerance, and the replaced curves' snap end points.
type FlatDocument = (acadrust::CadDocument, rustc_hash::FxHashMap<u64, Vec<[f64; 3]>>);

fn flatten_curves(source: &crate::scene::Scene, tolerances: rustc_hash::FxHashMap<u64, (f64, [f64; 2])>) -> Result<FlatDocument, String> {
    use acadrust::EntityType;
    let mut document = source.document.clone();
    let mut key_points = rustc_hash::FxHashMap::default();
    let mut handles: Vec<(u64, (f64, [f64; 2]))> = tolerances.into_iter().collect();
    handles.sort_by_key(|(handle, _)| *handle);
    for (handle, (tolerance, at)) in handles {
        let handle = acadrust::types::Handle::new(handle);
        let Some(entity) = document.get_entity(handle) else { continue };
        let located = |error: String| if error == TOO_LARGE { too_large(entity, at) } else { error };
        if let EntityType::Hatch(hatch) = entity {
            let flat = flatten_hatch(hatch, tolerance).map_err(located)?;
            document.replace_entity_arc(handle, std::sync::Arc::new(EntityType::Hatch(flat)));
            continue;
        }
        let Some((polyline, keys)) = flatten(entity, tolerance).map_err(located)? else { continue };
        key_points.insert(handle.value(), keys);
        document.replace_entity_arc(handle, std::sync::Arc::new(EntityType::LwPolyline(polyline)));
    }
    Ok((document, key_points))
}

fn scene_of(document: acadrust::CadDocument, annotation_scale: f32, layout: Option<&str>) -> crate::scene::Scene {
    let mut scene = crate::scene::Scene::new();
    scene.document = document;
    scene.annotation_scale = annotation_scale;
    if let Some(layout) = layout {
        scene.current_layout = layout.to_string();
    }
    scene.rebuild_derived_caches();
    if layout.is_some() {
        // The layout's own settings (PSLTSCALE among them), as when it plots.
        scene.load_current_layout_state();
    }
    scene
}

/// Prepare the model-space view of `source` for publication.
pub fn prepare_model(source: &crate::scene::Scene, transform: PageTransform) -> Result<Publication, String> {
    if source.current_layout != "Model" {
        return Err("The published view must be model space.".into());
    }
    let tolerance_cad = transform.placement.chord_tolerance_mm / transform.mapping.scale_mm_per_cad_unit;
    let mut tolerances = rustc_hash::FxHashMap::default();
    curve_tolerances(source, source.current_layout_block_handle_pub(), None, tolerance_cad, &mut tolerances);
    let (document, key_points) = flatten_curves(source, tolerances)?;
    let scene = scene_of(document, source.annotation_scale, None);
    let clip = [0.0, 0.0, transform.placement.width_pt as f64, transform.placement.height_pt as f64];
    Ok(Publication { scene, transform, key_points, clip, layout: None })
}

/// Prepare a paper layout of `source` (a model-space scene) for publication
/// through its reference viewport. `transform` is the model transform
/// (CAD → page) of [`super::layout::LayoutReference::transforms`].
pub fn prepare_layout(source: &crate::scene::Scene, reference: &super::layout::LayoutReference, transform: PageTransform) -> Result<Publication, String> {
    if source.current_layout != "Model" {
        return Err("The published layout must be read from model space.".into());
    }
    let paper = PageTransform { mapping: reference.paper_mapping(&transform.mapping), placement: transform.placement };
    // Every curve within the chord tolerance in the world, per instance as
    // it is drawn: through the reference (CAD → world), through each other
    // viewport (model → paper → world) and on the sheet (paper → world), in
    // each viewport's own annotation context.
    let chord = transform.placement.chord_tolerance_mm;
    let world_per_paper = paper.mapping.scale_mm_per_cad_unit;
    let all_visible = reference.annotation_all_visible;
    let context = ViewContext::of(source, reference.viewport, all_visible);
    let mut tolerances = rustc_hash::FxHashMap::default();
    curve_tolerances(source, context.model_block, Some(&context), chord / transform.mapping.scale_mm_per_cad_unit, &mut tolerances);
    for other in &reference.others {
        let view = ViewContext::of(source, other.viewport, all_visible);
        curve_tolerances(source, view.model_block, Some(&view), chord * other.model_per_paper / world_per_paper, &mut tolerances);
    }
    let paper_context = ViewContext::paper(source, reference.paper_block, all_visible);
    curve_tolerances(source, reference.paper_block, Some(&paper_context), chord / world_per_paper, &mut tolerances);
    let (document, key_points) = flatten_curves(source, tolerances)?;

    // Each viewport's outline on the page: its rectangle and any boundary
    // it is clipped to, cut within the tolerance and kept in f64.
    let outline = |rect: [f64; 4], boundary: Option<acadrust::types::Handle>| -> Result<Vec<Vec<[f64; 2]>>, String> {
        let [x0, y0, x1, y1] = rect;
        let mut clips = vec![[[x0, y0], [x1, y0], [x1, y1], [x0, y1]].iter().map(|p| page_point(&paper, *p)).collect()];
        if let Some(handle) = boundary {
            let entity = document.get_entity(handle).ok_or("A viewport's clipping boundary is missing.")?;
            let polygon = boundary_polygon(entity, chord / world_per_paper).ok_or("A viewport is clipped by a boundary SecurePlan cannot read.")?;
            clips.push(polygon.into_iter().map(|p| page_point(&paper, p)).collect());
        }
        Ok(clips)
    };
    let mut views = Vec::new();
    for other in &reference.others {
        let transform = LayerTransform { page: paper, to_page_space: Some(other.to_paper) };
        views.push(ViewLayer { viewport: other.viewport, model_per_paper: other.model_per_paper, transform, clips: outline(other.rect, other.boundary)? });
    }
    let position = reference.reference_position.min(views.len());
    views.insert(
        position,
        ViewLayer { viewport: reference.viewport, model_per_paper: reference.model_per_paper, transform: transform.into(), clips: outline(reference.viewport_rect, None)? },
    );

    let scene = scene_of(document, source.annotation_scale, Some(&reference.layout));
    let page = [0.0, 0.0, transform.placement.width_pt as f64, transform.placement.height_pt as f64];
    let [x0, y0, x1, y1] = reference.viewport_on_page(&paper);
    let clip = [x0.max(page[0]), y0.max(page[1]), x1.min(page[2]), y1.min(page[3])];
    Ok(Publication { scene, transform, key_points, clip, layout: Some(LayoutPublication { paper, reference: context, views }) })
}

fn page_point(transform: &PageTransform, [x, y]: [f64; 2]) -> [f64; 2] {
    let (x, y) = transform.apply(x, y);
    [x, y]
}

/// A closed boundary as a polygon in its plane's XY (a viewport's clipping
/// boundary, in paper space), within `tolerance`.
fn boundary_polygon(entity: &acadrust::EntityType, tolerance: f64) -> Option<Vec<[f64; 2]>> {
    let (polyline, _) = flatten(entity, tolerance).ok()??;
    let normal = (polyline.normal.x, polyline.normal.y, polyline.normal.z);
    let points: Vec<[f64; 2]> = polyline
        .vertices
        .iter()
        .map(|v| {
            let (x, y, _) = crate::scene::view::transform::ocs_point_to_wcs((v.location.x, v.location.y, polyline.elevation), normal);
            [x, y]
        })
        .collect();
    (points.len() >= 3).then_some(points)
}

impl Publication {
    /// Snap end points recorded for a replaced curve.
    pub(crate) fn key_points(&self, handle: u64) -> Option<&[[f64; 3]]> {
        self.key_points.get(&handle).map(Vec::as_slice)
    }
}

/// A published page: the vector PDF bytes and what could not be drawn.
pub struct PublishedPdf {
    pub bytes: Vec<u8>,
    /// Raster images in the published view, which the SecurePlan page does
    /// not draw; Apply reports them (F5) rather than dropping them silently.
    pub omitted_images: usize,
}

/// One group of page content and the transform that places it.
pub struct PageLayer {
    pub content: crate::io::pdf_export::PlotContent,
    pub transform: LayerTransform,
    /// Clip the group to each of these page-space polygons (points).
    pub clips: Vec<Vec<[f64; 2]>>,
}

fn plot_content(
    wires: Vec<crate::io::pdf_export::PlotWire>,
    hatches: Vec<crate::scene::model::hatch_model::HatchModel>,
    wipeouts: Vec<crate::scene::model::hatch_model::HatchModel>,
) -> crate::io::pdf_export::PlotContent {
    use crate::io::pdf_export::{PlotContent, PlotGroupSplits};
    PlotContent {
        group_splits: PlotGroupSplits { wires: wires.len(), hatches: hatches.len(), wipeouts: wipeouts.len(), images: 0 },
        wires: std::sync::Arc::new(wires),
        hatches,
        wipeouts,
        images: Vec::new(),
    }
}

fn with_depth(scene: &crate::scene::Scene, wires: Vec<crate::scene::WireModel>) -> Vec<crate::io::pdf_export::PlotWire> {
    let wires: Vec<_> = wires.into_iter().filter(|wire| wire.plot_visible).collect();
    let depths = scene.plot_wire_depths(&wires);
    wires.into_iter().zip(depths).map(|(wire, draw_depth)| crate::io::pdf_export::PlotWire { wire, draw_depth }).collect()
}

/// The model-space content of `scene` (what plots), as page content.
fn model_content(scene: &crate::scene::Scene) -> (crate::io::pdf_export::PlotContent, usize) {
    let (wires, _) = scene.plot_wire_groups(None);
    let mut hatches = scene.paper_plot_hatches().as_ref().clone();
    exact_fill_boundaries(&mut hatches);
    let content = plot_content(with_depth(scene, wires), hatches, scene.paper_plot_wipeouts().as_ref().clone());
    (content, scene.paper_plot_images().len())
}

/// Model space as `view` shows it, as native paper rendering draws it
/// (its frozen layers, annotation scale, layer overrides and saved render
/// mode), in model coordinates. With PSLTSCALE, linetype dashes keep their
/// paper length whatever the viewport's scale, as native projection does.
fn viewport_content(scene: &crate::scene::Scene, view: &ViewLayer, all_visible: bool) -> crate::io::pdf_export::PlotContent {
    let mut wires = scene.model_wires_for_viewport_arc(view.viewport, 0.0).as_ref().clone();
    if let Some(acadrust::EntityType::Viewport(viewport)) = scene.document.get_entity(view.viewport) {
        let flags = crate::scene::view::render::render_mode_flags(viewport.render_mode);
        for wire in wires.iter_mut().filter(|wire| wire.fill_is_3d) {
            if !flags.face3d_fill && !flags.mesh_fill {
                wire.fill_tris.clear();
                wire.fill_tris_low.clear();
            }
            if !flags.show_3d_edges {
                wire.points.clear();
                wire.points_low.clear();
            }
        }
    }
    if scene.document.header.paper_space_linetype_scaling {
        let k = view.model_per_paper as f32;
        for wire in &mut wires {
            wire.pattern_length *= k;
            wire.pattern = wire.pattern.map(|value| value * k);
        }
    }
    let (mut hatches, wipeouts) = scene.secureplan_viewport_fills(view.viewport, all_visible);
    exact_fill_boundaries(&mut hatches);
    plot_content(with_depth(scene, wires), hatches, wipeouts)
}

/// A layout sheet's paper-space content, as the upstream layout plot
/// assembles it (viewport borders as the plot settings say), whether paper
/// space plots last, and the raster images the page leaves out.
fn sheet_content(scene: &crate::scene::Scene) -> (crate::io::pdf_export::PlotContent, bool, usize) {
    let settings = scene.effective_plot_settings();
    let borders = settings.as_ref().is_none_or(|settings| settings.flags.plot_viewport_borders);
    let paper_last = settings.as_ref().is_some_and(|settings| settings.flags.draw_viewports_first);
    let (mut paper_wires, _) = scene.plot_wire_groups(None);
    paper_wires.retain(|wire| {
        borders
            || !crate::scene::Scene::handle_from_wire_name(&wire.name).and_then(|handle| scene.document.get_entity(handle)).is_some_and(|entity| {
                matches!(entity, acadrust::EntityType::Viewport(viewport) if !crate::scene::Scene::is_sheet_viewport(&scene.document, viewport))
            })
    });
    let omitted_images = scene.paper_plot_images().len() + scene.viewport_plot_fills().3.len();
    let content = plot_content(with_depth(scene, paper_wires), scene.paper_plot_hatches().as_ref().clone(), scene.paper_plot_wipeouts().as_ref().clone());
    (content, paper_last, omitted_images)
}

/// The published view as a one-page CON-01 PDF: model space, or a paper
/// layout's whole sheet with each viewport's model content drawn from model
/// space, exactly, clipped to the viewport.
pub fn page_pdf(publication: &Publication) -> Result<PublishedPdf, String> {
    let (layers, omitted_images) = match &publication.layout {
        None => {
            let (model, omitted) = model_content(&publication.scene);
            (vec![PageLayer { content: model, transform: publication.transform.into(), clips: Vec::new() }], omitted)
        }
        Some(layout) => {
            let scene = &publication.scene;
            let (paper, paper_last, omitted) = sheet_content(scene);
            let paper = PageLayer { content: paper, transform: layout.paper.into(), clips: Vec::new() };
            let views = layout
                .views
                .iter()
                .map(|view| PageLayer { content: viewport_content(scene, view, layout.reference.all_visible), transform: view.transform, clips: view.clips.clone() });
            // The layout's own order: viewports over paper space, unless its
            // plot settings draw paper space last.
            let layers = if paper_last { views.chain(std::iter::once(paper)).collect() } else { std::iter::once(paper).chain(views).collect() };
            (layers, omitted)
        }
    };
    let bytes = crate::io::pdf_export::secureplan_page_pdf(layers)?;
    Ok(PublishedPdf { bytes, omitted_images })
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2_011::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

fn save(doc: &mut lopdf::Document) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).map_err(|e| format!("PDF write failed: {e}"))?;
    Ok(bytes)
}

/// Finish the SecurePlan page: MediaBox exactly `[0 0 W H]`, `/Rotate 0`, no
/// CropBox, TrimBox or UserUnit (CON-01: one unit is one point), no dates or other varying metadata, compressed
/// streams, and a document ID derived from the content.
pub(crate) fn finish_pdf(mut doc: lopdf::Document, width_pt: u32, height_pt: u32) -> Result<Vec<u8>, String> {
    use lopdf::{Object, StringFormat};
    let pages = doc.get_pages();
    if pages.len() != 1 {
        return Err("A published plan has exactly one page.".into());
    }
    for (_, id) in pages {
        let page = doc.get_object_mut(id).and_then(Object::as_dict_mut).map_err(|e| e.to_string())?;
        page.set("MediaBox", vec![0.into(), 0.into(), i64::from(width_pt).into(), i64::from(height_pt).into()]);
        page.set("Rotate", 0);
        page.remove(b"CropBox");
        page.remove(b"TrimBox");
        page.remove(b"UserUnit");
        page.remove(b"Annots");
    }
    // The information dictionary and XMP metadata carry dates and a random
    // identifier; the published page carries neither.
    doc.trailer.remove(b"Info");
    doc.trailer.remove(b"ID");
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"Metadata");
        catalog.remove(b"OutputIntents");
    }
    doc.prune_objects();
    doc.renumber_objects();
    doc.compress();
    let digest = sha256(&save(&mut doc)?);
    let id = Object::String(digest[..16].to_vec(), StringFormat::Hexadecimal);
    doc.trailer.set("ID", vec![id.clone(), id]);
    save(&mut doc)
}

/// The SPSNAP file for the published view (CON-05).
pub fn page_snap(publication: &Publication) -> Vec<u8> {
    let geometry = snap::extract(publication);
    snap::write(&geometry, publication.transform.placement.width_pt, publication.transform.placement.height_pt)
}

// ── Apply outputs (PUB-03 model space, PUB-04) ─────────────────────────────

/// The largest output SecurePlan stores (a managed file).
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024 * 1024;
/// The default window's margin around the visible extents, per side, as a
/// fraction of the extents' width (x) and height (y).
pub const WINDOW_MARGIN: f64 = 0.02;

/// The extents of the model-space geometry the published view draws (what
/// plots: off, frozen and non-plotting layers excluded; lines and fills), as
/// `[x0, y0, x1, y1]`.
pub fn visible_extents(scene: &crate::scene::Scene) -> Option<[f64; 4]> {
    if scene.current_layout != "Model" {
        let (min, max) = scene.model_space_extents()?;
        return Some([min.x as f64, min.y as f64, max.x as f64, max.y as f64]);
    }
    let (wires, _) = scene.plot_wire_groups(None);
    let mut extents = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for wire in wires.iter().filter(|wire| wire.plot_visible) {
        for (index, [x, y, _]) in wire.points.iter().enumerate() {
            let [lx, ly, _] = wire.points_low.get(index).copied().unwrap_or([0.0; 3]);
            let (x, y) = (*x as f64 + lx as f64, *y as f64 + ly as f64);
            if x.is_finite() && y.is_finite() {
                extents = [extents[0].min(x), extents[1].min(y), extents[2].max(x), extents[3].max(y)];
            }
        }
    }
    // A solid fill has no lines.
    for fill in scene.paper_plot_hatches().iter().chain(scene.paper_plot_wipeouts().iter()) {
        for [x, y] in fill.boundary.iter() {
            let (x, y) = (fill.world_origin[0] + *x as f64, fill.world_origin[1] + *y as f64);
            if x.is_finite() && y.is_finite() {
                extents = [extents[0].min(x), extents[1].min(y), extents[2].max(x), extents[3].max(y)];
            }
        }
    }
    extents.iter().all(|v| v.is_finite()).then_some(extents)
}

/// The default published window: the visible extents with a 2% margin on
/// each side. A degenerate side gets a margin from the other.
pub fn default_window([x0, y0, x1, y1]: [f64; 4]) -> [f64; 4] {
    let size = (x1 - x0).max(y1 - y0).max(1.0);
    let mx = if x1 > x0 { (x1 - x0) * WINDOW_MARGIN } else { size * WINDOW_MARGIN };
    let my = if y1 > y0 { (y1 - y0) * WINDOW_MARGIN } else { size * WINDOW_MARGIN };
    [x0 - mx, y0 - my, x1 + mx, y1 + my]
}

/// The publication scale: the smallest millimetres per point of the 1-2-5
/// series (…, 1, 2, 5, 10, …) at which the page meets the CON-01 side and
/// area limits. A larger page is a finer float32 grid (a smaller `r`).
pub fn choose_mm_per_pt(window_cad: [f64; 4], mapping: &Mapping) -> Result<f64, PlacementError> {
    let [x0, y0, x1, y1] = window_cad;
    if !(window_cad.iter().all(|v| v.is_finite()) && x1 > x0 && y1 > y0) {
        return Err(PlacementError::InvalidWindow);
    }
    let corners = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]].map(|corner| mapping.cad_to_world(corner));
    let span = |axis: usize| {
        let values = corners.map(|c| c[axis]);
        values.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - values.iter().cloned().fold(f64::INFINITY, f64::min)
    };
    let (width, height) = (span(0), span(1));
    let least = (width / MAX_SIDE_PT as f64).max(height / MAX_SIDE_PT as f64).max((width * height / MAX_AREA_PT2 as f64).sqrt());
    if !(least.is_finite() && least > 0.0) {
        return Err(PlacementError::EmptyPage);
    }
    let mut decade = 10f64.powi(least.log10().floor() as i32 - 1);
    for _ in 0..40 {
        for step in [1.0, 2.0, 5.0] {
            let candidate = step * decade;
            if candidate >= least * (1.0 - 1e-12) && place_page(window_cad, mapping, candidate).is_ok() {
                return Ok(candidate);
            }
        }
        decade *= 10.0;
    }
    Err(PlacementError::InvalidScale)
}

/// The published view the user picked (PUB-03).
#[derive(Debug, Clone, PartialEq)]
pub enum PublishedView {
    /// A model-space window `[x0, y0, x1, y1]` in CAD coordinates.
    Model { window_cad: [f64; 4] },
    /// A paper layout's whole sheet, through its reference viewport.
    Layout(Box<super::layout::LayoutReference>),
}

impl PublishedView {
    /// The `view` of CON-02.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            PublishedView::Model { window_cad } => serde_json::json!({ "kind": "model", "windowCad": window_cad }),
            PublishedView::Layout(reference) => reference.view_json(),
        }
    }
}

/// What Apply publishes and how it maps: chosen in the Apply dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyPlan {
    pub view: PublishedView,
    pub mapping: Mapping,
    /// For a paper layout, the sheet's own scale ([`super::layout::LayoutReference::mm_per_pt`]).
    pub mm_per_pt: f64,
    /// The user ticked "Publish without N damaged items" for this Apply.
    pub damaged_acknowledged: bool,
}

impl ApplyPlan {
    /// CAD model coordinates → page points.
    pub fn transform(&self) -> Result<PageTransform, PlacementError> {
        match &self.view {
            PublishedView::Model { window_cad } => {
                Ok(PageTransform { mapping: self.mapping, placement: place_page(*window_cad, &self.mapping, self.mm_per_pt)? })
            }
            PublishedView::Layout(reference) => Ok(reference.transforms(&self.mapping)?.0),
        }
    }
}

/// The `placement` of `applyRequest` (CON-01).
pub fn placement_json(p: &Placement) -> serde_json::Value {
    serde_json::json!({
        "widthPt": p.width_pt,
        "heightPt": p.height_pt,
        "centerX": p.center_x,
        "centerY": p.center_y,
        "widthMm": p.width_mm,
        "heightMm": p.height_mm,
    })
}

/// Check what `applyRequest` carries for `plan` against the protocol's
/// schema bounds (CON-02, `common.schema.json`), before anything is built or
/// sent: the web would refuse the request and the session would close.
pub fn check_contract(plan: &ApplyPlan) -> Result<(), String> {
    let placement = plan.transform().map_err(|error| format!("The published page does not fit: {error:?}."))?.placement;
    let m = &plan.mapping;
    let mapping = serde_json::json!({
        "cadOrigin": m.cad_origin,
        "anchorMm": m.anchor_mm,
        "scaleMmPerCadUnit": m.scale_mm_per_cad_unit,
        "quarterTurns": m.quarter_turns,
    });
    let request = "/$defs/applyRequest/properties";
    for (file, pointer, value, path) in [
        ("common.schema.json", "/$defs/view".to_string(), plan.view.to_json(), "view"),
        ("common.schema.json", "/$defs/mapping".to_string(), mapping, "mapping"),
        ("common.schema.json", "/$defs/placement".to_string(), placement_json(&placement), "placement"),
        ("messages.schema.json", format!("{request}/roundingBoundMm"), placement.rounding_bound_mm.into(), "roundingBoundMm"),
        ("messages.schema.json", format!("{request}/chordToleranceMm"), placement.chord_tolerance_mm.into(), "chordToleranceMm"),
    ] {
        super::protocol::validate_node(file, &pointer, &value, path)
            .map_err(|error| format!("The published page is outside what SecurePlan accepts: {}.", error.0))?;
    }
    Ok(())
}

/// The frozen drawing state an Apply is built from. Taken once, when Apply
/// starts; later edits never reach it.
#[derive(Clone)]
pub struct Snapshot {
    pub document: acadrust::CadDocument,
    pub annotation_scale: f32,
    /// The bytes the document was loaded from, sent verbatim when unmodified.
    pub loaded: Option<super::session::Drawing>,
    pub modified: bool,
    pub pending_original: Option<super::session::Drawing>,
    /// Damaged items the reader dropped from the loaded drawing. They are
    /// not in the published PDF and snap file (nor in a written drawing), so
    /// every Apply needs the user's acknowledgement (PUB-01, PUB-04).
    pub lost_entities: usize,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").field("modified", &self.modified).finish_non_exhaustive()
    }
}

/// Everything Apply sends, built in memory from one snapshot.
#[derive(Debug, Clone)]
pub struct ApplyOutputs {
    pub drawing: super::session::Drawing,
    /// The drawing is the writer's output, not the verbatim loaded bytes.
    pub written: bool,
    pub original: Option<super::session::Drawing>,
    pub pdf: Vec<u8>,
    pub snap: Vec<u8>,
    pub transform: PageTransform,
    pub omitted_images: usize,
}

/// Why Apply stopped; nothing was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyError {
    pub code: super::session::ErrorCode,
    pub message: String,
}

impl ApplyError {
    pub(crate) fn new(code: super::session::ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

/// Build the drawing, PDF and snap file from `snapshot` alone, in memory (no
/// temporary files). Writer errors, known content loss and any output over
/// 50 MiB stop Apply.
pub fn build_outputs(snapshot: &Snapshot, plan: &ApplyPlan) -> Result<ApplyOutputs, ApplyError> {
    use super::session::{Drawing, ErrorCode, Format};
    let transform = plan.transform().map_err(|error| ApplyError::new(ErrorCode::Internal, format!("The published page does not fit: {error:?}.")))?;
    check_contract(plan).map_err(|message| ApplyError::new(ErrorCode::Internal, message))?;
    // Damaged items the reader dropped are published only with the user's
    // acknowledgement, every time, edited or not.
    if snapshot.lost_entities > 0 && !plan.damaged_acknowledged {
        return Err(ApplyError::new(
            ErrorCode::KnownLoss,
            format!("Tick \"Publish without {} damaged items\" to apply this drawing.", snapshot.lost_entities),
        ));
    }

    let written = snapshot.modified || snapshot.loaded.is_none();
    let drawing = match (&snapshot.loaded, snapshot.modified) {
        (Some(loaded), false) => loaded.clone(),
        (loaded, _) => {
            let (format, version, name) = match loaded {
                Some(loaded) => (loaded.format, loaded.version(), loaded.name.expose().clone()),
                None => (Format::Dxf, acadrust::DxfVersion::AC1032, "drawing.dxf".to_string()),
            };
            // PUB-04: an edited drawing is written in its original version;
            // one the writer cannot produce (R13) stops Apply rather than
            // being written as another version.
            if !super::export::writable(format, version) {
                let label = super::export::version_label(format, version);
                return Err(ApplyError::new(
                    ErrorCode::WriterError,
                    format!(
                        "SecurePlan CAD cannot write {label}, this drawing's version, so a changed drawing cannot be applied, even after undoing the changes. To apply it unchanged, close the drawing, choose Discard edits, and open the survey from SecurePlan again (or import the drawing again if SecurePlan does not have it yet). To keep changes, save the drawing as {} R14 or later in your CAD application and import that.",
                        format.ext().to_ascii_uppercase()
                    ),
                ));
            }
            let is_dxf = format == Format::Dxf;
            let dropped = crate::io::dropped_on_save_count(&snapshot.document, version, is_dxf);
            if dropped > 0 {
                return Err(ApplyError::new(
                    ErrorCode::KnownLoss,
                    format!("Writing the drawing as {} {} would drop {dropped} unsupported object(s).", format.ext().to_ascii_uppercase(), version.as_str()),
                ));
            }
            let bytes = crate::io::save_to_bytes(&snapshot.document, format.ext(), version)
                .map_err(|_| ApplyError::new(ErrorCode::WriterError, "The drawing could not be written."))?;
            let stem = std::path::Path::new(&name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "drawing".into());
            Drawing {
                bytes: std::sync::Arc::new(bytes),
                name: super::session::file_name(&format!("{stem}.{}", format.ext())).into(),
                format,
                format_version: version.as_str().to_string(),
            }
        }
    };

    // The published view comes from the same snapshot.
    let mut scene = crate::scene::Scene::new();
    scene.document = snapshot.document.clone();
    scene.annotation_scale = snapshot.annotation_scale;
    scene.rebuild_derived_caches();
    let publication = match &plan.view {
        PublishedView::Model { .. } => prepare_model(&scene, transform),
        PublishedView::Layout(reference) => prepare_layout(&scene, reference, transform),
    }
    .map_err(|message| ApplyError::new(ErrorCode::Internal, message))?;
    let pdf = page_pdf(&publication).map_err(|_| ApplyError::new(ErrorCode::WriterError, "The published PDF could not be written."))?;
    let snap = page_snap(&publication);

    let outputs = ApplyOutputs { drawing, written, original: snapshot.pending_original.clone(), pdf: pdf.bytes, snap, transform, omitted_images: pdf.omitted_images };
    let sizes = [
        ("drawing", outputs.drawing.bytes.len()),
        ("original drawing", outputs.original.as_ref().map_or(0, |o| o.bytes.len())),
        ("published PDF", outputs.pdf.len()),
        ("snap file", outputs.snap.len()),
    ];
    for (what, size) in sizes {
        if size > MAX_OUTPUT_BYTES {
            return Err(ApplyError::new(
                ErrorCode::OutputTooLarge,
                format!("The {what} is {:.1} MiB; SecurePlan stores files up to 50 MiB.", size as f64 / 1048576.0),
            ));
        }
    }
    Ok(outputs)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::vectors;
    use acadrust::entities::{Arc, Circle, Line, LwPolyline};
    use acadrust::types::{Vector2, Vector3};
    use acadrust::{CadDocument, EntityType};
    use serde_json::Value;

    fn pair(value: &Value) -> [f64; 2] {
        [value[0].as_f64().unwrap(), value[1].as_f64().unwrap()]
    }

    pub(crate) fn mapping(value: &Value) -> Mapping {
        Mapping {
            cad_origin: pair(&value["cadOrigin"]),
            anchor_mm: pair(&value["anchorMm"]),
            scale_mm_per_cad_unit: value["scaleMmPerCadUnit"].as_f64().unwrap(),
            quarter_turns: value["quarterTurns"].as_u64().unwrap() as u8,
        }
    }

    fn window(value: &Value) -> [f64; 4] {
        let v: Vec<f64> = value.as_array().unwrap().iter().map(|n| n.as_f64().unwrap()).collect();
        [v[0], v[1], v[2], v[3]]
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
    }

    /// The cubic Bézier of the fix-round review: its midpoint-only check
    /// passed a 250 mm chord that the curve leaves by ±12 mm.
    pub(crate) fn bulging_spline() -> (acadrust::entities::Spline, impl Fn(f64) -> [f64; 2]) {
        let control = [[0.0, 0.0], [1000.0 / 3.0, 512.0 / 3.0], [2000.0 / 3.0, -5120.0 / 3.0], [1000.0, 10752.0]];
        let spline = acadrust::entities::Spline::from_control_points(3, control.iter().map(|[x, y]| Vector3::new(*x, *y, 0.0)).collect());
        let at = move |t: f64| {
            let u = 1.0 - t;
            let b = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
            [0, 1].map(|i| (0..4).map(|k| b[k] * control[k][i]).sum())
        };
        (spline, at)
    }

    /// The largest distance from the exact curve (densely sampled) to the polyline.
    pub(crate) fn polyline_departure(points: &[[f64; 2]], curve: impl Fn(f64) -> [f64; 2]) -> f64 {
        let segment = |p: [f64; 2], a: [f64; 2], b: [f64; 2]| {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let length2 = dx * dx + dy * dy;
            let t = if length2 == 0.0 { 0.0 } else { (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / length2).clamp(0.0, 1.0) };
            (p[0] - a[0] - t * dx).hypot(p[1] - a[1] - t * dy)
        };
        (0..=20_000)
            .map(|i| curve(i as f64 / 20_000.0))
            .map(|p| points.windows(2).map(|w| segment(p, w[0], w[1])).fold(f64::INFINITY, f64::min))
            .fold(0.0, f64::max)
    }

    #[test]
    fn a_spline_is_published_within_the_tolerance_along_every_chord() {
        let (spline, curve) = bulging_spline();
        for tolerance in [0.1, 1.0] {
            let (polyline, _) = flatten(&EntityType::Spline(spline.clone()), tolerance).unwrap().expect("flattened");
            let points: Vec<[f64; 2]> = polyline.vertices.iter().map(|v| [v.location.x, v.location.y]).collect();
            let departure = polyline_departure(&points, &curve);
            assert!(departure <= tolerance * (1.0 + 1e-6), "the curve leaves a chord by {departure} mm at tolerance {tolerance}");
        }
    }

    #[test]
    fn splines_and_ellipses_of_every_kind_stay_within_the_tolerance() {
        use acadrust::entities::{Ellipse, Spline};
        let mut weighted = Spline::from_control_points(
            3,
            [[0.0, 0.0], [3000.0, 4000.0], [6000.0, -2000.0], [9000.0, 5000.0], [12000.0, 0.0], [15000.0, 3000.0]]
                .iter()
                .map(|[x, y]| Vector3::new(*x, *y, 0.0))
                .collect(),
        );
        weighted.weights = vec![1.0, 3.0, 0.5, 2.0, 1.0, 4.0];
        let mut fitted = Spline::new();
        fitted.fit_points = [[0.0, 0.0], [2000.0, 3000.0], [5000.0, -1000.0], [9000.0, 4000.0], [12000.0, 0.0]]
            .iter()
            .map(|[x, y]| Vector3::new(*x, *y, 0.0))
            .collect();
        let mut ellipse = Ellipse::new();
        ellipse.center = Vector3::new(1000.0, 2000.0, 0.0);
        ellipse.major_axis = Vector3::new(8000.0, 3000.0, 0.0);
        ellipse.minor_axis_ratio = 0.2;
        ellipse.start_parameter = 0.3;
        ellipse.end_parameter = 5.0;
        for entity in [EntityType::Spline(weighted), EntityType::Spline(fitted), EntityType::Ellipse(ellipse)] {
            let exact = crate::entities::curve::entity_curve(&entity).expect("a planar curve");
            let (polyline, _) = flatten(&entity, 0.1).unwrap().expect("flattened");
            let points: Vec<[f64; 2]> = polyline.vertices.iter().map(|v| [v.location.x, v.location.y]).collect();
            let departure = polyline_departure(&points, |t| {
                let p = exact.point_at(t);
                [p[0], p[1]]
            });
            assert!(departure <= 0.1 * (1.0 + 1e-6), "{:?}: {departure} mm", std::mem::discriminant(&entity));
            let [first, last] = [exact.point_at(0.0), exact.point_at(1.0)];
            assert!((points[0][0] - first[0]).hypot(points[0][1] - first[1]) < 1e-6, "starts on the curve");
            let end = points[points.len() - 1];
            assert!((end[0] - last[0]).hypot(end[1] - last[1]) < 1e-6, "ends on the curve");
        }
    }

    /// Unclamped and weighted periodic splines, as drawings carry them.
    pub(crate) fn awkward_splines() -> Vec<acadrust::entities::Spline> {
        use acadrust::entities::Spline;
        let spline = |degree: i32, points: &[[f64; 2]], knots: Vec<f64>, weights: Vec<f64>, periodic: bool| {
            let mut spline = Spline::new();
            spline.degree = degree;
            spline.control_points = points.iter().map(|[x, y]| Vector3::new(*x, *y, 0.0)).collect();
            spline.knots = knots;
            spline.weights = weights;
            spline.flags.periodic = periodic;
            spline.flags.rational = spline.weights.iter().any(|w| *w != 1.0);
            spline
        };
        // The review's case: degree 2, unclamped uniform knots.
        let unclamped = spline(2, &[[0.0, 0.0], [1000.0, 1000.0], [2000.0, 0.0]], vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0], vec![1.0; 3], false);
        let cubic = spline(
            3,
            &[[0.0, 0.0], [1000.0, 2000.0], [3000.0, -1000.0], [4000.0, 1500.0], [6000.0, 0.0]],
            (0..9).map(f64::from).collect(),
            vec![1.0, 2.0, 0.5, 3.0, 1.0],
            false,
        );
        // Periodic: the first `degree` controls repeated at the end, uniform knots, weights.
        let ring = [[0.0, 0.0], [2000.0, 0.0], [3000.0, 2000.0], [1000.0, 3000.0], [-1000.0, 1500.0]];
        let mut closed: Vec<[f64; 2]> = ring.to_vec();
        closed.extend_from_slice(&ring[..3]);
        let weights = vec![1.0, 2.0, 1.0, 3.0, 1.5, 1.0, 2.0, 1.0];
        let periodic = spline(3, &closed, (0..12).map(f64::from).collect(), weights, true);
        vec![unclamped, cubic, periodic]
    }

    #[test]
    fn a_spline_with_many_knots_is_published_quickly() {
        let started = std::time::Instant::now();
        let entity = crate::app::secureplan::convert::tests::many_knots(100_000);
        let (polyline, _) = flatten(&entity, 0.1).unwrap().expect("flattened");
        assert_eq!(polyline.vertices.len(), 100_000, "one point per control of a degree-1 spline");
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
    }

    #[test]
    fn unclamped_and_periodic_splines_are_published_within_the_tolerance() {
        for spline in awkward_splines() {
            let entity = EntityType::Spline(spline);
            let exact = crate::entities::curve::entity_curve(&entity).expect("a planar curve");
            let (polyline, _) = flatten(&entity, 0.1).unwrap().expect("flattened");
            let points: Vec<[f64; 2]> = polyline.vertices.iter().map(|v| [v.location.x, v.location.y]).collect();
            let departure = polyline_departure(&points, |t| {
                let p = exact.point_at(t);
                [p[0], p[1]]
            });
            assert!(departure <= 0.1 * (1.0 + 1e-6), "{departure} mm");
            let [first, last] = [exact.point_at(0.0), exact.point_at(1.0)];
            let end = points[points.len() - 1];
            assert!((points[0][0] - first[0]).hypot(points[0][1] - first[1]) < 1e-6, "starts on the curve");
            assert!((end[0] - last[0]).hypot(end[1] - last[1]) < 1e-6, "ends on the curve");
        }
    }

    #[test]
    fn placement_matches_the_vectors_including_float32_boundaries() {
        let placement = vectors::json("placement.json");
        for case in placement["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mapping = mapping(&case["mapping"]);
            let placed = place_page(window(&case["windowCad"]), &mapping, case["mmPerPt"].as_f64().unwrap()).unwrap();
            let expect = &case["expect"];
            assert_eq!(placed.width_pt as u64, expect["widthPt"].as_u64().unwrap(), "{name}");
            assert_eq!(placed.height_pt as u64, expect["heightPt"].as_u64().unwrap(), "{name}");
            for (got, key) in [
                (placed.center_x, "centerX"),
                (placed.center_y, "centerY"),
                (placed.width_mm, "widthMm"),
                (placed.height_mm, "heightMm"),
            ] {
                assert!(close(got, expect["placement"][key].as_f64().unwrap()), "{name} {key}");
            }
            assert!(close(placed.rounding_bound_mm, expect["roundingBoundMm"].as_f64().unwrap()), "{name} r");
            assert!(close(placed.chord_tolerance_mm, expect["chordToleranceMm"].as_f64().unwrap()), "{name} chord");
            assert_eq!(placed.starts_inside_canvas(), expect["startsInsideCanvas"].as_bool().unwrap(), "{name}");
            let transform = PageTransform { mapping, placement: placed };
            for point in case["points"].as_array().unwrap() {
                let cad = pair(&point["cad"]);
                let (x, y) = transform.apply(cad[0], cad[1]);
                let page = pair(&point["pagePt"]);
                assert!(close(x, page[0]) && close(y, page[1]), "{name} {cad:?}");
                // The one f32 rounding the writer performs gives the vector's value.
                let f32_page = pair(&point["pagePtF32"]);
                assert_eq!([(x as f32) as f64, (y as f32) as f64], f32_page, "{name} {cad:?}");
                // Total world error from that rounding stays within √2·r.
                let web = |p: [f64; 2]| {
                    let m = placed.width_mm / placed.width_pt as f64;
                    [placed.center_x - placed.width_mm / 2.0 + p[0] * m, placed.center_y - placed.height_mm / 2.0 + (placed.height_pt as f64 - p[1]) * m]
                };
                let world = mapping.cad_to_world(cad);
                let from_f32 = web(f32_page);
                let error = (from_f32[0] - world[0]).hypot(from_f32[1] - world[1]);
                assert!(error <= placed.rounding_bound_mm * std::f64::consts::SQRT_2 + 1e-9, "{name} error {error}");
            }
        }
        for case in placement["rejections"].as_array().unwrap() {
            let result = place_page(window(&case["windowCad"]), &mapping(&case["mapping"]), case["mmPerPt"].as_f64().unwrap());
            let expected = match case["expectError"].as_str().unwrap() {
                "sideTooLarge" => PlacementError::SideTooLarge,
                "areaTooLarge" => PlacementError::AreaTooLarge,
                "invalidScale" => PlacementError::InvalidScale,
                "invalidWindow" => PlacementError::InvalidWindow,
                _ => PlacementError::EmptyPage,
            };
            assert_eq!(result, Err(expected), "{}", case["name"]);
        }
    }

    /// PUB-04 with a version the writer cannot produce: an edited R13 drawing
    /// stops Apply with the writer's reason instead of being written as R14;
    /// an unedited one is still applied byte for byte.
    #[test]
    fn an_edited_r13_drawing_stops_apply_and_an_unedited_one_applies_verbatim() {
        use super::super::session::{Drawing, ErrorCode, Format};
        let scene = synthetic_dxf_scene();
        let window = [0.0, 0.0, 30000.0, 18000.0];
        let mapping = super::super::align::mapping_at(window, 1.0, 0, [0.0, 0.0]);
        let plan = ApplyPlan { view: PublishedView::Model { window_cad: window }, mapping, mm_per_pt: choose_mm_per_pt(window, &mapping).unwrap(), damaged_acknowledged: false };
        let snapshot = |format: Format, version: &str, modified: bool| {
            let bytes = std::sync::Arc::new(format!("{version} synthetic original bytes").into_bytes());
            let loaded = Drawing { bytes, name: format!("r13.{}", format.ext()).into(), format, format_version: version.into() };
            Snapshot { document: scene.document.clone(), annotation_scale: 1.0, loaded: Some(loaded), modified, pending_original: None, lost_entities: 0 }
        };
        for format in [Format::Dwg, Format::Dxf] {
            let error = build_outputs(&snapshot(format, "AC1012", true), &plan).unwrap_err();
            assert_eq!(error.code, ErrorCode::WriterError, "{format:?}");
            let label = format!("{} R13 (AC1012)", format.ext().to_ascii_uppercase());
            assert!(error.message.contains(&label) && error.message.contains("R14 or later") && error.message.contains("Discard edits"), "{}", error.message);
            let unedited = snapshot(format, "AC1012", false);
            let outputs = build_outputs(&unedited, &plan).expect("an unedited R13 drawing applies");
            assert_eq!(outputs.drawing.bytes, unedited.loaded.as_ref().unwrap().bytes, "verbatim");
            assert!(!outputs.written);
        }
        // R14, the oldest version the writer offers, is written as R14.
        let outputs = build_outputs(&snapshot(Format::Dwg, "AC1014", true), &plan).expect("R14 is written");
        assert_eq!(&outputs.drawing.bytes[..6], b"AC1014");
        assert_eq!(outputs.drawing.format_version, "AC1014");
    }

    /// A synthetic floor plan, written as DXF and read back like an import:
    /// lines, a circle, an arc, a closed room polyline and a legacy POLYLINE
    /// with a bulge.
    pub(crate) fn synthetic_dxf_scene() -> crate::scene::Scene {
        use acadrust::entities::{Polyline2D, Vertex2D};
        let mut doc = CadDocument::new();
        let line = |x0: f64, y0: f64, x1: f64, y1: f64| {
            EntityType::Line(Line::from_points(Vector3::new(x0, y0, 0.0), Vector3::new(x1, y1, 0.0)))
        };
        for entity in [
            line(0.0, 0.0, 30000.0, 0.0),
            line(30000.0, 0.0, 30000.0, 18000.0),
            line(30000.0, 18000.0, 0.0, 18000.0),
            line(0.0, 18000.0, 0.0, 0.0),
            // The known line whose PDF position the tests check.
            line(12345.6, 7890.1, 20000.0, 10000.0),
        ] {
            doc.add_entity(entity).unwrap();
        }
        let mut circle = Circle::new();
        circle.center = Vector3::new(5000.0, 5000.0, 0.0);
        circle.radius = 750.0;
        doc.add_entity(EntityType::Circle(circle)).unwrap();
        let mut arc = Arc::new();
        arc.center = Vector3::new(25000.0, 5000.0, 0.0);
        arc.radius = 1000.0;
        arc.start_angle = 0.0;
        arc.end_angle = std::f64::consts::FRAC_PI_2;
        doc.add_entity(EntityType::Arc(arc)).unwrap();
        let mut room = LwPolyline::from_points(vec![
            Vector2::new(2000.0, 12000.0),
            Vector2::new(8000.0, 12000.0),
            Vector2::new(8000.0, 16000.0),
            Vector2::new(2000.0, 16000.0),
        ]);
        room.is_closed = true;
        doc.add_entity(EntityType::LwPolyline(room)).unwrap();
        // A legacy POLYLINE: a half-circle bulge from (10000,2000) to (12000,2000), then straight up.
        let mut legacy = Polyline2D::new();
        let mut first = Vertex2D::new(Vector3::new(10000.0, 2000.0, 0.0));
        first.bulge = 1.0;
        legacy.add_vertex(first);
        legacy.add_vertex(Vertex2D::new(Vector3::new(12000.0, 2000.0, 0.0)));
        legacy.add_vertex(Vertex2D::new(Vector3::new(12000.0, 3000.0, 0.0)));
        doc.add_entity(EntityType::Polyline2D(legacy)).unwrap();
        let bytes = crate::io::save_to_bytes(&doc, "dxf", doc.version).unwrap();
        let mut scene = crate::scene::Scene::new();
        scene.document = crate::io::load_bytes("synthetic.dxf", bytes).unwrap();
        scene.rebuild_derived_caches();
        scene
    }

    /// The images a plain (not SecurePlan) scene of `doc` decodes.
    pub(crate) fn scene_images(doc: CadDocument) -> Vec<acadrust::types::Handle> {
        crate::app::secureplan::snap::tests::scene_of(doc).images.keys().copied().collect()
    }

    #[test]
    fn the_default_window_is_the_visible_extents_plus_two_percent() {
        let scene = synthetic_dxf_scene();
        let extents = visible_extents(&scene).expect("extents");
        for (got, want) in extents.iter().zip([0.0, 0.0, 30000.0, 18000.0]) {
            assert!((got - want).abs() < 0.01, "{extents:?}");
        }
        assert_eq!(default_window([0.0, 0.0, 100.0, 50.0]), [-2.0, -1.0, 102.0, 51.0]);
        // A hidden layer's geometry does not count.
        let mut doc = scene.document.clone();
        let mut far = Line::from_points(Vector3::new(1.0e6, 1.0e6, 0.0), Vector3::new(1.0e6 + 1.0, 1.0e6, 0.0));
        far.common.layer = "HIDDEN".into();
        let mut hidden = acadrust::tables::Layer::new("HIDDEN");
        hidden.flags.off = true;
        doc.layers.add(hidden).unwrap();
        doc.add_entity(EntityType::Line(far)).unwrap();
        let hidden_extents = visible_extents(&crate::app::secureplan::snap::tests::scene_of(doc)).unwrap();
        assert!(hidden_extents[2] < 31000.0, "{hidden_extents:?}");
    }

    #[test]
    fn the_scale_is_the_smallest_1_2_5_step_that_fits_the_page_limits() {
        let window = [0.0, 0.0, 30000.0, 18000.0];
        let mapping = Mapping { cad_origin: [0.0, 18000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        // 30 × 18 m needs at least √(540e6 / 64e6) ≈ 2.9 mm per point: 5.
        assert_eq!(choose_mm_per_pt(window, &mapping), Ok(5.0));
        let small = [0.0, 0.0, 300.0, 180.0];
        assert_eq!(choose_mm_per_pt(small, &mapping), Ok(0.05));
        assert!(place_page(small, &mapping, 0.05).is_ok());
        assert_eq!(choose_mm_per_pt([0.0, 0.0, 0.0, 1.0], &mapping), Err(PlacementError::InvalidWindow));
    }

    pub(crate) fn empty_survey_transform() -> PageTransform {
        let mapping = Mapping { cad_origin: [0.0, 18000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        let placement = place_page([0.0, 0.0, 30000.0, 18000.0], &mapping, 3.0).unwrap();
        PageTransform { mapping, placement }
    }

    pub(crate) fn publish(scene: &crate::scene::Scene, transform: PageTransform) -> (PublishedPdf, Publication) {
        let publication = prepare_model(scene, transform).unwrap();
        (page_pdf(&publication).unwrap(), publication)
    }

    pub(crate) fn page_dict(doc: &lopdf::Document) -> lopdf::Dictionary {
        let pages = doc.get_pages();
        assert_eq!(pages.len(), 1);
        doc.get_object(*pages.values().next().unwrap()).unwrap().as_dict().unwrap().clone()
    }

    /// Every operation of the page content.
    pub(crate) fn operations(bytes: &[u8]) -> Vec<lopdf::content::Operation> {
        let doc = lopdf::Document::load_mem(bytes).unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        lopdf::content::Content::decode(&doc.get_page_content(page).unwrap()).unwrap().operations
    }

    fn number(object: &lopdf::Object) -> f64 {
        object.as_float().map(f64::from).or_else(|_| object.as_i64().map(|v| v as f64)).unwrap()
    }

    /// Every `m`/`l` operand pair in the page content, in page points.
    pub(crate) fn content_points(bytes: &[u8]) -> Vec<[f64; 2]> {
        operations(bytes)
            .iter()
            .filter(|op| op.operator == "m" || op.operator == "l")
            .map(|op| [number(&op.operands[0]), number(&op.operands[1])])
            .collect()
    }

    #[test]
    fn the_published_pdf_follows_the_page_convention_and_is_deterministic() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let (first, _) = publish(&scene, transform);
        let (second, _) = publish(&scene, transform);
        assert_eq!(first.bytes, second.bytes, "identical bytes on a second run");
        assert_eq!(first.omitted_images, 0);

        let doc = lopdf::Document::load_mem(&first.bytes).unwrap();
        let page = page_dict(&doc);
        let media: Vec<i64> = page.get(b"MediaBox").unwrap().as_array().unwrap().iter().map(|o| o.as_i64().unwrap()).collect();
        assert_eq!(media, vec![0, 0, 10000, 6000]);
        assert_eq!(page.get(b"Rotate").unwrap().as_i64().unwrap(), 0);
        assert!(page.get(b"CropBox").is_err() && page.get(b"TrimBox").is_err());
        assert!(page.get(b"UserUnit").is_err(), "no /UserUnit: one unit is one point (CON-01)");
        assert!(doc.trailer.get(b"Info").is_err(), "no information dictionary (dates)");
        let text = String::from_utf8_lossy(&first.bytes);
        assert!(!text.contains("CreationDate") && !text.contains("ModDate") && !text.contains("PLOT"));
        // Every stream is compressed.
        for object in doc.objects.values() {
            if let Ok(stream) = object.as_stream() {
                assert_eq!(stream.dict.get(b"Filter").unwrap().as_name().unwrap(), b"FlateDecode");
            }
        }
        // The ID is derived from the content.
        let id = doc.trailer.get(b"ID").unwrap().as_array().unwrap();
        assert_eq!(id.len(), 2);
        assert_eq!(id[0].as_str().unwrap().len(), 16);
    }

    #[test]
    fn a_known_line_lands_where_the_placement_says() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let (pdf, _) = publish(&scene, transform);
        let points = content_points(&pdf.bytes);
        let r_pt = transform.placement.rounding_bound_mm / transform.placement.mm_per_pt;
        for cad in [[12345.6, 7890.1], [20000.0, 10000.0], [0.0, 18000.0], [30000.0, 0.0]] {
            let (x, y) = transform.apply(cad[0], cad[1]);
            let nearest = points
                .iter()
                .map(|p| (p[0] - x).hypot(p[1] - y))
                .fold(f64::INFINITY, f64::min);
            // The operand is the f32 page value; the written decimal adds no error beyond it.
            assert!(nearest <= r_pt * std::f64::consts::SQRT_2 + 1e-6, "{cad:?}: nearest operand {nearest} pt");
        }
    }

    #[test]
    fn published_curves_meet_the_chord_tolerance_and_ignore_the_viewport() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let (pdf, publication) = publish(&scene, transform);
        let (cx, cy) = transform.apply(5000.0, 5000.0);
        let radius_pt = 750.0 / transform.placement.mm_per_pt;
        let tolerance_pt = transform.placement.chord_tolerance_mm / transform.placement.mm_per_pt;
        let r_pt = transform.placement.rounding_bound_mm / transform.placement.mm_per_pt;
        let points = content_points(&pdf.bytes);
        let on_circle = |p: &[f64; 2]| ((p[0] - cx).hypot(p[1] - cy) - radius_pt).abs() < 0.05;
        let mut chords = 0;
        for pair in points.windows(2) {
            if on_circle(&pair[0]) && on_circle(&pair[1]) && (pair[0][0] - pair[1][0]).hypot(pair[0][1] - pair[1][1]) < 50.0 {
                let mid = [(pair[0][0] + pair[1][0]) / 2.0, (pair[0][1] + pair[1][1]) / 2.0];
                let sagitta = radius_pt - (mid[0] - cx).hypot(mid[1] - cy);
                assert!(sagitta <= tolerance_pt + 2.0 * r_pt, "chord error {sagitta} pt > {tolerance_pt} pt");
                chords += 1;
            }
        }
        // The editor's 48-segment circle would have a 1.6 mm chord error here.
        assert!(chords > 48, "only {chords} chords on the circle");

        // The PDF and the snap file draw the same polyline.
        let snaps = crate::app::secureplan::snap::extract(&publication);
        let circle_ends: Vec<[f32; 2]> = snaps
            .segments
            .iter()
            .flat_map(|s| [[s[0], s[1]], [s[2], s[3]]])
            .filter(|p| on_circle(&[p[0] as f64, p[1] as f64]))
            .collect();
        assert!(!circle_ends.is_empty());
        for end in circle_ends {
            assert!(points.iter().any(|p| (p[0] - end[0] as f64).abs() < 1e-3 && (p[1] - end[1] as f64).abs() < 1e-3), "{end:?} not drawn");
        }

        // Zooming the editor changes nothing in the published page.
        for (width, height) in [(10.0, 10.0), (20000.0, 20000.0)] {
            scene.set_render_pixel_scale(width, height);
            assert_eq!(publish(&scene, transform).0.bytes, pdf.bytes, "viewport {width}x{height} changed the PDF");
        }
    }

    #[test]
    fn drawing_unit_lengths_follow_a_non_default_publication_scale() {
        use acadrust::tables::LineType;
        // A feet drawing at 5 mm per point: 60.96 points per drawing unit.
        let mut doc = CadDocument::new();
        doc.line_types.add(LineType::dashed()).unwrap();
        let mut wide = LwPolyline::from_points(vec![Vector2::new(10.0, 10.0), Vector2::new(40.0, 10.0)]);
        wide.constant_width = 2.0;
        doc.add_entity(EntityType::LwPolyline(wide)).unwrap();
        let mut dashed = Line::from_points(Vector3::new(10.0, 30.0, 0.0), Vector3::new(40.0, 30.0, 0.0));
        dashed.common.linetype = "Dashed".into();
        doc.add_entity(EntityType::Line(dashed)).unwrap();
        let mut scene = crate::scene::Scene::new();
        scene.document = doc;
        scene.rebuild_derived_caches();
        let mapping = Mapping { cad_origin: [0.0, 50.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 304.8, quarter_turns: 0 };
        let placement = place_page([0.0, 0.0, 50.0, 50.0], &mapping, 5.0).unwrap();
        let transform = PageTransform { mapping, placement };
        let unit = transform.points_per_cad_unit();
        assert!((unit - 60.96).abs() < 1e-9);
        let (pdf, _) = publish(&scene, transform);
        let ops = operations(&pdf.bytes);
        let widths: Vec<f64> = ops.iter().filter(|op| op.operator == "w").map(|op| number(&op.operands[0])).collect();
        assert!(widths.iter().any(|w| (w - 2.0 * unit).abs() < 0.01), "polyline width not scaled: {widths:?}");
        let dashes: Vec<Vec<f64>> = ops
            .iter()
            .filter(|op| op.operator == "d")
            .map(|op| op.operands[0].as_array().unwrap().iter().map(number).collect())
            .filter(|dash: &Vec<f64>| !dash.is_empty())
            .collect();
        assert!(dashes.iter().any(|dash| dash == &vec![(0.5 * unit).round(), (0.25 * unit).round()]), "dashes not scaled: {dashes:?}");
    }
    /// A 3000 × 3000 mm window at 1 mm per point, one drawing unit per mm.
    fn square_transform() -> PageTransform {
        let mapping = Mapping { cad_origin: [0.0, 3000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        PageTransform { mapping, placement: place_page([0.0, 0.0, 3000.0, 3000.0], &mapping, 1.0).unwrap() }
    }

    fn replaced(publication: &Publication, handle: acadrust::types::Handle) -> LwPolyline {
        match publication.scene.document.get_entity(handle) {
            Some(EntityType::LwPolyline(polyline)) => polyline.clone(),
            other => panic!("not replaced by a polyline: {other:?}"),
        }
    }

    /// Widths along a replaced polyline: each piece starts where the previous
    /// ended, and the widths at the original vertices are the stored ones.
    fn assert_widths(polyline: &LwPolyline, at: &[((f64, f64), f64, f64)]) {
        let vertices = &polyline.vertices;
        // The last vertex starts no segment, so its widths are not drawn.
        for pair in vertices[..vertices.len() - 1].windows(2) {
            let joint = (pair[1].location.x, pair[1].location.y);
            if !at.iter().any(|(p, _, _)| (p.0 - joint.0).hypot(p.1 - joint.1) < 1e-6) {
                assert!((pair[0].end_width - pair[1].start_width).abs() < 1e-9, "width jumps at {joint:?}");
            }
        }
        for ((x, y), start, end_before) in at {
            let i = vertices.iter().position(|v| (v.location.x - x).hypot(v.location.y - y) < 1e-6).unwrap_or_else(|| panic!("({x}, {y}) is not a vertex"));
            assert!((vertices[i].start_width - start).abs() < 1e-9, "start width at ({x}, {y}): {}", vertices[i].start_width);
            if i > 0 {
                assert!((vertices[i - 1].end_width - end_before).abs() < 1e-9, "end width before ({x}, {y}): {}", vertices[i - 1].end_width);
            }
        }
    }

    #[test]
    fn replaced_polylines_keep_and_interpolate_their_widths() {
        use acadrust::entities::{LwVertex, Polyline2D, Vertex2D};
        let mut doc = CadDocument::new();
        // A bulged segment tapering from 10 to 30, then a straight one at 30.
        let vertex = |x: f64, y: f64, bulge: f64, start: f64, end: f64| {
            let mut v = LwVertex::new(Vector2::new(x, y));
            v.bulge = bulge;
            v.start_width = start;
            v.end_width = end;
            v
        };
        let mut tapered = LwPolyline::new();
        tapered.vertices = vec![vertex(0.0, 1000.0, 0.5, 10.0, 30.0), vertex(1000.0, 1000.0, 0.0, 30.0, 30.0), vertex(1000.0, 2000.0, 0.0, 0.0, 0.0)];
        let lw = doc.add_entity(EntityType::LwPolyline(tapered)).unwrap();
        // A legacy POLYLINE: default widths 20 → 40 on its bulged segment, its own 5 → 15 on the next.
        let mut legacy = Polyline2D::new();
        legacy.start_width = 20.0;
        legacy.end_width = 40.0;
        let mut first = Vertex2D::new(Vector3::new(2000.0, 1000.0, 0.0));
        first.bulge = 1.0;
        legacy.add_vertex(first);
        let mut second = Vertex2D::new(Vector3::new(2800.0, 1000.0, 0.0));
        second.start_width = 5.0;
        second.end_width = 15.0;
        legacy.add_vertex(second);
        legacy.add_vertex(Vertex2D::new(Vector3::new(2800.0, 2000.0, 0.0)));
        let pl = doc.add_entity(EntityType::Polyline2D(legacy)).unwrap();
        let scene = crate::app::secureplan::snap::tests::scene_of(doc);
        let transform = square_transform();
        let (pdf, publication) = publish(&scene, transform);

        let lw = replaced(&publication, lw);
        assert!(lw.vertices.len() > 10, "the bulge was not tessellated");
        assert_widths(&lw, &[((0.0, 1000.0), 10.0, 0.0), ((1000.0, 1000.0), 30.0, 30.0)]);
        let pl = replaced(&publication, pl);
        assert_widths(&pl, &[((2000.0, 1000.0), 20.0, 0.0), ((2800.0, 1000.0), 5.0, 40.0)]);

        // The PDF strokes both at their widest, as the exporter does for any
        // tapered polyline (one drawing unit is one point here).
        let widths: Vec<f64> = operations(&pdf.bytes).iter().filter(|op| op.operator == "w").map(|op| number(&op.operands[0])).collect();
        for width in [30.0, 40.0] {
            assert!(widths.iter().any(|w| (w - width).abs() < 0.01), "no stroke {width} wide: {widths:?}");
        }
    }

    /// The page point of a block point drawn through an INSERT with the given
    /// normal, at `insert_at` in its OCS, unrotated and unscaled.
    fn through_insert(transform: &PageTransform, normal: Vector3, insert_at: (f64, f64, f64), point: (f64, f64, f64)) -> (f64, f64) {
        let ocs = (insert_at.0 + point.0, insert_at.1 + point.1, insert_at.2 + point.2);
        let (x, y, _) = crate::scene::view::transform::ocs_point_to_wcs(ocs, (normal.x, normal.y, normal.z));
        transform.apply(x, y)
    }

    #[test]
    fn an_elevated_arc_in_a_tilted_insert_keeps_its_height_until_placed() {
        use acadrust::entities::Insert;
        let mut doc = CadDocument::new();
        let mut arc = Arc::new();
        arc.center = Vector3::new(0.0, 0.0, 500.0);
        arc.radius = 100.0;
        arc.start_angle = 0.0;
        arc.end_angle = std::f64::consts::FRAC_PI_2;
        crate::app::secureplan::snap::tests::block(&mut doc, "RAISED", vec![EntityType::Arc(arc)]);
        let tilt = 30.0_f64.to_radians();
        let normal = Vector3::new(0.0, -tilt.sin(), tilt.cos());
        let mut insert = Insert::new("RAISED", Vector3::new(1000.0, 1000.0, 0.0));
        insert.normal = normal;
        doc.add_entity(EntityType::Insert(insert)).unwrap();
        let scene = crate::app::secureplan::snap::tests::scene_of(doc);
        let transform = square_transform();
        let (pdf, publication) = publish(&scene, transform);
        let snaps = crate::app::secureplan::snap::extract(&publication);
        let drawn = content_points(&pdf.bytes);
        let at = |x: f64, y: f64| through_insert(&transform, normal, (1000.0, 1000.0, 0.0), (x, y, 500.0));
        let near = |points: &[[f64; 2]], (x, y): (f64, f64), within: f64| points.iter().any(|p| (p[0] - x).hypot(p[1] - y) < within);
        for end in [at(100.0, 0.0), at(0.0, 100.0)] {
            assert!(near(&drawn, end, 1e-3), "PDF misses the arc end {end:?}");
            assert!(crate::app::secureplan::snap::tests::has_point(&snaps, end), "no end-point snap at {end:?}");
            assert!(crate::app::secureplan::snap::tests::on_segments(&snaps, end), "no snap segment ends at {end:?}");
        }
        // The middle of the arc is drawn and snappable too, within the chord tolerance.
        let middle = at(100.0 * std::f64::consts::FRAC_1_SQRT_2, 100.0 * std::f64::consts::FRAC_1_SQRT_2);
        let ends: Vec<[f64; 2]> = snaps.segments.iter().flat_map(|s| [[s[0] as f64, s[1] as f64], [s[2] as f64, s[3] as f64]]).collect();
        assert!(near(&drawn, middle, 3.0) && near(&ends, middle, 3.0), "the arc is not where the insert places it");
    }

    #[test]
    fn nested_non_uniform_inserts_get_the_full_stretch() {
        use acadrust::entities::Insert;
        let mut doc = CadDocument::new();
        let mut circle = Circle::new();
        circle.radius = 100.0;
        crate::app::secureplan::snap::tests::block(&mut doc, "INNER", vec![EntityType::Circle(circle)]);
        let mut inner = Insert::new("INNER", Vector3::new(0.0, 0.0, 0.0));
        inner.rotation = std::f64::consts::FRAC_PI_4;
        crate::app::secureplan::snap::tests::block(&mut doc, "OUTER", vec![EntityType::Insert(inner)]);
        let outer = Insert::new("OUTER", Vector3::new(1500.0, 1500.0, 0.0)).with_scale(10.0, 1.0, 1.0);
        doc.add_entity(EntityType::Insert(outer)).unwrap();
        let scene = crate::app::secureplan::snap::tests::scene_of(doc);
        let transform = square_transform();
        let mut placed = Vec::new();
        walk_model(&scene, false, |entity, context| {
            if let EntityType::Circle(_) = entity {
                placed.push((entity.common().handle, context.transform));
            }
        });
        let [(handle, instance)] = placed.as_slice() else { panic!("one circle instance expected") };
        assert!((plan_scale(instance) - 10.0).abs() < 1e-9, "stretch {}", plan_scale(instance));
        let publication = prepare_model(&scene, transform).unwrap();
        let polyline = replaced(&publication, *handle);
        let world = |x: f64, y: f64| instance.apply(Vector3::new(x, y, 0.0));
        let mut worst = 0.0_f64;
        for pair in polyline.vertices.windows(2) {
            let (a, b) = (pair[0].location, pair[1].location);
            let middle = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
            let angle = middle.1.atan2(middle.0);
            let on_curve = world(100.0 * angle.cos(), 100.0 * angle.sin());
            let on_chord = world(middle.0, middle.1);
            worst = worst.max((on_curve.x - on_chord.x).hypot(on_curve.y - on_chord.y));
        }
        let tolerance = transform.placement.chord_tolerance_mm;
        assert!(worst <= tolerance * (1.0 + 1e-6), "chord error {worst} mm > {tolerance} mm");
    }

    #[test]
    fn curves_beyond_the_kernel_cap_still_meet_the_tolerance_or_are_refused() {
        use acadrust::entities::Ellipse;
        let transform = square_transform();
        let tolerance = transform.placement.chord_tolerance_mm;
        // A 100 km circle whose top crosses the window needs far more than the
        // kernel's 16,384 segments.
        let mut doc = CadDocument::new();
        let mut circle = Circle::new();
        circle.center = Vector3::new(1500.0, 1500.0 - 1.0e8, 0.0);
        circle.radius = 1.0e8;
        let handle = doc.add_entity(EntityType::Circle(circle.clone())).unwrap();
        let publication = prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).unwrap();
        let polyline = replaced(&publication, handle);
        assert!(polyline.vertices.len() > 16_385, "{} vertices", polyline.vertices.len());
        let worst = polyline
            .vertices
            .windows(2)
            .map(|pair| {
                let middle = ((pair[0].location.x + pair[1].location.x) / 2.0, (pair[0].location.y + pair[1].location.y) / 2.0);
                1.0e8 - (middle.0 - circle.center.x).hypot(middle.1 - circle.center.y)
            })
            .fold(0.0, f64::max);
        assert!(worst <= tolerance * (1.0 + 1e-6), "chord error {worst} mm > {tolerance} mm");

        // Past what any plan needs, curves are refused rather than drawn out of tolerance.
        let mut doc = CadDocument::new();
        circle.radius = 1.0e13;
        circle.center = Vector3::new(1500.0, 1500.0 - 1.0e13, 0.0);
        doc.add_entity(EntityType::Circle(circle)).unwrap();
        let refused = prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).err().expect("refused");
        assert!(refused.contains("too large"), "{refused}");
        // A 100 km ellipse is cut in steps its curvature bound allows, far
        // past the kernel's cap; one a hundred thousand times larger is refused.
        let mut ellipse = Ellipse::new();
        ellipse.center = Vector3::new(1500.0, 1500.0 - 1.0e8, 0.0);
        ellipse.major_axis = Vector3::new(0.0, 1.0e8, 0.0);
        ellipse.minor_axis_ratio = 0.5;
        let mut doc = CadDocument::new();
        let handle = doc.add_entity(EntityType::Ellipse(ellipse.clone())).unwrap();
        let publication = prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).unwrap();
        assert!(replaced(&publication, handle).vertices.len() > 16_385);
        ellipse.major_axis = Vector3::new(0.0, 1.0e13, 0.0);
        ellipse.center = Vector3::new(1500.0, 1500.0 - 1.0e13, 0.0);
        let mut doc = CadDocument::new();
        doc.add_entity(EntityType::Ellipse(ellipse)).unwrap();
        let refused = prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).err().expect("refused");
        assert!(refused.contains("too large"), "{refused}");
    }

    /// A 60 × 40 m window far from the origin at 1 mm per drawing unit, as
    /// for a site drawn in map coordinates, at the scale Apply chooses.
    fn far_transform() -> PageTransform {
        let window = [500_000.0, -200_000.0, 560_000.0, -160_000.0];
        let mapping = super::super::align::mapping_at(window, 1.0, 0, [0.0, 0.0]);
        PageTransform { mapping, placement: place_page(window, &mapping, choose_mm_per_pt(window, &mapping).unwrap()).unwrap() }
    }

    /// The exact arc of the segment from `a` to `b` with `bulge`, by its
    /// parameter in 0..=1, about its centre (exact far below the tolerance
    /// for the moderate bulges it is used with).
    fn bulge_arc(a: [f64; 2], b: [f64; 2], bulge: f64) -> impl Fn(f64) -> [f64; 2] {
        let theta = 4.0 * bulge.atan();
        let chord = (b[0] - a[0]).hypot(b[1] - a[1]);
        let radius = chord / (2.0 * (theta / 2.0).sin().abs());
        let offset = radius * (theta / 2.0).cos() * theta.signum();
        let centre = [(a[0] + b[0]) / 2.0 - (b[1] - a[1]) / chord * offset, (a[1] + b[1]) / 2.0 + (b[0] - a[0]) / chord * offset];
        let start = (a[1] - centre[1]).atan2(a[0] - centre[0]);
        move |t| [centre[0] + radius * (start + theta * t).cos(), centre[1] + radius * (start + theta * t).sin()]
    }

    /// Each side of a cut chain of `corners` (x, y, bulge) starts at its
    /// stored vertex exactly and stays within `tolerance` of its exact arc; a
    /// near-straight side, whose whole sagitta |bulge|·chord/2 is within the
    /// tolerance, is its chord alone.
    fn assert_sides_within(points: &[[f64; 2]], corners: &[[f64; 3]], closed: bool, tolerance: f64) {
        let sides = if closed { corners.len() } else { corners.len() - 1 };
        let mut from = 0;
        for (i, [ax, ay, bulge]) in corners[..sides].iter().copied().enumerate() {
            let [bx, by, _] = corners[(i + 1) % corners.len()];
            assert_eq!(points[from], [ax, ay], "side {i} does not start at its vertex");
            let to = from + 1 + points[from + 1..].iter().position(|p| *p == [bx, by]).unwrap_or_else(|| panic!("side {i} does not end at its vertex"));
            let side = &points[from..=to];
            let chord = (bx - ax).hypot(by - ay);
            if bulge.abs() * chord / 2.0 <= tolerance / 2.0 {
                assert_eq!(side.len(), 2, "side {i} (bulge {bulge}) is not its chord");
            } else {
                let departure = polyline_departure(side, bulge_arc([ax, ay], [bx, by], bulge));
                assert!(departure <= tolerance * (1.0 + 1e-6), "side {i} leaves its arc by {departure} mm");
            }
            from = to;
        }
        assert_eq!(from, points.len() - 1, "points past the last side");
    }

    /// The kernel cuts a clockwise bulge of a hatch boundary with its ends
    /// swapped, so its points ran backwards between the pinned vertices, the
    /// chord check failed and Apply refused the hatch as too large: any hatch
    /// bounded by a clockwise arc (a rounded corner, a door swing). Each arc
    /// is cut in order, within the tolerance.
    #[test]
    fn hatch_boundaries_with_clockwise_bulges_are_published() {
        use acadrust::entities::{BoundaryEdge, BoundaryPath, Hatch, PolylineEdge};
        let transform = far_transform();
        let tolerance = transform.placement.chord_tolerance_mm;
        let (x, y) = (540_000.0, -175_000.0);
        let corners = [[x, y, 0.0], [x + 2000.0, y, -1.0], [x + 2000.0, y + 1000.0, 0.0], [x, y + 1000.0, -0.414]];
        let mut path = BoundaryPath::new();
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge { vertices: corners.iter().map(|[x, y, b]| Vector3::new(*x, *y, *b)).collect(), is_closed: true }));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        let flat = flatten_hatch(&hatch, tolerance).expect("the hatch is published");
        let BoundaryEdge::Polyline(edge) = &flat.paths[0].edges[0] else { panic!("the boundary was not cut") };
        let points: Vec<[f64; 2]> = edge.vertices.iter().map(|v| [v.x, v.y]).collect();
        assert_sides_within(&points, &corners, true, tolerance);
        let mut doc = CadDocument::new();
        doc.add_entity(EntityType::Hatch(hatch)).unwrap();
        prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).expect("Apply publishes the hatch");
    }

    /// A hatch boundary segment whose bulge is rounding noise (a sweep below
    /// 1e-9 rad) was read by the kernel as a full circle thousands of
    /// kilometres across, cut at its 16,384-chord cap, and the hatch was
    /// refused as too large. It is its chord; a real bulge on the same
    /// boundary is still cut within the tolerance.
    #[test]
    fn hatch_boundaries_with_near_straight_bulges_are_published() {
        use acadrust::entities::{BoundaryEdge, BoundaryPath, Hatch, PolylineEdge};
        let transform = far_transform();
        let tolerance = transform.placement.chord_tolerance_mm;
        let (x, y) = (520_000.0, -185_000.0);
        let corners = [[x, y, 1e-10], [x + 20_000.0, y, -3e-12], [x + 20_000.0, y + 10_000.0, 0.4], [x, y + 10_000.0, 0.0]];
        let mut path = BoundaryPath::new();
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge { vertices: corners.iter().map(|[x, y, b]| Vector3::new(*x, *y, *b)).collect(), is_closed: true }));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        let flat = flatten_hatch(&hatch, tolerance).expect("the hatch is published");
        let BoundaryEdge::Polyline(edge) = &flat.paths[0].edges[0] else { panic!("the boundary was not cut") };
        let points: Vec<[f64; 2]> = edge.vertices.iter().map(|v| [v.x, v.y]).collect();
        assert_sides_within(&points, &corners, true, tolerance);
        let mut doc = CadDocument::new();
        doc.add_entity(EntityType::Hatch(hatch)).unwrap();
        prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).expect("Apply publishes the hatch");
    }

    /// A 20 m polyline segment with a bulge of 1.5e-12 has a radius near
    /// 3e15 mm: `1 − tol/r` rounded to 1, the step to 0, and the polyline was
    /// refused as too large. Near-straight segments are their chords, and
    /// every side starts at its stored vertex exactly (not a rounding of a
    /// far-away centre).
    #[test]
    fn polyline_segments_with_near_straight_bulges_are_published() {
        use acadrust::entities::LwVertex;
        let transform = far_transform();
        let tolerance = transform.placement.chord_tolerance_mm;
        let (x, y) = (530_000.0, -170_000.0);
        let corners = [[x, y, 1.5e-12], [x + 20_000.0, y, 2e-9], [x + 20_000.0, y + 15_000.0, -0.25], [x, y + 15_000.0, 0.0]];
        let mut polyline = LwPolyline::new();
        polyline.vertices = corners
            .iter()
            .map(|[x, y, bulge]| {
                let mut vertex = LwVertex::new(Vector2::new(*x, *y));
                vertex.bulge = *bulge;
                vertex
            })
            .collect();
        let (flat, _) = flatten(&EntityType::LwPolyline(polyline.clone()), tolerance).expect("the polyline is published").expect("replaced");
        let points: Vec<[f64; 2]> = flat.vertices.iter().map(|v| [v.location.x, v.location.y]).collect();
        assert_sides_within(&points, &corners, false, tolerance);
        let mut doc = CadDocument::new();
        doc.add_entity(EntityType::LwPolyline(polyline)).unwrap();
        prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).expect("Apply publishes the polyline");
    }

    /// Hatch arc edges are cut analytically, like arcs, with no kernel cap:
    /// a 100 km circular edge needs far more than 16,384 chords.
    #[test]
    fn hatch_arc_edges_beyond_the_kernel_cap_meet_the_tolerance() {
        use acadrust::entities::{BoundaryEdge, BoundaryPath, CircularArcEdge, Hatch};
        let transform = far_transform();
        let tolerance = transform.placement.chord_tolerance_mm;
        let (cx, cy, radius) = (530_000.0, -180_000.0 - 1.0e8, 1.0e8);
        let mut path = BoundaryPath::new();
        path.add_edge(BoundaryEdge::CircularArc(CircularArcEdge {
            center: Vector2::new(cx, cy),
            radius,
            start_angle: 0.0,
            end_angle: std::f64::consts::TAU,
            counter_clockwise: true,
        }));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        let flat = flatten_hatch(&hatch, tolerance).expect("the hatch is published");
        let BoundaryEdge::Polyline(edge) = &flat.paths[0].edges[0] else { panic!("the boundary was not cut") };
        assert!(edge.vertices.len() > 16_385, "{} vertices", edge.vertices.len());
        let worst = edge
            .vertices
            .windows(2)
            .map(|pair| radius - ((pair[0].x + pair[1].x) / 2.0 - cx).hypot((pair[0].y + pair[1].y) / 2.0 - cy))
            .fold(0.0, f64::max);
        assert!(worst <= tolerance * (1.0 + 1e-6), "chord error {worst} mm > {tolerance} mm");
    }

    /// A curve that is refused is named, with a point on it in drawing units
    /// (through its block reference), so the user can find it.
    #[test]
    fn a_refused_curve_is_named_with_its_drawing_position() {
        use acadrust::entities::Insert;
        let transform = far_transform();
        // A 1 rad arc of a radius of 1e13 mm needs millions of chords.
        let mut arc = Arc::new();
        arc.center = Vector3::new(0.0, -1.0e13, 0.0);
        arc.radius = 1.0e13;
        arc.start_angle = std::f64::consts::FRAC_PI_2;
        arc.end_angle = std::f64::consts::FRAC_PI_2 + 1.0;
        let mut doc = CadDocument::new();
        crate::app::secureplan::snap::tests::block(&mut doc, "HUGE", vec![EntityType::Arc(arc)]);
        doc.add_entity(EntityType::Insert(Insert::new("HUGE", Vector3::new(525_000.0, -180_000.0, 0.0)))).unwrap();
        let refused = prepare_model(&crate::app::secureplan::snap::tests::scene_of(doc), transform).err().expect("refused");
        assert!(refused.starts_with("An arc near (525000.00, -180000.00) is too large"), "{refused}");
        assert!(!refused.contains("scale"), "{refused}");
    }
}
