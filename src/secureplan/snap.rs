//! SPSNAP v1 (CON-05): the binary snap sidecar for a published page.
//!
//! Layout (little-endian): magic `SPSNAP01`, `u32 version = 1`, `u32 flags`
//! (bit 0: the body is exactly one gzip member, written with MTIME 0), `u32 pageWidthPt`,
//! `u32 pageHeightPt`, `u32 pointCount`, `u32 segmentCount`,
//! `u32 uncompressedBodyBytes`; then `f32[2·pointCount]` endpoints and vertices
//! and `f32[4·segmentCount]` segments, in CON-01 page points.
//!
//! Included: model geometry visible in the published view — lines and the
//! polylines of the prepared publication ([`super::publish::Publication`]),
//! which replaces every curve (arcs, circles, ellipses, splines, bulged
//! polylines, legacy POLYLINEs) by the same polyline the PDF draws, in the
//! curve's own plane — inside block references too, walked with the renderer's transforms and
//! visibility rules (invisible, off, frozen and non-plotting layers). Excluded:
//! text, mtext, dimensions, tables, leaders, hatches, images and paper
//! space. Everything is clipped to the page; on a paper layout, to its
//! reference viewport (`Publication::clip`), so only model geometry the
//! viewport shows contributes, and paper-space entities never do.

use std::io::Write;

use acadrust::types::{Transform, Vector3};
use acadrust::EntityType;

use super::publish::{PageTransform, Publication};

pub const MAGIC: &[u8; 8] = b"SPSNAP01";
pub const VERSION: u32 = 1;
pub const FLAG_GZIP: u32 = 1;
pub const HEADER_LEN: usize = 36;
pub const MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;

/// Snap geometry in page points.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SnapGeometry {
    pub points: Vec<[f32; 2]>,
    pub segments: Vec<[f32; 4]>,
}

struct Collector<'a> {
    page: &'a PageTransform,
    /// `[x0, y0, x1, y1]` in page points.
    clip: [f64; 4],
    out: SnapGeometry,
}

impl Collector<'_> {
    fn to_page(&self, transform: &Transform, [x, y, z]: [f64; 3]) -> (f64, f64) {
        let world = transform.apply(Vector3::new(x, y, z));
        self.page.apply(world.x, world.y)
    }

    fn point(&mut self, transform: &Transform, p: [f64; 3]) {
        let (px, py) = self.to_page(transform, p);
        let [x0, y0, x1, y1] = self.clip;
        if (x0..=x1).contains(&px) && (y0..=y1).contains(&py) {
            self.out.points.push([px as f32, py as f32]);
        }
    }

    /// Add a segment in page points, clipped to the clip rectangle (Liang–Barsky).
    fn page_segment(&mut self, (x0, y0): (f64, f64), (x1, y1): (f64, f64)) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
        let [cx0, cy0, cx1, cy1] = self.clip;
        for (p, q) in [(-dx, x0 - cx0), (dx, cx1 - x0), (-dy, y0 - cy0), (dy, cy1 - y0)] {
            if p == 0.0 {
                if q < 0.0 {
                    return;
                }
            } else {
                let t = q / p;
                if p < 0.0 {
                    t0 = t0.max(t);
                } else {
                    t1 = t1.min(t);
                }
            }
        }
        if t0 > t1 {
            return;
        }
        let at = |t: f64| [(x0 + t * dx) as f32, (y0 + t * dy) as f32];
        let [a, b] = [at(t0), at(t1)];
        if a != b {
            self.out.segments.push([a[0], a[1], b[0], b[1]]);
        }
    }

    fn segment(&mut self, transform: &Transform, a: [f64; 3], b: [f64; 3]) {
        let a = self.to_page(transform, a);
        let b = self.to_page(transform, b);
        self.page_segment(a, b);
    }

    fn entity(&mut self, publication: &Publication, entity: &EntityType, transform: &Transform) {
        match entity {
            EntityType::Line(line) => {
                let (a, b) = ([line.start.x, line.start.y, line.start.z], [line.end.x, line.end.y, line.end.z]);
                self.point(transform, a);
                self.point(transform, b);
                self.segment(transform, a, b);
            }
            // After preparation no polyline has a bulge; its vertices are in
            // its own plane (OCS), which may be tilted.
            EntityType::LwPolyline(polyline) => {
                let normal = (polyline.normal.x, polyline.normal.y, polyline.normal.z);
                let vertices: Vec<[f64; 3]> = polyline
                    .vertices
                    .iter()
                    .map(|v| {
                        let (x, y, z) = crate::scene::view::transform::ocs_point_to_wcs((v.location.x, v.location.y, polyline.elevation), normal);
                        [x, y, z]
                    })
                    .collect();
                match publication.key_points(polyline.common.handle.value()) {
                    Some(keys) => {
                        for key in keys {
                            self.point(transform, *key);
                        }
                    }
                    None => {
                        for vertex in &vertices {
                            self.point(transform, *vertex);
                        }
                    }
                }
                for pair in vertices.windows(2) {
                    self.segment(transform, pair[0], pair[1]);
                }
                if polyline.is_closed && vertices.len() > 2 {
                    self.segment(transform, vertices[vertices.len() - 1], vertices[0]);
                }
            }
            // Text, dimensions, hatches, images and every other kind
            // contribute no snaps (CON-05).
            _ => {}
        }
    }
}

/// Snap geometry for a prepared publication.
pub fn extract(publication: &Publication) -> SnapGeometry {
    let page = &publication.transform;
    let mut collector = Collector { page, clip: publication.clip, out: SnapGeometry::default() };
    super::publish::walk_model(&publication.scene, true, |entity, context| {
        collector.entity(publication, entity, &context.transform);
    });
    collector.out
}

/// Serialise snap geometry as SPSNAP v1 with a gzip body (MTIME 0).
pub fn write(geometry: &SnapGeometry, width_pt: u32, height_pt: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(geometry.points.len() * 8 + geometry.segments.len() * 16);
    for value in geometry.points.iter().flatten().chain(geometry.segments.iter().flatten()) {
        body.extend_from_slice(&value.to_le_bytes());
    }
    let mut out = Vec::with_capacity(HEADER_LEN + body.len() / 2);
    out.extend_from_slice(MAGIC);
    for value in [
        VERSION,
        FLAG_GZIP,
        width_pt,
        height_pt,
        geometry.points.len() as u32,
        geometry.segments.len() as u32,
        body.len() as u32,
    ] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut encoder = flate2::GzBuilder::new().mtime(0).write(out, flate2::Compression::best());
    encoder.write_all(&body).expect("writing to memory cannot fail");
    encoder.finish().expect("writing to memory cannot fail")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::secureplan::publish::tests::{empty_survey_transform, synthetic_dxf_scene};
    use acadrust::CadDocument;
    use crate::app::secureplan::vectors;
    use std::io::Read;

    /// A reader applying every CON-05 rule, checked against the vectors and
    /// then used to accept the writer's output.
    pub(crate) fn read(bytes: &[u8], page: (u32, u32)) -> Result<SnapGeometry, &'static str> {
        if bytes.len() < HEADER_LEN {
            return Err("header");
        }
        if &bytes[..8] != MAGIC {
            return Err("magic");
        }
        let field = |i: usize| u32::from_le_bytes(bytes[8 + i * 4..12 + i * 4].try_into().unwrap());
        let (version, flags, width, height, points, segments, body_len) =
            (field(0), field(1), field(2), field(3), field(4), field(5), field(6));
        if version != VERSION {
            return Err("version");
        }
        if flags & !FLAG_GZIP != 0 {
            return Err("flags");
        }
        if (width, height) != page {
            return Err("pageSize");
        }
        if body_len as u64 > MAX_BODY_BYTES {
            return Err("bodyTooLarge");
        }
        if points as u64 * 8 + segments as u64 * 16 != body_len as u64 {
            return Err("counts");
        }
        let body = if flags & FLAG_GZIP != 0 {
            // Exactly one gzip member: a second member or any trailing byte
            // left after the member's trailer is a `counts` rejection.
            let mut body = Vec::new();
            let mut decoder = flate2::bufread::GzDecoder::new(&bytes[HEADER_LEN..]);
            (&mut decoder)
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|_| "counts")?;
            if !decoder.into_inner().is_empty() {
                return Err("counts");
            }
            body
        } else {
            bytes[HEADER_LEN..].to_vec()
        };
        if body.len() != body_len as usize {
            return Err("counts");
        }
        let floats: Vec<f32> = body.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
        if floats.iter().any(|v| !v.is_finite()) {
            return Err("nonFinite");
        }
        let inside = |x: f32, y: f32| x >= -1.0 && x <= width as f32 + 1.0 && y >= -1.0 && y <= height as f32 + 1.0;
        let (point_values, segment_values) = floats.split_at(points as usize * 2);
        let geometry = SnapGeometry {
            points: point_values.as_chunks::<2>().0.to_vec(),
            segments: segment_values.as_chunks::<4>().0.to_vec(),
        };
        let all_inside = geometry.points.iter().all(|p| inside(p[0], p[1]))
            && geometry.segments.iter().all(|s| inside(s[0], s[1]) && inside(s[2], s[3]));
        if !all_inside {
            return Err("outsidePage");
        }
        Ok(geometry)
    }

    fn page_of(case: &serde_json::Value) -> (u32, u32) {
        (case["pdfPage"]["widthPt"].as_u64().unwrap() as u32, case["pdfPage"]["heightPt"].as_u64().unwrap() as u32)
    }

    #[test]
    fn the_reader_follows_every_snap_vector() {
        let cases = vectors::json("snap/cases.json");
        for case in cases["valid"].as_array().unwrap() {
            let geometry = read(&vectors::bytes(case["path"].as_str().unwrap()), page_of(case))
                .unwrap_or_else(|rule| panic!("{}: {rule}", case["name"]));
            assert_eq!(geometry.points.len(), case["expect"]["points"].as_array().unwrap().len());
        }
        for case in cases["invalid"].as_array().unwrap() {
            let result = read(&vectors::bytes(case["path"].as_str().unwrap()), page_of(case));
            assert!(result.is_err(), "{} was accepted", case["name"]);
        }
    }

    pub(crate) fn has_point(geometry: &SnapGeometry, (x, y): (f64, f64)) -> bool {
        geometry.points.iter().any(|p| (p[0] as f64 - x).abs() < 1e-3 && (p[1] as f64 - y).abs() < 1e-3)
    }

    pub(crate) fn on_segments(geometry: &SnapGeometry, (x, y): (f64, f64)) -> bool {
        geometry.segments.iter().any(|s| {
            [[s[0], s[1]], [s[2], s[3]]].iter().any(|p| (p[0] as f64 - x).abs() < 1e-3 && (p[1] as f64 - y).abs() < 1e-3)
        })
    }

    #[test]
    fn written_snaps_are_accepted_deterministic_and_on_the_geometry() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let publication = crate::app::secureplan::publish::prepare_model(&scene, transform).unwrap();
        let bytes = crate::app::secureplan::publish::page_snap(&publication);
        assert_eq!(bytes, crate::app::secureplan::publish::page_snap(&publication), "deterministic");
        assert_eq!(&bytes[36 + 4..36 + 8], &[0, 0, 0, 0], "gzip MTIME is 0");
        let geometry = read(&bytes, (10_000, 6_000)).expect("the reader accepts the writer's output");
        let at = |x: f64, y: f64| transform.apply(x, y);
        // Line ends, arc ends, room corners and the legacy POLYLINE's vertices.
        for point in [at(12345.6, 7890.1), at(26000.0, 5000.0), at(25000.0, 6000.0), at(8000.0, 16000.0), at(10000.0, 2000.0), at(12000.0, 2000.0), at(12000.0, 3000.0)] {
            assert!(has_point(&geometry, point), "missing snap point {point:?}");
        }
        // The circle contributes segments but no end points.
        let (cx, cy) = at(5000.0, 5000.0);
        assert!(!geometry.points.iter().any(|p| ((p[0] as f64 - cx).hypot(p[1] as f64 - cy) - 250.0).abs() < 0.01));
        // The POLYLINE's bulge is a half circle of radius 1000 below the chord,
        // within the chord tolerance.
        let (bx, by) = at(11000.0, 2000.0);
        let tolerance_pt = transform.placement.chord_tolerance_mm / transform.placement.mm_per_pt;
        let bulge: Vec<&[f32; 4]> = geometry
            .segments
            .iter()
            .filter(|s| {
                [[s[0], s[1]], [s[2], s[3]]].iter().all(|p| ((p[0] as f64 - bx).hypot(p[1] as f64 - by) - 1000.0 / 3.0).abs() < 0.01)
            })
            .collect();
        assert!(bulge.len() > 8, "bulge not tessellated: {}", bulge.len());
        for s in bulge {
            let mid = ((s[0] + s[2]) as f64 / 2.0, (s[1] + s[3]) as f64 / 2.0);
            assert!(1000.0 / 3.0 - (mid.0 - bx).hypot(mid.1 - by) <= tolerance_pt + 1e-3);
            // A positive bulge from left to right turns counter-clockwise: below the chord.
            assert!(mid.1 <= by, "the bulge runs on the wrong side of its chord");
        }
    }

    pub(crate) fn scene_of(doc: CadDocument) -> crate::scene::Scene {
        let mut scene = crate::scene::Scene::new();
        scene.document = doc;
        scene.rebuild_derived_caches();
        scene
    }

    fn unit_transform() -> PageTransform {
        use crate::app::secureplan::publish::{place_page, Mapping};
        let mapping = Mapping { cad_origin: [-300.0, 200.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        PageTransform { mapping, placement: place_page([-300.0, -100.0, 300.0, 200.0], &mapping, 1.0).unwrap() }
    }

    fn line(x0: f64, y0: f64, x1: f64, y1: f64) -> EntityType {
        EntityType::Line(acadrust::entities::Line::from_points(Vector3::new(x0, y0, 0.0), Vector3::new(x1, y1, 0.0)))
    }

    pub(crate) fn block(doc: &mut CadDocument, name: &str, members: Vec<EntityType>) {
        let mut record = acadrust::tables::BlockRecord::new(name);
        record.handle = doc.allocate_handle();
        let owner = record.handle;
        doc.block_records.add(record).unwrap();
        for mut member in members {
            member.common_mut().owner_handle = owner;
            doc.add_entity(member).unwrap();
        }
    }

    #[test]
    fn negative_z_normals_are_mirrored_like_the_renderer_draws_them() {
        use acadrust::entities::{Arc, Circle, Insert};
        let down = Vector3::new(0.0, 0.0, -1.0);
        let mut doc = CadDocument::new();
        let mut circle = Circle::new();
        circle.center = Vector3::new(100.0, 50.0, 0.0);
        circle.radius = 10.0;
        circle.normal = down;
        doc.add_entity(EntityType::Circle(circle)).unwrap();
        let mut arc = Arc::new();
        arc.radius = 100.0;
        arc.start_angle = 0.0;
        arc.end_angle = std::f64::consts::FRAC_PI_2;
        arc.normal = down;
        doc.add_entity(EntityType::Arc(arc)).unwrap();
        block(&mut doc, "MIRRORED", vec![line(0.0, 0.0, 10.0, 0.0)]);
        let mut insert = Insert::new("MIRRORED", Vector3::new(200.0, 0.0, 0.0));
        insert.normal = down;
        doc.add_entity(EntityType::Insert(insert)).unwrap();
        let scene = scene_of(doc);
        let transform = unit_transform();
        let (pdf, publication) = crate::app::secureplan::publish::tests::publish(&scene, transform);
        let geometry = extract(&publication);
        let at = |x: f64, y: f64| transform.apply(x, y);
        // OCS (x, y) with a -Z normal is WCS (-x, y).
        let (cx, cy) = at(-100.0, 50.0);
        let circle: Vec<_> = geometry.segments.iter().filter(|s| ((s[0] as f64 - cx).hypot(s[1] as f64 - cy) - 10.0).abs() < 0.01).collect();
        assert!(circle.len() > 8, "the circle is not at its mirrored centre");
        assert!(has_point(&geometry, at(-100.0, 0.0)) && has_point(&geometry, at(0.0, 100.0)), "arc ends not mirrored");
        assert!(has_point(&geometry, at(-200.0, 0.0)) && has_point(&geometry, at(-210.0, 0.0)), "insert not mirrored");
        // The PDF draws the same mirrored geometry.
        let drawn = crate::app::secureplan::publish::tests::content_points(&pdf.bytes);
        for point in [at(-200.0, 0.0), at(-210.0, 0.0), at(-100.0, 0.0), at(0.0, 100.0)] {
            assert!(drawn.iter().any(|p| (p[0] - point.0).abs() < 1e-3 && (p[1] - point.1).abs() < 1e-3), "PDF misses {point:?}");
        }
    }

    #[test]
    fn invisible_members_and_non_plotting_layers_contribute_no_snaps() {
        use acadrust::entities::Insert;
        let mut doc = CadDocument::new();
        let mut hidden = line(0.0, 0.0, 0.0, 50.0);
        hidden.common_mut().invisible = true;
        block(&mut doc, "PARTLY_HIDDEN", vec![line(0.0, 0.0, 50.0, 0.0), hidden]);
        doc.add_entity(EntityType::Insert(Insert::new("PARTLY_HIDDEN", Vector3::new(0.0, 0.0, 0.0)))).unwrap();
        let mut no_plot = acadrust::tables::Layer::new("NO_PLOT");
        no_plot.is_plottable = false;
        doc.layers.add(no_plot).unwrap();
        let mut guide = line(-100.0, 100.0, 100.0, 100.0);
        guide.common_mut().layer = "NO_PLOT".into();
        doc.add_entity(guide).unwrap();
        let transform = unit_transform();
        let publication = crate::app::secureplan::publish::prepare_model(&scene_of(doc), transform).unwrap();
        let geometry = extract(&publication);
        let at = |x: f64, y: f64| transform.apply(x, y);
        assert!(on_segments(&geometry, at(50.0, 0.0)), "the visible member is missing");
        assert!(!on_segments(&geometry, at(0.0, 50.0)), "an invisible block member was included");
        assert!(!on_segments(&geometry, at(100.0, 100.0)), "a non-plotting layer was included");
        assert_eq!(geometry.segments.len(), 1);
    }

    #[test]
    fn segments_are_clipped_to_the_page_and_hidden_layers_are_skipped() {
        let mut doc = CadDocument::new();
        doc.add_entity(line(-9000.0, 9000.0, 39000.0, 9000.0)).unwrap();
        let mut hidden = acadrust::tables::Layer::new("HIDDEN");
        hidden.flags.off = true;
        doc.layers.add(hidden).unwrap();
        let mut off = line(0.0, 0.0, 30000.0, 18000.0);
        off.common_mut().layer = "HIDDEN".into();
        doc.add_entity(off).unwrap();
        let transform = empty_survey_transform();
        let publication = crate::app::secureplan::publish::prepare_model(&scene_of(doc), transform).unwrap();
        let geometry = extract(&publication);
        assert_eq!(geometry.segments, vec![[0.0, 3000.0, 10000.0, 3000.0]]);
        assert!(geometry.points.is_empty(), "endpoints outside the page are dropped");
        read(&write(&geometry, 10_000, 6_000), (10_000, 6_000)).unwrap();
    }
}
