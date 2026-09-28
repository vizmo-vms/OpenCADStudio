//! Read-only SecurePlan design overlay (OVL-01, OVL-02).
//!
//! The web sends its current draft in world millimetres
//! (`application/vnd.secureplan.overlay+json`, `overlay.schema.json`). The
//! desktop keeps it outside the drawing, as plain data, and draws it over the
//! model-space viewport through the inverse of the alignment mapping, labelled
//! "SecurePlan design (read-only)". It is never a CAD entity, so it can never
//! be selected, edited, converted or written to the drawing. An
//! `overlayUpdate` replaces it; nothing is ever merged.
//!
//! Devices are drawn as the SecurePlan canvas draws them (schema version 2):
//! a circle in the device colour with a `#fbfaf4` ring, the device's icon,
//! equipment's badge and the label, at the canvas's sizes times the device's
//! symbol scale and the web's icon scale. Coverage is filled in the camera's
//! colour at the document's coverage opacity, with a stroke of the same
//! colour. Labels are `#17233b` (the web's) on a light viewport background
//! and `#fbfaf4` on a dark one. As on the web canvas, equipment's badge and
//! the labels of equipment and assets turn with their symbol; a camera's
//! label stays upright on the plan (both turn with the plan when the drawing
//! is aligned turned).
//!
//! Cost: each icon is decoded once per overlay (see [`super::icons`]) and
//! rendered once per icon and pixel-size bucket. The geometry is built into
//! a canvas cache that is rebuilt only when the overlay, the mapping, the
//! view or the viewport changes, not on every redraw. Off-screen devices and
//! coverage are skipped, a symbol under a few pixels is a plain dot, and
//! labels, badges and icons too small to read are left out.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use iced::advanced::image::Handle;
use iced::widget::canvas;
use iced::{mouse, Color, Element, Length, Point, Rectangle, Theme, Vector};

use super::align::world_to_cad;
use super::icons::{self, Decoded};
use super::publish::Mapping;
pub use super::symbols::DeviceKind;
use super::symbols::{BADGE_SIZE_MM, BADGE_TOP_MM, WEB_INK, WEB_LIGHT};
use crate::app::{Message, OpenCADStudio};

/// A wall material (`wallMaterial`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Material {
    Solid,
    Glass,
    Fence,
    Opening,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub kind: DeviceKind,
    /// The catalog's built-in symbol key (kept for export).
    pub symbol_key: String,
    /// The catalog display name.
    pub name: String,
    /// The element label the web canvas shows.
    pub label: String,
    pub color: Color,
    pub position: [f64; 2],
    /// World rotation in degrees (world y points down, as on the web canvas).
    pub rotation_deg: f64,
    pub symbol_scale: f64,
    /// Index into [`Overlay::icons`]; `None` draws the standard symbol.
    pub icon: Option<usize>,
    /// Equipment's badge text ("SW", "NVR").
    pub badge: Option<String>,
}

/// A camera's coverage polygon and its effective colour.
#[derive(Debug, Clone, PartialEq)]
pub struct Coverage {
    pub polygon: Vec<[f64; 2]>,
    pub color: Color,
}

/// The web's view settings (`display`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Display {
    pub icon_scale: f64,
    pub coverage_opacity: f64,
    pub show_labels: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self { icon_scale: 1.0, coverage_opacity: 0.16, show_labels: true }
    }
}

/// An icon, decoded once, and its screen images, made once per pixel-size
/// bucket.
pub struct Icon {
    pub id: String,
    /// `None` when it could not be decoded: its devices get the standard symbol.
    pub decoded: Option<Decoded>,
    images: Mutex<Vec<(u32, Handle)>>,
}

impl Icon {
    pub fn new(id: String, decoded: Option<Decoded>) -> Self {
        Self { id, decoded, images: Mutex::new(Vec::new()) }
    }

    /// The image to draw at about `px` pixels: an SVG is rendered at the
    /// next power of two between 16 and 256 pixels; a raster icon has one
    /// image, scaled when drawn.
    fn image(&self, px: f32) -> Option<Handle> {
        let decoded = self.decoded.as_ref()?;
        let bucket = match decoded {
            Decoded::Svg(_) => (px.max(1.0).ceil() as u32).next_power_of_two().clamp(16, 256),
            Decoded::Raster(_) => 0,
        };
        let mut images = self.images.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, handle)) = images.iter().find(|(b, _)| *b == bucket) {
            return Some(handle.clone());
        }
        let handle = match decoded {
            Decoded::Svg(tree) => Handle::from_rgba(bucket, bucket, icons::rasterize_svg(tree, bucket)?),
            Decoded::Raster(raster) => Handle::from_rgba(raster.width, raster.height, raster.rgba.clone()),
        };
        images.push((bucket, handle.clone()));
        Some(handle)
    }

    /// How many images were made (the tests).
    #[cfg(test)]
    fn images_made(&self) -> usize {
        self.images.lock().unwrap().len()
    }
}

impl std::fmt::Debug for Icon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Icon").field("id", &self.id).field("decoded", &self.decoded.is_some()).finish()
    }
}

impl PartialEq for Icon {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

/// The overlay payload, in world millimetres.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    /// Unique per parsed payload: the drawn geometry is rebuilt when it changes.
    pub id: u64,
    pub walls: Vec<(Material, [f64; 2], [f64; 2])>,
    pub doors: Vec<([f64; 2], [f64; 2])>,
    pub routes: Vec<Vec<[f64; 2]>>,
    pub devices: Vec<Device>,
    pub coverage: Vec<Coverage>,
    pub icons: Arc<Vec<Icon>>,
    pub display: Display,
}

impl Overlay {
    /// Whether the survey has no design elements at all.
    pub fn is_empty(&self) -> bool {
        self.walls.is_empty() && self.doors.is_empty() && self.routes.is_empty() && self.devices.is_empty()
    }
}

fn point(value: &serde_json::Value) -> [f64; 2] {
    [value[0].as_f64().unwrap_or_default(), value[1].as_f64().unwrap_or_default()]
}

fn color(hex: &str) -> Color {
    let channel = |i: usize| u8::from_str_radix(hex.get(1 + 2 * i..3 + 2 * i).unwrap_or("80"), 16).unwrap_or(0x80);
    Color::from_rgb8(channel(0), channel(1), channel(2))
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::from_rgb8(r, g, b)
}

/// Parse and validate an overlay payload against the pinned schema and its
/// semantic rules, decoding each icon once.
pub fn parse(bytes: &[u8]) -> Result<Overlay, String> {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    if bytes.len() as u64 > super::protocol::MAX_JSON_PAYLOAD_BYTES {
        return Err("overlay over 8 MiB".into());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| "overlay is not JSON".to_string())?;
    super::protocol::validate_payload("overlay.schema.json", &value).map_err(|e| e.to_string())?;
    let icons: Vec<Icon> =
        icons::parse_list(&value["icons"], &value["devices"])?.into_iter().map(|data| Icon::new(data.id.clone(), icons::decode(&data))).collect();
    let list = |key: &str| value[key].as_array().cloned().unwrap_or_default();
    let material = |name: &str| match name {
        "glass" => Material::Glass,
        "fence" => Material::Fence,
        "opening" => Material::Opening,
        _ => Material::Solid,
    };
    let display = &value["display"];
    let defaults = Display::default();
    Ok(Overlay {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        walls: list("walls")
            .iter()
            .map(|w| (material(w["material"].as_str().unwrap_or_default()), point(&w["start"]), point(&w["end"])))
            .collect(),
        doors: list("doors").iter().map(|d| (point(&d["start"]), point(&d["end"]))).collect(),
        routes: list("routes")
            .iter()
            .map(|r| r["points"].as_array().into_iter().flatten().map(point).collect())
            .collect(),
        devices: list("devices")
            .iter()
            .map(|d| Device {
                kind: DeviceKind::parse(d["kind"].as_str().unwrap_or_default()).unwrap_or(DeviceKind::Asset),
                symbol_key: d["symbolKey"].as_str().unwrap_or_default().to_string(),
                rotation_deg: d["rotationDeg"].as_f64().unwrap_or_default(),
                name: d["name"].as_str().unwrap_or_default().to_string(),
                label: d["label"].as_str().unwrap_or_default().to_string(),
                color: color(d["color"].as_str().unwrap_or("#808080")),
                position: point(&d["position"]),
                symbol_scale: d["symbolScale"].as_f64().unwrap_or(1.0),
                icon: d["iconId"].as_str().and_then(|id| icons.iter().position(|icon| icon.id == id)),
                badge: d["badge"].as_str().map(str::to_string),
            })
            .collect(),
        coverage: list("coverage")
            .iter()
            .map(|c| Coverage {
                polygon: c["polygon"].as_array().into_iter().flatten().map(point).collect(),
                color: color(c["color"].as_str().unwrap_or("#808080")),
            })
            .collect(),
        icons: Arc::new(icons),
        display: Display {
            icon_scale: display["iconScale"].as_f64().unwrap_or(defaults.icon_scale),
            coverage_opacity: display["coverageOpacity"].as_f64().unwrap_or(defaults.coverage_opacity),
            show_labels: display["showLabels"].as_bool().unwrap_or(defaults.show_labels),
        },
    })
}

/// A symbol whose circle is smaller than this radius (pixels) is a dot.
const DOT_BELOW_PX: f32 = 2.5;
/// The dot's side, in pixels.
const DOT_PX: f32 = 3.0;
/// Text smaller than this (pixels) is left out.
const MIN_TEXT_PX: f32 = 7.0;
/// An icon smaller than this (pixels) is left out; the circle remains.
const MIN_ICON_PX: f32 = 6.0;
/// Coverage outline width, world mm (the web's `strokeWidth`).
const COVERAGE_STROKE_MM: f32 = 30.0;

/// What sits inside a device's circle.
#[derive(Debug, Clone, PartialEq)]
enum Inside {
    /// Icon `index`, `size` pixels square, centred at `at`, turned `rotation`
    /// radians clockwise.
    Icon { index: usize, at: Point, size: f32, rotation: f32 },
    /// The standard symbol of the kind, `size` pixels across, facing `facing`.
    Standard { at: Point, size: f32, facing: Vector },
    Nothing,
}

/// Text centred at `at`, turned `rotation` radians clockwise about it.
#[derive(Debug, Clone, PartialEq)]
struct Caption {
    text: String,
    at: Point,
    size: f32,
    bold: bool,
    color: Color,
    rotation: f32,
}

/// Something to draw, in canvas pixels.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Line { points: Vec<Point>, color: Color, width: f32, dashed: bool, closed: bool },
    /// A filled polygon with an outline of the same colour (coverage).
    Area { points: Vec<Point>, color: Color, stroke: f32 },
    /// A device too small for its symbol.
    Dot { at: Point, color: Color },
    Device { at: Point, kind: DeviceKind, color: Color, radius: f32, ring: f32, inside: Inside, badge: Option<Caption>, label: Option<Caption> },
    /// A device under a previous alignment: a grey outline.
    Ghost { at: Point, radius: f32 },
}

const WALL: Color = Color { r: 0.10, g: 0.55, b: 0.95, a: 0.95 };
const DOOR: Color = Color { r: 0.95, g: 0.60, b: 0.10, a: 0.95 };
const ROUTE: Color = Color { r: 0.20, g: 0.80, b: 0.35, a: 0.95 };
const BEFORE: Color = Color { r: 0.6, g: 0.6, b: 0.6, a: 0.6 };

/// A point `local` (pixels, y down, in a frame turned `angle` radians
/// clockwise) from `at`.
fn turned(at: Point, angle: f32, [x, y]: [f32; 2]) -> Point {
    let (sin, cos) = angle.sin_cos();
    Point::new(at.x + x * cos - y * sin, at.y + x * sin + y * cos)
}

/// The overlay's shapes on screen for `mapping`, with `project` taking CAD
/// coordinates to the canvas. `before` draws a previous alignment in grey.
/// Only what can show inside `viewport` is kept. `ink` is the label colour.
fn shapes(overlay: &Overlay, mapping: &Mapping, before: bool, project: &dyn Fn([f64; 2]) -> Option<Point>, viewport: Rectangle, ink: Color) -> Vec<Shape> {
    let to_screen = |world: [f64; 2]| project(world_to_cad(mapping, world));
    let pick = |color: Color| if before { BEFORE } else { color };
    let mut out = Vec::new();
    let mut line = |points: &[[f64; 2]], color: Color, width: f32, dashed: bool, closed: bool| {
        let points: Option<Vec<Point>> = points.iter().map(|p| to_screen(*p)).collect();
        if let Some(points) = points.filter(|p| p.len() >= 2) {
            out.push(Shape::Line { points, color: pick(color), width, dashed: dashed || before, closed });
        }
    };
    for (material, a, b) in &overlay.walls {
        // Glass and openings are dashed so the material shows without colour.
        let dashed = matches!(material, Material::Glass | Material::Opening);
        line(&[*a, *b], WALL, 2.5, dashed, false);
    }
    for (a, b) in &overlay.doors {
        line(&[*a, *b], DOOR, 2.0, false, false);
    }
    for route in &overlay.routes {
        line(route, ROUTE, 1.5, false, false);
    }
    // One world millimetre in pixels and the direction of world +x on
    // screen, from the mapping and view (the same for every point: both are
    // similarities in plan view).
    let Some(origin) = overlay.devices.first().map(|d| d.position).or_else(|| overlay.coverage.first().and_then(|c| c.polygon.first().copied())) else {
        return out;
    };
    let (Some(o), Some(e)) = (to_screen(origin), to_screen([origin[0] + 1000.0, origin[1]])) else { return out };
    let px_per_mm = (e.x - o.x).hypot(e.y - o.y) / 1000.0;
    let east = (e.y - o.y).atan2(e.x - o.x);
    if !(px_per_mm.is_finite() && px_per_mm > 0.0) {
        return out;
    }
    let near = |at: Point, margin: f32| {
        at.x >= viewport.x - margin && at.x <= viewport.x + viewport.width + margin && at.y >= viewport.y - margin && at.y <= viewport.y + viewport.height + margin
    };
    if !before {
        let stroke = (COVERAGE_STROKE_MM * px_per_mm).max(1.0);
        let opacity = overlay.display.coverage_opacity as f32;
        for coverage in &overlay.coverage {
            let Some(points) = coverage.polygon.iter().map(|p| to_screen(*p)).collect::<Option<Vec<Point>>>() else { continue };
            let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
            for p in &points {
                (x0, y0, x1, y1) = (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y));
            }
            let visible = x1 >= viewport.x && x0 <= viewport.x + viewport.width && y1 >= viewport.y && y0 <= viewport.y + viewport.height;
            if points.len() >= 3 && visible && (x1 - x0 >= 1.0 || y1 - y0 >= 1.0) {
                out.push(Shape::Area { points, color: Color { a: opacity, ..coverage.color }, stroke });
            }
        }
    }
    let icon_scale = overlay.display.icon_scale as f32;
    for device in &overlay.devices {
        let Some(at) = to_screen(device.position) else { continue };
        let web = device.kind.web();
        // Pixels per millimetre of the symbol.
        let k = device.symbol_scale as f32 * icon_scale * px_per_mm;
        let radius = web.radius as f32 * k;
        let ring = web.ring_width as f32 * k;
        if before {
            if near(at, radius) {
                out.push(Shape::Ghost { at, radius: radius.max(DOT_PX) });
            }
            continue;
        }
        if radius < DOT_BELOW_PX {
            if near(at, DOT_PX) {
                out.push(Shape::Dot { at, color: device.color });
            }
            continue;
        }
        let rotation = device.rotation_deg.to_radians() as f32;
        let angle = if web.rotates { east + rotation } else { east };
        let label_size = web.label_size as f32 * k;
        let show_label = overlay.display.show_labels && label_size >= MIN_TEXT_PX && !device.label.trim().is_empty();
        let label_chars = if show_label { device.label.chars().count().min(80) as f32 } else { 0.0 };
        let reach = (radius + ring / 2.0).max(if show_label { (web.label_top + web.label_size) as f32 * k } else { 0.0 });
        if !near(at, reach + label_chars * 0.3 * label_size) {
            continue;
        }
        let icon_size = web.icon_size as f32 * k;
        let icon_at = turned(at, angle, [0.0, web.icon_offset_y as f32 * k]);
        let decoded = device.icon.filter(|&index| overlay.icons.get(index).is_some_and(|icon| icon.decoded.is_some()));
        let inside = match decoded {
            _ if icon_size < MIN_ICON_PX => Inside::Nothing,
            Some(index) => Inside::Icon { index, at: icon_at, size: icon_size, rotation: angle },
            None => {
                // A camera's standard symbol shows where it looks.
                let facing = if web.rotates { angle } else { east + rotation };
                Inside::Standard { at: icon_at, size: icon_size, facing: Vector::new(facing.cos(), facing.sin()) }
            }
        };
        let badge_size = BADGE_SIZE_MM as f32 * k;
        let badge = device.badge.as_ref().filter(|_| device.kind == DeviceKind::Equipment && badge_size >= MIN_TEXT_PX).map(|text| Caption {
            text: text.clone(),
            at: turned(at, angle, [0.0, (BADGE_TOP_MM + BADGE_SIZE_MM / 2.0) as f32 * k]),
            size: badge_size,
            bold: true,
            color: rgb(WEB_LIGHT),
            rotation: angle,
        });
        let label = show_label.then(|| Caption {
            text: device.label.chars().take(80).collect(),
            at: turned(at, angle, [0.0, (web.label_top + web.label_size / 2.0) as f32 * k]),
            size: label_size,
            bold: web.label_bold,
            color: ink,
            rotation: angle,
        });
        out.push(Shape::Device { at, kind: device.kind, color: device.color, radius, ring, inside, badge, label });
    }
    out
}

fn polygon(points: &[Point], closed: bool) -> canvas::Path {
    canvas::Path::new(|builder| {
        builder.move_to(points[0]);
        for p in &points[1..] {
            builder.line_to(*p);
        }
        if closed {
            builder.close();
        }
    })
}

fn caption(frame: &mut canvas::Frame, caption: &Caption) {
    let font = if caption.bold { iced::Font { weight: iced::font::Weight::Bold, ..iced::Font::MONOSPACE } } else { iced::Font::MONOSPACE };
    let text = |position: Point| canvas::Text {
        content: caption.text.clone(),
        position,
        color: caption.color,
        size: caption.size.into(),
        font,
        align_x: iced::alignment::Horizontal::Center.into(),
        align_y: iced::alignment::Vertical::Center,
        ..Default::default()
    };
    // Upright text is drawn as text; turned text is drawn as its outlines.
    let turn = caption.rotation.rem_euclid(std::f32::consts::TAU);
    if turn.min(std::f32::consts::TAU - turn) < 1e-4 {
        frame.fill_text(text(caption.at));
        return;
    }
    frame.with_save(|frame| {
        frame.translate(Vector::new(caption.at.x, caption.at.y));
        frame.rotate(caption.rotation);
        frame.fill_text(text(Point::ORIGIN));
    });
}

/// A circle under this radius (pixels) is drawn as a 12-sided polygon,
/// which tessellates several times faster and looks the same that small.
const POLYGON_BELOW_PX: f32 = 10.0;

fn circle(at: Point, radius: f32) -> canvas::Path {
    if radius >= POLYGON_BELOW_PX {
        return canvas::Path::circle(at, radius);
    }
    canvas::Path::new(|builder| {
        for i in 0..12 {
            let (sin, cos) = (i as f32 * std::f32::consts::TAU / 12.0).sin_cos();
            let p = Point::new(at.x + radius * cos, at.y + radius * sin);
            if i == 0 { builder.move_to(p) } else { builder.line_to(p) }
        }
        builder.close();
    })
}

/// Draw `shapes`, with `icons` for their icons.
fn draw_shapes(frame: &mut canvas::Frame, shapes: &[Shape], icons: &[Icon]) {
    let light = rgb(WEB_LIGHT);
    for shape in shapes {
        match shape {
            Shape::Line { points, color, width, dashed, closed } => {
                let mut stroke = canvas::Stroke::default().with_color(*color).with_width(*width);
                if *dashed {
                    stroke.line_dash = canvas::LineDash { segments: &[6.0, 4.0], offset: 0 };
                }
                frame.stroke(&polygon(points, *closed), stroke);
            }
            Shape::Area { points, color, stroke } => {
                let path = polygon(points, true);
                frame.fill(&path, *color);
                frame.stroke(&path, canvas::Stroke::default().with_color(*color).with_width(*stroke));
            }
            Shape::Dot { at, color } => {
                frame.fill_rectangle(Point::new(at.x - DOT_PX / 2.0, at.y - DOT_PX / 2.0), iced::Size::new(DOT_PX, DOT_PX), *color);
            }
            Shape::Ghost { at, radius } => {
                frame.stroke(&circle(*at, *radius), canvas::Stroke::default().with_color(BEFORE).with_width(1.5));
            }
            Shape::Device { at, kind, color, radius, ring, inside, badge, label } => {
                let body = circle(*at, *radius);
                frame.fill(&body, *color);
                frame.stroke(&body, canvas::Stroke::default().with_color(light).with_width(*ring));
                match inside {
                    Inside::Icon { index, at, size, rotation } => {
                        if let Some(handle) = icons.get(*index).and_then(|icon| icon.image(*size)) {
                            let bounds = Rectangle::new(Point::new(at.x - size / 2.0, at.y - size / 2.0), iced::Size::new(*size, *size));
                            frame.draw_image(bounds, canvas::Image::new(handle).rotation(iced::Radians(*rotation)));
                        }
                    }
                    Inside::Standard { at, size, facing } => {
                        let stroke = canvas::Stroke::default().with_color(light).with_width((size * 0.08).max(1.0));
                        let (fx, fy) = (facing.x, facing.y);
                        let half = size / 2.0;
                        // The kind's standard symbol, in the icon's box.
                        let local = |forward: f32, side: f32| Point::new(at.x + (forward * fx - side * fy) * half, at.y + (forward * fy + side * fx) * half);
                        let shape: Vec<Point> = match kind {
                            DeviceKind::Camera => {
                                frame.stroke(&circle(local(-0.35, 0.0), 0.3 * half), stroke);
                                [(-0.05, 0.0), (0.8, -0.45), (0.8, 0.45)].iter().map(|&(f, s)| local(f, s)).collect()
                            }
                            DeviceKind::Equipment => [(-0.6, -0.6), (0.6, -0.6), (0.6, 0.6), (-0.6, 0.6)].iter().map(|&(f, s)| local(f, s)).collect(),
                            DeviceKind::Asset => [(0.7, 0.0), (0.0, 0.7), (-0.7, 0.0), (0.0, -0.7)].iter().map(|&(f, s)| local(f, s)).collect(),
                        };
                        frame.stroke(&polygon(&shape, true), stroke);
                    }
                    Inside::Nothing => {}
                }
                for text in badge.iter().chain(label) {
                    caption(frame, text);
                }
            }
        }
    }
}

/// Everything the drawn geometry depends on: when it is unchanged, the
/// cached geometry is drawn again as it is.
#[derive(Debug, Clone, PartialEq)]
struct ViewKey {
    overlay: u64,
    current: Option<Mapping>,
    before: Option<Mapping>,
    camera: [f64; 10],
    tile: Rectangle,
    ink: Color,
}

fn camera_key(camera: &crate::scene::view::camera::Camera) -> [f64; 10] {
    let (t, r) = (camera.target, camera.rotation);
    let perspective = camera.projection == crate::scene::view::camera::Projection::Perspective;
    [t.x, t.y, t.z, r.x as f64, r.y as f64, r.z as f64, r.w as f64, camera.distance as f64, camera.fov_y as f64, f64::from(u8::from(perspective))]
}

/// The canvas's state: the cached geometry and what it was built for.
#[derive(Default)]
pub struct CanvasState {
    key: RefCell<Option<ViewKey>>,
    cache: canvas::Cache<iced::Renderer>,
}

struct OverlayCanvas<'a> {
    overlay: &'a Overlay,
    camera: crate::scene::view::camera::Camera,
    key: ViewKey,
}

impl OverlayCanvas<'_> {
    fn build(&self, frame: &mut canvas::Frame) {
        let tile = self.key.tile;
        let project = |[x, y]: [f64; 2]| {
            self.camera
                .project(glam::DVec3::new(x, y, 0.0), tile)
                .map(|p| Point::new(tile.x + p.x, tile.y + p.y))
                .filter(|p| p.x.is_finite() && p.y.is_finite())
        };
        let viewport = Rectangle::new(Point::ORIGIN, frame.size());
        let mut drawn = Vec::new();
        if let Some(before) = self.key.before {
            drawn.extend(shapes(self.overlay, &before, true, &project, viewport, self.key.ink));
        }
        if let Some(current) = self.key.current {
            drawn.extend(shapes(self.overlay, &current, false, &project, viewport, self.key.ink));
        }
        draw_shapes(frame, &drawn, &self.overlay.icons);
    }
}

impl canvas::Program<Message> for OverlayCanvas<'_> {
    type State = CanvasState;

    fn draw(&self, state: &CanvasState, renderer: &iced::Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<canvas::Geometry> {
        let mut key = state.key.borrow_mut();
        if key.as_ref() != Some(&self.key) {
            state.cache.clear();
            *key = Some(self.key.clone());
        }
        vec![state.cache.draw(renderer, bounds.size(), |frame| self.build(frame))]
    }
}

pub const LABEL: &str = "SecurePlan design (read-only)";

impl OpenCADStudio {
    /// Replace the overlay of the session's document (OVL-02).
    pub(crate) fn secureplan_overlay_update(&mut self, session: super::bridge::SessionId, transfer_id: u32, survey_empty: bool) {
        let parsed = self
            .secureplan
            .sessions
            .transfers
            .remove(&(session, transfer_id))
            .filter(|t| t.media_type == "application/vnd.secureplan.overlay+json")
            .map(|t| parse(t.bytes.expose()));
        match parsed {
            Some(Ok(overlay)) => {
                if let Some(bound) = self.secureplan.sessions.by_session_mut(session) {
                    bound.overlay = Some(overlay);
                    bound.survey_empty = survey_empty;
                }
            }
            _ => self.secureplan_protocol_error(session),
        }
    }

    /// The overlay layer for the model-space viewport, when the active tab is
    /// bound, has an overlay and it is switched on.
    pub(crate) fn secureplan_overlay_layer(&self) -> Option<Element<'_, Message>> {
        use iced::widget::{column, container, text};
        let bound = self.secureplan_active_bound()?;
        let overlay = bound.overlay.as_ref()?;
        if !self.secureplan.overlay_visible {
            return None;
        }
        let tab = &self.tabs[self.active_tab];
        if tab.scene.current_layout != "Model" || tab.is_start {
            return None;
        }
        // While the alignment dialog is open it previews the new mapping, with
        // the stored one in grey behind it (PUB-02).
        let (current, before) = match self.secureplan_alignment_preview() {
            Some(proposed) => (Some(proposed), bound.alignment.map(|a| a.mapping).filter(|m| *m != proposed)),
            None => (bound.alignment.map(|a| a.mapping), None),
        };
        let (vw, vh) = tab.scene.selection.borrow().vp_size;
        let tile = tab.scene.active_model_tile_bounds(vw, vh);
        let camera = tab.scene.camera.borrow().clone();
        // Labels readable on the viewport's background.
        let [r, g, b, _] = tab.scene.bg_color;
        let ink = if 0.2126 * r + 0.7152 * g + 0.0722 * b < 0.5 { rgb(WEB_LIGHT) } else { rgb(WEB_INK) };
        let key = ViewKey { overlay: overlay.id, current, before: before.filter(|_| current.is_some()), camera: camera_key(&camera), tile, ink };
        let note = if current.is_some() { LABEL.to_string() } else { format!("{LABEL}: align the drawing to show it") };
        let label = container(text(note).size(12)).padding([2, 6]).style(|theme: &Theme| container::Style {
            background: Some(iced::Background::Color(theme.palette().background.weakest.color)),
            ..Default::default()
        });
        let layer = iced::widget::stack![
            iced::widget::canvas(OverlayCanvas { overlay, camera, key }).width(Length::Fill).height(Length::Fill),
            column![label].padding([40, 8]).width(Length::Fill).align_x(iced::alignment::Horizontal::Right),
        ];
        Some(layer.width(Length::Fill).height(Length::Fill).into())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::icons::tests::{b64, png_bytes, CIRCLE_SVG};
    use crate::app::secureplan::vectors;

    pub(crate) fn sample_bytes() -> Vec<u8> {
        let sample = vectors::json("payloads/valid.json")["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["schema"] == "overlay.schema.json")
            .unwrap()["value"]
            .clone();
        serde_json::to_vec(&sample).unwrap()
    }

    /// An overlay payload with `walls` and the given devices, coverage and icons.
    pub(crate) fn payload(walls: &[([f64; 2], [f64; 2])], devices: Vec<serde_json::Value>, coverage: Vec<serde_json::Value>, icons: Vec<serde_json::Value>) -> serde_json::Value {
        let walls: Vec<_> = walls
            .iter()
            .enumerate()
            .map(|(i, (a, b))| serde_json::json!({ "id": format!("wall-{i}"), "material": "solid", "start": a, "end": b }))
            .collect();
        serde_json::json!({
            "schemaVersion": 2, "walls": walls, "doors": [], "routes": [], "devices": devices, "coverage": coverage, "icons": icons,
            "display": { "iconScale": 1, "coverageOpacity": 0.16, "showLabels": true },
        })
    }

    pub(crate) fn overlay_bytes(walls: &[([f64; 2], [f64; 2])]) -> Vec<u8> {
        serde_json::to_vec(&payload(walls, Vec::new(), Vec::new(), Vec::new())).unwrap()
    }

    pub(crate) fn device(i: usize, kind: &str, rotation: f64, icon: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": format!("d{i}"), "kind": kind, "symbolKey": "generic", "name": format!("Device {i}"), "label": format!("D-{i}"),
            "color": "#336699", "position": [1000.0, 1000.0], "rotationDeg": rotation, "symbolScale": 1, "iconId": icon,
            "badge": if kind == "equipment" { serde_json::json!("SW") } else { serde_json::Value::Null },
        })
    }

    pub(crate) fn icon(id: &str, media_type: &str, bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({ "id": id, "mediaType": media_type, "data": b64(bytes) })
    }

    /// Millimetres, no turn, 0.1 px per mm: screen = CAD / 10 with y flipped
    /// (y up in CAD).
    fn view() -> (Mapping, impl Fn([f64; 2]) -> Option<Point>) {
        let mapping = Mapping { cad_origin: [0.0, 0.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        (mapping, |cad: [f64; 2]| Some(Point::new(cad[0] as f32 / 10.0, -cad[1] as f32 / 10.0)))
    }

    const SCREEN: Rectangle = Rectangle { x: -5000.0, y: -5000.0, width: 10000.0, height: 10000.0 };

    fn devices_drawn(drawn: &[Shape]) -> Vec<&Shape> {
        drawn.iter().filter(|s| matches!(s, Shape::Device { .. })).collect()
    }

    #[test]
    fn overlays_follow_the_schema() {
        let overlay = parse(&sample_bytes()).expect("the vector sample parses");
        assert!(!overlay.is_empty());
        for sample in vectors::json("payloads/invalid.json")["samples"].as_array().unwrap() {
            if sample["schema"] == "overlay.schema.json" {
                assert!(parse(&serde_json::to_vec(&sample["value"]).unwrap()).is_err(), "{}", sample["name"]);
            }
        }
        assert!(parse(b"not json").is_err());
        let mut extra = payload(&[], Vec::new(), Vec::new(), Vec::new());
        extra["extra"] = serde_json::json!(1);
        assert!(parse(&serde_json::to_vec(&extra).unwrap()).is_err());
        let mut old = payload(&[], Vec::new(), Vec::new(), Vec::new());
        old["schemaVersion"] = serde_json::json!(1);
        assert!(parse(&serde_json::to_vec(&old).unwrap()).is_err(), "version 1 is refused");
    }

    #[test]
    fn version_two_carries_icons_display_labels_and_coverage_colour() {
        let svg = CIRCLE_SVG.as_bytes();
        let devices = vec![device(0, "camera", 0.0, Some("a1")), device(1, "equipment", 90.0, None)];
        let coverage = vec![serde_json::json!({ "elementId": "d0", "polygon": [[0, 0], [3000, 0], [0, 3000]], "color": "#ff8800" })];
        let mut value = payload(&[], devices, coverage, vec![icon("a1", "image/svg+xml", svg)]);
        value["display"] = serde_json::json!({ "iconScale": 1.5, "coverageOpacity": 0.3, "showLabels": false });
        let overlay = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(overlay.display, Display { icon_scale: 1.5, coverage_opacity: 0.3, show_labels: false });
        assert_eq!(overlay.devices[0].icon, Some(0));
        assert_eq!(overlay.devices[0].label, "D-0");
        assert_eq!(overlay.devices[1].icon, None, "null: the standard symbol");
        assert_eq!(overlay.devices[1].badge.as_deref(), Some("SW"));
        assert_eq!(overlay.coverage[0].color, Color::from_rgb8(0xff, 0x88, 0x00));
        assert!(overlay.icons[0].decoded.is_some());
        // Each parse is a new overlay for the cache.
        assert_ne!(parse(&serde_json::to_vec(&value).unwrap()).unwrap().id, overlay.id);
        // Semantic rules: a missing icon, duplicate ids.
        let missing = payload(&[], vec![device(0, "camera", 0.0, Some("b2"))], Vec::new(), vec![icon("a1", "image/svg+xml", svg)]);
        assert!(parse(&serde_json::to_vec(&missing).unwrap()).is_err());
        let twice = payload(&[], Vec::new(), Vec::new(), vec![icon("a1", "image/svg+xml", svg), icon("a1", "image/svg+xml", svg)]);
        assert!(parse(&serde_json::to_vec(&twice).unwrap()).is_err());
    }

    #[test]
    fn a_bad_icon_falls_back_to_the_standard_symbol() {
        let devices = vec![device(0, "camera", 0.0, Some("bad")), device(1, "asset", 0.0, Some("png"))];
        let icons = vec![icon("bad", "image/svg+xml", b"<svg><unclosed"), icon("png", "image/png", &png_bytes(8, 8))];
        let overlay = parse(&serde_json::to_vec(&payload(&[], devices, Vec::new(), icons)).unwrap()).expect("a bad icon never fails the overlay");
        assert!(overlay.icons[0].decoded.is_none() && overlay.icons[1].decoded.is_some());
        let (mapping, project) = view();
        let drawn = shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK);
        let insides: Vec<&Inside> = drawn.iter().filter_map(|s| if let Shape::Device { inside, .. } = s { Some(inside) } else { None }).collect();
        assert!(matches!(insides[0], Inside::Standard { .. }), "{:?}", insides[0]);
        assert!(matches!(insides[1], Inside::Icon { index: 1, .. }), "{:?}", insides[1]);
    }

    #[test]
    fn devices_are_sized_per_kind_like_the_web_canvas() {
        let mut devices = vec![device(0, "camera", 30.0, None), device(1, "equipment", 90.0, None), device(2, "asset", 0.0, None)];
        devices[2]["symbolScale"] = serde_json::json!(2);
        let mut value = payload(&[], devices, Vec::new(), Vec::new());
        value["display"]["iconScale"] = serde_json::json!(1.5);
        let overlay = parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        let (mapping, project) = view();
        let drawn = shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK);
        let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
        // 0.1 px per mm × icon scale 1.5 (× symbol scale 2 for the asset).
        let expect = [(DeviceKind::Camera, 39.0, 10.5, 60.0), (DeviceKind::Equipment, 49.5, 9.0, 58.5), (DeviceKind::Asset, 102.0, 16.5, 129.0)];
        for (shape, (kind, radius, ring, icon)) in devices_drawn(&drawn).into_iter().zip(expect) {
            let Shape::Device { at, kind: drawn_kind, radius: r, ring: w, inside, label, badge, .. } = shape else { unreachable!() };
            assert_eq!(*drawn_kind, kind);
            assert!(close(*r, radius) && close(*w, ring), "{kind:?}: {r} {w}");
            let Inside::Standard { size, at: icon_at, .. } = inside else { panic!("standard symbol") };
            assert!(close(*size, icon), "{kind:?}: icon {size}");
            let web = kind.web();
            let k = radius / web.radius as f32;
            match kind {
                // The equipment icon sits 52 mm "up" in its turned frame: turned
                // 90° clockwise, up is screen right.
                DeviceKind::Equipment => {
                    assert!(close(icon_at.x - at.x, 52.0 * k) && close(icon_at.y, at.y), "{icon_at:?} {at:?}");
                    let badge = badge.as_ref().expect("the badge");
                    assert!(close(badge.size, 115.0 * k) && badge.text == "SW");
                }
                _ => assert!(close(icon_at.x, at.x) && close(icon_at.y, at.y)),
            }
            // Labels (at least 7 px) stay upright below or beside the symbol.
            if let Some(label) = label {
                assert!(close(label.size, web.label_size as f32 * k));
            }
        }
        // The camera is not turned: its label is straight below it.
        let Shape::Device { at, label: Some(label), .. } = devices_drawn(&drawn)[0] else { panic!("camera label") };
        assert!(close(label.at.x, at.x) && close(label.at.y - at.y, (370.0 + 95.0) * 0.15));
    }

    #[test]
    fn tiny_symbols_become_dots_and_small_text_is_left_out() {
        let overlay = parse(&serde_json::to_vec(&payload(&[], vec![device(0, "equipment", 0.0, None)], Vec::new(), Vec::new())).unwrap()).unwrap();
        let mapping = Mapping { cad_origin: [0.0, 0.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        let at_scale = |px_per_mm: f32| {
            let project = move |cad: [f64; 2]| Some(Point::new(cad[0] as f32 * px_per_mm, -cad[1] as f32 * px_per_mm));
            shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK)
        };
        // 330 mm × 0.005 = 1.65 px: a dot.
        assert!(matches!(at_scale(0.005)[..], [Shape::Dot { .. }]));
        // 0.02 px/mm: a 6.6 px circle, label 3.6 px and badge 2.3 px left out.
        let Shape::Device { label, badge, .. } = &at_scale(0.02)[0] else { panic!("a symbol") };
        assert!(label.is_none() && badge.is_none());
        // 0.1 px/mm: an 18 px label and an 11.5 px badge.
        let Shape::Device { label, badge, .. } = &at_scale(0.1)[0] else { panic!("a symbol") };
        assert!(label.is_some() && badge.is_some());
        // Off screen: nothing.
        let project = |cad: [f64; 2]| Some(Point::new(cad[0] as f32 + 50000.0, -cad[1] as f32));
        assert!(shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK).is_empty());
    }

    #[test]
    fn coverage_is_filled_at_the_opacity_in_the_camera_colour() {
        let coverage = vec![serde_json::json!({ "elementId": "d0", "polygon": [[0, 0], [3000, 0], [0, 3000]], "color": "#ff8800" })];
        let overlay = parse(&serde_json::to_vec(&payload(&[], Vec::new(), coverage, Vec::new())).unwrap()).unwrap();
        let (mapping, project) = view();
        let drawn = shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK);
        let [Shape::Area { points, color, stroke }] = &drawn[..] else { panic!("{drawn:?}") };
        assert_eq!(points.len(), 3);
        assert_eq!(*color, Color { a: 0.16, ..Color::from_rgb8(0xff, 0x88, 0x00) });
        assert!((stroke - 3.0).abs() < 1e-4, "30 mm at 0.1 px/mm");
    }

    #[test]
    fn icon_images_are_made_once_per_size_bucket() {
        let overlay = parse(&serde_json::to_vec(&payload(&[], Vec::new(), Vec::new(), vec![icon("a1", "image/svg+xml", CIRCLE_SVG.as_bytes())])).unwrap()).unwrap();
        let icon = &overlay.icons[0];
        let first = icon.image(20.0).unwrap();
        assert_eq!(icon.image(30.0).unwrap().id(), first.id(), "20 and 30 px share the 32 px bucket");
        assert_ne!(icon.image(40.0).unwrap().id(), first.id());
        let _ = icon.image(5000.0);
        assert_eq!(icon.images_made(), 3, "32, 64 and the 256 px cap");
    }

    #[test]
    fn devices_keep_their_kind_and_turn_with_the_plan() {
        let devices = vec![device(0, "camera", 0.0, None), device(1, "equipment", 0.0, None), device(2, "asset", 0.0, None), device(3, "camera", 90.0, None)];
        let overlay = parse(&serde_json::to_vec(&payload(&[], devices, Vec::new(), Vec::new())).unwrap()).unwrap();
        assert_eq!(overlay.devices[3].rotation_deg, 90.0);
        assert_eq!(overlay.devices[0].symbol_key, "generic");
        let (mapping, project) = view();
        let facings = |mapping: &Mapping| -> Vec<(DeviceKind, Vector)> {
            shapes(&overlay, mapping, false, &project, SCREEN, Color::BLACK)
                .into_iter()
                .filter_map(|s| match s {
                    Shape::Device { kind, inside: Inside::Standard { facing, .. }, .. } => Some((kind, facing)),
                    _ => None,
                })
                .collect()
        };
        let drawn = facings(&mapping);
        let kinds: Vec<DeviceKind> = drawn.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [DeviceKind::Camera, DeviceKind::Equipment, DeviceKind::Asset, DeviceKind::Camera], "distinct symbols");
        let close = |v: Vector, x: f32, y: f32| (v.x - x).abs() < 1e-4 && (v.y - y).abs() < 1e-4;
        // World +x and world +y (down on the web canvas) are screen right and down.
        assert!(close(drawn[0].1, 1.0, 0.0), "{:?}", drawn[0].1);
        assert!(close(drawn[3].1, 0.0, 1.0), "{:?}", drawn[3].1);
        // A quarter-turn alignment turns the view direction with the plan.
        let turned = Mapping { quarter_turns: 1, ..mapping };
        let drawn = facings(&turned);
        assert!(close(drawn[0].1, 0.0, 1.0) || close(drawn[0].1, 0.0, -1.0), "{:?}", drawn[0].1);
    }

    /// As on the web canvas, equipment's badge and label and an asset's
    /// label turn with the symbol; a camera's label stays upright.
    #[test]
    fn badges_and_labels_turn_with_their_symbol() {
        let devices = vec![device(0, "equipment", 90.0, None), device(1, "asset", 45.0, None), device(2, "camera", 90.0, None)];
        let overlay = parse(&serde_json::to_vec(&payload(&[], devices, Vec::new(), Vec::new())).unwrap()).unwrap();
        let (mapping, project) = view();
        let drawn = shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK);
        let captions: Vec<(Option<Caption>, Caption, Point)> = drawn
            .into_iter()
            .filter_map(|s| match s {
                Shape::Device { badge, label, at, .. } => Some((badge, label.expect("a label"), at)),
                _ => None,
            })
            .collect();
        let quarter = std::f32::consts::FRAC_PI_2;
        let near = |a: f32, b: f32| (a - b).abs() < 1e-3;
        // The switch, turned 90° clockwise: "SW" and its label read downwards,
        // left of the symbol's centre (below it before the turn).
        let (badge, label, at) = &captions[0];
        let badge = badge.as_ref().expect("a badge");
        assert_eq!(badge.text, "SW");
        assert!(near(badge.rotation, quarter) && near(label.rotation, quarter), "{} {}", badge.rotation, label.rotation);
        assert!(badge.at.x < at.x && near(badge.at.y, at.y), "{:?} {:?}", badge.at, at);
        assert!(label.at.x < badge.at.x && near(label.at.y, at.y));
        assert!(near(captions[1].1.rotation, quarter / 2.0), "the asset's label turns with it");
        let (_, label, at) = &captions[2];
        assert!(near(label.rotation, 0.0) && label.at.y > at.y && near(label.at.x, at.x), "the camera's label stays upright below it");
        // A quarter-turn alignment turns every caption with the plan.
        let turned = shapes(&overlay, &Mapping { quarter_turns: 1, ..mapping }, false, &project, SCREEN, Color::BLACK);
        let Some(Shape::Device { label: Some(label), .. }) = turned.iter().find(|s| matches!(s, Shape::Device { kind: DeviceKind::Camera, .. })) else { panic!() };
        assert!(near(label.rotation.abs(), quarter), "{}", label.rotation);
        // Turned text is drawn (as outlines) without trouble.
        use iced::advanced::renderer::Headless;
        let renderer = iced::futures::executor::block_on(<iced::Renderer as Headless>::new(Default::default(), Some("tiny-skia"))).expect("a software renderer");
        let mut frame = canvas::Frame::new(&renderer, iced::Size::new(400.0, 400.0));
        draw_shapes(&mut frame, &shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK), &overlay.icons);
    }

    #[test]
    fn overlay_shapes_go_through_the_inverse_mapping() {
        let overlay = parse(&overlay_bytes(&[([0.0, 0.0], [3000.0, 0.0])])).unwrap();
        // Metres, one quarter turn, anchored at (1000, 1000) mm.
        let mapping = Mapping { cad_origin: [10.0, 20.0], anchor_mm: [1000.0, 1000.0], scale_mm_per_cad_unit: 1000.0, quarter_turns: 1 };
        let seen = std::cell::RefCell::new(Vec::new());
        let project = |cad: [f64; 2]| {
            seen.borrow_mut().push(cad);
            Some(Point::new(cad[0] as f32, cad[1] as f32))
        };
        let drawn = shapes(&overlay, &mapping, false, &project, SCREEN, Color::BLACK);
        assert_eq!(drawn.len(), 1);
        for (cad, world) in seen.borrow().iter().zip([[0.0, 0.0], [3000.0, 0.0]]) {
            let back = mapping.cad_to_world(*cad);
            assert!((back[0] - world[0]).abs() < 1e-9 && (back[1] - world[1]).abs() < 1e-9);
        }
    }

    /// Cost of the overlay with 2,000 devices over 50 icons and 500 coverage
    /// polygons, fitted (symbols a few pixels across) and zoomed in, in a
    /// release build: parsing, building the geometry when the view changes
    /// (first with the icons' images made), and redrawing an unchanged view
    /// from the cache. Run with
    /// `cargo test --release --features secureplan,secureplan-test --lib secureplan::overlay::tests::redraw_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn redraw_cost() {
        use iced::advanced::renderer::Headless;
        let renderer = iced::futures::executor::block_on(<iced::Renderer as Headless>::new(Default::default(), Some("wgpu")))
            .or_else(|| iced::futures::executor::block_on(<iced::Renderer as Headless>::new(Default::default(), None)))
            .expect("a headless renderer");
        let icons: Vec<_> = (0..50)
            .map(|i| {
                let svg = CIRCLE_SVG.replace("r=\"8\"", &format!("r=\"{}\"", 3 + i % 8)).replace("<svg ", &format!("<svg data-n=\"{i}\" "));
                icon(&format!("i{i}"), "image/svg+xml", svg.as_bytes())
            })
            .collect();
        let devices: Vec<_> = (0..2000)
            .map(|i| {
                let mut d = device(i, ["camera", "equipment", "asset"][i % 3], (i * 7 % 360) as f64, Some(&format!("i{}", i % 50)));
                d["position"] = serde_json::json!([(i % 50) as f64 * 2000.0, (i / 50) as f64 * 1500.0]);
                d
            })
            .collect();
        let coverage: Vec<_> = (0..500)
            .map(|i| {
                let (cx, cy) = ((i % 25) as f64 * 4000.0, (i / 25) as f64 * 3000.0);
                let polygon: Vec<_> = (0..48).map(|k| { let a = k as f64 / 48.0 * std::f64::consts::TAU; [cx + 3000.0 * a.cos(), cy + 3000.0 * a.sin()] }).collect();
                serde_json::json!({ "elementId": format!("d{i}"), "polygon": polygon, "color": "#2f80ed" })
            })
            .collect();
        let mut plain = payload(&[], devices.clone(), coverage.clone(), Vec::new());
        for device in plain["devices"].as_array_mut().unwrap() {
            device["iconId"] = serde_json::Value::Null;
        }
        let bytes = serde_json::to_vec(&plain).unwrap();
        let started = std::time::Instant::now();
        let _ = parse(&bytes).unwrap();
        eprintln!("redraw_cost parse without icons ({} KiB): {:?}", bytes.len() / 1024, started.elapsed());
        let bytes = serde_json::to_vec(&payload(&[], devices, coverage, icons)).unwrap();
        let started = std::time::Instant::now();
        let overlay = parse(&bytes).unwrap();
        eprintln!("redraw_cost parse with {} icons decoded: {:?}", overlay.icons.len(), started.elapsed());
        let mapping = Mapping { cad_origin: [0.0, 0.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        let size = iced::Size::new(1600.0, 1000.0);
        let viewport = Rectangle::new(Point::ORIGIN, size);
        for (name, px_per_mm) in [("fit", 0.016f32), ("zoomed", 0.1f32)] {
            let project = |cad: [f64; 2]| Some(Point::new(cad[0] as f32 * px_per_mm, 1000.0 + cad[1] as f32 * px_per_mm));
            let build = || {
                let mut frame = canvas::Frame::new(&renderer, size);
                let drawn = shapes(&overlay, &mapping, false, &project, viewport, Color::WHITE);
                draw_shapes(&mut frame, &drawn, &overlay.icons);
                std::hint::black_box(frame.into_geometry());
            };
            let started = std::time::Instant::now();
            build();
            let first = started.elapsed();
            let runs = 10;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                build();
            }
            let rebuild = started.elapsed() / runs;
            let started = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(shapes(&overlay, &mapping, false, &project, viewport, Color::WHITE));
            }
            let geometry = started.elapsed() / runs;
            // Split: coverage only, devices only.
            let drawn = shapes(&overlay, &mapping, false, &project, viewport, Color::WHITE);
            let (areas, others): (Vec<Shape>, Vec<Shape>) = drawn.into_iter().partition(|s| matches!(s, Shape::Area { .. }));
            for (what, part) in [("coverage", &areas), ("devices", &others)] {
                let started = std::time::Instant::now();
                for _ in 0..runs {
                    let mut frame = canvas::Frame::new(&renderer, size);
                    draw_shapes(&mut frame, part, &overlay.icons);
                    std::hint::black_box(frame.into_geometry());
                }
                eprintln!("redraw_cost {name} {what}: {} shapes {:?}", part.len(), started.elapsed() / runs);
            }
            eprintln!("redraw_cost {name} shapes()={geometry:?}");
            // A redraw with the view unchanged draws the cached geometry.
            let cache = canvas::Cache::<iced::Renderer>::new();
            let _ = cache.draw(&renderer, size, |frame| {
                let drawn = shapes(&overlay, &mapping, false, &project, viewport, Color::WHITE);
                draw_shapes(frame, &drawn, &overlay.icons);
            });
            let started = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(cache.draw(&renderer, size, |_| unreachable!("cached")));
            }
            let cached = started.elapsed() / runs;
            eprintln!("redraw_cost {name} renderer={} first (icons rendered)={first:?} view change={rebuild:?} unchanged view={cached:?}", renderer.name());
        }
    }
}
