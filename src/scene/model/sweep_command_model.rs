//! SWEEP command data mapped to the shared kernel history reconstruction.

use codec::entities::{Surface, SurfaceData, SurfaceKind, SurfaceSweepOptions};
use codec::objects::{SolidHistoryNodeBase, SolidHistorySweep};
use codec::types::Vector3;
use codec::EntityType;
use kernel::brep::Body;

use crate::command::{ExtrudeMode, SweepOptions};
use super::sweep_model::{embedded_path, embedded_revolve_profile};

fn embedded_sweep_profile(entity: &EntityType) -> Option<(codec::entities::EmbeddedEntity, [f64; 16])> {
    if let EntityType::Region(region) = entity {
        Some((codec::entities::EmbeddedEntity::Region(region.clone()), glam::DMat4::IDENTITY.to_cols_array()))
    } else {
        embedded_revolve_profile(entity)
    }
}

pub fn is_sweep_profile(entity: &EntityType) -> bool {
    embedded_sweep_profile(entity).is_some_and(|(profile, transform)| {
        kernel::acis::sweep_profile_geometry(&profile, transform).is_ok()
    })
}

pub fn is_sweep_path(entity: &EntityType) -> bool {
    match embedded_path(entity) {
        Some(codec::entities::EmbeddedEntity::Spline(value)) => {
            value.degree > 0 && (value.control_points.len() > value.degree as usize
                || value.fit_points.len() >= 2)
        }
        Some(_) => crate::entities::curve::entity_curve(entity)
            .is_some_and(|curve| curve.curve.length().is_finite() && curve.curve.length() > 1e-9),
        None => false,
    }
}

/// All selected profiles use one base point, preserving their relative offsets.
pub fn sweep_selection_options(profiles: &[EntityType], mut options: SweepOptions) -> Option<SweepOptions> {
    if options.base_point.is_some() {
        return Some(options);
    }
    let geometry = profiles.iter().map(|profile| {
        let (entity, transform) = embedded_sweep_profile(profile)?;
        let (plane, wires, _) = kernel::acis::sweep_profile_geometry(&entity, transform).ok()?;
        Some((plane, wires))
    }).collect::<Option<Vec<_>>>()?;
    options.base_point = Some(glam::DVec3::from_array(
        kernel::brep::sweep_profile_group_base(&geometry)?,
    ));
    Some(options)
}

pub fn sweep_record(profile: &EntityType, path: &EntityType, options: SweepOptions) -> Option<SolidHistorySweep> {
    let (sweep_entity, sweep_entity_transform) = embedded_sweep_profile(profile)?;
    let (plane, wires, _) = kernel::acis::sweep_profile_geometry(&sweep_entity, sweep_entity_transform).ok()?;
    let base_point = match options.base_point {
        Some(point) => point.to_array(),
        None => kernel::brep::sweep_profile_base(plane, &wires)?,
    };
    let mut base = SolidHistoryNodeBase::new(1);
    base.transform = glam::DMat4::IDENTITY.to_cols_array();
    let record = SolidHistorySweep {
        base,
        operation_major: 1,
        sweep_entity: Some(sweep_entity),
        path_entity: Some(embedded_path(path)?),
        scale_factor: options.scale,
        twist_angle: options.twist_angle,
        align_option: u8::from(options.align),
        has_align_start: true,
        bank: options.bank,
        sweep_entity_transform,
        path_entity_transform: glam::DMat4::IDENTITY.to_cols_array(),
        reference_point: Vector3::new(base_point[0], base_point[1], base_point[2]),
        ..SolidHistorySweep::default()
    };
    placed_sweep_record(profile, record)
}

/// The record the reference reads: the profile stored already placed at the
/// path start (base point, alignment and profile rotation applied) with flag
/// 295 set, and no reference point.
fn placed_sweep_record(profile: &EntityType, mut record: SolidHistorySweep) -> Option<SolidHistorySweep> {
    let (placed, _) = kernel::acis::sweep_history_placements(&record).ok()?;
    let embedded_to_world = glam::DMat4::from_cols(
        glam::DVec4::new(placed.x_axis[0], placed.x_axis[1], placed.x_axis[2], 0.0),
        glam::DVec4::new(placed.y_axis[0], placed.y_axis[1], placed.y_axis[2], 0.0),
        glam::DVec4::new(placed.z_axis[0], placed.z_axis[1], placed.z_axis[2], 0.0),
        glam::DVec4::new(placed.origin[0], placed.origin[1], placed.origin[2], 1.0),
    );
    // Source world geometry -> placed world geometry.
    let map = embedded_to_world * glam::DMat4::from_cols_array(&record.sweep_entity_transform).inverse();
    let m = map.to_cols_array_2d();
    let transform = codec::types::Transform::from_matrix(codec::types::Matrix4 {
        m: [
            [m[0][0], m[1][0], m[2][0], m[3][0]],
            [m[0][1], m[1][1], m[2][1], m[3][1]],
            [m[0][2], m[1][2], m[2][2], m[3][2]],
            [0.0, 0.0, 0.0, 1.0],
        ],
    });
    let mut moved = profile.clone();
    crate::scene::view::dispatch::apply_transform(&mut moved, &crate::command::EntityTransform::Affine(transform));
    let (sweep_entity, sweep_entity_transform) = embedded_sweep_profile(&moved)?;
    record.sweep_entity = Some(sweep_entity);
    record.sweep_entity_transform = sweep_entity_transform;
    record.flags_294_296 = [false, true, true];
    record.reference_point = Vector3::new(0.0, 0.0, 0.0);
    Some(record)
}

pub fn swept_with_options(profile: &EntityType, path: &EntityType, mode: ExtrudeMode, options: SweepOptions) -> Option<Body> {
    let record = sweep_record(profile, path, options)?;
    kernel::acis::rebuild_sweep_with_mode(&record, mode == ExtrudeMode::Surface).ok()
}

/// Preserve native construction parameters alongside the sheet's saved B-rep.
pub fn swept_surface_entity(record: &SolidHistorySweep) -> EntityType {
    let mut surface = Surface::new(SurfaceKind::Swept);
    if let Ok(point) = kernel::acis::sweep_history_reference_point(record) {
        surface.point_of_reference = Vector3::new(point[0], point[1], point[2]);
    }
    surface.surface_data = SurfaceData::Swept {
        class_version: 0,
        sweep_entity: record.sweep_entity.clone(),
        path_entity: record.path_entity.clone(),
        sweep_transform: glam::DMat4::IDENTITY.to_cols_array(),
        path_transform: glam::DMat4::IDENTITY.to_cols_array(),
        options: SurfaceSweepOptions {
            draft_angle: record.draft_angle,
            twist_angle: record.twist_angle,
            scale_factor: record.scale_factor,
            align_angle: record.align_angle,
            sweep_entity_transform: record.sweep_entity_transform,
            path_entity_transform: record.path_entity_transform,
            sweep_alignment_flags: record.align_option as i16,
            align_start: record.has_align_start,
            bank: record.bank,
            base_point_set: true,
            reference_vector: record.reference_point,
            ..SurfaceSweepOptions::default()
        },
    };
    EntityType::Surface(Box::new(surface))
}
