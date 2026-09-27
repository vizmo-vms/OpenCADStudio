//! Standard vector symbols for exported SecurePlan devices (EXP-02).
//!
//! Every device is drawn as a static block of its kind, the same shapes the
//! read-only design overlay uses: a camera is a circle with a view wedge along
//! its direction, equipment a square, an asset a diamond. The symbol does not
//! depend on the catalog icon, so a custom raster icon (`customIcon`) gets the
//! same standard symbol, and the export summary says so. Blocks are defined
//! at unit size, facing +X, on layer 0 with colour ByBlock: each INSERT scales
//! them to the symbol size in drawing units and gives them its layer and the
//! catalog colour.

use acadrust::entities::{Arc, Circle, Line, LwPolyline};
use acadrust::types::{Color, Vector2, Vector3};
use acadrust::EntityType;

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
}
