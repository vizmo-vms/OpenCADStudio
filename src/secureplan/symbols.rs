//! Vector symbols for exported SecurePlan devices (EXP-02).
//!
//! **Standard symbols** are one static block per kind, the shapes the overlay
//! falls back to: a camera is a circle with a view wedge along its direction,
//! equipment a square, an asset a diamond. They are defined at unit size,
//! facing +X, on layer 0 with colour ByBlock: each INSERT scales them to the
//! symbol size in drawing units and gives them its layer and the catalog
//! colour.
//!
//! **SecurePlan icons** are one static block per distinct icon, kind and
//! badge ([`icon_block`]), drawn as the SecurePlan canvas draws the device,
//! in world millimetres at symbol scale 1 with y up: a solid-hatch circle in
//! colour ByBlock (the INSERT's device colour), the `#fbfaf4` ring, the SVG
//! icon's linework and equipment's badge. SVG fills become solid hatches and
//! strokes polylines with their width, in the SVG's own colours; curves are
//! flattened to within [`ICON_TOLERANCE_MM`]. Raster icons have no linework,
//! so their devices keep the standard symbol, which the summary discloses.

use acadrust::entities::{Arc, BoundaryEdge, BoundaryPath, Circle, Hatch, Line, LwPolyline, PolylineEdge, Text, TextHorizontalAlignment, TextVerticalAlignment};
use acadrust::types::{Color, Vector2, Vector3};
use acadrust::EntityType;
use resvg::usvg;

/// A device kind (`devices[].kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceKind {
    Camera,
    Equipment,
    Asset,
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 3] = [DeviceKind::Camera, DeviceKind::Equipment, DeviceKind::Asset];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "camera" => Some(DeviceKind::Camera),
            "equipment" => Some(DeviceKind::Equipment),
            "asset" => Some(DeviceKind::Asset),
            _ => None,
        }
    }

    /// The upper-case tag in layer and block names.
    pub fn tag(self) -> &'static str {
        match self {
            DeviceKind::Camera => "CAMERA",
            DeviceKind::Equipment => "EQUIPMENT",
            DeviceKind::Asset => "ASSET",
        }
    }

    /// How the symbol looks, for the export summary.
    pub fn describe(self) -> &'static str {
        match self {
            DeviceKind::Camera => "a circle with a view wedge for cameras",
            DeviceKind::Equipment => "a square for equipment",
            DeviceKind::Asset => "a diamond for assets",
        }
    }
}

/// The symbol's radius in world millimetres: a 600 mm symbol on the plan.
pub const SYMBOL_RADIUS_MM: f64 = 300.0;

/// How the SecurePlan canvas draws a device (`ElementShape` in the web's
/// CanvasEditor.tsx), in world millimetres at symbol scale 1, world y down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WebSymbol {
    /// The filled circle, in the device colour.
    pub radius: f64,
    /// The ring on it, [`WEB_LIGHT`], centred on the circle.
    pub ring_width: f64,
    /// The icon's square box.
    pub icon_size: f64,
    /// The icon's centre below the device's (negative lifts it).
    pub icon_offset_y: f64,
    /// Whether the symbol turns with the device's rotation (a camera's does not).
    pub rotates: bool,
    /// The label's top, below the device's centre.
    pub label_top: f64,
    pub label_size: f64,
    pub label_bold: bool,
}

/// The ring and badge colour, `#fbfaf4`.
pub const WEB_LIGHT: [u8; 3] = [0xfb, 0xfa, 0xf4];
/// The web's label colour, `#17233b`, readable on a light background only.
pub const WEB_INK: [u8; 3] = [0x17, 0x23, 0x3b];
/// Equipment's badge ("SW", "NVR"): its top below the centre, and its size.
pub const BADGE_TOP_MM: f64 = 172.0;
pub const BADGE_SIZE_MM: f64 = 115.0;

impl DeviceKind {
    /// The web canvas's drawing of this kind.
    pub fn web(self) -> WebSymbol {
        match self {
            DeviceKind::Camera => WebSymbol {
                radius: 260.0,
                ring_width: 70.0,
                icon_size: 400.0,
                icon_offset_y: 0.0,
                rotates: false,
                label_top: 370.0,
                label_size: 190.0,
                label_bold: true,
            },
            DeviceKind::Equipment => WebSymbol {
                radius: 330.0,
                ring_width: 60.0,
                icon_size: 390.0,
                icon_offset_y: -52.0,
                rotates: true,
                label_top: 400.0,
                label_size: 180.0,
                label_bold: false,
            },
            DeviceKind::Asset => WebSymbol {
                radius: 340.0,
                ring_width: 55.0,
                icon_size: 430.0,
                icon_offset_y: 0.0,
                rotates: true,
                label_top: 420.0,
                label_size: 180.0,
                label_bold: false,
            },
        }
    }
}

/// The largest distance, in world mm at symbol scale 1, between an SVG curve
/// and the polyline that replaces it in an icon block (0.5 % of a 400 mm icon).
pub const ICON_TOLERANCE_MM: f64 = 2.0;
/// CAD text height (cap height) per unit of the canvas's font size.
const CAP_HEIGHT: f64 = 0.7;

fn light() -> Color {
    let [r, g, b] = WEB_LIGHT;
    Color::Rgb { r, g, b }
}

fn on_layer_zero(mut entity: EntityType, color: Color) -> EntityType {
    let common = entity.common_mut();
    common.layer = "0".into();
    common.color = color;
    entity
}

fn solid(loops: Vec<Vec<Vector2>>, color: Color) -> EntityType {
    let mut hatch = Hatch::solid();
    for (i, points) in loops.into_iter().enumerate() {
        let mut path = if i == 0 { BoundaryPath::external() } else { BoundaryPath::new() };
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(points, true)));
        hatch.paths.push(path);
    }
    on_layer_zero(EntityType::Hatch(hatch), color)
}

/// Points along a Bezier curve (`control`, without its start `from`), close
/// enough that no point of the curve is further than `tolerance` from them
/// (Wang's bound for uniform subdivision).
fn flatten(from: Vector2, control: &[Vector2], tolerance: f64, out: &mut Vec<Vector2>) {
    let points: Vec<Vector2> = std::iter::once(from).chain(control.iter().copied()).collect();
    let degree = (points.len() - 1) as f64;
    let second = points.windows(3).map(|w| (w[0] - w[1] * 2.0 + w[2]).length()).fold(0.0, f64::max);
    let segments = ((degree * (degree - 1.0) / 8.0 * second / tolerance).sqrt().ceil() as usize).clamp(1, 256);
    for step in 1..=segments {
        let t = step as f64 / segments as f64;
        // De Casteljau.
        let mut level = points.clone();
        while level.len() > 1 {
            level = level.windows(2).map(|w| w[0] * (1.0 - t) + w[1] * t).collect();
        }
        out.push(level[0]);
    }
}

fn paint_color(paint: &usvg::Paint) -> Color {
    match paint {
        usvg::Paint::Color(c) => Color::Rgb { r: c.red, g: c.green, b: c.blue },
        // Gradients and patterns: the canvas's light icon colour.
        _ => light(),
    }
}

/// The icon's fills and strokes in block coordinates: the SVG stretched over
/// a `size` square centred at (0, `centre_y`), y up.
fn icon_linework(tree: &usvg::Tree, size: f64, centre_y: f64, out: &mut Vec<EntityType>) {
    let (width, height) = (tree.size().width() as f64, tree.size().height() as f64);
    let (kx, ky) = (size / width, size / height);
    fn walk<'a>(group: &'a usvg::Group, paths: &mut Vec<&'a usvg::Path>) {
        for node in group.children() {
            match node {
                usvg::Node::Group(group) => walk(group, paths),
                usvg::Node::Path(path) if path.is_visible() => paths.push(path),
                _ => {}
            }
        }
    }
    let mut paths = Vec::new();
    walk(tree.root(), &mut paths);
    for path in paths {
        let transform = path.abs_transform();
        let at = |p: usvg::tiny_skia_path::Point| {
            let mut p = p;
            transform.map_point(&mut p);
            Vector2::new((p.x as f64 / width - 0.5) * size, centre_y - (p.y as f64 / height - 0.5) * size)
        };
        // Subpaths as polylines, and whether each is closed.
        let mut subpaths: Vec<(Vec<Vector2>, bool)> = Vec::new();
        for segment in path.data().segments() {
            use usvg::tiny_skia_path::PathSegment;
            match segment {
                PathSegment::MoveTo(p) => subpaths.push((vec![at(p)], false)),
                PathSegment::LineTo(p) => {
                    if let Some((points, _)) = subpaths.last_mut() {
                        points.push(at(p));
                    }
                }
                PathSegment::QuadTo(c, p) => {
                    if let Some((points, _)) = subpaths.last_mut() {
                        let from = *points.last().expect("a subpath starts with a point");
                        flatten(from, &[at(c), at(p)], ICON_TOLERANCE_MM, points);
                    }
                }
                PathSegment::CubicTo(c1, c2, p) => {
                    if let Some((points, _)) = subpaths.last_mut() {
                        let from = *points.last().expect("a subpath starts with a point");
                        flatten(from, &[at(c1), at(c2), at(p)], ICON_TOLERANCE_MM, points);
                    }
                }
                PathSegment::Close => {
                    if let Some((_, closed)) = subpaths.last_mut() {
                        *closed = true;
                    }
                }
            }
        }
        if let Some(fill) = path.fill() {
            let loops: Vec<Vec<Vector2>> = subpaths.iter().filter(|(points, _)| points.len() >= 3).map(|(points, _)| points.clone()).collect();
            if !loops.is_empty() {
                out.push(solid(loops, paint_color(fill.paint())));
            }
        }
        if let Some(stroke) = path.stroke() {
            let scale = (transform.sx as f64 * transform.sy as f64 - transform.kx as f64 * transform.ky as f64).abs().sqrt() * (kx * ky).sqrt();
            let width = stroke.width().get() as f64 * scale;
            for (points, closed) in subpaths.iter().filter(|(points, _)| points.len() >= 2) {
                let mut polyline = LwPolyline::from_points(points.clone());
                polyline.is_closed = *closed;
                polyline.constant_width = width;
                out.push(on_layer_zero(EntityType::LwPolyline(polyline), paint_color(stroke.paint())));
            }
        }
    }
}

/// The block content of a device drawn as the SecurePlan canvas draws it,
/// with the SVG icon `tree` and equipment's `badge`, in world mm at symbol
/// scale 1, y up, the device's facing along +X for the kinds that turn.
pub fn icon_block(kind: DeviceKind, tree: &usvg::Tree, badge: Option<&str>) -> Vec<EntityType> {
    let web = kind.web();
    let r = web.radius;
    // The filled circle, in the INSERT's colour, and the ring on its edge.
    let circle = |from: f64| {
        let mut edge = PolylineEdge::new(Vec::new(), true);
        edge.add_vertex(Vector2::new(from, 0.0), 1.0);
        edge.add_vertex(Vector2::new(-from, 0.0), 1.0);
        edge
    };
    let mut fill = Hatch::solid();
    let mut path = BoundaryPath::external();
    path.add_edge(BoundaryEdge::Polyline(circle(r)));
    fill.paths.push(path);
    let mut out = vec![on_layer_zero(EntityType::Hatch(fill), Color::ByBlock)];
    let mut ring = LwPolyline::new();
    ring.add_point_with_bulge(Vector2::new(r, 0.0), 1.0);
    ring.add_point_with_bulge(Vector2::new(-r, 0.0), 1.0);
    ring.is_closed = true;
    ring.constant_width = web.ring_width;
    out.push(on_layer_zero(EntityType::LwPolyline(ring), light()));
    icon_linework(tree, web.icon_size, -web.icon_offset_y, &mut out);
    if let Some(badge) = badge.filter(|b| !b.is_empty()) {
        // The canvas places the badge's top 172 mm below the centre.
        let at = Vector3::new(0.0, -(BADGE_TOP_MM + BADGE_SIZE_MM / 2.0), 0.0);
        let mut text = Text::with_value(badge, at).with_height(BADGE_SIZE_MM * CAP_HEIGHT);
        text.horizontal_alignment = TextHorizontalAlignment::Center;
        text.vertical_alignment = TextVerticalAlignment::Middle;
        text.alignment_point = Some(at);
        out.push(on_layer_zero(EntityType::Text(text), light()));
    }
    out
}

fn by_block(mut entity: EntityType) -> EntityType {
    let common = entity.common_mut();
    common.layer = "0".into();
    common.color = Color::ByBlock;
    entity
}

fn closed(points: &[(f64, f64)]) -> EntityType {
    let mut polyline = LwPolyline::from_points(points.iter().map(|&(x, y)| Vector2::new(x, y)).collect());
    polyline.is_closed = true;
    by_block(EntityType::LwPolyline(polyline))
}

/// The block content of `kind`'s symbol at unit radius, facing +X.
pub fn symbol(kind: DeviceKind) -> Vec<EntityType> {
    match kind {
        DeviceKind::Camera => {
            let mut body = Circle::new();
            body.radius = 1.0;
            // A 60° view wedge reaching to twice the body's radius.
            let half = 30f64.to_radians();
            let ray = |angle: f64| {
                by_block(EntityType::Line(Line::from_points(
                    Vector3::new(angle.cos(), angle.sin(), 0.0),
                    Vector3::new(2.0 * angle.cos(), 2.0 * angle.sin(), 0.0),
                )))
            };
            let mut front = Arc::new();
            front.radius = 2.0;
            // Counter-clockwise from −30° (as 330°) to 30°.
            front.start_angle = std::f64::consts::TAU - half;
            front.end_angle = half;
            vec![by_block(EntityType::Circle(body)), ray(-half), ray(half), by_block(EntityType::Arc(front))]
        }
        DeviceKind::Equipment => vec![closed(&[(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)])],
        DeviceKind::Asset => vec![closed(&[(1.0, 0.0), (0.0, 1.0), (-1.0, 0.0), (0.0, -1.0)])],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_are_unit_sized_by_block_and_on_layer_zero() {
        for kind in DeviceKind::ALL {
            let entities = symbol(kind);
            assert!(!entities.is_empty());
            for entity in &entities {
                assert_eq!(entity.common().layer, "0");
                assert_eq!(entity.common().color, Color::ByBlock);
            }
            assert_eq!(DeviceKind::parse(&kind.tag().to_ascii_lowercase()), Some(kind));
        }
        // The camera faces +X: its wedge lies right of the body.
        let EntityType::Arc(front) = &symbol(DeviceKind::Camera)[3] else { panic!("the wedge's front") };
        assert!(front.start_angle > std::f64::consts::PI && front.end_angle < std::f64::consts::FRAC_PI_2 && front.radius == 2.0);
    }

    /// Every vertex of the block's polylines and hatch loops.
    fn extent(entities: &[EntityType]) -> (f64, f64, f64, f64) {
        let mut points = Vec::new();
        for entity in entities {
            match entity {
                EntityType::LwPolyline(p) => points.extend(p.vertices.iter().map(|v| v.location)),
                EntityType::Hatch(h) => {
                    for path in &h.paths {
                        for edge in &path.edges {
                            if let BoundaryEdge::Polyline(edge) = edge {
                                points.extend(edge.vertices.iter().map(|v| Vector2::new(v.x, v.y)));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        points.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |(a, b, c, d), p| (a.min(p.x), b.min(p.y), c.max(p.x), d.max(p.y)))
    }

    #[test]
    fn icon_blocks_hold_the_filled_circle_ring_linework_and_badge() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24"><circle cx="12" cy="12" r="10" fill="none" stroke="#123456" stroke-width="2"/><path d="M4 4 L20 4 L20 20 Z" fill="#fbfaf4"/></svg>"##;
        let tree = crate::app::secureplan::icons::parse_svg(svg.as_bytes()).unwrap();
        let block = icon_block(DeviceKind::Equipment, &tree, Some("NVR"));
        // The filled circle in the device colour, then the ring.
        let EntityType::Hatch(fill) = &block[0] else { panic!("the filled circle") };
        assert!(fill.is_solid && block[0].common().color == Color::ByBlock && block[0].common().layer == "0");
        let EntityType::LwPolyline(ring) = &block[1] else { panic!("the ring") };
        assert_eq!((ring.constant_width, ring.vertices[0].location.x, ring.vertices[0].bulge), (60.0, 330.0, 1.0));
        assert_eq!(block[1].common().color, Color::Rgb { r: 0xfb, g: 0xfa, b: 0xf4 });
        // The SVG circle's stroke: a closed polyline in its colour, 2 × 390/24 wide,
        // within the tolerance of the 10/24 × 390 mm radius about the icon's centre (52 mm up).
        let stroke = block.iter().find_map(|e| match e {
            EntityType::LwPolyline(p) if e.common().color == Color::Rgb { r: 0x12, g: 0x34, b: 0x56 } => Some(p),
            _ => None,
        });
        let stroke = stroke.expect("the stroke");
        assert!(stroke.is_closed && (stroke.constant_width - 2.0 * 390.0 / 24.0).abs() < 1e-3, "{}", stroke.constant_width);
        let radius = 10.0 / 24.0 * 390.0;
        for v in &stroke.vertices {
            let d = v.location.x.hypot(v.location.y - 52.0);
            // usvg's cubic arcs stray 0.03 % from the circle.
            assert!(d <= radius + 0.1 && d >= radius - ICON_TOLERANCE_MM - 0.1, "{d}");
        }
        // The fill: a solid hatch in its colour; SVG y down is block y up.
        let triangle = block.iter().skip(2).find(|e| matches!(e, EntityType::Hatch(_))).expect("the filled triangle");
        let (x0, y0, x1, y1) = extent(std::slice::from_ref(triangle));
        let k = 390.0 / 24.0;
        assert!((x0 + 8.0 * k).abs() < 1e-3 && (x1 - 8.0 * k).abs() < 1e-3);
        assert!((y1 - (52.0 + 8.0 * k)).abs() < 1e-3 && (y0 - (52.0 - 8.0 * k)).abs() < 1e-3);
        // The badge, centred under the icon.
        let Some(EntityType::Text(badge)) = block.last() else { panic!("the badge") };
        assert_eq!(badge.value, "NVR");
        assert_eq!(badge.alignment_point, Some(Vector3::new(0.0, -(172.0 + 57.5), 0.0)));
        // A camera has no badge.
        assert!(!icon_block(DeviceKind::Camera, &tree, None).iter().any(|e| matches!(e, EntityType::Text(_))));
    }

    #[test]
    fn curves_are_flattened_within_the_tolerance() {
        // A quarter circle as a cubic, radius 1000.
        let k = 0.5523 * 1000.0;
        let control = [Vector2::new(1000.0, k), Vector2::new(k, 1000.0), Vector2::new(0.0, 1000.0)];
        let mut points = vec![Vector2::new(1000.0, 0.0)];
        flatten(points[0], &control, ICON_TOLERANCE_MM, &mut points);
        assert!(points.len() > 4 && points.len() < 64, "{}", points.len());
        for pair in points.windows(2) {
            let middle = (pair[0] + pair[1]) * 0.5;
            assert!(1000.0 - middle.length() <= ICON_TOLERANCE_MM + 0.3, "{}", middle.length());
        }
        // A straight "curve" is one segment.
        let mut line = vec![Vector2::new(0.0, 0.0)];
        flatten(line[0], &[Vector2::new(1.0, 0.0), Vector2::new(2.0, 0.0), Vector2::new(3.0, 0.0)], ICON_TOLERANCE_MM, &mut line);
        assert_eq!(line.len(), 2);
    }
}
