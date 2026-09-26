//! Test utilities: synthetic drawings generated in code, never real plans.
//! DWGs are written through the native writer.

use acadrust::entities::{Arc, Circle, Line, LwPolyline};
use acadrust::types::{Vector2, Vector3};
use acadrust::{CadDocument, DxfVersion, EntityType};

/// A synthetic 30 × 18 m millimetre floor plan: an outline, a room, a
/// column (circle), a door swing (arc) and a known diagonal line.
pub fn synthetic_document() -> CadDocument {
    let mut doc = CadDocument::new();
    doc.header.insertion_units = 4; // millimetres
    let line = |x0: f64, y0: f64, x1: f64, y1: f64| EntityType::Line(Line::from_points(Vector3::new(x0, y0, 0.0), Vector3::new(x1, y1, 0.0)));
    for entity in [
        line(0.0, 0.0, 30000.0, 0.0),
        line(30000.0, 0.0, 30000.0, 18000.0),
        line(30000.0, 18000.0, 0.0, 18000.0),
        line(0.0, 18000.0, 0.0, 0.0),
        line(12345.6, 7890.1, 20000.0, 10000.0),
    ] {
        doc.add_entity(entity).expect("add a line");
    }
    let mut room = LwPolyline::from_points(vec![
        Vector2::new(2000.0, 12000.0),
        Vector2::new(8000.0, 12000.0),
        Vector2::new(8000.0, 16000.0),
        Vector2::new(2000.0, 16000.0),
    ]);
    room.is_closed = true;
    doc.add_entity(EntityType::LwPolyline(room)).expect("add a room");
    let mut column = Circle::new();
    column.center = Vector3::new(5000.0, 5000.0, 0.0);
    column.radius = 300.0;
    doc.add_entity(EntityType::Circle(column)).expect("add a column");
    let mut swing = Arc::new();
    swing.center = Vector3::new(25000.0, 5000.0, 0.0);
    swing.radius = 900.0;
    swing.start_angle = 0.0;
    swing.end_angle = std::f64::consts::FRAC_PI_2;
    doc.add_entity(EntityType::Arc(swing)).expect("add a door swing");
    doc
}

/// The synthetic plan as DXF (AutoCAD 2018) bytes.
pub fn synthetic_dxf() -> Vec<u8> {
    crate::io::save_to_bytes(&synthetic_document(), "dxf", DxfVersion::AC1032).expect("write the synthetic DXF")
}

/// The synthetic plan as DWG (AutoCAD 2018) bytes, from the native writer.
pub fn synthetic_dwg() -> Vec<u8> {
    crate::io::save_to_bytes(&synthetic_document(), "dwg", DxfVersion::AC1032).expect("write the synthetic DWG")
}
