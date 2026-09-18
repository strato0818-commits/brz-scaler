use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use brdb::{
    assets::LiteralComponent,
    schema::{BrdbSchemaGlobalData, BrdbStruct},
    BrFsReader, Brick, BrickSize, BrickType, Brz, ComponentChunkSoA, Direction, IntoReader,
    Position, PrefabJson, PrefabPivot, Quat4f, Rotation, UnsavedGrid, Vector3f,
};

const PASTE_CORRECTION: Position = Position {
    x: -1000,
    y: -1000,
    z: -1024,
};
const MAX_BRICK_HALF_EXTENT: u16 = 4000;
const MAX_SPLIT_PIECES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ScaleStats {
    pub grids: usize,
    pub input_bricks: usize,
    pub output_bricks: usize,
    pub preserved_entities: usize,
    pub skipped_basic_bricks: usize,
    pub dropped_components: usize,
    pub dropped_wires: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ScaleOptions {
    pub factors: [f64; 3],
}

impl ScaleOptions {
    pub fn new(factor: f64) -> Self {
        Self {
            factors: [factor; 3],
        }
    }
}

#[derive(Clone)]
struct JointData {
    entity: u32,
    offset: Vector3f,
    rotation: Quat4f,
}

struct RetainedBrick {
    brick: Brick,
    joint: Option<JointData>,
}

struct EntityLocationPatch {
    entity: u32,
    old: Vector3f,
    new: Vector3f,
}

struct EntityChunkPatch {
    chunk: brdb::ChunkIndex,
    bytes: Vec<u8>,
    locations: Vec<EntityLocationPatch>,
}

struct JointEdge {
    parent: u32,
    child: u32,
    correction: [f64; 3],
}

/// Scale every procedural brick in a BRZ archive and omit basic bricks, except
/// structural joint hosts needed to keep entity grids connected.
///
/// Brickadia stores procedural brick sizes as half-extents, in the same integer
/// coordinate units used by brick positions. Main-grid bricks are normalized
/// individually around the retained build's X/Y center and Z bottom, then
/// receive Brickadia's raw prefab-coordinate correction. Prefab bounds are
/// rebuilt from the centered, bottom-flat result. Components and joint metadata
/// are remapped to the repacked brick indices; wires are currently omitted.
pub fn scale_brz(input: &Path, output: &Path, factor: f64) -> Result<ScaleStats, String> {
    scale_brz_with_options(input, output, ScaleOptions::new(factor))
}

pub fn scale_brz_with_options(
    input: &Path,
    output: &Path,
    options: ScaleOptions,
) -> Result<ScaleStats, String> {
    let factors = options.factors;
    validate_factors(factors)?;
    if same_path(input, output) {
        return Err("input and output paths must be different".into());
    }

    let archive =
        Brz::open(input).map_err(|error| format!("failed to open {}: {error}", input.display()))?;
    let reader = (&archive).into_reader();
    let global = reader
        .global_data()
        .map_err(|error| format!("failed to read global brick data: {error}"))?;
    let component_schema = reader
        .components_schema()
        .map_err(|error| format!("failed to read component schema: {error}"))?;
    let mut prefab = reader
        .prefab_json()
        .map_err(|error| format!("failed to read prefab metadata: {error}"))?;
    let main_offset = prefab
        .as_ref()
        .map(|prefab| {
            Position::new(
                prefab.added_global_grid_offset.x,
                prefab.added_global_grid_offset.y,
                prefab.added_global_grid_offset.z,
            )
        })
        .unwrap_or(Position::ZERO);

    let entity_chunks = reader
        .entity_chunk_index()
        .map_err(|error| format!("failed to read entity index: {error}"))?;
    let has_main_grid = reader
        .find_file_by_path("World/0/Bricks/Grids/1/ChunkIndex.mps")
        .map_err(|error| format!("failed to inspect the main brick grid: {error}"))?
        .is_some();
    let mut grid_ids = BTreeSet::new();
    if has_main_grid {
        grid_ids.insert(1usize);
    }
    let mut entity_count = 0;
    let mut entity_chunk_patches = Vec::new();
    let mut entity_rotations = HashMap::new();
    for &chunk in &entity_chunks {
        let entity_path = format!("World/0/Entities/Chunks/{chunk}.mps");
        let found = reader
            .find_file_by_path(&entity_path)
            .map_err(|error| format!("failed to locate entity chunk: {error}"))?
            .ok_or_else(|| format!("missing entity chunk {chunk}"))?;
        let bytes = reader
            .find_blob(found.blob_id)
            .map_err(|error| format!("failed to locate entity chunk data: {error}"))?
            .read()
            .map_err(|error| format!("failed to read entity chunk data: {error}"))?;
        let mut locations = Vec::new();
        for entity in reader
            .entity_chunk(chunk)
            .map_err(|error| format!("failed to read entity chunk: {error}"))?
        {
            entity_count += 1;
            if entity.is_brick_grid() {
                if let Some(id) = entity.id {
                    grid_ids.insert(id);
                    entity_rotations.insert(id as u32, entity.rotation);
                }
                locations.push(EntityLocationPatch {
                    entity: entity.id.unwrap_or_default() as u32,
                    old: entity.location,
                    new: scale_vector(entity.location, factors),
                });
            }
        }
        entity_chunk_patches.push(EntityChunkPatch {
            chunk,
            bytes,
            locations,
        });
    }

    let mut replacements = Vec::with_capacity(grid_ids.len());
    let mut joint_edges = Vec::new();
    let mut stats = ScaleStats {
        preserved_entities: entity_count,
        ..Default::default()
    };
    let mut main_intended_bounds = None;
    for grid_id in grid_ids {
        let chunks = reader
            .brick_chunk_index(grid_id)
            .map_err(|error| format!("failed to read brick grid {grid_id}: {error}"))?;
        let mut scaled_grid = UnsavedGrid::default();
        let mut retained_bricks = Vec::new();
        stats.grids += 1;

        for chunk in chunks {
            stats.dropped_wires += chunk.num_wires as usize;
            let bricks = reader
                .brick_chunk_soa(grid_id, chunk.index)
                .map_err(|error| {
                    format!(
                        "failed to read grid {grid_id} chunk {}: {error}",
                        chunk.index
                    )
                })?;

            let mut decoded = bricks
                .iter_bricks(chunk.index, global.clone())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("failed to decode a brick in grid {grid_id}: {error}"))?;
            stats.input_bricks += decoded.len();

            let mut joints = HashMap::new();
            if chunk.num_components > 0 {
                let (components, component_data) = reader
                    .component_chunk_soa(grid_id, chunk.index)
                    .map_err(|error| {
                        format!("failed to read components in grid {grid_id}: {error}")
                    })?;
                attach_components(&mut decoded, &components, component_data, global.as_ref())?;
                for (((&brick_index, &entity), &offset), &rotation) in components
                    .joint_brick_indices
                    .iter()
                    .zip(&components.joint_entity_references)
                    .zip(&components.joint_initial_relative_offsets)
                    .zip(&components.joint_initial_relative_rotations)
                {
                    joints.insert(
                        brick_index as usize,
                        JointData {
                            entity,
                            offset,
                            rotation,
                        },
                    );
                }
            }

            for (index, brick) in decoded.into_iter().enumerate() {
                let joint = joints.remove(&index);
                if !matches!(brick.asset, BrickType::Procedural { .. }) && joint.is_none() {
                    stats.dropped_components += brick.components.len();
                    stats.skipped_basic_bricks += 1;
                    continue;
                }
                retained_bricks.push(RetainedBrick { brick, joint });
            }
        }

        let source_bounds = if grid_id == 1 {
            brick_bounds(
                &retained_bricks
                    .iter()
                    .filter(|item| matches!(item.brick.asset, BrickType::Procedural { .. }))
                    .map(|item| item.brick.clone())
                    .collect::<Vec<_>>(),
                main_offset,
            )
        } else {
            None
        };
        if let Some((source_min, source_max)) = source_bounds {
            for patch in &mut entity_chunk_patches {
                for location in &mut patch.locations {
                    location.new =
                        normalize_main_vector(location.old, source_min, source_max, factors);
                }
            }
        }
        for mut retained in retained_bricks {
            let brick = &mut retained.brick;
            let source_position = brick.position;
            if let BrickType::Procedural { size, .. } = &mut brick.asset {
                *size = scale_size_oriented(*size, brick.direction, brick.rotation, factors)?;
            }
            if grid_id == 1 {
                let (source_min, source_max) = source_bounds
                    .ok_or_else(|| "main grid bounds disappeared during scaling".to_string())?;
                let intended = normalize_main_position(
                    source_position + main_offset,
                    source_min,
                    source_max,
                    factors,
                )?;
                brick.position = intended;
                include_bounds(&mut main_intended_bounds, brick.local_bounds());
                brick.position += PASTE_CORRECTION;
            } else {
                brick.position = scale_position_about_paste_baseline(source_position, factors)?;
            }
            for piece in split_oversized_brick(brick)? {
                let (new_chunk, new_index) = scaled_grid
                    .add_brick(global.as_ref(), &piece)
                    .map_err(|error| format!("failed to repack grid {grid_id}: {error}"))?;
                if let Some(joint) = &retained.joint {
                    let automatic = automatic_joint_socket_offset(joint.rotation, factors);
                    let component_chunk = scaled_grid.components.entry(new_chunk).or_default();
                    component_chunk.joint_brick_indices.push(new_index as u32);
                    component_chunk.joint_entity_references.push(joint.entity);
                    component_chunk
                        .joint_initial_relative_offsets
                        .push(offset_vector(
                            scale_vector(joint.offset, factors),
                            automatic,
                        )?);
                    component_chunk
                        .joint_initial_relative_rotations
                        .push(joint.rotation);
                    joint_edges.push(JointEdge {
                        parent: grid_id as u32,
                        child: joint.entity,
                        correction: automatic,
                    });
                }
                stats.output_bricks += 1;
            }
        }

        let pending_grid = scaled_grid
            .to_pending(
                global.proc_brick_starting_index(),
                component_schema.as_ref(),
            )
            .map_err(|error| format!("failed to encode brick grid {grid_id}: {error}"))?;
        replacements.push((grid_id, pending_grid));
    }

    let mut pending = archive
        .to_pending()
        .map_err(|error| format!("failed to unpack input archive: {error}"))?;
    apply_joint_hierarchy_offsets(&mut entity_chunk_patches, &entity_rotations, &joint_edges)?;
    patch_entity_locations(&mut pending, entity_chunk_patches)?;
    for (grid_id, pending_grid) in replacements {
        *pending
            .cd_mut(format!("World/0/Bricks/Grids/{grid_id}"))
            .map_err(|error| format!("failed to replace brick grid {grid_id}: {error}"))? =
            pending_grid;
    }
    use brdb::pending::BrPendingFs::File;
    if let Some(prefab) = &mut prefab {
        rebuild_prefab_metadata(prefab, main_intended_bounds);
        let bytes = serde_json::to_vec(prefab)
            .map_err(|error| format!("failed to encode prefab metadata: {error}"))?;
        *pending
            .cd_mut("Meta/Prefab.json")
            .map_err(|error| format!("failed to replace prefab metadata: {error}"))? =
            File(Some(bytes));
    }

    Brz::write_pending(output, pending)
        .map_err(|error| format!("failed to write {}: {error}", output.display()))?;
    Ok(stats)
}

fn rebuild_prefab_metadata(prefab: &mut PrefabJson, bounds: Option<(Position, Position)>) {
    let pivot = bounds
        .map(|(min, max)| PrefabPivot::from_bounds(min, max))
        .unwrap_or_default();
    prefab.pivots.bottom_studs_pivot = pivot;
    prefab.pivots.studs_expanded_pivot = pivot;
    prefab.pivots.top_studs_pivot = pivot;
    prefab.pivots.bounds_pivot = pivot;
    prefab.added_global_grid_offset = Default::default();
}

fn validate_factors(factors: [f64; 3]) -> Result<(), String> {
    if factors
        .iter()
        .any(|factor| !factor.is_finite() || *factor < 1.0 || factor.fract() != 0.0)
    {
        Err("all scale factors must be whole numbers of 1 or greater".into())
    } else {
        Ok(())
    }
}

fn attach_components(
    bricks: &mut [Brick],
    components: &ComponentChunkSoA,
    component_data: Vec<BrdbStruct>,
    global: &BrdbSchemaGlobalData,
) -> Result<(), String> {
    let mut brick_indices = components.component_brick_indices.iter();
    let mut data = component_data.into_iter();
    for counter in &components.component_type_counters {
        let type_name = global
            .component_type_names
            .get_index(counter.type_index as usize)
            .ok_or_else(|| format!("component type index {} is missing", counter.type_index))?;
        let has_data = global.get_struct_name(type_name).is_some();
        for _ in 0..counter.num_instances {
            let brick_index = *brick_indices
                .next()
                .ok_or_else(|| "component brick index list is truncated".to_string())?
                as usize;
            let brick = bricks
                .get_mut(brick_index)
                .ok_or_else(|| format!("component refers to missing brick {brick_index}"))?;
            let component = if has_data {
                let values = data
                    .next()
                    .ok_or_else(|| format!("component data for {type_name} is truncated"))?
                    .as_hashmap()
                    .map_err(|error| format!("failed to decode {type_name}: {error}"))?
                    .into_iter()
                    .map(|(name, value)| (name.into(), value))
                    .collect();
                LiteralComponent::new_from_data(type_name.clone(), Arc::new(values))
            } else {
                LiteralComponent::new(type_name.clone())
            };
            brick.components.push(Box::new(component));
        }
    }
    Ok(())
}

fn scale_vector(vector: Vector3f, factors: [f64; 3]) -> Vector3f {
    Vector3f {
        x: (f64::from(vector.x) * factors[0]) as f32,
        y: (f64::from(vector.y) * factors[1]) as f32,
        z: (f64::from(vector.z) * factors[2]) as f32,
    }
}

fn offset_vector(vector: Vector3f, offset: [f64; 3]) -> Result<Vector3f, String> {
    if offset
        .iter()
        .any(|value| !value.is_finite() || value.abs() > f64::from(f32::MAX))
    {
        return Err("joint offsets must be finite numbers within the BRZ limit".into());
    }
    Ok(Vector3f {
        x: vector.x + offset[0] as f32,
        y: vector.y + offset[1] as f32,
        z: vector.z + offset[2] as f32,
    })
}

fn automatic_joint_socket_offset(rotation: Quat4f, factors: [f64; 3]) -> [f64; 3] {
    rotate_vector(
        rotation,
        [24.0 * (factors[0] - 1.0), 24.0 * (factors[1] - 1.0), 0.0],
    )
}

fn add_offsets(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn rotate_vector(q: Quat4f, vector: [f64; 3]) -> [f64; 3] {
    let qx = f64::from(q.x);
    let qy = f64::from(q.y);
    let qz = f64::from(q.z);
    let qw = f64::from(q.w);
    let [x, y, z] = vector;
    [
        (1.0 - 2.0 * (qy * qy + qz * qz)) * x
            + 2.0 * (qx * qy - qz * qw) * y
            + 2.0 * (qx * qz + qy * qw) * z,
        2.0 * (qx * qy + qz * qw) * x
            + (1.0 - 2.0 * (qx * qx + qz * qz)) * y
            + 2.0 * (qy * qz - qx * qw) * z,
        2.0 * (qx * qz - qy * qw) * x
            + 2.0 * (qy * qz + qx * qw) * y
            + (1.0 - 2.0 * (qx * qx + qy * qy)) * z,
    ]
}

fn scale_size_oriented(
    size: BrickSize,
    direction: Direction,
    rotation: Rotation,
    factors: [f64; 3],
) -> Result<BrickSize, String> {
    let basis = orientation_basis(direction, rotation);
    let local_factor = |axis: usize| {
        (0..3)
            .map(|world| f64::from(basis[axis][world].abs()) * factors[world])
            .sum::<f64>()
    };
    Ok(BrickSize::new(
        scale_half_extent(size.x, local_factor(0), "brick X size")?,
        scale_half_extent(size.y, local_factor(1), "brick Y size")?,
        scale_half_extent(size.z, local_factor(2), "brick Z size")?,
    ))
}

fn scale_half_extent(value: u16, factor: f64, field: &str) -> Result<u16, String> {
    // Bias dimensions outward while centers use nearest rounding. At fractional
    // scales this trades tiny overlaps for avoiding visible seams between
    // bricks that shared a face before scaling.
    let scaled = (f64::from(value) * factor).ceil().max(1.0);
    if scaled > f64::from(u16::MAX) {
        return Err(overflow(field));
    }
    Ok(scaled as u16)
}

fn normalize_main_position(
    logical: Position,
    min: Position,
    max: Position,
    factors: [f64; 3],
) -> Result<Position, String> {
    let center_x = f64::from(min.x) + f64::from(max.x - min.x) / 2.0;
    let center_y = f64::from(min.y) + f64::from(max.y - min.y) / 2.0;
    position_from_scaled([
        factors[0] * (f64::from(logical.x) - center_x),
        factors[1] * (f64::from(logical.y) - center_y),
        factors[2] * f64::from(logical.z - min.z),
    ])
}

fn normalize_main_vector(
    logical: Vector3f,
    min: Position,
    max: Position,
    factors: [f64; 3],
) -> Vector3f {
    let center_x = f64::from(min.x - PASTE_CORRECTION.x) + f64::from(max.x - min.x) / 2.0;
    let center_y = f64::from(min.y - PASTE_CORRECTION.y) + f64::from(max.y - min.y) / 2.0;
    let bottom_z = f64::from(min.z - PASTE_CORRECTION.z);
    Vector3f {
        x: (factors[0] * (f64::from(logical.x) - center_x)) as f32,
        y: (factors[1] * (f64::from(logical.y) - center_y)) as f32,
        z: (factors[2] * (f64::from(logical.z) - bottom_z)) as f32,
    }
}

fn scale_position_about_paste_baseline(
    position: Position,
    factors: [f64; 3],
) -> Result<Position, String> {
    position_from_scaled([
        f64::from(PASTE_CORRECTION.x) + f64::from(position.x - PASTE_CORRECTION.x) * factors[0],
        f64::from(PASTE_CORRECTION.y) + f64::from(position.y - PASTE_CORRECTION.y) * factors[1],
        f64::from(PASTE_CORRECTION.z) + f64::from(position.z - PASTE_CORRECTION.z) * factors[2],
    ])
}

fn patch_entity_locations(
    pending: &mut brdb::pending::BrPendingFs,
    chunks: Vec<EntityChunkPatch>,
) -> Result<(), String> {
    use brdb::pending::BrPendingFs::File;
    for mut chunk in chunks {
        for location in chunk.locations {
            let old = msgpack_vector(location.old);
            let new = msgpack_vector(location.new);
            let matches: Vec<_> = chunk
                .bytes
                .windows(old.len())
                .enumerate()
                .filter_map(|(index, bytes)| (bytes == old).then_some(index))
                .collect();
            if matches.len() != 1 {
                return Err(format!(
                    "expected one encoded dynamic-grid location in entity chunk {}, found {}",
                    chunk.chunk,
                    matches.len()
                ));
            }
            let index = matches[0];
            chunk.bytes[index..index + new.len()].copy_from_slice(&new);
        }
        *pending
            .cd_mut(format!("World/0/Entities/Chunks/{}.mps", chunk.chunk))
            .map_err(|error| {
                format!("failed to replace entity chunk {}: {error}", chunk.chunk)
            })? = File(Some(chunk.bytes));
    }
    Ok(())
}

fn msgpack_vector(vector: Vector3f) -> Vec<u8> {
    // BRDB's Vector3f is a flat array: its f32 payload has no per-value
    // MessagePack marker.
    let mut bytes = Vec::with_capacity(12);
    for value in [vector.x, vector.y, vector.z] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn position_from_scaled(values: [f64; 3]) -> Result<Position, String> {
    let scale_axis = |scaled: f64, axis: &str| -> Result<i32, String> {
        if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
            return Err(overflow(&format!("brick {axis} position")));
        }
        let scaled = scaled.round() as i32;
        // Chunk coordinates are serialized as i16 values.
        let chunk = scaled.div_euclid(brdb::CHUNK_SIZE);
        if !(i16::MIN as i32..=i16::MAX as i32).contains(&chunk) {
            return Err(overflow(&format!("brick {axis} chunk coordinate")));
        }
        Ok(scaled)
    };

    Ok(Position::new(
        scale_axis(values[0], "X")?,
        scale_axis(values[1], "Y")?,
        scale_axis(values[2], "Z")?,
    ))
}

fn brick_bounds(bricks: &[Brick], offset: Position) -> Option<(Position, Position)> {
    let mut bounds = None;
    for brick in bricks {
        let (min, max) = brick.local_bounds();
        include_bounds(&mut bounds, (min + offset, max + offset));
    }
    bounds
}

fn include_bounds(bounds: &mut Option<(Position, Position)>, next: (Position, Position)) {
    if let Some((min, max)) = bounds {
        min.x = min.x.min(next.0.x);
        min.y = min.y.min(next.0.y);
        min.z = min.z.min(next.0.z);
        max.x = max.x.max(next.1.x);
        max.y = max.y.max(next.1.y);
        max.z = max.z.max(next.1.z);
    } else {
        *bounds = Some(next);
    }
}

fn split_oversized_brick(brick: &Brick) -> Result<Vec<Brick>, String> {
    let BrickType::Procedural { size, .. } = &brick.asset else {
        return Ok(vec![brick.clone()]);
    };
    let x_parts = split_axis(size.x);
    let y_parts = split_axis(size.y);
    let z_parts = split_axis(size.z);
    let piece_count = x_parts
        .len()
        .checked_mul(y_parts.len())
        .and_then(|count| count.checked_mul(z_parts.len()))
        .ok_or_else(|| "brick subdivision count overflowed".to_string())?;
    if piece_count > MAX_SPLIT_PIECES {
        return Err(format!(
            "one scaled brick would require {piece_count} pieces; maximum is {MAX_SPLIT_PIECES}"
        ));
    }

    let basis = orientation_basis(brick.direction, brick.rotation);
    let mut pieces = Vec::with_capacity(piece_count);
    for &(x_offset, x_half) in &x_parts {
        for &(y_offset, y_half) in &y_parts {
            for &(z_offset, z_half) in &z_parts {
                let mut piece = brick.clone();
                let BrickType::Procedural { size, .. } = &mut piece.asset else {
                    unreachable!()
                };
                *size = BrickSize::new(x_half, y_half, z_half);
                let offset = transform_local_offset([x_offset, y_offset, z_offset], basis);
                piece.position += Position::new(offset[0], offset[1], offset[2]);
                pieces.push(piece);
            }
        }
    }
    Ok(pieces)
}

fn split_axis(half_extent: u16) -> Vec<(i32, u16)> {
    if half_extent <= MAX_BRICK_HALF_EXTENT {
        return vec![(0, half_extent)];
    }
    let mut parts = Vec::new();
    let mut cursor = -i32::from(half_extent);
    let end = i32::from(half_extent);
    while cursor < end {
        let full_length = (end - cursor).min(i32::from(MAX_BRICK_HALF_EXTENT) * 2);
        let half = full_length / 2;
        parts.push((cursor + half, half as u16));
        cursor += full_length;
    }
    parts
}

fn orientation_basis(direction: Direction, rotation: Rotation) -> [[i32; 3]; 3] {
    let (mut x, mut y, z) = match direction {
        Direction::XPositive => ([0, 0, -1], [0, 1, 0], [1, 0, 0]),
        Direction::XNegative => ([0, 0, 1], [0, 1, 0], [-1, 0, 0]),
        Direction::YPositive => ([1, 0, 0], [0, 0, -1], [0, 1, 0]),
        Direction::YNegative => ([1, 0, 0], [0, 0, 1], [0, -1, 0]),
        Direction::ZPositive => ([1, 0, 0], [0, 1, 0], [0, 0, 1]),
        Direction::ZNegative => ([-1, 0, 0], [0, 1, 0], [0, 0, -1]),
        Direction::MAX => ([1, 0, 0], [0, 1, 0], [0, 0, 1]),
    };
    let turns = match rotation {
        Rotation::Deg0 => 0,
        Rotation::Deg90 => 1,
        Rotation::Deg180 => 2,
        Rotation::Deg270 => 3,
    };
    for _ in 0..turns {
        (x, y) = ([-y[0], -y[1], -y[2]], x);
    }
    [x, y, z]
}

fn transform_local_offset(local: [i32; 3], basis: [[i32; 3]; 3]) -> [i32; 3] {
    let mut world = [0; 3];
    for axis in 0..3 {
        world[axis] =
            basis[0][axis] * local[0] + basis[1][axis] * local[1] + basis[2][axis] * local[2];
    }
    world
}

fn apply_joint_hierarchy_offsets(
    chunks: &mut [EntityChunkPatch],
    rotations: &HashMap<u32, Quat4f>,
    edges: &[JointEdge],
) -> Result<(), String> {
    let children: HashSet<_> = edges.iter().map(|edge| edge.child).collect();
    let mut world_offsets = HashMap::from([(1_u32, [0.0; 3])]);
    for &entity in rotations.keys() {
        if !children.contains(&entity) {
            world_offsets.insert(entity, [0.0; 3]);
        }
    }

    let mut unresolved: Vec<_> = (0..edges.len()).collect();
    while !unresolved.is_empty() {
        let before = unresolved.len();
        unresolved.retain(|&index| {
            let edge = &edges[index];
            let Some(parent_offset) = world_offsets.get(&edge.parent).copied() else {
                return true;
            };
            let local = if edge.parent == 1 {
                edge.correction
            } else {
                let Some(rotation) = rotations.get(&edge.parent).copied() else {
                    return true;
                };
                rotate_vector(rotation, edge.correction)
            };
            world_offsets.insert(edge.child, add_offsets(parent_offset, local));
            false
        });
        if unresolved.len() == before {
            return Err(
                "dynamic-grid joint hierarchy contains an unresolved cycle or parent".into(),
            );
        }
    }

    for location in chunks.iter_mut().flat_map(|chunk| &mut chunk.locations) {
        if let Some(offset) = world_offsets.get(&location.entity) {
            location.new.x += offset[0] as f32;
            location.new.y += offset[1] as f32;
            location.new.z += offset[2] as f32;
        }
    }
    Ok(())
}

fn overflow(field: &str) -> String {
    format!("{field} exceeds the BRZ format limit at the requested scale")
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brdb::{assets, Brick, IntoReader, World};

    #[test]
    fn scales_procedural_bricks_and_omits_basic_bricks() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let input = std::env::temp_dir().join(format!("brz-scale-{nonce}-input.brz"));
        let output = std::env::temp_dir().join(format!("brz-scale-{nonce}-output.brz"));

        let procedural = Brick {
            asset: BrickType::Procedural {
                asset: assets::bricks::PB_DEFAULT_BRICK,
                size: BrickSize::new(5, 10, 3),
            },
            position: Position::new(-20, 30, 6),
            ..Brick::default()
        };

        let basic = Brick {
            asset: assets::bricks::B_1X1_BRICK_SIDE,
            ..Brick::default()
        };

        let mut world = World::new();
        world.bricks = vec![procedural, basic];
        world.make_prefab();
        Brz::save(&input, &world).unwrap();

        let stats = scale_brz(&input, &output, 4.0).unwrap();
        assert_eq!(stats.input_bricks, 2);
        assert_eq!(stats.output_bricks, 1);
        assert_eq!(stats.skipped_basic_bricks, 1);

        let archive = Brz::open(&output).unwrap();
        let reader = archive.into_reader();
        let global = reader.global_data().unwrap();
        let chunks = reader.brick_chunk_index(1).unwrap();
        let bricks: Vec<_> = chunks
            .into_iter()
            .flat_map(|chunk| {
                reader
                    .brick_chunk_soa(1, chunk.index)
                    .unwrap()
                    .iter_bricks(chunk.index, global.clone())
                    .collect::<Vec<_>>()
            })
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(bricks.len(), 1);
        assert_eq!(bricks[0].position, Position::new(-1000, -1000, -1012));
        assert!(matches!(
            bricks[0].asset,
            BrickType::Procedural {
                size: BrickSize {
                    x: 20,
                    y: 40,
                    z: 12
                },
                ..
            }
        ));

        std::fs::remove_file(input).unwrap();
        std::fs::remove_file(output).unwrap();
    }

    #[test]
    fn rejects_size_overflow() {
        assert!(scale_size_oriented(
            BrickSize::new(u16::MAX, 1, 1),
            Direction::ZPositive,
            Rotation::Deg0,
            [2.0; 3]
        )
        .is_err());
    }

    #[test]
    fn rejects_fractional_scale_factors() {
        assert!(validate_factors([1.5, 2.0, 3.0]).is_err());
        assert!(validate_factors([1.0, 2.0, 3.0]).is_ok());
    }

    #[test]
    fn rebuilds_prefab_from_intended_bounds() {
        let mut prefab = PrefabJson::default();
        prefab.added_global_grid_offset.x = 155;
        prefab.added_global_grid_offset.y = -130;
        prefab.added_global_grid_offset.z = -4;

        rebuild_prefab_metadata(
            &mut prefab,
            Some((Position::new(-40, -20, 0), Position::new(40, 20, 32))),
        );

        assert_eq!(prefab.pivots.bounds_pivot.center.x, 0.0);
        assert_eq!(prefab.pivots.bounds_pivot.center.y, 0.0);
        assert_eq!(prefab.pivots.bounds_pivot.center.z, 16.0);
        assert_eq!(prefab.pivots.bounds_pivot.half_extent.x, 40.0);
        assert_eq!(prefab.pivots.bounds_pivot.half_extent.y, 20.0);
        assert_eq!(prefab.pivots.bounds_pivot.half_extent.z, 16.0);
        assert_eq!(prefab.added_global_grid_offset.x, 0);
        assert_eq!(prefab.added_global_grid_offset.y, 0);
        assert_eq!(prefab.added_global_grid_offset.z, 0);
    }

    #[test]
    fn normalizes_each_main_grid_position() {
        let min = Position::new(-20, -20, -8);
        let max = Position::new(20, 20, 8);
        let center = normalize_main_position(Position::new(0, 0, 0), min, max, [2.0; 3]).unwrap();
        let corner =
            normalize_main_position(Position::new(-15, -15, -6), min, max, [2.0; 3]).unwrap();

        assert_eq!(center, Position::new(0, 0, 16));
        assert_eq!(corner, Position::new(-30, -30, 4));
    }

    #[test]
    fn splits_oversized_bricks_without_gaps() {
        let brick = Brick {
            asset: BrickType::Procedural {
                asset: assets::bricks::PB_DEFAULT_BRICK,
                size: BrickSize::new(10000, 4500, 1000),
            },
            position: Position::new(50, -20, 200),
            ..Brick::default()
        };
        let pieces = split_oversized_brick(&brick).unwrap();
        assert_eq!(pieces.len(), 6);
        assert!(pieces.iter().all(|piece| matches!(
            piece.asset,
            BrickType::Procedural { size, .. }
                if size.x <= 4000 && size.y <= 4000 && size.z <= 4000
        )));
        let bounds = brick_bounds(&pieces, Position::ZERO).unwrap();
        assert_eq!(bounds, brick.local_bounds());
    }

    #[test]
    fn rotates_subdivision_offsets_with_the_brick() {
        let brick = Brick {
            asset: BrickType::Procedural {
                asset: assets::bricks::PB_DEFAULT_BRICK,
                size: BrickSize::new(5000, 200, 200),
            },
            rotation: Rotation::Deg90,
            ..Brick::default()
        };
        let pieces = split_oversized_brick(&brick).unwrap();
        let mut positions: Vec<_> = pieces.iter().map(|piece| piece.position).collect();
        positions.sort_by_key(|position| position.y);
        assert_eq!(
            positions,
            [Position::new(0, -4000, 0), Position::new(0, 1000, 0)]
        );
    }

    #[test]
    fn rotates_automatic_socket_offset_into_joint_space() {
        let rotation = Quat4f {
            x: 0.5,
            y: -0.5,
            z: 0.5,
            w: -0.5,
        };
        assert_eq!(
            automatic_joint_socket_offset(rotation, [2.0; 3]),
            [0.0, -24.0, -24.0]
        );
        assert_eq!(
            automatic_joint_socket_offset(rotation, [4.0; 3]),
            [0.0, -72.0, -72.0]
        );
        assert_eq!(
            automatic_joint_socket_offset(rotation, [1.0; 3]),
            [0.0, 0.0, 0.0]
        );
    }
}
