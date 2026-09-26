//! SPSNAP v1 (CON-05): the binary snap sidecar for a published page.
//!
//! Layout (little-endian): magic `SPSNAP01`, `u32 version = 1`, `u32 flags`
//! (bit 0: the body is gzip, written with MTIME 0), `u32 pageWidthPt`,
//! `u32 pageHeightPt`, `u32 pointCount`, `u32 segmentCount`,
//! `u32 uncompressedBodyBytes`; then `f32[2·pointCount]` endpoints and vertices
//! and `f32[4·segmentCount]` segments, in CON-01 page points.
//!
//! Included: model geometry visible in the published view — lines,
//! lightweight polylines (bulges tessellated), arcs and circles, and the same
//! inside block references. Excluded: text, mtext, dimensions, hatches,
//! images and paper space. Curves are tessellated within the placement's
//! chord tolerance; everything is clipped to the page.

use std::io::Write;

use acadrust::{CadDocument, EntityType};

use super::publish::PageTransform;

pub const MAGIC: &[u8; 8] = b"SPSNAP01";
pub const VERSION: u32 = 1;
pub const FLAG_GZIP: u32 = 1;
pub const HEADER_LEN: usize = 36;
pub const MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;
/// Block nesting beyond this is not followed (DSK-01 depth limit).
const MAX_BLOCK_DEPTH: usize = 32;

/// Snap geometry in page points.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SnapGeometry {
    pub points: Vec<[f32; 2]>,
    pub segments: Vec<[f32; 4]>,
}

/// A 2D affine map in CAD units: `(a·x + c·y + e, b·x + d·y + f)`.
#[derive(Debug, Clone, Copy)]
struct Affine([f64; 6]);

impl Affine {
    const IDENTITY: Affine = Affine([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    /// `self ∘ other`: apply `other` first.
    fn then_from(&self, other: &Affine) -> Affine {
        let [a, b, c, d, e, f] = self.0;
        let [a2, b2, c2, d2, e2, f2] = other.0;
        Affine([
            a * a2 + c * b2,
            b * a2 + d * b2,
            a * c2 + c * d2,
            b * c2 + d * d2,
            a * e2 + c * f2 + e,
            b * e2 + d * f2 + f,
        ])
    }

    /// The largest linear scale, for tessellating curves.
    fn max_scale(&self) -> f64 {
        let [a, b, c, d, _, _] = self.0;
        (a * a + b * b).sqrt().max((c * c + d * d).sqrt())
    }
}

struct Collector<'a> {
    doc: &'a CadDocument,
    page: &'a PageTransform,
    width: f64,
    height: f64,
    /// Chord tolerance in world mm.
    chord_mm: f64,
    out: SnapGeometry,
}

impl Collector<'_> {
    fn visible(&self, entity: &EntityType) -> bool {
        let layer = &entity.common().layer;
        self.doc.layers.get(layer).is_none_or(|layer| !layer.is_off() && !layer.is_frozen())
    }

    fn to_page(&self, affine: &Affine, x: f64, y: f64) -> (f64, f64) {
        let (cx, cy) = affine.apply(x, y);
        self.page.apply(cx, cy)
    }

    fn point(&mut self, affine: &Affine, x: f64, y: f64) {
        let (px, py) = self.to_page(affine, x, y);
        if (0.0..=self.width).contains(&px) && (0.0..=self.height).contains(&py) {
            self.out.points.push([px as f32, py as f32]);
        }
    }

    /// Add a segment in page points, clipped to the page (Liang–Barsky).
    fn page_segment(&mut self, (x0, y0): (f64, f64), (x1, y1): (f64, f64)) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
        for (p, q) in [(-dx, x0), (dx, self.width - x0), (-dy, y0), (dy, self.height - y0)] {
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

    fn segment(&mut self, affine: &Affine, (x0, y0): (f64, f64), (x1, y1): (f64, f64)) {
        let a = self.to_page(affine, x0, y0);
        let b = self.to_page(affine, x1, y1);
        self.page_segment(a, b);
    }

    /// Tessellate an arc (CAD units, radians, counter-clockwise from `start`
    /// through `sweep`) within the chord tolerance.
    fn arc(&mut self, affine: &Affine, center: (f64, f64), radius: f64, start: f64, sweep: f64) {
        if !(radius.is_finite() && radius > 0.0 && sweep.is_finite()) || sweep == 0.0 {
            return;
        }
        let radius_mm = radius * affine.max_scale() * self.page.mapping.scale_mm_per_cad_unit;
        let step = if self.chord_mm >= radius_mm {
            std::f64::consts::FRAC_PI_2
        } else {
            (2.0 * (1.0 - self.chord_mm / radius_mm).acos()).min(std::f64::consts::FRAC_PI_2)
        };
        let count = ((sweep.abs() / step).ceil() as usize).clamp(1, 1 << 16);
        let at = |i: usize| {
            let angle = start + sweep * i as f64 / count as f64;
            (center.0 + radius * angle.cos(), center.1 + radius * angle.sin())
        };
        for i in 0..count {
            self.segment(affine, at(i), at(i + 1));
        }
    }

    fn entity(&mut self, entity: &EntityType, affine: &Affine, depth: usize) {
        if !self.visible(entity) {
            return;
        }
        match entity {
            EntityType::Line(line) => {
                let (a, b) = ((line.start.x, line.start.y), (line.end.x, line.end.y));
                self.point(affine, a.0, a.1);
                self.point(affine, b.0, b.1);
                self.segment(affine, a, b);
            }
            EntityType::LwPolyline(polyline) => {
                let vertices = &polyline.vertices;
                for vertex in vertices {
                    self.point(affine, vertex.location.x, vertex.location.y);
                }
                let count = if polyline.is_closed { vertices.len() } else { vertices.len().saturating_sub(1) };
                for i in 0..count {
                    let (from, to) = (&vertices[i], &vertices[(i + 1) % vertices.len()]);
                    let (a, b) = ((from.location.x, from.location.y), (to.location.x, to.location.y));
                    if from.bulge.abs() < 1e-12 {
                        self.segment(affine, a, b);
                        continue;
                    }
                    // bulge = tan(θ/4): radius and centre from the chord.
                    let theta = 4.0 * from.bulge.atan();
                    let chord = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
                    if chord == 0.0 {
                        continue;
                    }
                    let radius = chord / (2.0 * (theta / 2.0).sin().abs());
                    let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
                    let sagitta_offset = radius * (theta / 2.0).cos() * theta.signum();
                    let normal = (-(b.1 - a.1) / chord, (b.0 - a.0) / chord);
                    let center = (mid.0 + normal.0 * sagitta_offset, mid.1 + normal.1 * sagitta_offset);
                    let start = (a.1 - center.1).atan2(a.0 - center.0);
                    self.arc(affine, center, radius, start, theta);
                }
            }
            EntityType::Arc(arc) => {
                let mut sweep = arc.end_angle - arc.start_angle;
                if sweep <= 0.0 {
                    sweep += std::f64::consts::TAU;
                }
                let center = (arc.center.x, arc.center.y);
                let at = |angle: f64| (center.0 + arc.radius * angle.cos(), center.1 + arc.radius * angle.sin());
                let (s, e) = (at(arc.start_angle), at(arc.end_angle));
                self.point(affine, s.0, s.1);
                self.point(affine, e.0, e.1);
                self.arc(affine, center, arc.radius, arc.start_angle, sweep);
            }
            EntityType::Circle(circle) => {
                self.arc(affine, (circle.center.x, circle.center.y), circle.radius, 0.0, std::f64::consts::TAU);
            }
            EntityType::Insert(insert) if depth < MAX_BLOCK_DEPTH => {
                let (sin, cos) = insert.rotation.sin_cos();
                let (sx, sy) = (insert.x_scale(), insert.y_scale());
                let columns = insert.column_count.max(1);
                let rows = insert.row_count.max(1);
                for row in 0..rows {
                    for column in 0..columns {
                        let (ox, oy) = (column as f64 * insert.column_spacing, row as f64 * insert.row_spacing);
                        // Block space → rotated, scaled, placed at the insertion point
                        // (array offsets are along the rotated axes).
                        let local = Affine([
                            cos * sx,
                            sin * sx,
                            -sin * sy,
                            cos * sy,
                            insert.insert_point.x + cos * ox - sin * oy,
                            insert.insert_point.y + sin * ox + cos * oy,
                        ]);
                        let combined = affine.then_from(&local);
                        let block: Vec<&EntityType> = self.doc.entities_in_block(&insert.block_name).collect();
                        for child in block {
                            self.entity(child, &combined, depth + 1);
                        }
                    }
                }
            }
            // Text, mtext, dimensions, hatches, images and everything else
            // contribute no snaps (CON-05).
            _ => {}
        }
    }
}

/// Snap geometry for the model-space view of `doc` on the page `transform`.
pub fn extract_model(doc: &CadDocument, transform: &PageTransform) -> SnapGeometry {
    let mut collector = Collector {
        doc,
        page: transform,
        width: transform.placement.width_pt as f64,
        height: transform.placement.height_pt as f64,
        chord_mm: transform.placement.chord_tolerance_mm,
        out: SnapGeometry::default(),
    };
    for entity in doc.model_space_entities() {
        collector.entity(entity, &Affine::IDENTITY, 0);
    }
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
            let mut body = Vec::new();
            flate2::read::GzDecoder::new(&bytes[HEADER_LEN..])
                .take(MAX_BODY_BYTES + 1)
                .read_to_end(&mut body)
                .map_err(|_| "counts")?;
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

    #[test]
    fn written_snaps_are_accepted_deterministic_and_on_the_geometry() {
        let scene = synthetic_dxf_scene();
        let transform = empty_survey_transform();
        let bytes = crate::app::secureplan::publish::model_snap(&scene.document, transform);
        assert_eq!(bytes, crate::app::secureplan::publish::model_snap(&scene.document, transform), "deterministic");
        assert_eq!(&bytes[36 + 4..36 + 8], &[0, 0, 0, 0], "gzip MTIME is 0");
        let geometry = read(&bytes, (10_000, 6_000)).expect("the reader accepts the writer's output");
        // Five lines (10 endpoints), the arc's two ends and four room vertices.
        assert_eq!(geometry.points.len(), 16);
        let (x, y) = transform.apply(12345.6, 7890.1);
        assert!(geometry.points.iter().any(|p| (p[0] as f64 - x).abs() < 1e-3 && (p[1] as f64 - y).abs() < 1e-3));
        // The circle (r = 750 mm, 250 pt) is tessellated within the chord tolerance.
        let circle_segments = geometry
            .segments
            .iter()
            .filter(|s| {
                let (cx, cy) = transform.apply(5000.0, 5000.0);
                ((s[0] as f64 - cx).hypot(s[1] as f64 - cy) - 250.0).abs() < 0.01
            })
            .count();
        let tolerance_pt = transform.placement.chord_tolerance_mm / transform.placement.mm_per_pt;
        let expected = (std::f64::consts::TAU / (2.0 * (1.0 - tolerance_pt / 250.0).acos())).ceil() as usize;
        assert_eq!(circle_segments, expected);
    }

    #[test]
    fn segments_are_clipped_to_the_page_and_hidden_layers_are_skipped() {
        let mut doc = CadDocument::new();
        let line = |x0: f64, y0: f64, x1: f64, y1: f64| {
            EntityType::Line(acadrust::entities::Line::from_points(
                acadrust::types::Vector3::new(x0, y0, 0.0),
                acadrust::types::Vector3::new(x1, y1, 0.0),
            ))
        };
        doc.add_entity(line(-9000.0, 9000.0, 39000.0, 9000.0)).unwrap();
        let mut hidden = acadrust::tables::Layer::new("HIDDEN");
        hidden.flags.off = true;
        doc.layers.add(hidden).unwrap();
        let mut off = line(0.0, 0.0, 30000.0, 18000.0);
        off.common_mut().layer = "HIDDEN".into();
        doc.add_entity(off).unwrap();
        let transform = empty_survey_transform();
        let geometry = extract_model(&doc, &transform);
        assert_eq!(geometry.segments, vec![[0.0, 3000.0, 10000.0, 3000.0]]);
        assert!(geometry.points.is_empty(), "endpoints outside the page are dropped");
        read(&write(&geometry, 10_000, 6_000), (10_000, 6_000)).unwrap();
    }
}
