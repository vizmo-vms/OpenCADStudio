//! Test utilities: synthetic drawings generated in code, never real plans.
//! DWGs are written through the native writer.

use acadrust::entities::{Arc, Circle, Line, LwPolyline, Viewport};
use acadrust::types::{Handle, Vector2, Vector3};
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

/// The synthetic paper layout's name.
pub const LAYOUT: &str = "Sheet A1";

/// A 1:100 plan viewport, 300 × 180 mm on the sheet, showing the synthetic
/// plan's middle (15000, 9000), twisted by `twist_degrees`.
pub fn plan_viewport(center: (f64, f64), twist_degrees: f64) -> Viewport {
    let mut viewport = Viewport::new();
    viewport.id = 2;
    viewport.center = Vector3::new(center.0, center.1, 0.0);
    viewport.width = 300.0;
    viewport.height = 180.0;
    viewport.view_height = 18000.0;
    viewport.view_target = Vector3::new(15000.0, 9000.0, 0.0);
    viewport.view_direction = Vector3::UNIT_Z;
    viewport.twist_angle = twist_degrees.to_radians();
    viewport.status.is_on = true;
    viewport
}

/// Add the paper layout [`LAYOUT`] to `scene`: an A1 millimetre sheet
/// (841 × 594 mm) with a title-block line along its bottom and `viewports`.
/// Leaves model space current.
pub fn add_layout(scene: &mut crate::scene::Scene, viewports: Vec<Viewport>) -> Vec<Handle> {
    scene.document.add_layout(LAYOUT).expect("add the layout");
    for object in scene.document.objects.values_mut() {
        if let acadrust::objects::ObjectType::Layout(layout) = object {
            if layout.name == LAYOUT {
                layout.paper_width = 841.0;
                layout.paper_height = 594.0;
                layout.plot_paper_units = 1;
            }
        }
    }
    scene.rebuild_derived_caches();
    scene.set_current_layout(LAYOUT.into());
    scene.add_entity(EntityType::Line(Line::from_points(Vector3::new(20.0, 20.0, 0.0), Vector3::new(821.0, 20.0, 0.0))));
    let handles = viewports.into_iter().map(|viewport| scene.add_entity(EntityType::Viewport(viewport))).collect();
    scene.set_current_layout("Model".into());
    scene.rebuild_derived_caches();
    handles
}

/// The synthetic plan with the layout [`LAYOUT`], whose one viewport shows
/// it at 1:100, as DXF (AutoCAD 2018) bytes.
pub fn synthetic_layout_dxf() -> Vec<u8> {
    let mut scene = crate::scene::Scene::new();
    scene.document = synthetic_document();
    add_layout(&mut scene, vec![plan_viewport((420.0, 300.0), 0.0)]);
    crate::io::save_to_bytes(&scene.document, "dxf", DxfVersion::AC1032).expect("write the synthetic layout DXF")
}
