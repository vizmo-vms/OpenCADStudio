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

/// A published page: the vector PDF bytes and what could not be drawn.
pub struct PublishedPdf {
    pub bytes: Vec<u8>,
    /// Raster images in the published view, which the SecurePlan page does
    /// not draw; Apply reports them (F5) rather than dropping them silently.
    pub omitted_images: usize,
}

/// The model-space view of `scene` as a one-page CON-01 PDF.
pub fn model_pdf(scene: &crate::scene::Scene, transform: PageTransform) -> Result<PublishedPdf, String> {
    use crate::io::pdf_export::{PlotContent, PlotGroupSplits, PlotWire};
    if scene.current_layout != "Model" {
        return Err("The published view must be model space.".into());
    }
    let (wires, _) = scene.plot_wire_groups(None);
    let wires: Vec<_> = wires.into_iter().filter(|wire| wire.plot_visible).collect();
    let depths = scene.plot_wire_depths(&wires);
    let wires: Vec<PlotWire> = wires.into_iter().zip(depths).map(|(wire, draw_depth)| PlotWire { wire, draw_depth }).collect();
    let hatches = scene.paper_plot_hatches().as_ref().clone();
    let wipeouts = scene.paper_plot_wipeouts().as_ref().clone();
    let omitted_images = scene.paper_plot_images().len();
    let content = PlotContent {
        group_splits: PlotGroupSplits { wires: wires.len(), hatches: hatches.len(), wipeouts: wipeouts.len(), images: 0 },
        wires: std::sync::Arc::new(wires),
        hatches,
        wipeouts,
        images: Vec::new(),
    };
    let bytes = crate::io::pdf_export::secureplan_page_pdf(content, transform)?;
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
/// CropBox or TrimBox, no dates or other varying metadata, compressed
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

/// Build the SPSNAP file for the model-space view (CON-05).
pub fn model_snap(doc: &acadrust::CadDocument, transform: PageTransform) -> Vec<u8> {
    let geometry = snap::extract_model(doc, &transform);
    snap::write(&geometry, transform.placement.width_pt, transform.placement.height_pt)
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

    /// A synthetic floor plan, written as DXF and read back like an import.
    pub(crate) fn synthetic_dxf_scene() -> crate::scene::Scene {
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
        let bytes = crate::io::save_to_bytes(&doc, "dxf", doc.version).unwrap();
        let mut scene = crate::scene::Scene::new();
        scene.document = crate::io::load_bytes("synthetic.dxf", bytes).unwrap();
        scene.rebuild_derived_caches();
        scene
    }

    pub(crate) fn empty_survey_transform() -> PageTransform {
        let mapping = Mapping { cad_origin: [0.0, 18000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        let placement = place_page([0.0, 0.0, 30000.0, 18000.0], &mapping, 3.0).unwrap();
        PageTransform { mapping, placement }
    }

    fn page_dict(doc: &lopdf::Document) -> lopdf::Dictionary {
        let pages = doc.get_pages();
        assert_eq!(pages.len(), 1);
        doc.get_object(*pages.values().next().unwrap()).unwrap().as_dict().unwrap().clone()
    }

    #[test]
    fn the_published_pdf_follows_the_page_convention_and_is_deterministic() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let first = model_pdf(&scene, transform).unwrap();
        let second = model_pdf(&scene, transform).unwrap();
        assert_eq!(first.bytes, second.bytes, "identical bytes on a second run");
        assert_eq!(first.omitted_images, 0);

        let doc = lopdf::Document::load_mem(&first.bytes).unwrap();
        let page = page_dict(&doc);
        let media: Vec<i64> = page.get(b"MediaBox").unwrap().as_array().unwrap().iter().map(|o| o.as_i64().unwrap()).collect();
        assert_eq!(media, vec![0, 0, 10000, 6000]);
        assert_eq!(page.get(b"Rotate").unwrap().as_i64().unwrap(), 0);
        assert!(page.get(b"CropBox").is_err() && page.get(b"TrimBox").is_err());
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

    /// Every `m`/`l` operand pair in the page content, in page points.
    pub(crate) fn content_points(bytes: &[u8]) -> Vec<[f64; 2]> {
        let doc = lopdf::Document::load_mem(bytes).unwrap();
        let page = *doc.get_pages().values().next().unwrap();
        let content = lopdf::content::Content::decode(&doc.get_page_content(page).unwrap()).unwrap();
        content
            .operations
            .iter()
            .filter(|op| op.operator == "m" || op.operator == "l")
            .map(|op| {
                let n = |o: &lopdf::Object| o.as_float().map(f64::from).or_else(|_| o.as_i64().map(|v| v as f64)).unwrap();
                [n(&op.operands[0]), n(&op.operands[1])]
            })
            .collect()
    }

    #[test]
    fn a_known_line_lands_where_the_placement_says() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let pdf = model_pdf(&scene, transform).unwrap();
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
}
