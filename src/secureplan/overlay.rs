//! Read-only SecurePlan design overlay (OVL-01, OVL-02).
//!
//! The web sends its current draft in world millimetres
//! (`application/vnd.secureplan.overlay+json`, `overlay.schema.json`). The
//! desktop keeps it outside the drawing, as plain data, and draws it over the
//! model-space viewport through the inverse of the alignment mapping, labelled
//! "SecurePlan design (read-only)". It is never a CAD entity, so it can never
//! be selected, edited, converted or written to the drawing. An
//! `overlayUpdate` replaces it; nothing is ever merged.

use iced::widget::canvas;
use iced::{mouse, Color, Element, Length, Point, Rectangle, Theme};

use super::align::world_to_cad;
use super::publish::Mapping;
use crate::app::{Message, OpenCADStudio};

/// A wall material (`wallMaterial`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Material {
    Solid,
    Glass,
    Fence,
    Opening,
}

/// What a device is, which decides its symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Camera,
    Equipment,
    Asset,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub kind: DeviceKind,
    /// The catalog's built-in symbol key (kept for export; the overlay draws
    /// one symbol per kind).
    pub symbol_key: String,
    pub name: String,
    pub color: Color,
    pub position: [f64; 2],
    /// World rotation in degrees (world y points down, as on the web canvas).
    pub rotation_deg: f64,
}

/// The overlay payload, in world millimetres.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overlay {
    pub walls: Vec<(Material, [f64; 2], [f64; 2])>,
    pub doors: Vec<([f64; 2], [f64; 2])>,
    pub routes: Vec<Vec<[f64; 2]>>,
    pub devices: Vec<Device>,
    pub coverage: Vec<Vec<[f64; 2]>>,
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

/// Parse and validate an overlay payload against the pinned schema.
pub fn parse(bytes: &[u8]) -> Result<Overlay, String> {
    if bytes.len() as u64 > super::protocol::MAX_JSON_PAYLOAD_BYTES {
        return Err("overlay over 8 MiB".into());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| "overlay is not JSON".to_string())?;
    super::protocol::validate_payload("overlay.schema.json", &value).map_err(|e| e.to_string())?;
    let list = |key: &str| value[key].as_array().cloned().unwrap_or_default();
    let material = |name: &str| match name {
        "glass" => Material::Glass,
        "fence" => Material::Fence,
        "opening" => Material::Opening,
        _ => Material::Solid,
    };
    Ok(Overlay {
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
                kind: match d["kind"].as_str().unwrap_or_default() {
                    "camera" => DeviceKind::Camera,
                    "equipment" => DeviceKind::Equipment,
                    _ => DeviceKind::Asset,
                },
                symbol_key: d["symbolKey"].as_str().unwrap_or_default().to_string(),
                rotation_deg: d["rotationDeg"].as_f64().unwrap_or_default(),
                name: d["name"].as_str().unwrap_or_default().to_string(),
                color: color(d["color"].as_str().unwrap_or("#808080")),
                position: point(&d["position"]),
            })
            .collect(),
        coverage: list("coverage")
            .iter()
            .map(|c| c["polygon"].as_array().into_iter().flatten().map(point).collect())
            .collect(),
    })
}

/// A stroke on screen.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Line { points: Vec<Point>, color: Color, width: f32, dashed: bool, closed: bool },
    /// A device: a circle for a camera (with a view-direction wedge), a
    /// square for equipment, a diamond for an asset. `facing` is the screen
    /// direction of the device's rotation.
    Marker { at: Point, kind: DeviceKind, facing: iced::Vector, color: Color, label: String },
}

const WALL: Color = Color { r: 0.10, g: 0.55, b: 0.95, a: 0.95 };
const DOOR: Color = Color { r: 0.95, g: 0.60, b: 0.10, a: 0.95 };
const ROUTE: Color = Color { r: 0.20, g: 0.80, b: 0.35, a: 0.95 };
const COVERAGE: Color = Color { r: 0.95, g: 0.85, b: 0.20, a: 0.6 };
const BEFORE: Color = Color { r: 0.6, g: 0.6, b: 0.6, a: 0.6 };

/// The overlay's shapes on screen for `mapping`, with `project` taking CAD
/// coordinates to the viewport. `before` draws a previous alignment in grey.
fn shapes(overlay: &Overlay, mapping: &Mapping, before: bool, project: &dyn Fn([f64; 2]) -> Option<Point>) -> Vec<Shape> {
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
    if !before {
        for polygon in &overlay.coverage {
            line(polygon, COVERAGE, 1.0, true, true);
        }
    }
    for device in &overlay.devices {
        // The rotation is measured in the world; its screen direction comes
        // from mapping a point one metre ahead, whatever the alignment turn.
        let angle = device.rotation_deg.to_radians();
        let ahead = [device.position[0] + 1000.0 * angle.cos(), device.position[1] + 1000.0 * angle.sin()];
        if let (Some(at), Some(tip)) = (to_screen(device.position), to_screen(ahead)) {
            let (dx, dy) = (tip.x - at.x, tip.y - at.y);
            let length = dx.hypot(dy);
            let facing = if length > 0.0 { iced::Vector::new(dx / length, dy / length) } else { iced::Vector::new(1.0, 0.0) };
            let label = if before { String::new() } else { device.name.chars().take(40).collect() };
            out.push(Shape::Marker { at, kind: device.kind, facing, color: pick(device.color), label });
        }
    }
    out
}

struct OverlayCanvas {
    shapes: Vec<Shape>,
}

impl canvas::Program<Message> for OverlayCanvas {
    type State = ();

    fn draw(&self, _state: &(), renderer: &iced::Renderer, _theme: &Theme, bounds: Rectangle, _cursor: mouse::Cursor) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        for shape in &self.shapes {
            match shape {
                Shape::Line { points, color, width, dashed, closed } => {
                    let path = canvas::Path::new(|builder| {
                        builder.move_to(points[0]);
                        for p in &points[1..] {
                            builder.line_to(*p);
                        }
                        if *closed {
                            builder.close();
                        }
                    });
                    let mut stroke = canvas::Stroke::default().with_color(*color).with_width(*width);
                    if *dashed {
                        stroke.line_dash = canvas::LineDash { segments: &[6.0, 4.0], offset: 0 };
                    }
                    frame.stroke(&path, stroke);
                }
                Shape::Marker { at, kind, facing, color, label } => {
                    let stroke = canvas::Stroke::default().with_color(*color).with_width(2.0);
                    let (fx, fy) = (facing.x, facing.y);
                    // Rotate the symbol's local (forward, side) axes to `facing`.
                    let local = |forward: f32, side: f32| Point::new(at.x + forward * fx - side * fy, at.y + forward * fy + side * fx);
                    let polygon = |corners: &[(f32, f32)]| {
                        canvas::Path::new(|builder| {
                            builder.move_to(local(corners[0].0, corners[0].1));
                            for (forward, side) in &corners[1..] {
                                builder.line_to(local(*forward, *side));
                            }
                            builder.close();
                        })
                    };
                    match kind {
                        DeviceKind::Camera => {
                            frame.stroke(&canvas::Path::circle(*at, 5.0), stroke);
                            // The field-of-view wedge points where the camera looks.
                            frame.stroke(&polygon(&[(0.0, 0.0), (22.0, -11.0), (22.0, 11.0)]), stroke);
                        }
                        DeviceKind::Equipment => frame.stroke(&polygon(&[(-5.0, -5.0), (5.0, -5.0), (5.0, 5.0), (-5.0, 5.0)]), stroke),
                        DeviceKind::Asset => frame.stroke(&polygon(&[(-6.0, 0.0), (0.0, -6.0), (6.0, 0.0), (0.0, 6.0)]), stroke),
                    }
                    if !label.is_empty() {
                        frame.fill_text(canvas::Text {
                            content: label.clone(),
                            position: Point::new(at.x + 8.0, at.y - 6.0),
                            color: *color,
                            size: 12.0.into(),
                            ..Default::default()
                        });
                    }
                }
            }
        }
        vec![frame.into_geometry()]
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
        let mut shapes_on_screen = Vec::new();
        if let Some(mapping) = current {
            let (vw, vh) = tab.scene.selection.borrow().vp_size;
            let bounds = tab.scene.active_model_tile_bounds(vw, vh);
            let camera = tab.scene.camera.borrow();
            let project = |[x, y]: [f64; 2]| {
                camera
                    .project(glam::DVec3::new(x, y, 0.0), bounds)
                    .map(|p| Point::new(bounds.x + p.x, bounds.y + p.y))
                    .filter(|p| p.x.is_finite() && p.y.is_finite())
            };
            if let Some(before) = before {
                shapes_on_screen.extend(shapes(overlay, &before, true, &project));
            }
            shapes_on_screen.extend(shapes(overlay, &mapping, false, &project));
        }
        let note = if current.is_some() { LABEL.to_string() } else { format!("{LABEL}: align the drawing to show it") };
        let label = container(text(note).size(12)).padding([2, 6]).style(|theme: &Theme| container::Style {
            background: Some(iced::Background::Color(theme.palette().background.weakest.color)),
            ..Default::default()
        });
        let layer = iced::widget::stack![
            iced::widget::canvas(OverlayCanvas { shapes: shapes_on_screen }).width(Length::Fill).height(Length::Fill),
            column![label].padding([40, 8]).width(Length::Fill).align_x(iced::alignment::Horizontal::Right),
        ];
        Some(layer.width(Length::Fill).height(Length::Fill).into())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
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

    pub(crate) fn overlay_bytes(walls: &[([f64; 2], [f64; 2])]) -> Vec<u8> {
        let walls: Vec<_> = walls
            .iter()
            .enumerate()
            .map(|(i, (a, b))| serde_json::json!({ "id": format!("wall-{i}"), "material": "solid", "start": a, "end": b }))
            .collect();
        serde_json::to_vec(&serde_json::json!({ "schemaVersion": 1, "walls": walls, "doors": [], "routes": [], "devices": [], "coverage": [] })).unwrap()
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
        assert!(parse(br#"{"schemaVersion":1,"walls":[],"doors":[],"routes":[],"devices":[],"coverage":[],"extra":1}"#).is_err());
    }

    fn device_bytes(devices: &[(&str, f64)]) -> Vec<u8> {
        let devices: Vec<_> = devices
            .iter()
            .enumerate()
            .map(|(i, (kind, rotation))| {
                serde_json::json!({ "id": format!("d{i}"), "kind": kind, "symbolKey": "generic", "name": format!("Device {i}"), "color": "#336699", "position": [1000.0, 1000.0], "rotationDeg": rotation })
            })
            .collect();
        serde_json::to_vec(&serde_json::json!({ "schemaVersion": 1, "walls": [], "doors": [], "routes": [], "devices": devices, "coverage": [] })).unwrap()
    }

    fn markers(drawn: &[Shape]) -> Vec<(DeviceKind, iced::Vector)> {
        drawn.iter().filter_map(|s| if let Shape::Marker { kind, facing, .. } = s { Some((*kind, *facing)) } else { None }).collect()
    }

    #[test]
    fn devices_keep_their_kind_and_a_camera_faces_its_rotation() {
        let overlay = parse(&device_bytes(&[("camera", 0.0), ("equipment", 0.0), ("asset", 0.0), ("camera", 90.0)])).unwrap();
        assert_eq!(overlay.devices[3].rotation_deg, 90.0);
        assert_eq!(overlay.devices[0].symbol_key, "generic");
        // Millimetres, no turn: screen = CAD with y flipped (y up in CAD).
        let mapping = Mapping { cad_origin: [0.0, 0.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        let project = |cad: [f64; 2]| Some(Point::new(cad[0] as f32, -cad[1] as f32));
        let drawn = markers(&shapes(&overlay, &mapping, false, &project));
        let kinds: Vec<DeviceKind> = drawn.iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [DeviceKind::Camera, DeviceKind::Equipment, DeviceKind::Asset, DeviceKind::Camera], "distinct symbols");
        let close = |v: iced::Vector, x: f32, y: f32| (v.x - x).abs() < 1e-4 && (v.y - y).abs() < 1e-4;
        // World +x and world +y (down on the web canvas) are screen right and down.
        assert!(close(drawn[0].1, 1.0, 0.0), "{:?}", drawn[0].1);
        assert!(close(drawn[3].1, 0.0, 1.0), "{:?}", drawn[3].1);
        // A quarter-turn alignment turns the view direction with the plan.
        let turned = Mapping { quarter_turns: 1, ..mapping };
        let drawn = markers(&shapes(&overlay, &turned, false, &project));
        assert!(close(drawn[0].1, 0.0, 1.0) || close(drawn[0].1, 0.0, -1.0), "{:?}", drawn[0].1);
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
        let drawn = shapes(&overlay, &mapping, false, &project);
        assert_eq!(drawn.len(), 1);
        for (cad, world) in seen.borrow().iter().zip([[0.0, 0.0], [3000.0, 0.0]]) {
            let back = mapping.cad_to_world(*cad);
            assert!((back[0] - world[0]).abs() < 1e-9 && (back[1] - world[1]).abs() < 1e-9);
        }
    }
}
