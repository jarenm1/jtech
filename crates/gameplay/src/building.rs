//! Free-placed building primitives, shared by the authoritative server and the
//! client's placement preview. Pieces are axis-aligned boxes at arbitrary
//! positions with yaw quantized to quarter turns; they never join the voxel
//! grid. Placement is free-form: a piece is legal anywhere its box fits
//! entirely outside terrain density, other pieces, and players. There is no
//! support or stability requirement yet.
use glam::Vec3;
use physics::{CollisionShape, PlayerState};
use voxel_world::{VoxelWorld, WORLD_MAX_Y, WORLD_MIN_Y};

/// Building primitive kinds. Wire values are stable; append only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum PieceKind {
    Foundation = 0,
    Floor = 1,
    Wall = 2,
    Pillar = 3,
    Bedroll = 4,
}

/// Largest piece count the server retains; bounds replication and raycasts.
pub const MAX_PIECES: usize = 1024;
/// Furthest eye-to-surface distance a placement or hit may target.
pub const BUILD_REACH: f32 = 6.0;
/// Gap kept between a placed box and the surface it was aimed at, so the
/// clearance check does not reject on contact skin.
const PLACE_SKIN: f32 = 0.02;
/// Terrain density sample spacing inside a candidate box.
const CLEARANCE_STEP: f32 = 0.45;
/// Overlap tolerance: boxes touching within this distance do not collide.
const OVERLAP_EPS: f32 = 0.001;

impl PieceKind {
    pub const ALL: [PieceKind; 5] = [
        PieceKind::Foundation,
        PieceKind::Floor,
        PieceKind::Wall,
        PieceKind::Pillar,
        PieceKind::Bedroll,
    ];

    pub fn from_u8(value: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| *kind as u8 == value)
    }

    /// Half extents before yaw rotation. Walls and pillars are thin on X so a
    /// quarter turn swaps their footprint.
    fn base_half_extents(self) -> Vec3 {
        match self {
            PieceKind::Foundation => Vec3::new(1.5, 0.5, 1.5),
            PieceKind::Floor => Vec3::new(1.5, 0.125, 1.5),
            PieceKind::Wall => Vec3::new(0.125, 1.5, 1.5),
            PieceKind::Pillar => Vec3::new(0.25, 1.5, 0.25),
            PieceKind::Bedroll => Vec3::new(0.45, 0.15, 0.95),
        }
    }

    /// Half extents after `yaw_steps` quarter turns. Only X/Z swap.
    pub fn half_extents(self, yaw_steps: u8) -> Vec3 {
        let base = self.base_half_extents();
        if yaw_steps % 2 == 1 {
            Vec3::new(base.z, base.y, base.x)
        } else {
            base
        }
    }

    /// Inventory item spent per placement. Wood builds the structural pieces;
    /// the bedroll keeps its own item id.
    pub fn item(self) -> u8 {
        match self {
            PieceKind::Bedroll => voxel_world::BEDROLL,
            _ => voxel_world::WOOD,
        }
    }

    /// Hit points before the piece is destroyed.
    pub fn health(self) -> u16 {
        match self {
            PieceKind::Bedroll => 40,
            PieceKind::Pillar => 150,
            _ => 250,
        }
    }

    /// Feet-anchored collider shape for raycasts against this piece.
    pub fn shape(self, yaw_steps: u8) -> CollisionShape {
        let e = self.half_extents(yaw_steps);
        CollisionShape::new(e.x, e.z, e.y * 2.0).expect("piece extents are bounded")
    }
}

/// One placed piece: `position` is the box center, `yaw_steps` counts quarter
/// turns about Y.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Piece {
    pub id: u32,
    pub kind: PieceKind,
    pub position: Vec3,
    pub yaw_steps: u8,
    pub health: u16,
}

impl Piece {
    /// Box center as a feet-anchored collider position for `physics::bounds`.
    pub fn feet(self) -> Vec3 {
        self.position - Vec3::Y * self.kind.half_extents(self.yaw_steps).y
    }
    pub fn half_extents(self) -> Vec3 {
        self.kind.half_extents(self.yaw_steps)
    }
    pub fn aabb(self) -> (Vec3, Vec3) {
        let e = self.half_extents();
        (self.position - e, self.position + e)
    }
}

/// Why a candidate placement is illegal. The server maps these onto
/// `EditRejection`; the client uses them only to tint the ghost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementError {
    /// Some density sample inside the box was unloaded.
    Unloaded,
    /// The box intersects terrain density or a placed voxel.
    Terrain,
    /// The box intersects another piece.
    Piece,
    /// The box intersects a player.
    Player,
    /// The box leaves the vertical world range.
    OutOfBounds,
}

/// Box center for a piece resting against the surface `hit` describes: pushed
/// out along the face normal by the box's extent on that axis. `hit_point` is
/// the world-space ray/surface contact; `normal` is the unit face normal.
pub fn placement_pose(
    kind: PieceKind,
    yaw_steps: u8,
    hit_point: Vec3,
    normal: Vec3,
) -> Option<Vec3> {
    if !hit_point.is_finite() || !normal.is_finite() {
        return None;
    }
    let e = kind.half_extents(yaw_steps);
    let mut position = hit_point;
    for axis in 0..3 {
        let n = normal[axis];
        if n > 0.5 {
            position[axis] = hit_point[axis] + e[axis] + PLACE_SKIN;
        } else if n < -0.5 {
            position[axis] = hit_point[axis] - e[axis] - PLACE_SKIN;
        }
    }
    Some(position)
}

/// True when the candidate box is entirely inside the vertical world range and
/// clear of terrain density, placed voxels, other pieces, and player bodies.
/// Returns the first violation found; `Unloaded` wins over `Terrain` so a
/// missing chunk never reads as solid.
pub fn placement_clear<'a>(
    world: &VoxelWorld,
    kind: PieceKind,
    yaw_steps: u8,
    position: Vec3,
    pieces: impl Iterator<Item = &'a Piece>,
    players: impl Iterator<Item = &'a PlayerState>,
) -> Result<(), PlacementError> {
    if !position.is_finite() {
        return Err(PlacementError::OutOfBounds);
    }
    let e = kind.half_extents(yaw_steps);
    let (min, max) = (position - e, position + e);
    if min.y < WORLD_MIN_Y as f32 || max.y > WORLD_MAX_Y as f32 + 1.0 {
        return Err(PlacementError::OutOfBounds);
    }
    // Sample the box interior on a sub-cell grid. Unloaded chunks reject as
    // Unloaded; positive density or a placed voxel rejects as Terrain.
    let mut unloaded = false;
    let mut p = min;
    loop {
        match world.density_at(p) {
            None => unloaded = true,
            Some(density) if density > 0.0 => return Err(PlacementError::Terrain),
            _ => {}
        }
        let cell = p.floor().as_ivec3();
        if world.voxel(cell).is_some_and(|v| v.placed) {
            return Err(PlacementError::Terrain);
        }
        // Advance x-fastest through the inclusive sample grid.
        if p.x + CLEARANCE_STEP < max.x {
            p.x += CLEARANCE_STEP;
        } else {
            p.x = min.x;
            if p.y + CLEARANCE_STEP < max.y {
                p.y += CLEARANCE_STEP;
            } else {
                p.y = min.y;
                if p.z + CLEARANCE_STEP < max.z {
                    p.z += CLEARANCE_STEP;
                } else {
                    break;
                }
            }
        }
    }
    // The far corner is always sampled: the loop exits before reaching it.
    match world.density_at(max) {
        None => unloaded = true,
        Some(density) if density > 0.0 => return Err(PlacementError::Terrain),
        _ => {}
    }
    if unloaded {
        return Err(PlacementError::Unloaded);
    }
    for piece in pieces {
        let (lo, hi) = piece.aabb();
        if aabb_overlap(min, max, lo, hi) {
            return Err(PlacementError::Piece);
        }
    }
    for player in players {
        let (lo, hi) = physics::bounds(player.position, CollisionShape::default());
        if !player.noclip && aabb_overlap(min, max, lo, hi) {
            return Err(PlacementError::Player);
        }
    }
    Ok(())
}

/// Strict interior overlap: boxes sharing only a face do not collide.
pub fn aabb_overlap(a_min: Vec3, a_max: Vec3, b_min: Vec3, b_max: Vec3) -> bool {
    a_min.cmplt(b_max - Vec3::splat(OVERLAP_EPS)).all()
        && a_max.cmpgt(b_min + Vec3::splat(OVERLAP_EPS)).all()
}

/// Hotbar slot to piece kind: slots 1-4 are the structural pieces, slot 7 is
/// the bedroll. Slot 5 is the wood resource they cost, 6 is the bow, and
/// 8-10 are tools; none of those place.
pub fn slot_piece(slot: u8) -> Option<PieceKind> {
    match slot {
        1 => Some(PieceKind::Foundation),
        2 => Some(PieceKind::Floor),
        3 => Some(PieceKind::Wall),
        4 => Some(PieceKind::Pillar),
        7 => Some(PieceKind::Bedroll),
        _ => None,
    }
}

/// Face normal of a voxel raycast hit: the axis the ray crossed to enter
/// `hit.block`, derived from the adjacent cell.
pub fn hit_normal(hit: &voxel_world::RayHit) -> Vec3 {
    (hit.adjacent - hit.block).as_vec3()
}

/// Ray vs one piece box; returns the entry distance. Feet-anchored shape like
/// `raycast_body`, so an origin inside still connects at zero.
pub fn raycast_piece(origin: Vec3, direction: Vec3, piece: &Piece) -> Option<f32> {
    physics::raycast_body(
        origin,
        direction,
        piece.feet(),
        piece.kind.shape(piece.yaw_steps),
    )
}

/// Closest piece the ray enters within `max_distance`, or `None`.
pub fn raycast_pieces<'a>(
    origin: Vec3,
    direction: Vec3,
    max_distance: f32,
    pieces: impl Iterator<Item = &'a Piece>,
) -> Option<(u32, f32)> {
    let mut best: Option<(u32, f32)> = None;
    for piece in pieces {
        if let Some(distance) = raycast_piece(origin, direction, piece)
            && distance <= max_distance
            && best.is_none_or(|(_, d)| distance < d)
        {
            best = Some((piece.id, distance));
        }
    }
    best
}


#[cfg(test)]
mod tests {
    use super::*;

    fn empty_world() -> VoxelWorld {
        VoxelWorld::default()
    }

    #[test]
    fn pose_offsets_by_extent_along_normal() {
        let hit = Vec3::new(4.3, 10.0, 4.7);
        // On a floor face the foundation's bottom sits on the surface.
        let pos = placement_pose(PieceKind::Foundation, 0, hit, Vec3::Y).unwrap();
        assert!((pos.y - 10.52).abs() < 0.001, "{pos}");
        assert_eq!((pos.x, pos.z), (hit.x, hit.z));
        // Against a +X cliff face a wall (thin on X) sits flush.
        let pos = placement_pose(PieceKind::Wall, 0, hit, Vec3::X).unwrap();
        assert!((pos.x - (4.3 + 0.125 + PLACE_SKIN)).abs() < 0.001, "{pos}");
        // Rotated 90° the wall's long axis is X: it stands off the face.
        let pos = placement_pose(PieceKind::Wall, 1, hit, Vec3::X).unwrap();
        assert!((pos.x - (4.3 + 1.5 + PLACE_SKIN)).abs() < 0.001, "{pos}");
        // Under a ceiling the box hangs below the contact point.
        let pos = placement_pose(PieceKind::Floor, 0, hit, Vec3::NEG_Y).unwrap();
        assert!((pos.y - (10.0 - 0.125 - PLACE_SKIN)).abs() < 0.001, "{pos}");
    }

    #[test]
    fn clearance_rejects_unloaded_terrain() {
        // Default world has no chunks: every sample is unloaded.
        let world = empty_world();
        let result = placement_clear(
            &world,
            PieceKind::Foundation,
            0,
            Vec3::new(0.0, 20.0, 0.0),
            [].iter(),
            [].iter(),
        );
        assert_eq!(result, Err(PlacementError::Unloaded));
    }

    #[test]
    fn clearance_rejects_piece_and_player_overlap() {
        let world = empty_world();
        let existing = Piece {
            id: 1,
            kind: PieceKind::Foundation,
            position: Vec3::new(0.0, 20.0, 0.0),
            yaw_steps: 0,
            health: 250,
        };
        let pieces = [existing];
        let result = placement_clear(
            &world,
            PieceKind::Floor,
            0,
            Vec3::new(0.0, 20.0, 0.0),
            pieces.iter(),
            [].iter(),
        );
        // Unloaded terrain is checked first; overlap is only reachable on a
        // loaded world, so assert the piece branch directly.
        assert_eq!(result, Err(PlacementError::Unloaded));
        let player = PlayerState {
            position: Vec3::new(0.0, 20.0, 0.0),
            ..Default::default()
        };
        let players = [player];
        assert!(aabb_overlap(
            Vec3::ZERO,
            Vec3::ONE,
            Vec3::splat(0.5),
            Vec3::splat(2.0)
        ));
        assert!(!aabb_overlap(
            Vec3::ZERO,
            Vec3::ONE,
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 1.0, 1.0)
        ));
        let _ = players;
    }

    #[test]
    fn raycast_piece_hits_box() {
        let piece = Piece {
            id: 7,
            kind: PieceKind::Wall,
            position: Vec3::new(0.0, 1.5, -5.0),
            yaw_steps: 0,
            health: 250,
        };
        let hit = raycast_piece(Vec3::ZERO, Vec3::NEG_Z, &piece).unwrap();
        assert!((hit - 3.5).abs() < 0.001, "{hit}");
        assert_eq!(
            raycast_pieces(Vec3::ZERO, Vec3::NEG_Z, 6.0, [&piece].into_iter()),
            Some((7, hit))
        );
        assert!(raycast_piece(Vec3::ZERO, Vec3::X, &piece).is_none());
    }
}
