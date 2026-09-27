//! CAD export composition and output (EXP-02, EXP-03).
//!
//! `exportRequest` carries the saved snapshot's applied drawing and an export
//! payload (`export-payload.schema.json`, world millimetres). The desktop reads
//! the drawing in memory, never the open document, and adds the design on new
//! `SECUREPLAN-*` layers through the inverse of the survey's stored mapping:
//!
//! | Content | CAD representation | Layer |
//! | --- | --- | --- |
//! | Cameras, equipment, assets | a static symbol block per kind, with a text label | `…-CAMERA`, `…-EQUIPMENT`, `…-ASSET` |
//! | Walls | lines, one layer per material and status | `…-WALL-<MATERIAL>-<STATUS>` |
//! | Doors and openings | lines, one layer per status | `…-DOOR-<STATUS>` |
//! | Cable routes | polylines, with a "label (cable type)" text | `…-ROUTE` |
//! | Targets | a circle with a direction line; the web's engineering label (its achieved PPM) and a "Required N PPM" text | `…-TARGET` |
//! | General drawings | polylines, rectangles, ellipses, arrows, with their label | `…-DRAWING` |
//! | Engineering labels of walls, doors and other elements | text | `…-LABEL` |
//! | Notes (when chosen) | multiline text | `…-NOTE` |
//! | Coverage (when chosen) | closed outlines | `…-COVERAGE` |
//!
//! Each element's label is drawn once. A device's, route's or drawing's label
//! belongs to that row's representation (EXP-02: "static blocks with visible
//! labels", "polylines with type/label", "reference geometry and
//! annotations"), drawn from the element's own fields on its own layer; the
//! `labels[]` entry the web also sends for that element is not drawn again.
//! A target's label is its `labels[]` entry, which carries the PPM the design
//! achieves there, drawn on the target's layer; its required PPM, from the
//! target itself, is a separate text below it. The other `labels[]` entries
//! (doors and the rest) are the engineering labels.
//!
//! Layer and block names are collision-free: when any of them already exists
//! (a re-imported earlier export), every new name takes the first free
//! `SECUREPLAN-<n>-` prefix instead, so nothing merges into existing layers.
//! Existing entities, layers and blocks are never changed, deleted or renamed.
//!
//! Writing checks the writer's result, the known loss (objects the chosen
//! format and version cannot hold, and damaged items the reader dropped),
//! which the user must acknowledge, and that the written header names the
//! chosen format and version.

use std::sync::Arc;

use acadrust::entities::{Block, BlockEnd, Circle, Ellipse, Insert, LwPolyline, MText, Text};
use acadrust::tables::{BlockRecord, Layer};
use acadrust::types::{Color, Vector2, Vector3};
use acadrust::{CadDocument, DxfVersion, EntityType};
use serde::Deserialize;

use super::align::world_to_cad;
use super::publish::Mapping;
use super::session::Format;
use super::symbols::{self, DeviceKind};

/// The export payload's media type (BRG-06).
pub const MEDIA_TYPE: &str = "application/vnd.secureplan.export+json";
/// Text height on the plan, in world mm.
pub const TEXT_HEIGHT_MM: f64 = 200.0;
/// A target marker's radius, in world mm.
pub const TARGET_RADIUS_MM: f64 = 200.0;
/// The arrowhead of a drawn arrow, as the web draws it (world mm).
const ARROW_LENGTH_MM: f64 = 320.0;
const ARROW_WIDTH_MM: f64 = 260.0;

type Point = [f64; 2];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotId {
    pub version: u64,
    pub document_hash: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Options {
    pub include_notes: bool,
    pub include_coverage: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Wall {
    pub id: String,
    pub material: String,
    pub status: String,
    pub start: Point,
    pub end: Point,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Door {
    pub id: String,
    pub status: String,
    pub start: Point,
    pub end: Point,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub id: String,
    pub cable_type: String,
    pub label: String,
    pub status: String,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub icon_key: String,
    pub custom_icon: bool,
    pub color: String,
    pub label: String,
    pub status: String,
    pub position: Point,
    pub rotation_deg: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    pub label: String,
    pub position: Point,
    pub rotation_deg: f64,
    pub required_ppm: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drawing {
    pub id: String,
    pub drawing_type: String,
    pub label: String,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Label {
    pub element_id: String,
    pub text: String,
    pub position: Point,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Note {
    pub id: String,
    pub text: String,
    pub position: Point,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub element_id: String,
    pub polygon: Vec<Point>,
}

/// The export payload (EXP-01), validated against the pinned schema.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub schema_version: u32,
    pub snapshot: SnapshotId,
    pub options: Options,
    pub walls: Vec<Wall>,
    pub doors: Vec<Door>,
    pub routes: Vec<Route>,
    pub devices: Vec<Device>,
    pub targets: Vec<Target>,
    pub drawings: Vec<Drawing>,
    pub labels: Vec<Label>,
    pub notes: Vec<Note>,
    pub coverage: Vec<Coverage>,
}

/// Parse and validate an export payload.
pub fn parse_payload(bytes: &[u8]) -> Result<Payload, String> {
    if bytes.len() as u64 > super::protocol::MAX_JSON_PAYLOAD_BYTES {
        return Err("The export payload is over 8 MiB.".into());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| "The export payload is not JSON.".to_string())?;
    super::protocol::validate_payload("export-payload.schema.json", &value).map_err(|e| format!("The export payload is invalid: {}.", e.0))?;
    serde_json::from_value(value).map_err(|_| "The export payload is invalid.".to_string())
}

/// What the export adds, for the summary and the tests.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Composition {
    /// Each new layer and how many entities it holds, in creation order.
    pub layers: Vec<(String, usize)>,
    /// The new symbol blocks.
    pub blocks: Vec<String>,
    /// `SECUREPLAN-`, or `SECUREPLAN-<n>-` when the plain names were taken.
    pub prefix: String,
    /// Layers named `SECUREPLAN…` the drawing already had.
    pub existing_secureplan_layers: usize,
    /// Devices whose catalog icon is a custom raster: drawn with the standard symbol.
    pub custom_icons: usize,
    pub walls: usize,
    pub doors: usize,
    pub routes: usize,
    pub devices: usize,
    pub targets: usize,
    pub drawings: usize,
    pub labels: usize,
    pub notes: Option<usize>,
    pub coverage: Option<usize>,
}

impl Composition {
    /// The summary lines of the export dialog.
    pub fn summary(&self) -> Vec<String> {
        let optional = |count: Option<usize>, what: &str| match count {
            Some(count) => format!("{count} {what}"),
            None => format!("{what}: not included"),
        };
        let mut lines = vec![format!(
            "Adds {} walls, {} doors and openings, {} cable routes, {} devices, {} targets, {} drawings, {} engineering labels, {} and {}.",
            self.walls,
            self.doors,
            self.routes,
            self.devices,
            self.targets,
            self.drawings,
            self.labels,
            optional(self.notes, "notes"),
            optional(self.coverage, "coverage outlines"),
        )];
        let names: Vec<&str> = self.layers.iter().map(|(name, _)| name.as_str()).collect();
        match names.len() {
            0 => lines.push("The design is empty: the drawing is exported as it is.".into()),
            n if n <= 8 => lines.push(format!("New layers: {}.", names.join(", "))),
            n => lines.push(format!("{n} new layers: {}, …", names[..8].join(", "))),
        }
        if self.devices > 0 {
            let shapes: Vec<&str> = DeviceKind::ALL.iter().map(|kind| kind.describe()).collect();
            lines.push(format!("Devices are standard symbols ({}), labelled with their SecurePlan label.", shapes.join(", ")));
        }
        if self.custom_icons > 0 {
            lines.push(format!(
                "{} device(s) use a custom icon in SecurePlan; the export draws the standard symbol of their kind instead.",
                self.custom_icons
            ));
        }
        lines.push("Existing drawing content is not changed. Security linework already in the drawing (for example from a re-imported export) stays and may overlap the new layers.".into());
        if self.existing_secureplan_layers > 0 {
            lines.push(format!(
                "The drawing already has {} SECUREPLAN layer(s); the new layers are named {}… so nothing merges into them.",
                self.existing_secureplan_layers, self.prefix
            ));
        }
        lines
    }
}

/// `In Place` → `IN-PLACE`: a status or material as a layer-name part.
fn tag(value: &str) -> String {
    value.trim().to_ascii_uppercase().replace(' ', "-")
}

/// ACI colours of the new layers (non-colour cues are the layer names).
fn layer_color(key: &str) -> i16 {
    match key.split('-').next().unwrap_or_default() {
        "WALL" if key.starts_with("WALL-GLASS") => 4,
        "WALL" if key.starts_with("WALL-FENCE") => 8,
        "WALL" if key.starts_with("WALL-OPENING") => 9,
        "WALL" => 5,
        "DOOR" => 30,
        "ROUTE" => 3,
        "TARGET" => 6,
        "NOTE" => 2,
        "COVERAGE" => 51,
        _ => 7,
    }
}

/// Entities to add, each on a layer given by its key (the name without the prefix).
struct Plan {
    items: Vec<(String, EntityType)>,
    kinds: Vec<DeviceKind>,
}

impl Plan {
    fn add(&mut self, key: &str, entity: EntityType) {
        self.items.push((key.to_string(), entity));
    }
}

fn polyline(points: &[Vector3], closed: bool) -> EntityType {
    let mut polyline = LwPolyline::from_points(points.iter().map(|p| Vector2::new(p.x, p.y)).collect());
    polyline.is_closed = closed;
    EntityType::LwPolyline(polyline)
}

fn text(value: &str, at: Vector3, height: f64) -> EntityType {
    EntityType::Text(Text::with_value(value, at).with_height(height))
}

/// Escape MTEXT control characters so a note reads as typed.
fn mtext_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\n' => out.push_str("\\P"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// `#rrggbb` → a true colour.
fn rgb(hex: &str) -> Color {
    let channel = |i: usize| u8::from_str_radix(hex.get(1 + 2 * i..3 + 2 * i).unwrap_or("80"), 16).unwrap_or(0x80);
    Color::Rgb { r: channel(0), g: channel(1), b: channel(2) }
}

/// Add the design in `payload` to `document`, mapped back to CAD
/// coordinates with the inverse of `mapping`.
pub fn compose(document: &mut CadDocument, payload: &Payload, mapping: &Mapping) -> Composition {
    let cad = |p: Point| {
        let [x, y] = world_to_cad(mapping, p);
        Vector3::new(x, y, 0.0)
    };
    // Drawing units per world mm (the mapping is a similarity).
    let unit = 1.0 / mapping.scale_mm_per_cad_unit;
    let height = TEXT_HEIGHT_MM * unit;
    // A world direction (degrees, world y down) as a CAD angle in radians.
    let angle = |at: Point, degrees: f64| {
        let r = degrees.to_radians();
        let (a, b) = (cad(at), cad([at[0] + r.cos() * 1000.0, at[1] + r.sin() * 1000.0]));
        (b.y - a.y).atan2(b.x - a.x).rem_euclid(std::f64::consts::TAU)
    };
    let mut plan = Plan { items: Vec::new(), kinds: Vec::new() };
    let mut composition = Composition::default();

    for wall in &payload.walls {
        let key = format!("WALL-{}-{}", tag(&wall.material), tag(&wall.status));
        plan.add(&key, polyline(&[cad(wall.start), cad(wall.end)], false));
    }
    for door in &payload.doors {
        plan.add(&format!("DOOR-{}", tag(&door.status)), polyline(&[cad(door.start), cad(door.end)], false));
    }
    for route in &payload.routes {
        let points: Vec<Vector3> = route.points.iter().map(|p| cad(*p)).collect();
        plan.add("ROUTE", polyline(&points, false));
        let caption = match (route.label.trim(), route.cable_type.trim()) {
            ("", "") => None,
            (label, "") => Some(label.to_string()),
            ("", cable) => Some(cable.to_string()),
            (label, cable) => Some(format!("{label} ({cable})")),
        };
        if let Some(caption) = caption {
            // Beside the middle of the route's longest segment.
            let longest = route
                .points
                .windows(2)
                .max_by(|a, b| {
                    let length = |s: &[Point]| (s[1][0] - s[0][0]).hypot(s[1][1] - s[0][1]);
                    length(a).total_cmp(&length(b))
                })
                .map(|s| [(s[0][0] + s[1][0]) / 2.0, (s[0][1] + s[1][1]) / 2.0])
                .unwrap_or(route.points[0]);
            plan.add("ROUTE", text(&caption, cad(longest), height));
        }
    }
    for device in &payload.devices {
        let kind = DeviceKind::parse(&device.kind).unwrap_or(DeviceKind::Asset);
        if !plan.kinds.contains(&kind) {
            plan.kinds.push(kind);
        }
        composition.custom_icons += usize::from(device.custom_icon);
        let at = cad(device.position);
        let mut insert = Insert::new(format!("SYMBOL-{}", kind.tag()), at)
            .with_uniform_scale(symbols::SYMBOL_RADIUS_MM * unit)
            .with_rotation(angle(device.position, device.rotation_deg));
        insert.common.color = rgb(&device.color);
        plan.add(kind.tag(), EntityType::Insert(insert));
        let caption = [device.label.trim(), device.name.trim()].into_iter().find(|s| !s.is_empty()).unwrap_or(match kind {
            DeviceKind::Camera => "Camera",
            DeviceKind::Equipment => "Equipment",
            DeviceKind::Asset => "Asset",
        });
        let beside = Vector3::new(at.x + 2.2 * symbols::SYMBOL_RADIUS_MM * unit, at.y - height / 2.0, 0.0);
        plan.add(kind.tag(), text(caption, beside, height));
    }
    for target in &payload.targets {
        let at = cad(target.position);
        let evaluated = payload.labels.iter().find(|label| label.element_id == target.id);
        let mut marker = Circle::new();
        marker.center = at;
        marker.radius = TARGET_RADIUS_MM * unit;
        plan.add("TARGET", EntityType::Circle(marker));
        let r = target.rotation_deg.to_radians();
        let tip = [target.position[0] + r.cos() * 2.0 * TARGET_RADIUS_MM, target.position[1] + r.sin() * 2.0 * TARGET_RADIUS_MM];
        plan.add("TARGET", polyline(&[at, cad(tip)], false));
        // The web's label, with the achieved PPM, where the web puts it;
        // without one, the target's own label beside it.
        let (caption, place) = match evaluated {
            Some(label) => (label.text.clone(), cad(label.position)),
            None => (
                if target.label.trim().is_empty() { "Target".to_string() } else { target.label.trim().to_string() },
                Vector3::new(at.x + 1.5 * TARGET_RADIUS_MM * unit, at.y - height / 2.0, 0.0),
            ),
        };
        plan.add("TARGET", text(&caption, place, height));
        let ppm = super::ui::format_number(target.required_ppm);
        plan.add("TARGET", text(&format!("Required {ppm} PPM"), Vector3::new(place.x, place.y - 1.5 * height, 0.0), height));
    }
    for drawing in &payload.drawings {
        let points = &drawing.points;
        let (first, last) = (points[0], points[points.len() - 1]);
        match drawing.drawing_type.as_str() {
            "rectangle" => {
                let corners = [first, [last[0], first[1]], last, [first[0], last[1]]].map(cad);
                plan.add("DRAWING", polyline(&corners, true));
            }
            "ellipse" => {
                let (rx, ry) = ((last[0] - first[0]).abs() / 2.0, (last[1] - first[1]).abs() / 2.0);
                let centre = [(first[0] + last[0]) / 2.0, (first[1] + last[1]) / 2.0];
                if rx < 1e-9 || ry < 1e-9 {
                    plan.add("DRAWING", polyline(&[cad(first), cad(last)], false));
                } else {
                    let major = if rx >= ry { [centre[0] + rx, centre[1]] } else { [centre[0], centre[1] + ry] };
                    let (c, m) = (cad(centre), cad(major));
                    let mut ellipse = Ellipse::new();
                    ellipse.center = c;
                    ellipse.major_axis = Vector3::new(m.x - c.x, m.y - c.y, 0.0);
                    ellipse.minor_axis_ratio = rx.min(ry) / rx.max(ry);
                    plan.add("DRAWING", EntityType::Ellipse(ellipse));
                }
            }
            "arrow" => {
                plan.add("DRAWING", polyline(&[cad(first), cad(last)], false));
                let length = (last[0] - first[0]).hypot(last[1] - first[1]);
                if length > 0.0 {
                    let (dx, dy) = ((last[0] - first[0]) / length, (last[1] - first[1]) / length);
                    let base = [last[0] - dx * ARROW_LENGTH_MM, last[1] - dy * ARROW_LENGTH_MM];
                    let half = ARROW_WIDTH_MM / 2.0;
                    let head = [last, [base[0] - dy * half, base[1] + dx * half], [base[0] + dy * half, base[1] - dx * half]].map(cad);
                    plan.add("DRAWING", polyline(&head, true));
                }
            }
            // Freehand and lines: through every point.
            _ => plan.add("DRAWING", polyline(&points.iter().map(|p| cad(*p)).collect::<Vec<_>>(), false)),
        }
        if !drawing.label.trim().is_empty() {
            plan.add("DRAWING", text(drawing.label.trim(), cad(first), height));
        }
    }
    // Labels of elements whose row already draws them are not drawn twice.
    let labelled: std::collections::HashSet<&str> = payload
        .devices
        .iter()
        .map(|d| d.id.as_str())
        .chain(payload.routes.iter().map(|r| r.id.as_str()))
        .chain(payload.targets.iter().map(|t| t.id.as_str()))
        .chain(payload.drawings.iter().map(|d| d.id.as_str()))
        .collect();
    for label in payload.labels.iter().filter(|label| !labelled.contains(label.element_id.as_str())) {
        plan.add("LABEL", text(&label.text, cad(label.position), height));
        composition.labels += 1;
    }
    if payload.options.include_notes {
        for note in &payload.notes {
            let value = if note.text.trim().is_empty() { "(empty note)".to_string() } else { mtext_value(&note.text) };
            let mut mtext = MText::with_value(value, cad(note.position));
            mtext.height = height;
            plan.add("NOTE", EntityType::MText(mtext));
        }
    }
    if payload.options.include_coverage {
        for coverage in &payload.coverage {
            plan.add("COVERAGE", polyline(&coverage.polygon.iter().map(|p| cad(*p)).collect::<Vec<_>>(), true));
        }
    }
    composition.walls = payload.walls.len();
    composition.doors = payload.doors.len();
    composition.routes = payload.routes.len();
    composition.devices = payload.devices.len();
    composition.targets = payload.targets.len();
    composition.drawings = payload.drawings.len();
    composition.notes = payload.options.include_notes.then_some(payload.notes.len());
    composition.coverage = payload.options.include_coverage.then_some(payload.coverage.len());

    // Layer keys in order of first use, and the symbol blocks.
    let mut keys: Vec<String> = Vec::new();
    for (key, _) in &plan.items {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    plan.kinds.sort();
    let block_keys: Vec<String> = plan.kinds.iter().map(|kind| format!("SYMBOL-{}", kind.tag())).collect();
    composition.existing_secureplan_layers =
        document.layers.iter().filter(|layer| layer.name.to_ascii_uppercase().starts_with("SECUREPLAN")).count();
    // One prefix for every new name, the first under which none exists.
    let prefix = (1..)
        .map(|n| if n == 1 { "SECUREPLAN-".to_string() } else { format!("SECUREPLAN-{n}-") })
        .find(|prefix| {
            keys.iter().all(|key| !document.layers.contains(&format!("{prefix}{key}")))
                && block_keys.iter().all(|key| !document.block_records.contains(&format!("{prefix}{key}")))
        })
        .expect("a free prefix");

    for key in &keys {
        let mut layer = Layer::new(format!("{prefix}{key}"));
        layer.handle = document.allocate_handle();
        layer.color = Color::from_index(layer_color(key));
        let _ = document.layers.add(layer);
    }
    for (kind, key) in plan.kinds.iter().zip(&block_keys) {
        define_block(document, &format!("{prefix}{key}"), symbols::symbol(*kind));
        composition.blocks.push(format!("{prefix}{key}"));
    }
    let mut counts = vec![0usize; keys.len()];
    for (key, mut entity) in plan.items {
        let slot = keys.iter().position(|k| *k == key).expect("listed above");
        counts[slot] += 1;
        entity.common_mut().layer = format!("{prefix}{key}");
        if let EntityType::Insert(insert) = &mut entity {
            insert.block_name = format!("{prefix}{}", insert.block_name);
        }
        let _ = document.add_entity(entity);
    }
    composition.layers = keys.iter().map(|key| format!("{prefix}{key}")).zip(counts).collect();
    composition.prefix = prefix;
    composition
}

/// Define block `name` with `entities` at the origin. Every handle is newly
/// allocated, so it can never collide with one the drawing already uses.
fn define_block(document: &mut CadDocument, name: &str, entities: Vec<EntityType>) {
    let record_handle = document.allocate_handle();
    let block_handle = document.allocate_handle();
    let end_handle = document.allocate_handle();
    let mut record = BlockRecord::new(name);
    record.handle = record_handle;
    record.block_entity_handle = block_handle;
    record.block_end_handle = end_handle;
    if document.block_records.add(record).is_err() {
        return;
    }
    let mut block = Block::new(name, Vector3::ZERO);
    block.common.handle = block_handle;
    block.common.owner_handle = record_handle;
    let _ = document.add_entity(EntityType::Block(block));
    let mut end = BlockEnd::new();
    end.common.handle = end_handle;
    end.common.owner_handle = record_handle;
    let _ = document.add_entity(EntityType::BlockEnd(end));
    for mut entity in entities {
        entity.common_mut().owner_handle = record_handle;
        let _ = document.add_entity(entity);
    }
}

// ── Output (EXP-03) ─────────────────────────────────────────────────────────

/// The formats and versions the native writer offers, as the upstream Save
/// dialog lists them.
pub fn choices() -> &'static [&'static str] {
    crate::io::SAVE_FORMAT_OPTIONS
}

/// The format and version of choice `index`.
pub fn choice(index: usize) -> (Format, DxfVersion) {
    let (ext, version) = crate::io::parse_save_format(choices().get(index).copied().unwrap_or("DWG 2018"));
    (if ext == "dxf" { Format::Dxf } else { Format::Dwg }, version)
}

/// Whether the native writer offers `format` in `version`. DWG and DXF R13
/// (AC1012) are read but never written (EXP-03, PUB-04).
pub fn writable(format: Format, version: DxfVersion) -> bool {
    (0..choices().len()).any(|index| choice(index) == (format, version))
}

/// `DWG R13 (AC1012)`: the format, release name where there is one, and code.
pub fn version_label(format: Format, version: DxfVersion) -> String {
    let release = match version {
        DxfVersion::AC1012 => "R13 ",
        DxfVersion::AC1014 => "R14 ",
        _ => "",
    };
    format!("{} {release}({})", format.ext().to_ascii_uppercase(), version.as_str())
}

/// The choice for the applied drawing's own format and version (EXP-03's
/// default), or the nearest newer version the writer offers.
pub fn default_choice(format: Format, version: &str) -> usize {
    let wanted = DxfVersion::parse(version).unwrap_or(DxfVersion::AC1032);
    let candidates: Vec<(usize, DxfVersion)> =
        (0..choices().len()).map(|i| (i, choice(i))).filter(|(_, (f, _))| *f == format).map(|(i, (_, v))| (i, v)).collect();
    candidates
        .iter()
        .filter(|(_, v)| *v >= wanted)
        .min_by_key(|(_, v)| *v)
        .or_else(|| candidates.iter().max_by_key(|(_, v)| *v))
        .map_or(0, |(i, _)| *i)
}

/// Objects the writer cannot put into `format` and `version`.
pub fn unwritable(document: &CadDocument, format: Format, version: DxfVersion) -> usize {
    crate::io::dropped_on_save_count(document, version, format == Format::Dxf)
}

/// Why a written export was not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The native writer failed (`WRITER_ERROR`).
    Writer,
    /// The written header does not name the chosen format and version
    /// (`HEADER_MISMATCH`).
    Header,
}

impl WriteError {
    pub fn code(self) -> &'static str {
        match self {
            WriteError::Writer => "WRITER_ERROR",
            WriteError::Header => "HEADER_MISMATCH",
        }
    }
}

/// The format and version code a drawing's header names: the DWG version
/// code, or an ASCII DXF's `$ACADVER`.
pub fn header_version(bytes: &[u8]) -> Option<(Format, String)> {
    match Format::sniff(bytes)? {
        Format::Dwg => Some((Format::Dwg, String::from_utf8_lossy(&bytes[..6]).into_owned())),
        Format::Dxf => {
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]).into_owned();
            let mut lines = text.lines().map(str::trim);
            lines.by_ref().find(|line| *line == "$ACADVER")?;
            let (code, value) = (lines.next()?, lines.next()?);
            (code == "1").then(|| (Format::Dxf, value.to_string()))
        }
    }
}

/// Whether `bytes` are a drawing whose header names `format` and `version`.
pub fn check_header(bytes: &[u8], format: Format, version: DxfVersion) -> Result<(), WriteError> {
    match header_version(bytes) {
        Some((written, code)) if written == format && code == version.as_str() => Ok(()),
        _ => Err(WriteError::Header),
    }
}

/// Write `document` as `format` and `version`, and check the header says so.
pub fn write(document: &CadDocument, format: Format, version: DxfVersion) -> Result<Vec<u8>, WriteError> {
    let bytes = crate::io::save_to_bytes(document, format.ext(), version).map_err(|_| WriteError::Writer)?;
    check_header(&bytes, format, version)?;
    Ok(bytes)
}

/// A file name `exportResult` may carry: no directory, no control
/// characters, at most 255 characters.
pub fn result_name(path: &std::path::Path) -> String {
    let name: String = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() || c == '/' || c == '\\' { '_' } else { c })
        .take(255)
        .collect();
    if name.is_empty() { "export".into() } else { name }
}

// ── The export under way ────────────────────────────────────────────────────

/// One export job: the session it answers and a serial unique in the
/// process. Workers, the Save dialog and the dialog itself carry it; a result
/// for any other key (an older export, or one of an earlier session of the
/// same survey) is ignored, whatever its request id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobKey {
    pub session: super::bridge::SessionId,
    pub serial: u64,
}

/// Where an export is.
#[derive(Debug, Clone)]
pub enum Stage {
    /// Reading the drawing and adding the design, on a worker.
    Composing,
    /// The export dialog is up (or waits for another dialog to close).
    Choosing,
    /// The chosen format is being written, on a worker.
    Writing,
    /// Written and checked; waiting for the Save dialog.
    Saving { bytes: Arc<Vec<u8>>, format: Format, version: DxfVersion },
    /// Being written to the chosen file, on a worker.
    Storing,
}

/// One `exportRequest` being answered.
#[derive(Debug, Clone)]
pub struct ExportJob {
    pub key: JobKey,
    pub request_id: String,
    pub stage: Stage,
    /// The dialog, while another SecurePlan dialog is showing.
    pub waiting: Option<Box<super::ui::export_dialog::ExportDialog>>,
    /// A copy of the dialog as it was shown: if another dialog replaces it
    /// before the user answers, it comes back once that one closes.
    pub shown: Option<Box<super::ui::export_dialog::ExportDialog>>,
    /// A test or the `secureplan-test` driver saves here instead of asking.
    pub target: Option<std::path::PathBuf>,
}

/// A composed export, carried back from the worker.
pub struct Composed {
    pub document: CadDocument,
    pub composition: Composition,
    pub format: Format,
    pub version: String,
    /// Damaged items the reader dropped from the applied drawing.
    pub lost_entities: usize,
    pub stem: String,
}

/// Read the applied drawing and add the design (on a worker).
pub fn prepare(drawing: &super::session::Drawing, payload: &[u8], snapshot: &serde_json::Value, mapping: &Mapping) -> Result<Composed, String> {
    let payload = parse_payload(payload)?;
    let same = snapshot["version"].as_u64() == Some(payload.snapshot.version)
        && snapshot["documentHash"].as_str() == Some(payload.snapshot.document_hash.as_str());
    if !same {
        return Err("The export payload belongs to another saved version of the survey.".into());
    }
    let (mut document, report) = super::import::load_drawing(drawing.name.expose(), drawing.bytes.as_ref().clone()).map_err(|e| e.message)?;
    crate::app::style_ops::ensure_standard_styles(&mut document);
    let composition = compose(&mut document, &payload, mapping);
    Ok(Composed { document, composition, format: report.format, version: report.version, lost_entities: report.lost_entities, stem: drawing.stem() })
}

/// A finished composition, for the UI thread.
#[derive(Debug, Clone)]
pub struct ComposeDone {
    pub tab_id: u64,
    pub key: JobKey,
    pub result: super::Carry<Result<Composed, String>>,
}

/// A finished write, for the UI thread.
#[derive(Debug, Clone)]
pub struct WriteDone {
    pub tab_id: u64,
    pub key: JobKey,
    pub format: Format,
    pub version: DxfVersion,
    pub result: super::Carry<Result<Vec<u8>, WriteError>>,
}

/// A finished file write, for the UI thread.
#[derive(Debug, Clone)]
pub struct SaveDone {
    pub tab_id: u64,
    pub key: JobKey,
    /// The saved file's name (no directory), for `exportResult`.
    pub name: String,
    pub format: Format,
    pub version: DxfVersion,
    pub saved: bool,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::{testutil, vectors};

    /// The web's valid export sample.
    pub(crate) fn sample_payload() -> serde_json::Value {
        let samples = vectors::json("payloads/valid.json");
        samples["samples"].as_array().unwrap().iter().find(|s| s["name"] == "export").unwrap()["value"].clone()
    }

    /// Every row of EXP-02: all included, and a custom icon.
    pub(crate) fn full_payload() -> Payload {
        let mut value = sample_payload();
        value["options"] = serde_json::json!({ "includeNotes": true, "includeCoverage": true });
        value["coverage"] = serde_json::json!([{ "elementId": "camera-1", "polygon": [[6000, 3000], [9000, 3000], [9000, 6000]] }]);
        value["devices"].as_array_mut().unwrap().push(serde_json::json!({
            "id": "reader-1", "kind": "asset", "name": "Card reader", "iconKey": "card-reader", "customIcon": true,
            "color": "#00ff00", "label": "", "status": "In Place", "position": [1000, 2000], "rotationDeg": 0,
        }));
        // A wall's own label: an engineering label.
        value["labels"].as_array_mut().unwrap().push(serde_json::json!({ "elementId": "wall-1", "text": "W-01", "position": [6000, 100] }));
        value["drawings"].as_array_mut().unwrap().extend([
            serde_json::json!({ "id": "d-rect", "drawingType": "rectangle", "label": "Store", "points": [[100, 100], [1100, 600]] }),
            serde_json::json!({ "id": "d-ell", "drawingType": "ellipse", "label": "", "points": [[2000, 100], [3000, 600]] }),
            serde_json::json!({ "id": "d-free", "drawingType": "freehand", "label": "", "points": [[0, 0], [100, 50], [300, 60]] }),
        ]);
        parse_payload(value.to_string().as_bytes()).unwrap()
    }

    /// 1 CAD unit = 1 mm, CAD (x, y) → world (x, 18000 − y).
    pub(crate) fn mapping() -> Mapping {
        Mapping { cad_origin: [0.0, 18000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 }
    }

    fn on_layer<'a>(document: &'a CadDocument, layer: &'a str) -> impl Iterator<Item = &'a EntityType> + 'a {
        document.entities().filter(move |e| e.common().layer == layer)
    }

    #[test]
    fn every_row_gets_its_layer_and_user_entities_are_untouched() {
        let mut document = testutil::synthetic_document();
        let before: Vec<(u64, String)> = document.entities().map(|e| (e.common().handle.value(), format!("{e:?}"))).collect();
        let layers_before = document.layers.iter().count();
        let payload = full_payload();
        let composition = compose(&mut document, &payload, &mapping());
        // Nothing the drawing had changed.
        for (handle, debug) in &before {
            let entity = document.get_entity(acadrust::types::Handle::new(*handle)).expect("still there");
            assert_eq!(&format!("{entity:?}"), debug, "entity {handle:X} changed");
        }
        assert_eq!(document.layers.iter().count(), layers_before + composition.layers.len());
        let names: Vec<&str> = composition.layers.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "SECUREPLAN-WALL-GLASS-PROPOSED",
                "SECUREPLAN-DOOR-IN-PLACE",
                "SECUREPLAN-ROUTE",
                "SECUREPLAN-CAMERA",
                "SECUREPLAN-ASSET",
                "SECUREPLAN-TARGET",
                "SECUREPLAN-DRAWING",
                "SECUREPLAN-LABEL",
                "SECUREPLAN-NOTE",
                "SECUREPLAN-COVERAGE",
            ]
        );
        // Walls and doors: lines at the inverse mapping (world y 0 → CAD y 18000).
        let wall: Vec<_> = on_layer(&document, "SECUREPLAN-WALL-GLASS-PROPOSED").collect();
        let EntityType::LwPolyline(wall) = wall[0] else { panic!("wall linework") };
        assert_eq!((wall.vertices[0].location, wall.vertices[1].location), (Vector2::new(0.0, 18000.0), Vector2::new(12000.0, 18000.0)));
        assert_eq!(on_layer(&document, "SECUREPLAN-DOOR-IN-PLACE").count(), 1);
        // The route and its "label (cable type)".
        let route: Vec<_> = on_layer(&document, "SECUREPLAN-ROUTE").collect();
        assert!(matches!(route[0], EntityType::LwPolyline(p) if p.vertices.len() == 2));
        assert!(matches!(route[1], EntityType::Text(t) if t.value == "C-01 (Cat6)"));
        // Devices: symbol blocks with their catalog colour, labelled.
        assert_eq!(composition.blocks, ["SECUREPLAN-SYMBOL-CAMERA", "SECUREPLAN-SYMBOL-ASSET"]);
        let camera: Vec<_> = on_layer(&document, "SECUREPLAN-CAMERA").collect();
        let EntityType::Insert(insert) = camera[0] else { panic!("a symbol") };
        assert_eq!(insert.block_name, "SECUREPLAN-SYMBOL-CAMERA");
        assert_eq!(insert.insert_point, Vector3::new(6000.0, 15000.0, 0.0));
        assert_eq!(insert.common.color, Color::Rgb { r: 0x2f, g: 0x80, b: 0xed });
        assert_eq!(insert.x_scale(), symbols::SYMBOL_RADIUS_MM);
        // 45° in the world (y down) is −45° in CAD (y up).
        assert!((insert.rotation - 315f64.to_radians()).abs() < 1e-12, "{}", insert.rotation);
        assert!(matches!(camera[1], EntityType::Text(t) if t.value == "CAM-01"));
        let asset: Vec<_> = on_layer(&document, "SECUREPLAN-ASSET").collect();
        assert!(matches!(asset[1], EntityType::Text(t) if t.value == "Card reader"), "an empty label falls back to the name");
        assert_eq!(composition.custom_icons, 1);
        let block = document.block_records.get("SECUREPLAN-SYMBOL-CAMERA").expect("the camera symbol");
        assert_eq!(block.entity_handles.len(), symbols::symbol(DeviceKind::Camera).len());
        // Targets, drawings, labels, notes and coverage.
        let target: Vec<_> = on_layer(&document, "SECUREPLAN-TARGET").collect();
        assert!(matches!(target[2], EntityType::Text(t) if t.value == "Entry face"), "no evaluated label in this payload: its own");
        assert!(matches!(target[3], EntityType::Text(t) if t.value == "Required 250 PPM"));
        let drawings: Vec<_> = on_layer(&document, "SECUREPLAN-DRAWING").collect();
        assert!(drawings.iter().any(|e| matches!(e, EntityType::LwPolyline(p) if p.is_closed && p.vertices.len() == 4)), "rectangle");
        assert!(drawings.iter().any(|e| matches!(e, EntityType::Ellipse(el) if (el.minor_axis_ratio - 0.5).abs() < 1e-12)), "ellipse");
        assert!(drawings.iter().any(|e| matches!(e, EntityType::LwPolyline(p) if p.is_closed && p.vertices.len() == 3)), "arrowhead");
        assert!(drawings.iter().any(|e| matches!(e, EntityType::Text(t) if t.value == "Store")));
        // The wall's label is an engineering label; the camera's is drawn once, with its symbol.
        let labels: Vec<_> = on_layer(&document, "SECUREPLAN-LABEL").collect();
        assert!(matches!(labels[..], [EntityType::Text(t)] if t.value == "W-01"), "{labels:?}");
        assert_eq!(composition.labels, 1);
        let texts = |value: &str| document.entities().filter(|e| matches!(e, EntityType::Text(t) if t.value == value)).count();
        assert_eq!(texts("CAM-01"), 1, "the camera's label, once");
        assert!(matches!(on_layer(&document, "SECUREPLAN-NOTE").next(), Some(EntityType::MText(t)) if t.value == "Synthetic note"));
        assert!(matches!(on_layer(&document, "SECUREPLAN-COVERAGE").next(), Some(EntityType::LwPolyline(p)) if p.is_closed));
        assert_eq!((composition.notes, composition.coverage), (Some(1), Some(1)));
        assert!(composition.summary().iter().any(|line| line.contains("custom icon")));
        assert!(composition.summary().iter().any(|line| line.contains("may overlap")));
    }

    #[test]
    fn the_design_goes_back_through_the_inverse_of_a_turned_scaled_mapping() {
        // Centimetre drawing units, a quarter turn and an anchor.
        let mapping = Mapping { cad_origin: [100.0, 500.0], anchor_mm: [1000.0, 2000.0], scale_mm_per_cad_unit: 10.0, quarter_turns: 1 };
        let mut document = testutil::synthetic_document();
        let payload = full_payload();
        compose(&mut document, &payload, &mapping);
        let wall = on_layer(&document, "SECUREPLAN-WALL-GLASS-PROPOSED").next().unwrap();
        let EntityType::LwPolyline(wall) = wall else { panic!("wall linework") };
        for (vertex, world) in wall.vertices.iter().zip([payload.walls[0].start, payload.walls[0].end]) {
            let back = mapping.cad_to_world([vertex.location.x, vertex.location.y]);
            assert!((back[0] - world[0]).abs() < 1e-9 && (back[1] - world[1]).abs() < 1e-9, "{back:?} {world:?}");
        }
        let camera = on_layer(&document, "SECUREPLAN-CAMERA").next().unwrap();
        let EntityType::Insert(insert) = camera else { panic!("a symbol") };
        let back = mapping.cad_to_world([insert.insert_point.x, insert.insert_point.y]);
        assert!((back[0] - 6000.0).abs() < 1e-9 && (back[1] - 3000.0).abs() < 1e-9);
        // 600 mm and 200 mm on the plan are 60 and 20 centimetres.
        assert!((insert.x_scale() - 30.0).abs() < 1e-12);
        let label = on_layer(&document, "SECUREPLAN-CAMERA").nth(1).unwrap();
        assert!(matches!(label, EntityType::Text(t) if (t.height - 20.0).abs() < 1e-12));
        // The camera's facing (45° in the world) points the same way on the plan.
        let facing = [6000.0 + 45f64.to_radians().cos(), 3000.0 + 45f64.to_radians().sin()];
        let tip = mapping.cad_to_world([insert.insert_point.x + insert.rotation.cos(), insert.insert_point.y + insert.rotation.sin()]);
        let (dx, dy) = (tip[0] - 6000.0, tip[1] - 3000.0);
        let length = dx.hypot(dy);
        assert!((dx / length - (facing[0] - 6000.0)).abs() < 1e-9 && (dy / length - (facing[1] - 3000.0)).abs() < 1e-9);
    }

    #[test]
    fn a_target_shows_the_achieved_ppm_once_and_the_required_ppm_apart() {
        let mut value = sample_payload();
        // As the web sends it: the target's engineering label carries the
        // PPM the design achieves there (0 here), not the required 250.
        value["labels"].as_array_mut().unwrap().push(serde_json::json!({ "elementId": "target-1", "text": "Entry face · 0 PPM", "position": [7000, 3900] }));
        let payload = parse_payload(value.to_string().as_bytes()).unwrap();
        let mut document = testutil::synthetic_document();
        compose(&mut document, &payload, &mapping());
        let texts = |value: &str| document.entities().filter(|e| matches!(e, EntityType::Text(t) if t.value == value)).map(|e| e.common().layer.clone()).collect::<Vec<_>>();
        assert_eq!(texts("Entry face · 0 PPM"), ["SECUREPLAN-TARGET"], "the achieved PPM, once, with the target");
        assert_eq!(texts("Required 250 PPM"), ["SECUREPLAN-TARGET"], "the requirement, apart");
        assert!(texts("Entry face").is_empty() && texts("Entry face (250 px/m)").is_empty(), "no second label");
        let placed = document
            .entities()
            .find_map(|e| match e {
                EntityType::Text(t) if t.value == "Entry face · 0 PPM" => Some(t.insertion_point),
                _ => None,
            })
            .unwrap();
        assert_eq!(placed, Vector3::new(7000.0, 18000.0 - 3900.0, 0.0), "where the web places it");
    }

    #[test]
    fn notes_and_coverage_stay_out_unless_chosen() {
        let mut value = sample_payload();
        value["options"] = serde_json::json!({ "includeNotes": false, "includeCoverage": false });
        let payload = parse_payload(value.to_string().as_bytes()).unwrap();
        let mut document = testutil::synthetic_document();
        let composition = compose(&mut document, &payload, &mapping());
        assert!(composition.layers.iter().all(|(name, _)| name != "SECUREPLAN-NOTE" && name != "SECUREPLAN-COVERAGE"));
        assert_eq!((composition.notes, composition.coverage), (None, None));
    }

    #[test]
    fn an_earlier_export_in_the_drawing_is_never_merged_into() {
        let payload = full_payload();
        let mut document = testutil::synthetic_document();
        compose(&mut document, &payload, &mapping());
        let first_entities = document.entities().count();
        // Export again from a drawing that already holds the first export.
        let second = compose(&mut document, &payload, &mapping());
        assert_eq!(second.prefix, "SECUREPLAN-2-");
        assert!(second.layers.iter().all(|(name, _)| name.starts_with("SECUREPLAN-2-")));
        assert_eq!(second.blocks, ["SECUREPLAN-2-SYMBOL-CAMERA", "SECUREPLAN-2-SYMBOL-ASSET"]);
        assert_eq!(second.existing_secureplan_layers, 10);
        let first_layer = on_layer(&document, "SECUREPLAN-ROUTE").count();
        assert_eq!(first_layer, 2, "the first export's layers gained nothing");
        assert!(document.entities().count() > first_entities);
        assert!(second.summary().iter().any(|line| line.contains("SECUREPLAN-2-")));
    }

    #[test]
    fn the_payload_is_validated_against_the_schema() {
        let mut value = sample_payload();
        value["comments"] = serde_json::json!([]);
        assert!(parse_payload(value.to_string().as_bytes()).is_err(), "comments are never exported");
        let mut value = sample_payload();
        value["devices"][0]["unitCostMinor"] = serde_json::json!(100);
        assert!(parse_payload(value.to_string().as_bytes()).is_err(), "pricing is never exported");
        assert!(parse_payload(b"not json").is_err());
        assert!(parse_payload(sample_payload().to_string().as_bytes()).is_ok());
    }

    #[test]
    fn the_written_header_is_checked_against_the_choice() {
        let document = testutil::synthetic_document();
        for (format, version) in [(Format::Dwg, DxfVersion::AC1032), (Format::Dxf, DxfVersion::AC1015), (Format::Dwg, DxfVersion::AC1018)] {
            let bytes = write(&document, format, version).unwrap();
            assert_eq!(header_version(&bytes), Some((format, version.as_str().to_string())));
        }
        assert_eq!(header_version(b"AC1027\0\0\0"), Some((Format::Dwg, "AC1027".into())));
        let dxf = testutil::synthetic_dxf();
        assert_eq!(header_version(&dxf), Some((Format::Dxf, "AC1032".into())));
        // A header naming another version or format fails the check.
        let mut other = crate::io::save_to_bytes(&document, "dxf", DxfVersion::AC1032).unwrap();
        assert_eq!(check_header(&other, Format::Dxf, DxfVersion::AC1032), Ok(()));
        let at = other.windows(6).position(|w| w == b"AC1032").unwrap();
        other[at..at + 6].copy_from_slice(b"AC1027");
        assert_eq!(check_header(&other, Format::Dxf, DxfVersion::AC1032), Err(WriteError::Header));
        assert_eq!(check_header(&other, Format::Dwg, DxfVersion::AC1027), Err(WriteError::Header));
        assert_eq!(check_header(b"not a drawing", Format::Dxf, DxfVersion::AC1032), Err(WriteError::Header));
        assert_eq!(header_version(b"not a drawing"), None);
    }

    #[test]
    fn the_default_is_the_applied_drawings_format_and_version() {
        assert_eq!(choice(default_choice(Format::Dxf, "AC1018")), (Format::Dxf, DxfVersion::AC1018));
        assert_eq!(choice(default_choice(Format::Dwg, "AC1032")), (Format::Dwg, DxfVersion::AC1032));
        // R13 is not offered: the nearest newer version.
        assert_eq!(choice(default_choice(Format::Dwg, "AC1012")), (Format::Dwg, DxfVersion::AC1014));
    }

    #[test]
    fn a_composed_export_reopens_through_the_native_reader() {
        let payload = full_payload();
        for format in [Format::Dwg, Format::Dxf] {
            let mut document = testutil::synthetic_document();
            // The drawing's own model-space entities.
            let own = |doc: &CadDocument| {
                let model = doc.header.model_space_block_handle;
                doc.entities().filter(|e| e.common().owner_handle == model && !e.common().layer.starts_with("SECUREPLAN")).count()
            };
            let user = own(&document);
            let composition = compose(&mut document, &payload, &mapping());
            let bytes = write(&document, format, DxfVersion::AC1032).unwrap();
            let name = format!("export.{}", format.ext());
            let reread = crate::io::load_bytes(&name, bytes).expect("the export reopens");
            for (layer, count) in &composition.layers {
                assert!(reread.layers.contains(layer), "{format:?}: layer {layer}");
                let found = reread.entities().filter(|e| e.common().layer == *layer).count();
                assert_eq!(found, *count, "{format:?}: entities on {layer}");
            }
            let cam = reread.entities().filter(|e| matches!(e, EntityType::Text(t) if t.value == "CAM-01")).count();
            assert_eq!(cam, 1, "{format:?}: the device label appears once");
            for block in &composition.blocks {
                assert!(reread.block_records.get(block).is_some_and(|b| !b.entity_handles.is_empty()), "{format:?}: block {block}");
            }
            assert_eq!(own(&reread), user, "{format:?}: the drawing's own entities");
        }
    }
}
