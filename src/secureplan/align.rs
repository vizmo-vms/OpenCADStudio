//! Alignment of the drawing to the survey (PUB-02).
//!
//! The mapping takes CAD model coordinates to survey world millimetres
//! exactly as `secureplan-vectors/placement.json` defines it:
//! `d = ((x − ox)·s, −(y − oy)·s)`, `world = anchor + rot(q, d)`. Scale comes
//! from the drawing's declared units, or from the units or known-length
//! calibration the user chooses for a unitless or ambiguous drawing.
//! Translation is a CAD point and the survey position it lands on; rotation is
//! a quarter turn. On an empty survey the published page is placed at world
//! (0, 0) (CON-03); on a survey with content the page must start inside the
//! canvas (minX ≥ 0 and minY ≥ 0), or Apply is blocked.

use serde_json::{json, Value};

use super::publish::{Mapping, Placement};

/// The `cadUnits` of CON-02.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Units {
    Mm,
    Cm,
    M,
    In,
    Ft,
    /// No declared unit: the scale came from a known-length calibration.
    Unitless,
}

impl Units {
    pub const CHOICES: [Units; 5] = [Units::Mm, Units::Cm, Units::M, Units::In, Units::Ft];

    pub fn as_str(self) -> &'static str {
        match self {
            Units::Mm => "mm",
            Units::Cm => "cm",
            Units::M => "m",
            Units::In => "in",
            Units::Ft => "ft",
            Units::Unitless => "unitless",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "mm" => Units::Mm,
            "cm" => Units::Cm,
            "m" => Units::M,
            "in" => Units::In,
            "ft" => Units::Ft,
            "unitless" => Units::Unitless,
            _ => return None,
        })
    }

    /// Millimetres per drawing unit; `None` for a unitless drawing.
    pub fn mm_per_unit(self) -> Option<f64> {
        Some(match self {
            Units::Mm => 1.0,
            Units::Cm => 10.0,
            Units::M => 1000.0,
            Units::In => 25.4,
            Units::Ft => 304.8,
            Units::Unitless => return None,
        })
    }

    /// The unit a drawing declares in `$INSUNITS`. `None` when it declares
    /// none, or one SecurePlan does not use (miles, microns, …): the user
    /// then chooses the units or calibrates.
    pub fn declared(insunits: i16) -> Option<Self> {
        Some(match insunits {
            1 => Units::In,
            2 => Units::Ft,
            4 => Units::Mm,
            5 => Units::Cm,
            6 => Units::M,
            _ => return None,
        })
    }
}

/// How the drawing sits in the survey: its units and the mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alignment {
    pub units: Units,
    pub mapping: Mapping,
}

impl Alignment {
    /// The stored alignment of a `cadPlan`, reused by later Applies.
    pub fn from_cad_plan(plan: &Value) -> Option<Self> {
        let mapping = &plan["mapping"];
        let pair = |value: &Value| Some([value[0].as_f64()?, value[1].as_f64()?]);
        Some(Self {
            units: Units::parse(plan["cadUnits"].as_str()?)?,
            mapping: Mapping {
                cad_origin: pair(&mapping["cadOrigin"])?,
                anchor_mm: pair(&mapping["anchorMm"])?,
                scale_mm_per_cad_unit: mapping["scaleMmPerCadUnit"].as_f64().filter(|s| *s > 0.0 && s.is_finite())?,
                quarter_turns: u8::try_from(mapping["quarterTurns"].as_u64()?).ok().filter(|q| *q < 4)?,
            },
        })
    }

    pub fn mapping_json(&self) -> Value {
        let m = &self.mapping;
        json!({
            "cadOrigin": m.cad_origin,
            "anchorMm": m.anchor_mm,
            "scaleMmPerCadUnit": m.scale_mm_per_cad_unit,
            "quarterTurns": m.quarter_turns,
        })
    }
}

/// World mm → CAD model coordinates: the inverse of [`Mapping::cad_to_world`].
pub fn world_to_cad(mapping: &Mapping, [x, y]: [f64; 2]) -> [f64; 2] {
    let (rx, ry) = (x - mapping.anchor_mm[0], y - mapping.anchor_mm[1]);
    // Undo rot(q): rot(1) = (−y, x), so its inverse is (y, −x), and so on.
    let (dx, dy) = match mapping.quarter_turns % 4 {
        0 => (rx, ry),
        1 => (ry, -rx),
        2 => (-rx, -ry),
        _ => (-ry, rx),
    };
    let s = mapping.scale_mm_per_cad_unit;
    [mapping.cad_origin[0] + dx / s, mapping.cad_origin[1] - dy / s]
}

/// The corner of `window_cad` that lands on the page's top-left (the world
/// minimum) under `quarter_turns`.
pub fn top_left_corner(window_cad: [f64; 4], quarter_turns: u8) -> [f64; 2] {
    let [x0, y0, x1, y1] = window_cad;
    match quarter_turns % 4 {
        0 => [x0, y1],
        1 => [x0, y0],
        2 => [x1, y0],
        _ => [x1, y1],
    }
}

/// A mapping that puts the top-left of `window_cad` at `anchor_mm`.
pub fn mapping_at(window_cad: [f64; 4], scale: f64, quarter_turns: u8, anchor_mm: [f64; 2]) -> Mapping {
    Mapping { cad_origin: top_left_corner(window_cad, quarter_turns), anchor_mm, scale_mm_per_cad_unit: scale, quarter_turns: quarter_turns % 4 }
}

/// Why a placement cannot be published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementBlock {
    /// minX or minY < 0 on a survey with content (PUB-02).
    OutsideCanvas,
}

impl std::fmt::Display for PlacementBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlacementBlock::OutsideCanvas => f.write_str("Move the plan so it starts inside the canvas."),
        }
    }
}

/// On a survey with content the page must start inside the canvas. A tiny
/// negative from floating point (below a micrometre) counts as zero.
pub fn check_placement(placement: &Placement) -> Result<(), PlacementBlock> {
    let [x, y] = placement.page_world_min;
    if x >= -1e-3 && y >= -1e-3 {
        Ok(())
    } else {
        Err(PlacementBlock::OutsideCanvas)
    }
}

/// Trim `window_cad` so its page starts inside the canvas (world x, y ≥ 0)
/// under `mapping`, when it would otherwise cross the canvas origin. Quarter
/// turns keep the window axis-aligned, so the trimmed world box maps back to
/// a CAD window. A window wholly outside the canvas is left as it is.
pub fn clip_to_canvas(window_cad: [f64; 4], mapping: &Mapping) -> [f64; 4] {
    let [x0, y0, x1, y1] = window_cad;
    let corners = [[x0, y0], [x1, y1]].map(|c| mapping.cad_to_world(c));
    let (wx0, wx1) = (corners[0][0].min(corners[1][0]), corners[0][0].max(corners[1][0]));
    let (wy0, wy1) = (corners[0][1].min(corners[1][1]), corners[0][1].max(corners[1][1]));
    if wx1 <= 0.0 || wy1 <= 0.0 || (wx0 >= 0.0 && wy0 >= 0.0) {
        return window_cad;
    }
    let a = world_to_cad(mapping, [wx0.max(0.0), wy0.max(0.0)]);
    let b = world_to_cad(mapping, [wx1, wy1]);
    [a[0].min(b[0]), a[1].min(b[1]), a[0].max(b[0]), a[1].max(b[1])]
}

/// The scale from a known length: `real_mm` measured over `cad_length`
/// drawing units.
pub fn calibrated_scale(cad_length: f64, real_mm: f64) -> Option<f64> {
    let scale = real_mm / cad_length;
    (cad_length > 0.0 && real_mm > 0.0 && scale.is_finite() && scale > 0.0 && scale <= 1e6).then_some(scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::publish::{place_page, tests::mapping};
    use crate::app::secureplan::vectors;

    #[test]
    fn the_inverse_mapping_undoes_the_vectors_mapping() {
        for case in vectors::json("placement.json")["cases"].as_array().unwrap() {
            let mapping = mapping(&case["mapping"]);
            for point in case["points"].as_array().unwrap() {
                let cad = [point["cad"][0].as_f64().unwrap(), point["cad"][1].as_f64().unwrap()];
                let world = [point["worldMm"][0].as_f64().unwrap(), point["worldMm"][1].as_f64().unwrap()];
                let back = world_to_cad(&mapping, world);
                assert!((back[0] - cad[0]).abs() < 1e-6 && (back[1] - cad[1]).abs() < 1e-6, "{}: {back:?} ≠ {cad:?}", case["name"]);
            }
        }
    }

    #[test]
    fn a_mapping_anchored_at_the_origin_reproduces_the_vectors() {
        // Each vector anchors its window's top-left corner; building the
        // mapping from the window, turn and anchor gives the same mapping.
        for case in vectors::json("placement.json")["cases"].as_array().unwrap() {
            let expected = mapping(&case["mapping"]);
            let window: Vec<f64> = case["windowCad"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let window = [window[0], window[1], window[2], window[3]];
            let min = case["expect"]["pageWorldMin"].as_array().unwrap();
            let anchor = [min[0].as_f64().unwrap(), min[1].as_f64().unwrap()];
            let built = mapping_at(window, expected.scale_mm_per_cad_unit, expected.quarter_turns, anchor);
            let mm_per_pt = case["mmPerPt"].as_f64().unwrap();
            let a = place_page(window, &built, mm_per_pt).unwrap();
            let b = place_page(window, &expected, mm_per_pt).unwrap();
            assert_eq!((a.width_pt, a.height_pt), (b.width_pt, b.height_pt), "{}", case["name"]);
            assert!((a.center_x - b.center_x).abs() < 1e-6 && (a.center_y - b.center_y).abs() < 1e-6, "{}", case["name"]);
            assert_eq!(check_placement(&a).is_ok(), case["expect"]["startsInsideCanvas"].as_bool().unwrap());
        }
    }

    #[test]
    fn a_page_that_starts_outside_the_canvas_is_blocked() {
        let window = [0.0, 0.0, 1000.0, 1000.0];
        for (anchor, allowed) in [([0.0, 0.0], true), ([500.0, 0.0], true), ([-1.0, 0.0], false), ([0.0, -250.0], false)] {
            let placement = place_page(window, &mapping_at(window, 1.0, 0, anchor), 1.0).unwrap();
            let result = check_placement(&placement);
            assert_eq!(result.is_ok(), allowed, "{anchor:?}");
            if let Err(block) = result {
                assert_eq!(block.to_string(), "Move the plan so it starts inside the canvas.");
            }
        }
    }

    #[test]
    fn a_default_window_crossing_the_canvas_origin_is_trimmed() {
        // The stored plan starts at the drawing's top-left corner; a 2% margin
        // around it would start the page before the canvas.
        let mapping = Mapping { cad_origin: [0.0, 18000.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 0 };
        let trimmed = clip_to_canvas([-600.0, -360.0, 30600.0, 18360.0], &mapping);
        assert_eq!(trimmed, [0.0, -360.0, 30600.0, 18000.0]);
        let placement = place_page(trimmed, &mapping, 5.0).unwrap();
        assert!(check_placement(&placement).is_ok());
        // Turned a quarter, the trimmed sides differ, and inside stays as is.
        let turned = Mapping { cad_origin: [0.0, 0.0], anchor_mm: [0.0, 0.0], scale_mm_per_cad_unit: 1.0, quarter_turns: 1 };
        let trimmed = clip_to_canvas([-10.0, -10.0, 100.0, 50.0], &turned);
        assert!(check_placement(&place_page(trimmed, &turned, 1.0).unwrap()).is_ok(), "{trimmed:?}");
        assert_eq!(clip_to_canvas([10.0, 10.0, 20.0, 20.0], &turned), [10.0, 10.0, 20.0, 20.0]);
    }

    #[test]
    fn units_come_from_the_drawing_or_a_calibration() {
        assert_eq!(Units::declared(4), Some(Units::Mm));
        assert_eq!(Units::declared(2).and_then(Units::mm_per_unit), Some(304.8));
        assert_eq!(Units::declared(0), None, "unitless needs a choice");
        assert_eq!(Units::declared(3), None, "miles are not a plan unit");
        assert_eq!(calibrated_scale(250.0, 5000.0), Some(20.0));
        assert_eq!(calibrated_scale(0.0, 5000.0), None);
        assert_eq!(calibrated_scale(1.0, -1.0), None);
    }

    #[test]
    fn a_stored_mapping_is_reused() {
        let sample = vectors::json("messages/valid.json")["samples"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "openSession-edit")
            .unwrap()["message"]["cadPlan"]
            .clone();
        let alignment = Alignment::from_cad_plan(&sample).expect("the stored mapping");
        assert_eq!(alignment.units.as_str(), sample["cadUnits"]);
        let round_trip = json!({ "cadUnits": alignment.units.as_str(), "mapping": alignment.mapping_json() });
        assert_eq!(Alignment::from_cad_plan(&round_trip), Some(alignment));
        assert_eq!(Alignment::from_cad_plan(&Value::Null), None);
    }
}
