//! CAD geometry to SecurePlan wall and route candidates (CNV-01, CNV-02).
//!
//! The user selects model-space geometry in a clean drawing whose plan is the
//! survey's current one and chooses **Create SecurePlan walls** or **Create
//! cable route**. The selection is walked through the renderer's own block
//! transforms (nested, arrayed and mirrored instances included, with the
//! visibility the drawing shows), curves are cut into chords that depart from
//! the curve by at most 1 mm in the plan, and every point is mapped to world
//! millimetres with the survey's stored mapping:
//!
//! - **Walls:** every straight segment is a candidate; one shorter than
//!   150 mm is rejected (`tooShort`) at its middle.
//! - **Route:** the pieces must join end to end (ends within 1 mm are one
//!   point) into one ordered path. A gap or a branch refuses the route with its
//!   location; nothing is sent. A point within 1 mm of the one before it is
//!   dropped (`pointsTooClose`), and the route must be at least 150 mm long.
//!
//! A curve that cannot be cut within 1 mm (a spline through space, or one far
//! larger than any plan) is `unsupportedCurve`. Selected objects that are not
//! lines or curves (text, hatches, dimensions, …) are skipped and counted.
//! Nothing here reads CAD layer names: the web asks for layer, status and
//! material (CNV-03).

use std::collections::HashSet;

use acadrust::types::{Handle, Vector3};
use acadrust::EntityType;
use serde_json::{json, Value};

use super::publish::Mapping;

/// The chord error allowed when a curve is converted, in world mm.
pub const CHORD_TOLERANCE_MM: f64 = 1.0;
/// The shortest wall and the shortest route (world mm).
pub const MIN_LENGTH_MM: f64 = 150.0;
/// The closest two consecutive route points may be, and how close two ends
/// must be to join (world mm).
pub const MIN_POINT_SPACING_MM: f64 = 1.0;
/// Schema limits (`convert-candidates.schema.json`).
pub const MAX_WALLS: usize = 10_000;
pub const MAX_ROUTE_POINTS: usize = 10_000;
pub const MAX_REJECTED: usize = 1_000;
/// Separate groups of pieces whose gaps are located (the notice names five).
const MAX_GAP_GROUPS: usize = 64;
/// World coordinates the protocol accepts (`coordinateMm`).
const MAX_COORDINATE_MM: f64 = 10_000_000.0;

/// What the user asked SecurePlan to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Walls,
    Route,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Walls => "walls",
            Kind::Route => "route",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "walls" | "wall" => Some(Kind::Walls),
            "route" | "cable" => Some(Kind::Route),
            _ => None,
        }
    }
}

/// Why a part of the selection was not converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Gap,
    Branch,
    TooShort,
    PointsTooClose,
    UnsupportedCurve,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Gap => "gap",
            Reason::Branch => "branch",
            Reason::TooShort => "tooShort",
            Reason::PointsTooClose => "pointsTooClose",
            Reason::UnsupportedCurve => "unsupportedCurve",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Reason::Gap => "a gap between pieces",
            Reason::Branch => "a branch",
            Reason::TooShort => "shorter than 150 mm",
            Reason::PointsTooClose => "a point within 1 mm of the one before it",
            Reason::UnsupportedCurve => "a curve that cannot be cut within 1 mm",
        }
    }
}

/// A refused part and where it is, in world mm.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rejected {
    pub reason: Reason,
    pub at_mm: [f64; 2],
}

/// The candidates `convertRequest` carries (`convert-candidates.schema.json`).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidates {
    pub kind: Kind,
    /// Walls: (start, end) in world mm.
    pub walls: Vec<([f64; 2], [f64; 2])>,
    /// Route: the ordered path in world mm.
    pub points: Vec<[f64; 2]>,
    /// At most [`MAX_REJECTED`]; `rejected_total` counts them all.
    pub rejected: Vec<Rejected>,
    pub rejected_total: usize,
    /// Selected objects that are not lines or curves.
    pub skipped: usize,
}

impl Candidates {
    /// The `candidates` of `convertRequest`.
    pub fn to_json(&self) -> Value {
        let rejected: Vec<Value> = self.rejected.iter().map(|r| json!({ "reason": r.reason.as_str(), "atMm": r.at_mm })).collect();
        match self.kind {
            Kind::Walls => json!({
                "kind": "walls",
                "walls": self.walls.iter().map(|(start, end)| json!({ "start": start, "end": end })).collect::<Vec<_>>(),
                "rejected": rejected,
            }),
            Kind::Route => json!({ "kind": "route", "points": self.points, "rejected": rejected }),
        }
    }

    /// The route's length in world mm.
    pub fn route_length_mm(&self) -> f64 {
        self.points.windows(2).map(|pair| distance(pair[0], pair[1])).sum()
    }

    /// One line for the user: what is sent and what is not.
    pub fn summary(&self) -> String {
        let sent = match self.kind {
            Kind::Walls => format!("{} wall candidate(s)", self.walls.len()),
            Kind::Route => format!("a cable route of {} points, {:.0} mm long", self.points.len(), self.route_length_mm()),
        };
        let mut line = format!("SecurePlan: sent {sent}.");
        if self.rejected_total > 0 {
            line.push_str(&format!(" {} part(s) were not converted.", self.rejected_total));
        }
        if self.skipped > 0 {
            line.push_str(&format!(" {} selected object(s) are not lines or curves and were skipped.", self.skipped));
        }
        line.push_str(" Choose the layer, status and material in SecurePlan.");
        line
    }
}

/// Why nothing can be sent, with the places to look at (world mm).
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub message: String,
    pub at_mm: Vec<[f64; 2]>,
}

impl Refusal {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), at_mm: Vec::new() }
    }

    fn at(reason: Reason, places: Vec<[f64; 2]>) -> Self {
        let message = match reason {
            Reason::Gap => "The selection is not one connected path: the pieces do not meet.",
            Reason::Branch => "The selection branches: a cable route must be one path without branches.",
            Reason::UnsupportedCurve => "The selection has a curve that cannot be cut within 1 mm, so the route would have a gap.",
            Reason::TooShort | Reason::PointsTooClose => "The route is shorter than 150 mm.",
        };
        Self { message: message.into(), at_mm: places }
    }
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    (b[0] - a[0]).hypot(b[1] - a[1])
}

fn midpoint(a: [f64; 2], b: [f64; 2]) -> [f64; 2] {
    [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]
}

/// One selected line or curve instance as a chain of world points.
#[derive(Debug, Clone, PartialEq)]
struct Chain {
    points: Vec<[f64; 2]>,
}

/// What the selection is made of.
#[derive(Debug, Default)]
struct Pieces {
    chains: Vec<Chain>,
    unsupported: Vec<[f64; 2]>,
    skipped: usize,
}

/// Walk the selected top-level model-space entities (block references are
/// followed into their content) and cut every line and curve into a chain of
/// world points within [`CHORD_TOLERANCE_MM`].
fn pieces<S: std::hash::BuildHasher>(scene: &crate::scene::Scene, selected: &HashSet<Handle, S>, mapping: &Mapping) -> Pieces {
    let mut pieces = Pieces::default();
    let block = scene.current_layout_block_handle_pub();
    let to_world = |transform: &acadrust::types::Transform, point: Vector3| {
        let placed = transform.apply(point);
        mapping.cad_to_world([placed.x, placed.y])
    };
    super::publish::walk_block_where(
        scene,
        block,
        None,
        // Only the selection, and everything inside a selected block reference.
        |entity, context| context.is_instanced() || selected.contains(&entity.common().handle),
        |entity, context| {
            let transform = &context.transform;
            let chain: Option<Vec<[f64; 2]>> = match entity {
                EntityType::Line(line) => Some(vec![to_world(transform, line.start), to_world(transform, line.end)]),
                EntityType::Polyline3D(polyline) => {
                    // The drawn vertices: not a spline fit's frame.
                    let mut points: Vec<[f64; 2]> =
                        polyline.vertices.iter().filter(|v| v.flags & 16 == 0).map(|v| to_world(transform, v.position)).collect();
                    if polyline.is_closed() && points.len() > 2 {
                        points.push(points[0]);
                    }
                    Some(points)
                }
                EntityType::LwPolyline(_)
                | EntityType::Polyline2D(_)
                | EntityType::Arc(_)
                | EntityType::Circle(_)
                | EntityType::Ellipse(_)
                | EntityType::Spline(_) => {
                    // The tolerance in the entity's own units, for this instance.
                    let local = CHORD_TOLERANCE_MM / (mapping.scale_mm_per_cad_unit * super::publish::plan_scale(transform));
                    match super::publish::flatten(entity, local) {
                        Ok(Some((polyline, _))) => Some(
                            polyline
                                .vertices
                                .iter()
                                .map(|v| {
                                    let [x, y, z] = super::publish::ocs_to_wcs(polyline.normal, (v.location.x, v.location.y), polyline.elevation);
                                    to_world(transform, Vector3::new(x, y, z))
                                })
                                .collect(),
                        ),
                        Ok(None) | Err(_) => {
                            pieces.unsupported.push(to_world(transform, curve_anchor(entity)));
                            None
                        }
                    }
                }
                _ => {
                    // Text, hatches, points, solids, …: not linework.
                    if !context.is_instanced() {
                        pieces.skipped += 1;
                    }
                    None
                }
            };
            if let Some(points) = chain.filter(|points| points.len() >= 2) {
                pieces.chains.push(Chain { points });
            }
        },
    );
    pieces
}

/// A point of a curve to report it at.
fn curve_anchor(entity: &EntityType) -> Vector3 {
    match entity {
        EntityType::Ellipse(ellipse) => ellipse.center,
        EntityType::Spline(spline) => spline.fit_points.first().or(spline.control_points.first()).copied().unwrap_or(Vector3::ZERO),
        EntityType::Circle(circle) => circle.center,
        EntityType::Arc(arc) => arc.center,
        _ => Vector3::ZERO,
    }
}

fn in_range(point: [f64; 2]) -> bool {
    point.iter().all(|v| v.is_finite() && v.abs() <= MAX_COORDINATE_MM)
}

/// Record a rejected part, keeping at most [`MAX_REJECTED`].
fn reject(rejected: &mut Vec<Rejected>, total: &mut usize, reason: Reason, at_mm: [f64; 2]) {
    *total += 1;
    if rejected.len() < MAX_REJECTED {
        rejected.push(Rejected { reason, at_mm });
    }
}

/// The candidates for `kind` from the selected entities of `scene` (its model
/// space), mapped with `mapping`.
pub fn candidates<S: std::hash::BuildHasher>(
    scene: &crate::scene::Scene,
    selected: &HashSet<Handle, S>,
    mapping: &Mapping,
    kind: Kind,
) -> Result<Candidates, Refusal> {
    let pieces = pieces(scene, selected, mapping);
    if pieces.chains.iter().flat_map(|c| &c.points).chain(&pieces.unsupported).any(|p| !in_range(*p)) {
        return Err(Refusal::new("Part of the selection lies outside the survey's coordinate range (±10 km)."));
    }
    let result = match kind {
        Kind::Walls => walls(&pieces),
        Kind::Route => route(&pieces),
    };
    result.map(|mut candidates| {
        candidates.skipped = pieces.skipped;
        candidates
    })
}

fn walls(pieces: &Pieces) -> Result<Candidates, Refusal> {
    let (mut rejected, mut total) = (Vec::new(), 0);
    let mut walls = Vec::new();
    for &at in &pieces.unsupported {
        reject(&mut rejected, &mut total, Reason::UnsupportedCurve, at);
    }
    for chain in &pieces.chains {
        for pair in chain.points.windows(2) {
            let length = distance(pair[0], pair[1]);
            // A repeated vertex is no part at all.
            if length <= 1e-9 {
                continue;
            }
            if length < MIN_LENGTH_MM {
                reject(&mut rejected, &mut total, Reason::TooShort, midpoint(pair[0], pair[1]));
            } else {
                walls.push((pair[0], pair[1]));
            }
        }
    }
    if walls.is_empty() {
        let message = if pieces.chains.is_empty() && pieces.unsupported.is_empty() {
            "The selection has no lines or curves to convert."
        } else {
            "No straight segment of the selection is at least 150 mm long."
        };
        return Err(Refusal { message: message.into(), at_mm: rejected.iter().map(|r| r.at_mm).take(5).collect() });
    }
    if walls.len() > MAX_WALLS {
        return Err(Refusal::new(format!("The selection makes {} walls; SecurePlan takes at most {MAX_WALLS} at a time. Select fewer.", walls.len())));
    }
    Ok(Candidates { kind: Kind::Walls, walls, points: Vec::new(), rejected, rejected_total: total, skipped: 0 })
}

/// Points of `chains` joined when within [`MIN_POINT_SPACING_MM`].
struct Nodes(Vec<[f64; 2]>);

impl Nodes {
    fn of(&mut self, point: [f64; 2]) -> usize {
        match self.0.iter().position(|node| distance(*node, point) < MIN_POINT_SPACING_MM) {
            Some(index) => index,
            None => {
                self.0.push(point);
                self.0.len() - 1
            }
        }
    }
}

fn root(parent: &mut [usize], mut node: usize) -> usize {
    while parent[node] != node {
        parent[node] = parent[parent[node]];
        node = parent[node];
    }
    node
}

/// The distance from `point` to the segment `a`–`b`.
fn to_segment(point: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let length2 = dx * dx + dy * dy;
    if length2 == 0.0 {
        return distance(point, a);
    }
    let t = (((point[0] - a[0]) * dx + (point[1] - a[1]) * dy) / length2).clamp(0.0, 1.0);
    distance(point, [a[0] + t * dx, a[1] + t * dy])
}

fn route(pieces: &Pieces) -> Result<Candidates, Refusal> {
    if !pieces.unsupported.is_empty() {
        return Err(Refusal::at(Reason::UnsupportedCurve, pieces.unsupported.iter().copied().take(5).collect()));
    }
    let chains: Vec<&Chain> = pieces.chains.iter().filter(|c| c.points.windows(2).any(|p| distance(p[0], p[1]) > 1e-9)).collect();
    if chains.is_empty() {
        return Err(Refusal::new("The selection has no lines or curves to convert."));
    }
    // Each chain joins the nodes at its two ends.
    let mut nodes = Nodes(Vec::new());
    let ends: Vec<(usize, usize)> = chains
        .iter()
        .map(|chain| (nodes.of(chain.points[0]), nodes.of(*chain.points.last().expect("two points"))))
        .collect();
    let mut degree = vec![0usize; nodes.0.len()];
    for &(a, b) in &ends {
        degree[a] += 1;
        degree[b] += 1;
    }
    // Branches: three or more pieces meeting at a point, or a piece ending
    // on another piece away from that piece's ends.
    let mut branches: Vec<[f64; 2]> = degree.iter().enumerate().filter(|(_, d)| **d > 2).map(|(node, _)| nodes.0[node]).collect();
    for (i, &(a, b)) in ends.iter().enumerate() {
        for node in [a, b] {
            let at = nodes.0[node];
            let touches = chains.iter().enumerate().any(|(j, other)| {
                let (c, d) = ends[j];
                j != i
                    && node != c
                    && node != d
                    && other.points.windows(2).any(|segment| to_segment(at, segment[0], segment[1]) < MIN_POINT_SPACING_MM)
            });
            if touches && !branches.iter().any(|b| distance(*b, at) < MIN_POINT_SPACING_MM) {
                branches.push(at);
            }
        }
    }
    if !branches.is_empty() {
        return Err(Refusal::at(Reason::Branch, branches));
    }
    // Pieces that do not meet: one gap per missing link, where the nearest
    // ends of separate groups are.
    let mut parent: Vec<usize> = (0..nodes.0.len()).collect();
    for &(a, b) in &ends {
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        parent[ra] = rb;
    }
    let groups: Vec<usize> = (0..nodes.0.len()).map(|node| root(&mut parent, node)).collect();
    let mut distinct: Vec<usize> = groups.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() > 1 {
        // Where the path breaks: each group's free ends (a closed loop has
        // none: any of its points), paired across groups nearest first
        // (Kruskal), one gap per missing link. Past MAX_GAP_GROUPS groups
        // only the first groups' ends are named, so the work stays small.
        let mut ends: Vec<(usize, usize)> = Vec::new();
        for &group in distinct.iter().take(MAX_GAP_GROUPS) {
            let members: Vec<usize> = (0..nodes.0.len()).filter(|&n| groups[n] == group).collect();
            let free: Vec<usize> = members.iter().copied().filter(|&n| degree[n] == 1).collect();
            let pick = if free.is_empty() { vec![members[0]] } else { free };
            ends.extend(pick.into_iter().map(|node| (group, node)));
        }
        let mut links: Vec<(f64, usize, usize)> = Vec::new();
        for (i, &(ga, a)) in ends.iter().enumerate() {
            for &(gb, b) in &ends[i + 1..] {
                if ga != gb {
                    links.push((distance(nodes.0[a], nodes.0[b]), a, b));
                }
            }
        }
        links.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
        let mut joined: Vec<usize> = (0..nodes.0.len()).collect();
        let mut gaps = Vec::new();
        for (_, a, b) in links {
            let (ra, rb) = (root(&mut joined, groups[a]), root(&mut joined, groups[b]));
            if ra != rb {
                joined[ra] = rb;
                gaps.push(midpoint(nodes.0[a], nodes.0[b]));
            }
        }
        return Err(Refusal::at(Reason::Gap, gaps));
    }
    // One path (or one loop): start at the first free end, in drawing order.
    let start = ends.iter().flat_map(|&(a, b)| [a, b]).find(|&node| degree[node] == 1).unwrap_or(ends[0].0);
    let mut used = vec![false; chains.len()];
    let mut at = start;
    let mut path: Vec<[f64; 2]> = Vec::new();
    while let Some(next) = (0..chains.len()).find(|&i| !used[i] && (ends[i].0 == at || ends[i].1 == at)) {
        used[next] = true;
        let forward = ends[next].0 == at;
        let mut points = chains[next].points.clone();
        if !forward {
            points.reverse();
        }
        // The joint is the path's own last point.
        let skip = usize::from(!path.is_empty());
        path.extend(points.into_iter().skip(skip));
        at = if forward { ends[next].1 } else { ends[next].0 };
    }
    // Consecutive points closer than 1 mm: the later one goes, except the
    // path's end, which replaces the point before it.
    let (mut rejected, mut total) = (Vec::new(), 0);
    let mut points: Vec<[f64; 2]> = Vec::with_capacity(path.len());
    let last = path.len() - 1;
    for (index, point) in path.into_iter().enumerate() {
        match points.last() {
            Some(previous) if distance(*previous, point) < MIN_POINT_SPACING_MM => {
                if index == last && points.len() > 1 {
                    let dropped = points.pop().expect("more than one");
                    reject(&mut rejected, &mut total, Reason::PointsTooClose, dropped);
                    points.push(point);
                } else {
                    reject(&mut rejected, &mut total, Reason::PointsTooClose, point);
                }
            }
            _ => points.push(point),
        }
    }
    let length: f64 = points.windows(2).map(|pair| distance(pair[0], pair[1])).sum();
    if points.len() < 2 || length < MIN_LENGTH_MM {
        return Err(Refusal::at(Reason::TooShort, points.first().copied().into_iter().collect()));
    }
    if points.len() > MAX_ROUTE_POINTS {
        return Err(Refusal::new(format!(
            "The route has {} points; SecurePlan takes at most {MAX_ROUTE_POINTS}. Select a shorter path.",
            points.len()
        )));
    }
    Ok(Candidates { kind: Kind::Route, walls: Vec::new(), points, rejected, rejected_total: total, skipped: 0 })
}

/// The reasons of `rejected`, counted, for the user.
pub fn rejected_lines(candidates: &Candidates) -> Vec<String> {
    let mut lines = Vec::new();
    for reason in [Reason::TooShort, Reason::PointsTooClose, Reason::UnsupportedCurve, Reason::Gap, Reason::Branch] {
        let count = candidates.rejected.iter().filter(|r| r.reason == reason).count();
        if count > 0 {
            lines.push(format!("{count} not converted: {}", reason.describe()));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use acadrust::entities::{Arc, Insert, Line, LwPolyline};
    use acadrust::types::Vector2;

    /// 1 CAD unit = 1 mm, CAD (x, y) → world (x, 20000 − y).
    fn mapping() -> Mapping {
        Mapping { cad_origin: [0.0, 20000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 }
    }

    fn line(x0: f64, y0: f64, x1: f64, y1: f64) -> EntityType {
        EntityType::Line(Line::from_points(Vector3::new(x0, y0, 0.0), Vector3::new(x1, y1, 0.0)))
    }

    fn scene_with(entities: Vec<EntityType>) -> (crate::scene::Scene, HashSet<Handle>) {
        let mut scene = crate::scene::Scene::new();
        let handles = entities.into_iter().map(|e| scene.add_entity(e)).collect();
        scene.rebuild_derived_caches();
        (scene, handles)
    }

    fn close(a: [f64; 2], b: [f64; 2]) -> bool {
        distance(a, b) < 1e-6
    }

    #[test]
    fn walls_come_from_straight_segments_in_world_mm_and_short_ones_are_rejected() {
        let mut room = LwPolyline::from_points(vec![Vector2::new(0.0, 0.0), Vector2::new(4000.0, 0.0), Vector2::new(4000.0, 3000.0)]);
        room.is_closed = true;
        let (scene, selected) = scene_with(vec![line(1000.0, 1000.0, 1100.0, 1000.0), EntityType::LwPolyline(room)]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap();
        assert_eq!(found.walls.len(), 3, "the closed room's three sides");
        assert!(close(found.walls[0].0, [0.0, 20000.0]) && close(found.walls[0].1, [4000.0, 20000.0]));
        assert!(close(found.walls[2].0, [4000.0, 17000.0]) && close(found.walls[2].1, [0.0, 20000.0]), "closing side");
        assert_eq!(found.rejected, vec![Rejected { reason: Reason::TooShort, at_mm: [1050.0, 19000.0] }]);
        let json = found.to_json();
        crate::app::secureplan::protocol::validate_payload("convert-candidates.schema.json", &json).unwrap();
        assert_eq!(json["walls"][0]["start"], json!([0.0, 20000.0]));
    }

    #[test]
    fn mirrored_and_nested_block_instances_are_placed_by_their_transforms() {
        let mut scene = crate::scene::Scene::new();
        let segment = vec![line(0.0, 0.0, 1000.0, 0.0)];
        scene.define_block_raw("WALLPIECE", Vector3::ZERO, segment);
        let outer = Insert::new("WALLPIECE", Vector3::new(0.0, 0.0, 0.0));
        scene.define_block_raw("NEST", Vector3::ZERO, vec![EntityType::Insert(outer)]);
        // Mirrored in X (scale −1) at (5000, 1000), and a nested one rotated 90° at (0, 0).
        let mut mirrored = Insert::new("WALLPIECE", Vector3::new(5000.0, 1000.0, 0.0));
        mirrored.set_x_scale(-1.0);
        let nested = Insert::new("NEST", Vector3::new(100.0, 200.0, 0.0)).with_rotation(std::f64::consts::FRAC_PI_2);
        let unselected = scene.add_entity(line(0.0, 0.0, 9000.0, 0.0));
        let selected: HashSet<Handle> =
            [scene.add_entity(EntityType::Insert(mirrored)), scene.add_entity(EntityType::Insert(nested))].into_iter().collect();
        assert!(!selected.contains(&unselected));
        scene.rebuild_derived_caches();
        let found = candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap();
        assert_eq!(found.walls.len(), 2, "only the selected instances");
        // Mirrored: CAD (5000, 1000) → (4000, 1000); world y = 20000 − y.
        assert!(close(found.walls[0].0, [5000.0, 19000.0]) && close(found.walls[0].1, [4000.0, 19000.0]), "{:?}", found.walls[0]);
        // Rotated 90° about (100, 200): (0,0)→(100,200), (1000,0)→(100,1200).
        assert!(close(found.walls[1].0, [100.0, 19800.0]) && close(found.walls[1].1, [100.0, 18800.0]), "{:?}", found.walls[1]);
    }

    #[test]
    fn arcs_are_cut_within_a_millimetre_in_the_plan_even_when_scaled() {
        let mut swing = Arc::new();
        swing.center = Vector3::new(0.0, 0.0, 0.0);
        swing.radius = 30.0;
        swing.start_angle = 0.0;
        swing.end_angle = std::f64::consts::PI;
        let mut scene = crate::scene::Scene::new();
        scene.define_block_raw("SWING", Vector3::ZERO, vec![EntityType::Arc(swing)]);
        // Scaled 100×: a 3000 mm radius in the plan.
        let insert = Insert::new("SWING", Vector3::new(10000.0, 10000.0, 0.0)).with_uniform_scale(100.0);
        let selected: HashSet<Handle> = [scene.add_entity(EntityType::Insert(insert))].into_iter().collect();
        scene.rebuild_derived_caches();
        let found = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap();
        let centre = [10000.0, 10000.0];
        for pair in found.points.windows(2) {
            let middle = midpoint(pair[0], pair[1]);
            let sagitta = 3000.0 - distance(middle, centre);
            assert!(sagitta <= CHORD_TOLERANCE_MM + 1e-9, "chord departs {sagitta} mm");
            assert!((distance(pair[0], centre) - 3000.0).abs() < 1e-6, "on the arc");
        }
        assert!(found.points.len() > 40, "cut finely enough: {}", found.points.len());
    }

    #[test]
    fn a_route_joins_pieces_end_to_end_in_order() {
        // Drawn out of order and one reversed: the path still runs A → D.
        let (scene, selected) = scene_with(vec![
            line(2000.0, 0.0, 2000.0, 1500.0),
            line(0.0, 0.0, 1000.0, 0.0),
            line(2000.0, 0.0, 1000.0, 0.0),
        ]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap();
        let expected = [[2000.0, 18500.0], [2000.0, 20000.0], [1000.0, 20000.0], [0.0, 20000.0]];
        assert_eq!(found.points.len(), 4, "{:?}", found.points);
        for (got, want) in found.points.iter().zip(expected) {
            assert!(close(*got, want), "{got:?} {want:?}");
        }
        assert!((found.route_length_mm() - 3500.0).abs() < 1e-9);
        crate::app::secureplan::protocol::validate_payload("convert-candidates.schema.json", &found.to_json()).unwrap();
    }

    #[test]
    fn gaps_and_branches_refuse_the_route_with_their_location() {
        // A 10 mm gap between (1000, 0) and (1010, 0).
        let (scene, selected) = scene_with(vec![line(0.0, 0.0, 1000.0, 0.0), line(1010.0, 0.0, 3000.0, 0.0)]);
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert_eq!(refused.at_mm.len(), 1);
        assert!(close(refused.at_mm[0], [1005.0, 20000.0]), "{:?}", refused.at_mm);
        assert!(refused.message.contains("do not meet"));
        // Ends within 1 mm join.
        let (scene, selected) = scene_with(vec![line(0.0, 0.0, 1000.0, 0.0), line(1000.5, 0.0, 3000.0, 0.0)]);
        assert_eq!(candidates(&scene, &selected, &mapping(), Kind::Route).unwrap().points.len(), 3);
        // Three pieces meeting at (1000, 0).
        let (scene, selected) =
            scene_with(vec![line(0.0, 0.0, 1000.0, 0.0), line(1000.0, 0.0, 2000.0, 0.0), line(1000.0, 0.0, 1000.0, 900.0)]);
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert!(refused.message.contains("branches"));
        assert_eq!(refused.at_mm, vec![[1000.0, 20000.0]]);
        // A piece ending in the middle of another (a T).
        let (scene, selected) = scene_with(vec![line(0.0, 0.0, 2000.0, 0.0), line(1000.0, 0.0, 1000.0, 900.0)]);
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert_eq!(refused.at_mm, vec![[1000.0, 20000.0]], "the T is a branch");
    }

    #[test]
    fn many_separate_pieces_are_refused_quickly() {
        // 4000 separate 200 mm lines: a gap refusal, without pairing every end.
        let lines = (0..4000).map(|i| line(0.0, i as f64 * 1000.0, 200.0, i as f64 * 1000.0)).collect();
        let (scene, selected) = scene_with(lines);
        let started = std::time::Instant::now();
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert!(refused.message.contains("do not meet"));
        assert_eq!(refused.at_mm.len(), MAX_GAP_GROUPS - 1);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn short_parts_are_rejected_by_the_minimums() {
        // A route under 150 mm is refused.
        let (scene, selected) = scene_with(vec![line(0.0, 0.0, 100.0, 0.0), line(100.0, 0.0, 149.0, 0.0)]);
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert!(refused.message.contains("150 mm"));
        // A point 0.5 mm after the one before it is dropped and reported;
        // the route's end stays where it is.
        let polyline = LwPolyline::from_points(vec![
            Vector2::new(0.0, 0.0),
            Vector2::new(0.5, 0.0),
            Vector2::new(1000.0, 0.0),
            Vector2::new(1000.0, 999.6),
            Vector2::new(1000.0, 1000.0),
        ]);
        let (scene, selected) = scene_with(vec![EntityType::LwPolyline(polyline)]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap();
        assert_eq!(found.points.len(), 3, "{:?}", found.points);
        assert!(close(found.points[2], [1000.0, 19000.0]), "the end is kept");
        assert_eq!(found.rejected.iter().map(|r| r.reason).collect::<Vec<_>>(), [Reason::PointsTooClose, Reason::PointsTooClose]);
        // Walls: nothing long enough is refused outright.
        let (scene, selected) = scene_with(vec![line(0.0, 0.0, 149.9, 0.0)]);
        assert!(candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap_err().message.contains("150 mm"));
    }

    #[test]
    fn text_is_skipped_and_nothing_else_is_converted() {
        let text = EntityType::Text(acadrust::entities::Text::with_value("ROOM", Vector3::new(0.0, 0.0, 0.0)));
        let (scene, selected) = scene_with(vec![text, line(0.0, 0.0, 1000.0, 0.0)]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap();
        assert_eq!((found.walls.len(), found.skipped), (1, 1));
    }
}
