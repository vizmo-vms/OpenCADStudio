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

    /// The worker stopped unexpectedly.
    pub fn failed() -> Self {
        Self::new("The conversion stopped unexpectedly. Nothing was sent to SecurePlan.")
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

/// What the selection is made of, cut into world points. Built on the UI
/// thread within [`MAX_PIECE_POINTS`]; the candidates are assembled from it on
/// a worker.
#[derive(Debug, Default)]
pub struct Pieces {
    chains: Vec<Chain>,
    unsupported: Vec<[f64; 2]>,
    skipped: usize,
    /// Points gathered so far; past [`MAX_PIECE_POINTS`] nothing more is cut.
    points: usize,
    over_budget: bool,
}

/// The most points a selection may be cut into: past it, conversion is
/// refused before any further work (a wall or route has at most 10,000).
pub const MAX_PIECE_POINTS: usize = 200_000;
/// A route's pieces share their joints: at most twice its points.
const MAX_ROUTE_PIECE_POINTS: usize = 2 * MAX_ROUTE_POINTS;

/// Walk the selected top-level model-space entities (block references are
/// followed into their content) and cut every line and curve into a chain of
/// world points within [`CHORD_TOLERANCE_MM`], stopping at
/// [`MAX_PIECE_POINTS`].
pub fn pieces<S: std::hash::BuildHasher>(scene: &crate::scene::Scene, selected: &HashSet<Handle, S>, mapping: &Mapping) -> Pieces {
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
            if pieces.over_budget {
                return;
            }
            let transform = &context.transform;
            let chain: Option<Vec<[f64; 2]>> = match entity {
                EntityType::Line(line) => Some(vec![to_world(transform, line.start), to_world(transform, line.end)]),
                EntityType::Polyline3D(polyline) if polyline.vertices.len() > MAX_PIECE_POINTS - pieces.points => {
                    pieces.over_budget = true;
                    None
                }
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
                    // Cut within what is left of the budget, never further.
                    let left = MAX_PIECE_POINTS + 1 - pieces.points;
                    match super::publish::flatten_within(entity, local, left) {
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
                        Err(error) if error == super::publish::OVER_LIMIT => {
                            pieces.over_budget = true;
                            None
                        }
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
                pieces.points += points.len();
                pieces.over_budget = pieces.points > MAX_PIECE_POINTS;
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
/// space), mapped with `mapping`: [`pieces`] then [`assemble`].
pub fn candidates<S: std::hash::BuildHasher>(
    scene: &crate::scene::Scene,
    selected: &HashSet<Handle, S>,
    mapping: &Mapping,
    kind: Kind,
) -> Result<Candidates, Refusal> {
    assemble(&pieces(scene, selected, mapping), kind)
}

/// The candidates for `kind` from `pieces` (on a worker): near-linear work,
/// with the budgets checked first.
pub fn assemble(pieces: &Pieces, kind: Kind) -> Result<Candidates, Refusal> {
    if pieces.over_budget {
        return Err(Refusal::new(format!(
            "The selection is too large to convert at once (more than {MAX_PIECE_POINTS} points of linework). Select fewer objects."
        )));
    }
    if kind == Kind::Route && pieces.points > MAX_ROUTE_PIECE_POINTS {
        return Err(Refusal::new(format!(
            "The selection is too large for one cable route (SecurePlan takes at most {MAX_ROUTE_POINTS} points). Select a shorter path."
        )));
    }
    if pieces.chains.iter().flat_map(|c| &c.points).chain(&pieces.unsupported).any(|p| !in_range(*p)) {
        return Err(Refusal::new("Part of the selection lies outside the survey's coordinate range (±10 km)."));
    }
    let result = match kind {
        Kind::Walls => walls(pieces),
        Kind::Route => route(pieces),
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

/// A uniform grid of cells `size` wide, holding indices.
struct Grid {
    size: f64,
    cells: std::collections::HashMap<(i64, i64), Vec<usize>>,
}

impl Grid {
    fn new(size: f64) -> Self {
        Self { size, cells: Default::default() }
    }

    fn cell(&self, point: [f64; 2]) -> (i64, i64) {
        ((point[0] / self.size).floor() as i64, (point[1] / self.size).floor() as i64)
    }

    /// The indices in the cells around `point` (its cell and the eight next to it).
    fn near(&self, point: [f64; 2]) -> impl Iterator<Item = usize> + '_ {
        let (x, y) = self.cell(point);
        (-1..=1).flat_map(move |dx| (-1..=1).map(move |dy| (x + dx, y + dy))).flat_map(|key| self.cells.get(&key).into_iter().flatten().copied())
    }
}

/// Points of `chains` joined when within [`MIN_POINT_SPACING_MM`]: a point
/// becomes the lowest-numbered node within that distance, found through a
/// grid of 1 mm cells.
struct Nodes {
    points: Vec<[f64; 2]>,
    grid: Grid,
}

impl Nodes {
    fn of(&mut self, point: [f64; 2]) -> usize {
        let found = self.grid.near(point).filter(|&node| distance(self.points[node], point) < MIN_POINT_SPACING_MM).min();
        match found {
            Some(index) => index,
            None => {
                self.points.push(point);
                let key = self.grid.cell(point);
                self.grid.cells.entry(key).or_default().push(self.points.len() - 1);
                self.points.len() - 1
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

/// At most this many branch places are gathered (the notice names five).
const MAX_BRANCHES: usize = 64;

fn route(pieces: &Pieces) -> Result<Candidates, Refusal> {
    if !pieces.unsupported.is_empty() {
        return Err(Refusal::at(Reason::UnsupportedCurve, pieces.unsupported.iter().copied().take(5).collect()));
    }
    let chains: Vec<&Chain> = pieces.chains.iter().filter(|c| c.points.windows(2).any(|p| distance(p[0], p[1]) > 1e-9)).collect();
    if chains.is_empty() {
        return Err(Refusal::new("The selection has no lines or curves to convert."));
    }
    // Each chain joins the nodes at its two ends.
    let mut nodes = Nodes { points: Vec::new(), grid: Grid::new(MIN_POINT_SPACING_MM) };
    let ends: Vec<(usize, usize)> = chains
        .iter()
        .map(|chain| (nodes.of(chain.points[0]), nodes.of(*chain.points.last().expect("two points"))))
        .collect();
    let nodes = nodes.points;
    let mut incident: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for (chain, &(a, b)) in ends.iter().enumerate() {
        incident[a].push(chain);
        if b != a {
            incident[b].push(chain);
        }
    }
    let degree: Vec<usize> = (0..nodes.len())
        .map(|node| incident[node].iter().map(|&c| usize::from(ends[c].0 == node) + usize::from(ends[c].1 == node)).sum())
        .collect();
    // Branches: three or more pieces meeting at a point, or a piece ending
    // on another piece away from that piece's ends. Segments are found
    // through a grid whose cells are at least 2 mm and about as long as an
    // average segment: each segment is entered in the cells around points
    // half a cell apart along it, so any point within 1 mm of it lies in a
    // cell next to one of those.
    let mut branches: Vec<[f64; 2]> = (0..nodes.len()).filter(|&node| degree[node] > 2).map(|node| nodes[node]).take(MAX_BRANCHES).collect();
    let segments: Vec<(usize, [f64; 2], [f64; 2])> =
        chains.iter().enumerate().flat_map(|(c, chain)| chain.points.windows(2).map(move |w| (c, w[0], w[1]))).collect();
    let total: f64 = segments.iter().map(|(_, a, b)| distance(*a, *b)).sum();
    let mut grid = Grid::new((total / segments.len().max(1) as f64).max(2.0 * MIN_POINT_SPACING_MM));
    for (index, &(_, a, b)) in segments.iter().enumerate() {
        let steps = (distance(a, b) / (grid.size / 2.0)).ceil().max(1.0) as usize;
        let mut keys = std::collections::HashSet::new();
        for step in 0..=steps {
            let t = step as f64 / steps as f64;
            let (x, y) = grid.cell([a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    keys.insert((x + dx, y + dy));
                }
            }
        }
        for key in keys {
            grid.cells.entry(key).or_default().push(index);
        }
    }
    'ends: for (i, &(a, b)) in ends.iter().enumerate() {
        for node in [a, b] {
            if branches.len() >= MAX_BRANCHES {
                break 'ends;
            }
            let at = nodes[node];
            let (x, y) = grid.cell(at);
            let touches = grid.cells.get(&(x, y)).into_iter().flatten().any(|&index| {
                let (j, sa, sb) = segments[index];
                let (c, d) = ends[j];
                j != i && node != c && node != d && to_segment(at, sa, sb) < MIN_POINT_SPACING_MM
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
    let mut parent: Vec<usize> = (0..nodes.len()).collect();
    for &(a, b) in &ends {
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        parent[ra] = rb;
    }
    let groups: Vec<usize> = (0..nodes.len()).map(|node| root(&mut parent, node)).collect();
    let mut distinct: Vec<usize> = groups.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() > 1 {
        // Where the path breaks: each group's free ends (a closed loop has
        // none: any of its points), paired across groups nearest first
        // (Kruskal), one gap per missing link. Past MAX_GAP_GROUPS groups
        // only the first groups' ends are named, so the work stays small.
        let named: std::collections::HashSet<usize> = distinct.iter().copied().take(MAX_GAP_GROUPS).collect();
        let mut members: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
        for (node, group) in groups.iter().enumerate() {
            if named.contains(group) {
                members.entry(*group).or_default().push(node);
            }
        }
        let mut ends: Vec<(usize, usize)> = Vec::new();
        for (&group, nodes_of) in &members {
            let free: Vec<usize> = nodes_of.iter().copied().filter(|&n| degree[n] == 1).collect();
            let pick = if free.is_empty() { vec![nodes_of[0]] } else { free };
            ends.extend(pick.into_iter().map(|node| (group, node)));
        }
        let mut links: Vec<(f64, usize, usize)> = Vec::new();
        for (i, &(ga, a)) in ends.iter().enumerate() {
            for &(gb, b) in &ends[i + 1..] {
                if ga != gb {
                    links.push((distance(nodes[a], nodes[b]), a, b));
                }
            }
        }
        links.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
        let mut joined: Vec<usize> = (0..nodes.len()).collect();
        let mut gaps = Vec::new();
        for (_, a, b) in links {
            let (ra, rb) = (root(&mut joined, groups[a]), root(&mut joined, groups[b]));
            if ra != rb {
                joined[ra] = rb;
                gaps.push(midpoint(nodes[a], nodes[b]));
            }
        }
        return Err(Refusal::at(Reason::Gap, gaps));
    }
    // One path (or one loop): start at the first free end, in drawing order.
    let start = ends.iter().flat_map(|&(a, b)| [a, b]).find(|&node| degree[node] == 1).unwrap_or(ends[0].0);
    let mut used = vec![false; chains.len()];
    let mut at = start;
    let mut path: Vec<[f64; 2]> = Vec::new();
    while let Some(next) = incident[at].iter().copied().find(|&i| !used[i]) {
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
    // path's end, which stays and replaces every point before it that is
    // too close (the start excepted).
    let (mut rejected, mut total) = (Vec::new(), 0);
    let mut points: Vec<[f64; 2]> = Vec::with_capacity(path.len());
    let last = path.len() - 1;
    for (index, point) in path.into_iter().enumerate() {
        match points.last() {
            Some(previous) if distance(*previous, point) < MIN_POINT_SPACING_MM => {
                if index == last {
                    while points.len() > 1 && distance(*points.last().expect("more than one"), point) < MIN_POINT_SPACING_MM {
                        let dropped = points.pop().expect("more than one");
                        reject(&mut rejected, &mut total, Reason::PointsTooClose, dropped);
                    }
                    points.push(point);
                } else {
                    reject(&mut rejected, &mut total, Reason::PointsTooClose, point);
                }
            }
            _ => points.push(point),
        }
    }
    let length: f64 = points.windows(2).map(|pair| distance(pair[0], pair[1])).sum();
    let spaced = points.windows(2).all(|pair| distance(pair[0], pair[1]) >= MIN_POINT_SPACING_MM);
    if points.len() < 2 || length < MIN_LENGTH_MM || !spaced {
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

/// What a conversion works on, taken on the UI thread when it starts: the
/// drawing as it is then (an immutable copy) and the selected handles.
pub struct Snapshot {
    pub document: acadrust::CadDocument,
    pub annotation_scale: f32,
    pub selected: HashSet<Handle>,
    pub mapping: Mapping,
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot").field("selected", &self.selected.len()).finish_non_exhaustive()
    }
}

/// Cut the snapshot's selection and assemble the candidates (on a worker).
pub fn prepare(snapshot: &Snapshot, kind: Kind) -> Result<Candidates, Refusal> {
    let mut scene = crate::scene::Scene::new();
    scene.document = snapshot.document.clone();
    scene.annotation_scale = snapshot.annotation_scale;
    scene.rebuild_derived_caches();
    candidates(&scene, &snapshot.selected, &snapshot.mapping, kind)
}

/// What a conversion was started under: its candidates are sent only if the
/// session, plan, document and drawing are still the same.
#[derive(Debug, Clone, PartialEq)]
pub struct Origin {
    /// The conversion job: only it may clear the tab's `busy: convert`.
    pub job: u64,
    pub session: super::bridge::SessionId,
    pub base_identity: String,
    pub generation: u64,
    pub revision: u64,
}

/// Candidates assembled on a worker, for the UI thread.
#[derive(Debug, Clone)]
pub struct Done {
    pub tab_id: u64,
    pub kind: Kind,
    pub origin: Origin,
    pub mapping: Mapping,
    pub result: super::Carry<Result<Candidates, Refusal>>,
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
pub(crate) mod tests {
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
    fn a_spline_is_converted_within_a_millimetre_along_every_chord() {
        let (spline, curve) = crate::app::secureplan::publish::tests::bulging_spline();
        let (scene, selected) = scene_with(vec![EntityType::Spline(spline)]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap();
        // Back to CAD coordinates: world (x, 20000 − y).
        let cad: Vec<[f64; 2]> = found.points.iter().map(|p| [p[0], 20000.0 - p[1]]).collect();
        let departure = crate::app::secureplan::publish::tests::polyline_departure(&cad, curve);
        assert!(departure <= CHORD_TOLERANCE_MM * (1.0 + 1e-6), "the curve leaves a chord by {departure} mm");
    }

    #[test]
    fn unclamped_and_periodic_splines_are_converted_within_a_millimetre() {
        for spline in crate::app::secureplan::publish::tests::awkward_splines() {
            let entity = EntityType::Spline(spline);
            let exact = crate::entities::curve::entity_curve(&entity).expect("a planar curve");
            let (scene, selected) = scene_with(vec![entity]);
            let found = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap();
            let cad: Vec<[f64; 2]> = found.points.iter().map(|p| [p[0], 20000.0 - p[1]]).collect();
            let departure = crate::app::secureplan::publish::tests::polyline_departure(&cad, |t| {
                let p = exact.point_at(t);
                [p[0], p[1]]
            });
            assert!(departure <= CHORD_TOLERANCE_MM * (1.0 + 1e-6), "{departure} mm");
        }
    }

    /// A degree-1 spline through `count` control points in a zigzag.
    pub(crate) fn many_knots(count: usize) -> EntityType {
        let points = (0..count).map(|i| Vector3::new(i as f64 * 20.0 % 9_000_000.0, if i % 2 == 0 { 0.0 } else { 300.0 }, 0.0)).collect();
        EntityType::Spline(acadrust::entities::Spline::from_control_points(1, points))
    }

    #[test]
    fn a_spline_with_many_knots_is_handled_quickly_and_the_budget_holds_inside_a_curve() {
        let started = std::time::Instant::now();
        let (scene, selected) = scene_with(vec![many_knots(100_000)]);
        let walls = candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap_err();
        assert!(walls.message.contains("at most"), "{}", walls.message);
        let route = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert!(route.message.contains("too large"), "{}", route.message);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(3), "{elapsed:?}");
        // One circle that would need ~700,000 chords at 1 mm: the cut stops at
        // the budget instead of producing them.
        let mut circle = acadrust::entities::Circle::new();
        circle.radius = 1.0e11;
        let (scene, selected) = scene_with(vec![EntityType::Circle(circle)]);
        let pieces = pieces(&scene, &selected, &mapping());
        assert!(pieces.over_budget && pieces.points <= MAX_PIECE_POINTS, "{} points", pieces.points);
        assert!(assemble(&pieces, Kind::Walls).unwrap_err().message.contains("too large to convert at once"));
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
    fn oversized_selections_are_refused_or_handled_quickly() {
        // 100,000 separate 200 mm lines: over the route's budget, refused
        // before any pairing of ends; as walls, over the wall limit.
        let spot = |i: usize| (((i % 316) * 300) as f64, ((i / 316) * 300) as f64);
        let lines: Vec<EntityType> = (0..100_000).map(spot).map(|(x, y)| line(x, y, x + 200.0, y)).collect();
        let (scene, selected) = scene_with(lines);
        let started = std::time::Instant::now();
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert!(refused.message.contains("too large"), "{}", refused.message);
        let refused = candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap_err();
        assert!(refused.message.contains("at most"), "{}", refused.message);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(4), "{elapsed:?}");
        // Past the budget, cutting stops and the conversion is refused.
        let (scene, selected) = scene_with((0..100_001).map(spot).map(|(x, y)| line(x, y, x + 200.0, y)).collect());
        let pieces = pieces(&scene, &selected, &mapping());
        assert!(pieces.over_budget && pieces.points <= MAX_PIECE_POINTS + 2);
        assert!(assemble(&pieces, Kind::Walls).unwrap_err().message.contains("too large to convert at once"));
        // Just under the route's budget (9,999 separate lines): the gaps are
        // found without comparing every pair of ends.
        let lines: Vec<EntityType> = (0..9_999).map(spot).map(|(x, y)| line(x, y, x + 200.0, y)).collect();
        let (scene, selected) = scene_with(lines);
        let started = std::time::Instant::now();
        let refused = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap_err();
        assert!(refused.message.contains("do not meet"));
        assert_eq!(refused.at_mm.len(), MAX_GAP_GROUPS - 1);
        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
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
    fn a_reversing_tail_leaves_no_step_under_a_millimetre() {
        // (0,0) → (200,0) → (201.1,0) → (200.5,0): keeping the end must not
        // leave (200,0) and (200.5,0) side by side.
        let polyline = LwPolyline::from_points(vec![Vector2::new(0.0, 0.0), Vector2::new(200.0, 0.0), Vector2::new(201.1, 0.0), Vector2::new(200.5, 0.0)]);
        let (scene, selected) = scene_with(vec![EntityType::LwPolyline(polyline)]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Route).unwrap();
        for pair in found.points.windows(2) {
            assert!(distance(pair[0], pair[1]) >= MIN_POINT_SPACING_MM, "{:?}", found.points);
        }
        assert!(close(*found.points.last().unwrap(), [200.5, 20000.0]), "the end is kept");
        assert!(close(found.points[0], [0.0, 20000.0]));
        assert_eq!(found.rejected.iter().filter(|r| r.reason == Reason::PointsTooClose).count(), 2);
    }

    #[test]
    fn text_is_skipped_and_nothing_else_is_converted() {
        let text = EntityType::Text(acadrust::entities::Text::with_value("ROOM", Vector3::new(0.0, 0.0, 0.0)));
        let (scene, selected) = scene_with(vec![text, line(0.0, 0.0, 1000.0, 0.0)]);
        let found = candidates(&scene, &selected, &mapping(), Kind::Walls).unwrap();
        assert_eq!((found.walls.len(), found.skipped), (1, 1));
    }
}
