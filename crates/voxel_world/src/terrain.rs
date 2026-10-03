//! Native, bounded terrain evaluation graph.
//!
//! Scheme terrain packages describe a small composable expression graph plus a
//! biome table. `game_packages::load_terrain` turns that description into
//! [`TerrainExpr`] values and calls [`TerrainGenerator::compile`], producing
//! this immutable native type. Voxel generation evaluates the compiled graph
//! once per world column; no Scheme interpreter runs during generation.
//!
//! The graph is a topologically ordered DAG of at most [`MAX_GRAPH_NODES`]
//! nodes and at most [`MAX_GRAPH_DEPTH`] of nesting. Evaluation writes every
//! node once into a fixed scratch buffer, so per-column cost is proportional to
//! the graph size and cannot grow from untrusted package input.

use crate::{
    AIR, CHUNK_SIZE, Chunk, DENSITY_AIR, DENSITY_SOLID, DIRT, GRASS, SAND, STONE, Storage, Voxel,
    WORLD_MAX_Y, WORLD_MIN_Y,
};
use glam::IVec3;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

/// Version of the Scheme authoring surface accepted by the loader.
pub const TERRAIN_API_VERSION: u32 = 1;
/// Hard cap on compiled graph size.
pub const MAX_GRAPH_NODES: usize = 512;
/// Hard cap on expression nesting depth.
pub const MAX_GRAPH_DEPTH: usize = 64;
/// Hard cap on curve control points.
pub const MAX_CURVE_POINTS: usize = 16;
/// Hard cap on declared biomes.
pub const MAX_BIOMES: usize = 32;
/// Hard cap on declared scatter species.
pub const MAX_SPECIES: usize = 64;
/// Hard cap on blocks of soil below a surface block.
pub const MAX_SOIL_DEPTH: u8 = 16;
/// Largest accepted jittered-grid spacing, in blocks.
pub const MAX_SCATTER_SPACING: f32 = 64.0;
/// Largest accepted sink below the surface, in blocks.
pub const MAX_SCATTER_SINK: f32 = 8.0;
/// Salt separating the scatter field from terrain and material hashes.
const SCATTER_SALT: u64 = 0x5CA7_7E12_5CA7_7E12;

const NO_OPERAND: u16 = u16::MAX;
const DIV_EPSILON: f32 = 1e-6;

// ---------------------------------------------------------------------------
// Authoring schema
// ---------------------------------------------------------------------------

/// Parameters of a single fBm or ridged value-noise field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoiseSpec {
    /// Lattice cycles per world block. Must be finite and in `(0, 1]`.
    pub frequency: f32,
    /// Octave count in `1..=12`.
    pub octaves: u8,
    /// Frequency multiplier between octaves in `1..=4`.
    pub lacunarity: f32,
    /// Amplitude multiplier between octaves in `0..=1`.
    pub gain: f32,
    /// Per-field salt mixed into the seed so independent fields decorrelate.
    pub salt: u64,
    /// Ridged fields fold each octave around its midpoint, producing crests.
    pub ridged: bool,
}

/// One biome rule. Biomes are evaluated in declaration order against the
/// column's temperature, moisture and raw height. Membership fades across a
/// fixed width at each range edge; earlier rules claim their coverage first and
/// the final declared biome is the fallback. Soil depth blends across the
/// weighted contributions and a seeded world-coordinate hash picks the
/// surface/subsurface material pair.
#[derive(Clone, Debug, PartialEq)]
pub struct BiomeSpec {
    pub name: String,
    pub surface: u8,
    pub subsurface: u8,
    pub depth: u8,
    pub temperature: (f32, f32),
    pub moisture: (f32, f32),
    pub height: (f32, f32),
}

/// One scatterable species: a model plus the rules that place it.
///
/// Candidates come from a world-aligned jittered grid of `spacing`-block
/// cells, so placement is independent of chunk boundaries and call order. A
/// candidate survives when its density roll, cluster mask, biome membership,
/// altitude, moisture and slope all admit it.
#[derive(Clone, Debug, PartialEq)]
pub struct SpeciesSpec {
    pub name: String,
    /// Model path relative to the owning package's `assets/` directory.
    pub model: String,
    /// Jittered-grid cell size in blocks: the minimum spacing between instances.
    pub spacing: f32,
    /// Probability in `0..=1` that a candidate cell yields an instance.
    pub density: f32,
    /// Largest neighbouring height delta, in blocks per block, that admits it.
    pub slope_max: f32,
    /// Absolute world-height band in blocks.
    pub altitude: (f32, f32),
    pub moisture: (f32, f32),
    /// Uniform scale range applied to the model.
    pub scale: (f32, f32),
    /// Blocks to sink the model below the surface. A flat base cannot follow a
    /// slope, so the downhill edge floats by `slope * footprint`; sinking hides
    /// that gap. Zero places the origin exactly on the surface.
    pub sink: f32,
    /// Low-frequency cluster mask `(frequency, threshold)`. A frequency of
    /// zero disables clustering and admits every candidate.
    pub cluster: (f32, f32),
}

/// Scatter rules: the species table plus per-biome membership.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScatterSpec {
    pub species: Vec<SpeciesSpec>,
    /// Per biome index, the species indices allowed in that biome.
    pub biomes: Vec<Vec<u16>>,
}

/// One placed scatter instance in world space. `y` is the rendered surface
/// height less the species sink, so a model authored with its base at the
/// origin sits on the ground with its base buried by `sink` blocks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScatterInstance {
    /// Index into the compiled species table.
    pub species: u16,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Rotation about the vertical axis, in radians.
    pub yaw: f32,
    pub scale: f32,
}

/// Composable expression graph evaluated per world column.
#[derive(Clone, Debug, PartialEq)]
pub enum TerrainExpr {
    Constant(f32),
    X,
    Z,
    Noise {
        spec: NoiseSpec,
        x: Box<TerrainExpr>,
        z: Box<TerrainExpr>,
    },
    Add(Box<TerrainExpr>, Box<TerrainExpr>),
    Subtract(Box<TerrainExpr>, Box<TerrainExpr>),
    Multiply(Box<TerrainExpr>, Box<TerrainExpr>),
    Divide(Box<TerrainExpr>, Box<TerrainExpr>),
    Min(Box<TerrainExpr>, Box<TerrainExpr>),
    Max(Box<TerrainExpr>, Box<TerrainExpr>),
    Abs(Box<TerrainExpr>),
    Negate(Box<TerrainExpr>),
    Sqrt(Box<TerrainExpr>),
    Pow(Box<TerrainExpr>, f32),
    Clamp {
        value: Box<TerrainExpr>,
        low: Box<TerrainExpr>,
        high: Box<TerrainExpr>,
    },
    Mix {
        a: Box<TerrainExpr>,
        b: Box<TerrainExpr>,
        t: Box<TerrainExpr>,
    },
    Smoothstep {
        edge0: Box<TerrainExpr>,
        edge1: Box<TerrainExpr>,
        x: Box<TerrainExpr>,
    },
    Step {
        edge: Box<TerrainExpr>,
        x: Box<TerrainExpr>,
    },
    SmoothMin {
        a: Box<TerrainExpr>,
        b: Box<TerrainExpr>,
        k: f32,
    },
    SmoothMax {
        a: Box<TerrainExpr>,
        b: Box<TerrainExpr>,
        k: f32,
    },
    ScaleBias {
        value: Box<TerrainExpr>,
        scale: f32,
        bias: f32,
    },
    Curve {
        x: Box<TerrainExpr>,
        points: Vec<(f32, f32)>,
    },
}

/// Complete description handed to [`TerrainGenerator::compile`].
#[derive(Clone, Debug)]
pub struct TerrainSpec {
    /// Stable diagnostic identity of the authoring source.
    pub identity: String,
    /// Authoring version, independent of the compiled layout.
    pub version: u32,
    /// Neighbour height delta at or above which a column is surfaced as stone.
    pub stone_slope: f32,
    pub height: TerrainExpr,
    pub temperature: TerrainExpr,
    pub moisture: TerrainExpr,
    pub soil: TerrainExpr,
    pub biomes: Vec<BiomeSpec>,
    /// Organic scatter rules. Empty disables scatter entirely.
    pub scatter: ScatterSpec,
}

// ---------------------------------------------------------------------------
// Compiled graph
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Op {
    Constant(f32),
    X,
    Z,
    Noise(NoiseSpec),
    Add,
    Subtract,
    Multiply,
    Divide,
    Min,
    Max,
    Abs,
    Negate,
    Sqrt,
    Pow(f32),
    Clamp,
    Mix,
    Smoothstep,
    Step,
    SmoothMin(f32),
    SmoothMax(f32),
    ScaleBias { scale: f32, bias: f32 },
    Curve(u16),
}

#[derive(Clone, Copy, Debug)]
struct Node {
    op: Op,
    a: u16,
    b: u16,
    c: u16,
}

#[derive(Clone, Debug)]
struct Curve {
    xs: Box<[f32]>,
    ys: Box<[f32]>,
}

#[derive(Clone, Debug)]
struct BiomeDef {
    name: Box<str>,
    surface: u8,
    subsurface: u8,
    depth: u8,
    temperature: (f32, f32),
    moisture: (f32, f32),
    height: (f32, f32),
}

#[derive(Clone, Debug)]
struct SpeciesDef {
    name: Box<str>,
    model: Box<str>,
    spacing: f32,
    density: f32,
    slope_max: f32,
    altitude: (f32, f32),
    moisture: (f32, f32),
    scale: (f32, f32),
    sink: f32,
    cluster: (f32, f32),
}

#[derive(Clone, Debug, Default)]
struct CompiledScatter {
    species: Box<[SpeciesDef]>,
    /// Per biome index, the species indices allowed there.
    biomes: Box<[Box<[u16]>]>,
}

#[derive(Debug)]
struct CompiledGraph {
    nodes: Box<[Node]>,
    curves: Box<[Curve]>,
    biomes: Box<[BiomeDef]>,
    height: u16,
    temperature: u16,
    moisture: u16,
    soil: u16,
    identity: Box<str>,
    version: u32,
    stone_slope: f32,
    scatter: CompiledScatter,
}

/// Immutable compiled terrain graph shared by reference across generation.
#[derive(Clone, Debug)]
pub struct TerrainGenerator {
    inner: Arc<CompiledGraph>,
}

/// One evaluated world column.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainSample {
    /// Topmost solid block of the column.
    pub height: i32,
    /// Unclamped, unrounded graph height. The smooth density field crosses
    /// zero at this world y; `height` is only its rounded, clamped form.
    pub raw_height: f32,
    /// Biome rule that matched this column.
    pub biome: Biome,
    /// Surface block before the slope override applied during generation.
    pub surface: u8,
    /// Soil block below the surface.
    pub subsurface: u8,
    /// Soil thickness in blocks below the surface.
    pub soil: u8,
    pub temperature: f32,
    pub moisture: f32,
}

/// Index of a declared biome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Biome(pub u16);

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

struct Compiler {
    nodes: Vec<Node>,
    curves: Vec<Curve>,
}

fn finite(name: &str, value: f32) -> Result<(), String> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(format!("{name} must be finite"))
    }
}

fn validate_noise(spec: &NoiseSpec) -> Result<(), String> {
    if !spec.frequency.is_finite() || spec.frequency <= 0.0 || spec.frequency > 1.0 {
        return Err("noise frequency must be finite and in (0, 1]".into());
    }
    if spec.octaves == 0 || spec.octaves > 12 {
        return Err("noise octaves must be in 1..=12".into());
    }
    if !spec.lacunarity.is_finite() || !(1.0..=4.0).contains(&spec.lacunarity) {
        return Err("noise lacunarity must be finite and in 1..=4".into());
    }
    if !spec.gain.is_finite() || !(0.0..=1.0).contains(&spec.gain) {
        return Err("noise gain must be finite and in 0..=1".into());
    }
    Ok(())
}

fn validate_curve(points: &[(f32, f32)]) -> Result<(), String> {
    if points.len() < 2 || points.len() > MAX_CURVE_POINTS {
        return Err(format!(
            "curve must have 2..={MAX_CURVE_POINTS} control points"
        ));
    }
    let mut previous = f32::NEG_INFINITY;
    for &(x, y) in points {
        if !x.is_finite() || !y.is_finite() {
            return Err("curve control points must be finite".into());
        }
        if x <= previous {
            return Err("curve control point x values must strictly increase".into());
        }
        previous = x;
    }
    Ok(())
}

impl Compiler {
    fn push(&mut self, op: Op, a: u16, b: u16, c: u16) -> Result<u16, String> {
        if self.nodes.len() >= MAX_GRAPH_NODES {
            return Err(format!(
                "terrain graph exceeds the {MAX_GRAPH_NODES} node limit"
            ));
        }
        self.nodes.push(Node { op, a, b, c });
        Ok((self.nodes.len() - 1) as u16)
    }

    fn push_curve(&mut self, points: &[(f32, f32)]) -> Result<u16, String> {
        let (xs, ys): (Vec<f32>, Vec<f32>) = points.iter().copied().unzip();
        self.curves.push(Curve {
            xs: xs.into_boxed_slice(),
            ys: ys.into_boxed_slice(),
        });
        Ok((self.curves.len() - 1) as u16)
    }

    fn compile_expr(&mut self, expr: &TerrainExpr, depth: usize) -> Result<u16, String> {
        if depth > MAX_GRAPH_DEPTH {
            return Err(format!(
                "terrain expression exceeds the {MAX_GRAPH_DEPTH} depth limit"
            ));
        }
        match expr {
            TerrainExpr::Constant(value) => {
                finite("constant", *value)?;
                self.push(Op::Constant(*value), NO_OPERAND, NO_OPERAND, NO_OPERAND)
            }
            TerrainExpr::X => self.push(Op::X, NO_OPERAND, NO_OPERAND, NO_OPERAND),
            TerrainExpr::Z => self.push(Op::Z, NO_OPERAND, NO_OPERAND, NO_OPERAND),
            TerrainExpr::Noise { spec, x, z } => {
                validate_noise(spec)?;
                let a = self.compile_expr(x, depth + 1)?;
                let b = self.compile_expr(z, depth + 1)?;
                self.push(Op::Noise(*spec), a, b, NO_OPERAND)
            }
            TerrainExpr::Add(l, r) => {
                let a = self.compile_expr(l, depth + 1)?;
                let b = self.compile_expr(r, depth + 1)?;
                self.push(Op::Add, a, b, NO_OPERAND)
            }
            TerrainExpr::Subtract(l, r) => {
                let a = self.compile_expr(l, depth + 1)?;
                let b = self.compile_expr(r, depth + 1)?;
                self.push(Op::Subtract, a, b, NO_OPERAND)
            }
            TerrainExpr::Multiply(l, r) => {
                let a = self.compile_expr(l, depth + 1)?;
                let b = self.compile_expr(r, depth + 1)?;
                self.push(Op::Multiply, a, b, NO_OPERAND)
            }
            TerrainExpr::Divide(l, r) => {
                let a = self.compile_expr(l, depth + 1)?;
                let b = self.compile_expr(r, depth + 1)?;
                self.push(Op::Divide, a, b, NO_OPERAND)
            }
            TerrainExpr::Min(l, r) => {
                let a = self.compile_expr(l, depth + 1)?;
                let b = self.compile_expr(r, depth + 1)?;
                self.push(Op::Min, a, b, NO_OPERAND)
            }
            TerrainExpr::Max(l, r) => {
                let a = self.compile_expr(l, depth + 1)?;
                let b = self.compile_expr(r, depth + 1)?;
                self.push(Op::Max, a, b, NO_OPERAND)
            }
            TerrainExpr::Abs(v) => {
                let a = self.compile_expr(v, depth + 1)?;
                self.push(Op::Abs, a, NO_OPERAND, NO_OPERAND)
            }
            TerrainExpr::Negate(v) => {
                let a = self.compile_expr(v, depth + 1)?;
                self.push(Op::Negate, a, NO_OPERAND, NO_OPERAND)
            }
            TerrainExpr::Sqrt(v) => {
                let a = self.compile_expr(v, depth + 1)?;
                self.push(Op::Sqrt, a, NO_OPERAND, NO_OPERAND)
            }
            TerrainExpr::Pow(v, exponent) => {
                finite("pow exponent", *exponent)?;
                let a = self.compile_expr(v, depth + 1)?;
                self.push(Op::Pow(*exponent), a, NO_OPERAND, NO_OPERAND)
            }
            TerrainExpr::Clamp { value, low, high } => {
                let a = self.compile_expr(value, depth + 1)?;
                let b = self.compile_expr(low, depth + 1)?;
                let c = self.compile_expr(high, depth + 1)?;
                self.push(Op::Clamp, a, b, c)
            }
            TerrainExpr::Mix { a, b, t } => {
                let a = self.compile_expr(a, depth + 1)?;
                let b = self.compile_expr(b, depth + 1)?;
                let c = self.compile_expr(t, depth + 1)?;
                self.push(Op::Mix, a, b, c)
            }
            TerrainExpr::Smoothstep { edge0, edge1, x } => {
                let a = self.compile_expr(edge0, depth + 1)?;
                let b = self.compile_expr(edge1, depth + 1)?;
                let c = self.compile_expr(x, depth + 1)?;
                self.push(Op::Smoothstep, a, b, c)
            }
            TerrainExpr::Step { edge, x } => {
                let a = self.compile_expr(edge, depth + 1)?;
                let b = self.compile_expr(x, depth + 1)?;
                self.push(Op::Step, a, b, NO_OPERAND)
            }
            TerrainExpr::SmoothMin { a, b, k } => {
                if !k.is_finite() || *k <= 0.0 {
                    return Err("smooth-min/smooth-max k must be finite and positive".into());
                }
                let a = self.compile_expr(a, depth + 1)?;
                let b = self.compile_expr(b, depth + 1)?;
                self.push(Op::SmoothMin(*k), a, b, NO_OPERAND)
            }
            TerrainExpr::SmoothMax { a, b, k } => {
                if !k.is_finite() || *k <= 0.0 {
                    return Err("smooth-min/smooth-max k must be finite and positive".into());
                }
                let a = self.compile_expr(a, depth + 1)?;
                let b = self.compile_expr(b, depth + 1)?;
                self.push(Op::SmoothMax(*k), a, b, NO_OPERAND)
            }
            TerrainExpr::ScaleBias { value, scale, bias } => {
                finite("scale", *scale)?;
                finite("bias", *bias)?;
                let a = self.compile_expr(value, depth + 1)?;
                self.push(
                    Op::ScaleBias {
                        scale: *scale,
                        bias: *bias,
                    },
                    a,
                    NO_OPERAND,
                    NO_OPERAND,
                )
            }
            TerrainExpr::Curve { x, points } => {
                validate_curve(points)?;
                let a = self.compile_expr(x, depth + 1)?;
                let index = self.push_curve(points)?;
                self.push(Op::Curve(index), a, NO_OPERAND, NO_OPERAND)
            }
        }
    }
}

fn compile_biomes(specs: &[BiomeSpec]) -> Result<Box<[BiomeDef]>, String> {
    if specs.is_empty() {
        return Err("terrain must declare at least one biome".into());
    }
    if specs.len() > MAX_BIOMES {
        return Err(format!("terrain declares more than {MAX_BIOMES} biomes"));
    }
    let mut biomes = Vec::with_capacity(specs.len());
    for spec in specs {
        if spec.name.is_empty() || spec.name.len() > 64 {
            return Err("biome names must be non-empty and at most 64 bytes".into());
        }
        if !matches!(spec.surface, GRASS | DIRT | STONE | SAND) {
            return Err(format!(
                "biome {} surface must be grass, dirt, stone or sand",
                spec.name
            ));
        }
        if !matches!(spec.subsurface, GRASS | DIRT | STONE | SAND) {
            return Err(format!(
                "biome {} subsurface must be grass, dirt, stone or sand",
                spec.name
            ));
        }
        if spec.depth > MAX_SOIL_DEPTH {
            return Err(format!(
                "biome {} soil depth must be at most {MAX_SOIL_DEPTH}",
                spec.name
            ));
        }
        for (label, (low, high)) in [
            ("temperature", spec.temperature),
            ("moisture", spec.moisture),
        ] {
            if !low.is_finite() || !high.is_finite() || low > high || low < -2.0 || high > 3.0 {
                return Err(format!(
                    "biome {} {label} range must be finite, ordered and within -2..=3",
                    spec.name
                ));
            }
        }
        let (low, high) = spec.height;
        if !low.is_finite() || !high.is_finite() || low > high || low < -1000.0 || high > 5000.0 {
            return Err(format!(
                "biome {} height range must be finite, ordered and within -1000..=5000",
                spec.name
            ));
        }
        biomes.push(BiomeDef {
            name: spec.name.clone().into_boxed_str(),
            surface: spec.surface,
            subsurface: spec.subsurface,
            depth: spec.depth,
            temperature: spec.temperature,
            moisture: spec.moisture,
            height: spec.height,
        });
    }
    Ok(biomes.into_boxed_slice())
}

fn compile_scatter(spec: &ScatterSpec, biome_count: usize) -> Result<CompiledScatter, String> {
    if spec.species.len() > MAX_SPECIES {
        return Err(format!(
            "terrain declares more than {MAX_SPECIES} scatter species"
        ));
    }
    let mut species = Vec::with_capacity(spec.species.len());
    for entry in &spec.species {
        let name = entry.name.as_str();
        if name.is_empty() || name.len() > 64 {
            return Err("species names must be non-empty and at most 64 bytes".into());
        }
        if species
            .iter()
            .any(|existing: &SpeciesDef| &*existing.name == name)
        {
            return Err(format!("duplicate scatter species {name:?}"));
        }
        if entry.model.is_empty() || entry.model.len() > 256 || !entry.model.ends_with(".glb") {
            return Err(format!(
                "species {name} model must be a relative .glb path of at most 256 bytes"
            ));
        }
        if !entry.spacing.is_finite() || entry.spacing <= 0.0 || entry.spacing > MAX_SCATTER_SPACING
        {
            return Err(format!(
                "species {name} spacing must be finite and in (0, {MAX_SCATTER_SPACING}]"
            ));
        }
        if !entry.density.is_finite() || !(0.0..=1.0).contains(&entry.density) {
            return Err(format!("species {name} density must be finite and in 0..=1"));
        }
        if !entry.slope_max.is_finite() || entry.slope_max < 0.0 {
            return Err(format!("species {name} slope-max must be finite and non-negative"));
        }
        for (label, (low, high)) in [
            ("altitude", entry.altitude),
            ("moisture", entry.moisture),
            ("scale", entry.scale),
        ] {
            if !low.is_finite() || !high.is_finite() || low > high {
                return Err(format!(
                    "species {name} {label} range must be finite and ordered"
                ));
            }
        }
        if entry.scale.0 <= 0.0 {
            return Err(format!("species {name} scale must be positive"));
        }
        if !entry.sink.is_finite() || !(0.0..=MAX_SCATTER_SINK).contains(&entry.sink) {
            return Err(format!(
                "species {name} sink must be finite and in 0..={MAX_SCATTER_SINK}"
            ));
        }
        let (frequency, threshold) = entry.cluster;
        if !frequency.is_finite() || frequency < 0.0 {
            return Err(format!("species {name} cluster frequency must be finite and non-negative"));
        }
        if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
            return Err(format!(
                "species {name} cluster threshold must be finite and in 0..=1"
            ));
        }
        species.push(SpeciesDef {
            name: entry.name.clone().into_boxed_str(),
            model: entry.model.clone().into_boxed_str(),
            spacing: entry.spacing,
            density: entry.density,
            slope_max: entry.slope_max,
            altitude: entry.altitude,
            moisture: entry.moisture,
            scale: entry.scale,
            sink: entry.sink,
            cluster: entry.cluster,
        });
    }
    if !spec.biomes.is_empty() && spec.biomes.len() != biome_count {
        return Err(format!(
            "scatter rules must cover all {biome_count} biomes, received {}",
            spec.biomes.len()
        ));
    }
    let mut biomes = Vec::with_capacity(biome_count);
    for index in 0..biome_count {
        let list = spec.biomes.get(index).map_or(&[][..], |list| list.as_slice());
        for &species_index in list {
            if usize::from(species_index) >= species.len() {
                return Err(format!(
                    "biome {index} references unknown species index {species_index}"
                ));
            }
        }
        biomes.push(list.to_vec().into_boxed_slice());
    }
    Ok(CompiledScatter {
        species: species.into_boxed_slice(),
        biomes: biomes.into_boxed_slice(),
    })
}

impl TerrainGenerator {
    /// Compile an authoring description into an immutable generator.
    pub fn compile(spec: TerrainSpec) -> Result<Self, String> {
        if spec.identity.is_empty() || spec.identity.len() > 256 {
            return Err("generator identity must be non-empty and at most 256 bytes".into());
        }
        if spec.version == 0 {
            return Err("generator version must be at least 1".into());
        }
        if !spec.stone_slope.is_finite() || spec.stone_slope <= 0.0 || spec.stone_slope > 32.0 {
            return Err("stone slope must be finite and in (0, 32]".into());
        }
        let biomes = compile_biomes(&spec.biomes)?;
        let scatter = compile_scatter(&spec.scatter, biomes.len())?;
        let mut compiler = Compiler {
            nodes: Vec::new(),
            curves: Vec::new(),
        };
        let height = compiler.compile_expr(&spec.height, 0)?;
        let temperature = compiler.compile_expr(&spec.temperature, 0)?;
        let moisture = compiler.compile_expr(&spec.moisture, 0)?;
        let soil = compiler.compile_expr(&spec.soil, 0)?;
        let graph = CompiledGraph {
            nodes: compiler.nodes.into_boxed_slice(),
            curves: compiler.curves.into_boxed_slice(),
            biomes,
            height,
            temperature,
            moisture,
            soil,
            identity: spec.identity.into_boxed_str(),
            version: spec.version,
            stone_slope: spec.stone_slope,
            scatter,
        };
        let generator = Self {
            inner: Arc::new(graph),
        };
        generator.validate()?;
        Ok(generator)
    }

    /// Stable diagnostic identity of the authoring source.
    pub fn identity(&self) -> &str {
        &self.inner.identity
    }

    /// Authoring version reported by the package.
    pub fn version(&self) -> u32 {
        self.inner.version
    }

    pub fn node_count(&self) -> usize {
        self.inner.nodes.len()
    }

    pub fn biome_count(&self) -> usize {
        self.inner.biomes.len()
    }

    pub fn biome_name(&self, biome: Biome) -> &str {
        self.inner
            .biomes
            .get(biome.0 as usize)
            .map_or("unknown", |b| &b.name)
    }

    pub(crate) fn stone_slope(&self) -> f32 {
        self.inner.stone_slope
    }

    /// Evaluate one world column. Deterministic in `(x, z, seed)` and
    /// independent of call order.
    pub fn sample(&self, x: i64, z: i64, seed: u64) -> TerrainSample {
        let mut scratch = [0.0f64; MAX_GRAPH_NODES];
        self.sample_into(x, z, seed, &mut scratch).0
    }

    /// Exact minimum and maximum generated surface heights for the 32x32
    /// columns of chunk `(cx, cz)`.
    pub fn column_bounds(&self, cx: i32, cz: i32, seed: u64) -> (i32, i32) {
        self.column_grid(cx, cz, seed).interior_bounds()
    }

    /// Number of compiled scatter species.
    pub fn species_count(&self) -> usize {
        self.inner.scatter.species.len()
    }

    /// Authoring name of a compiled species index.
    pub fn species_name(&self, index: u16) -> &str {
        self.inner
            .scatter
            .species
            .get(index as usize)
            .map_or("unknown", |entry| &entry.name)
    }

    /// Model path of a compiled species index, relative to its package assets.
    pub fn species_model(&self, index: u16) -> &str {
        self.inner
            .scatter
            .species
            .get(index as usize)
            .map_or("", |entry| &entry.model)
    }

    /// Deterministic scatter instances whose anchor falls inside chunk `coord`.
    ///
    /// Candidates come from a world-aligned jittered grid per species, so the
    /// result is independent of chunk boundaries and call order: every instance
    /// belongs to exactly one chunk, the one holding its surface block. `y` is
    /// the rendered surface height less the species sink, so a model authored
    /// with its base at the origin sits on the ground.
    pub fn scatter_chunk(&self, coord: IVec3, seed: u64) -> Vec<ScatterInstance> {
        if self.inner.scatter.species.is_empty() {
            return Vec::new();
        }
        self.scatter_column(coord.x, coord.z, seed)
            .remove(&coord.y)
            .unwrap_or_default()
    }

    /// Every deterministic scatter instance anchored in chunk column
    /// `(cx, cz)`, grouped by the chunk row holding its surface block. Same
    /// candidates and per-chunk ordering as [`Self::scatter_chunk`], but the
    /// shared column work is paid once instead of per row.
    fn scatter_column(&self, cx: i32, cz: i32, seed: u64) -> BTreeMap<i32, Vec<ScatterInstance>> {
        let scatter = &self.inner.scatter;
        let mut by_row = BTreeMap::new();
        if scatter.species.is_empty() {
            return by_row;
        }
        let base_x = i64::from(cx) * i64::from(CHUNK_SIZE);
        let base_z = i64::from(cz) * i64::from(CHUNK_SIZE);
        let end_x = base_x + i64::from(CHUNK_SIZE);
        let end_z = base_z + i64::from(CHUNK_SIZE);
        let mut scratch = [0.0f64; MAX_GRAPH_NODES];
        for (index, species) in scatter.species.iter().enumerate() {
            let spacing = f64::from(species.spacing);
            // One cell of slack: a cell whose origin sits just outside the
            // chunk can still jitter its anchor inside it.
            let cx0 = (base_x as f64 / spacing).floor() as i64 - 1;
            let cx1 = (end_x as f64 / spacing).ceil() as i64;
            let cz0 = (base_z as f64 / spacing).floor() as i64 - 1;
            let cz1 = (end_z as f64 / spacing).ceil() as i64;
            let salt = seed ^ SCATTER_SALT ^ (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            for cz in cz0..=cz1 {
                for cx in cx0..=cx1 {
                    let wx = (cx as f64 + hash01(cx, cz, salt ^ 0x11)) * spacing;
                    let wz = (cz as f64 + hash01(cx, cz, salt ^ 0x22)) * spacing;
                    if wx < base_x as f64 || wx >= end_x as f64 {
                        continue;
                    }
                    if wz < base_z as f64 || wz >= end_z as f64 {
                        continue;
                    }
                    if hash01(cx, cz, salt ^ 0x33) > f64::from(species.density) {
                        continue;
                    }
                    if species.cluster.0 > 0.0 {
                        let mask = value_noise(
                            wx * f64::from(species.cluster.0),
                            wz * f64::from(species.cluster.0),
                            salt ^ 0x44,
                        );
                        if !(mask > f64::from(species.cluster.1)) {
                            continue;
                        }
                    }
                    let x = wx.floor() as i64;
                    let z = wz.floor() as i64;
                    let sample = self.sample_into(x, z, seed, &mut scratch).0;
                    // Scatter is a column property, but the interest set loads
                    // several chunks per column. Assign each instance to the
                    // chunk holding its surface block so the vertical stack
                    // emits it exactly once.
                    let row = sample.height.div_euclid(CHUNK_SIZE);
                    let allowed = scatter
                        .biomes
                        .get(sample.biome.0 as usize)
                        .is_some_and(|list| list.contains(&(index as u16)));
                    if !allowed {
                        continue;
                    }
                    let height = sample.raw_height;
                    if height < species.altitude.0 || height > species.altitude.1 {
                        continue;
                    }
                    if sample.moisture < species.moisture.0 || sample.moisture > species.moisture.1
                    {
                        continue;
                    }
                    if self.slope_at(x, z, sample.height, seed, &mut scratch) > species.slope_max {
                        continue;
                    }
                    let scale = species.scale.0
                        + (species.scale.1 - species.scale.0) * hash01(cx, cz, salt ^ 0x55) as f32;
                    let yaw = (hash01(cx, cz, salt ^ 0x66) * std::f64::consts::TAU) as f32;
                    by_row.entry(row).or_insert_with(Vec::new).push(ScatterInstance {
                        species: index as u16,
                        x: wx as f32,
                        // The mesher contours the density field, whose ramp
                        // crosses zero at `raw_height`, so that — not the
                        // rounded block top — is where the surface renders.
                        // `sink` buries the base so a slope's downhill edge
                        // does not float.
                        y: sample.raw_height - species.sink,
                        z: wz as f32,
                        yaw,
                        scale,
                    });
                }
            }
        }
        by_row
    }

    /// Largest absolute height delta to the four axis neighbours, in blocks.
    fn slope_at(
        &self,
        x: i64,
        z: i64,
        height: i32,
        seed: u64,
        scratch: &mut [f64; MAX_GRAPH_NODES],
    ) -> f32 {
        let mut max = 0;
        for (dx, dz) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
            let neighbour = self.sample_into(x + dx, z + dz, seed, scratch).0.height;
            max = max.max((neighbour - height).abs());
        }
        max as f32
    }

    fn sample_into(
        &self,
        x: i64,
        z: i64,
        seed: u64,
        scratch: &mut [f64; MAX_GRAPH_NODES],
    ) -> (TerrainSample, bool) {
        let finite_values = self.inner.eval_all(x as f64, z as f64, seed, scratch);
        let raw_height = scratch[self.inner.height as usize];
        let temperature = scratch[self.inner.temperature as usize].clamp(0.0, 1.0) as f32;
        let moisture = scratch[self.inner.moisture as usize].clamp(0.0, 1.0) as f32;
        let soil_factor = scratch[self.inner.soil as usize].clamp(0.0, 1.0) as f32;
        let height = clamp_height(raw_height);
        let height_for_biome = if raw_height.is_finite() {
            raw_height as f32
        } else {
            height as f32
        };
        let pick = hash01(x, z, seed ^ 0xB10B_E5EED) as f32;
        let (biome, depth) = self
            .inner
            .select_biome(temperature, moisture, height_for_biome, pick);
        let definition = &self.inner.biomes[biome as usize];
        let soil = (soil_factor * depth).round() as u8;
        (
            TerrainSample {
                height,
                raw_height: raw_height as f32,
                biome: Biome(biome),
                surface: definition.surface,
                subsurface: definition.subsurface,
                soil: soil.min(MAX_SOIL_DEPTH),
                temperature,
                moisture,
            },
            finite_values,
        )
    }

    fn column_grid(&self, cx: i32, cz: i32, seed: u64) -> ColumnGrid {
        let base_x = i64::from(cx) * i64::from(CHUNK_SIZE);
        let base_z = i64::from(cz) * i64::from(CHUNK_SIZE);
        let mut samples = Vec::with_capacity(ColumnGrid::WIDTH * ColumnGrid::WIDTH);
        let mut scratch = [0.0f64; MAX_GRAPH_NODES];
        for dz in -1..=CHUNK_SIZE {
            for dx in -1..=CHUNK_SIZE {
                samples.push(
                    self.sample_into(
                        base_x + i64::from(dx),
                        base_z + i64::from(dz),
                        seed,
                        &mut scratch,
                    )
                    .0,
                );
            }
        }
        ColumnGrid { samples }
    }

    /// Prepare one chunk column for repeated use: the sampled column grid
    /// (surface bounds plus the margins chunk generation needs), the cave
    /// shelter field, and every scatter instance grouped by destination chunk
    /// row. Producing one [`PreparedColumn`] per `(cx, cz)` lets a survey and
    /// several stacked chunk fills share all column-scoped work.
    pub fn prepare_column(&self, cx: i32, cz: i32, seed: u64) -> PreparedColumn {
        PreparedColumn {
            grid: self.column_grid(cx, cz, seed),
            shelter: shelter_field(cx, cz, self, seed),
            scatter: self.scatter_column(cx, cz, seed),
            stone_slope: self.stone_slope(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        let mut scratch = [0.0f64; MAX_GRAPH_NODES];
        for seed in [0u64, 1, 0xDEAD_BEEF] {
            for (x, z) in [
                (0i64, 0i64),
                (-1024, 777),
                (4096, -4096),
                (300_000, 300_000),
                (-250_000, 64_000),
            ] {
                if !self.sample_into(x, z, seed, &mut scratch).1 {
                    return Err(format!(
                        "terrain graph produced a non-finite value at x={x} z={z} seed={seed}"
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Default for TerrainGenerator {
    /// The native mirror of the shipped `packages/terrain` default package.
    /// `game_packages` tests assert that sampling both produces identical
    /// columns so the two authoring sources cannot drift silently.
    fn default() -> Self {
        Self::compile(default_spec()).expect("the built-in terrain graph is valid")
    }
}

impl CompiledGraph {
    fn select_biome(&self, temperature: f32, moisture: f32, height: f32, pick: f32) -> (u16, f32) {
        fn membership(value: f32, range: (f32, f32), width: f32) -> f32 {
            if range.0 == range.1 {
                return if value == range.0 { 1.0 } else { 0.0 };
            }
            let width = width.min((range.1 - range.0) * 0.5);
            let t = ((value - range.0).min(range.1 - value) / width).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        }
        let last = self.biomes.len() - 1;
        let mut remaining = 1.0;
        let mut total = 0.0;
        let mut depth = 0.0;
        let mut selected = None;
        for (index, biome) in self.biomes.iter().enumerate() {
            let coverage = if index == last {
                1.0
            } else {
                membership(temperature, biome.temperature, 0.08)
                    * membership(moisture, biome.moisture, 0.08)
                    * membership(height, biome.height, 12.0)
            };
            let weight = remaining * coverage;
            remaining -= weight;
            total += weight;
            depth += weight * f32::from(biome.depth);
            if selected.is_none() && pick < total {
                selected = Some(index as u16);
            }
        }
        (selected.unwrap_or(last as u16), depth)
    }

    #[inline]
    fn eval_all(&self, x: f64, z: f64, seed: u64, values: &mut [f64; MAX_GRAPH_NODES]) -> bool {
        let mut finite_values = true;
        for index in 0..self.nodes.len() {
            let node = self.nodes[index];
            let value = self.eval_node(node, values, x, z, seed);
            if value.is_finite() {
                values[index] = value;
            } else {
                values[index] = 0.0;
                finite_values = false;
            }
        }
        finite_values
    }

    #[inline]
    fn eval_node(
        &self,
        node: Node,
        values: &[f64; MAX_GRAPH_NODES],
        x: f64,
        z: f64,
        seed: u64,
    ) -> f64 {
        let a = if node.a == NO_OPERAND {
            0.0
        } else {
            values[node.a as usize]
        };
        let b = if node.b == NO_OPERAND {
            0.0
        } else {
            values[node.b as usize]
        };
        let c = if node.c == NO_OPERAND {
            0.0
        } else {
            values[node.c as usize]
        };
        match node.op {
            Op::Constant(value) => f64::from(value),
            Op::X => x,
            Op::Z => z,
            Op::Noise(spec) => f64::from(noise2(&spec, a, b, seed)),
            Op::Add => a + b,
            Op::Subtract => a - b,
            Op::Multiply => a * b,
            Op::Divide => {
                if b.abs() < f64::from(DIV_EPSILON) {
                    0.0
                } else {
                    a / b
                }
            }
            Op::Min => a.min(b),
            Op::Max => a.max(b),
            Op::Abs => a.abs(),
            Op::Negate => -a,
            Op::Sqrt => a.max(0.0).sqrt(),
            Op::Pow(exponent) => a.powf(f64::from(exponent)),
            Op::Clamp => {
                if b <= c {
                    a.max(b).min(c)
                } else {
                    a
                }
            }
            Op::Mix => {
                let t = c.clamp(0.0, 1.0);
                a + (b - a) * t
            }
            Op::Smoothstep => {
                if a == b {
                    if c < a { 0.0 } else { 1.0 }
                } else {
                    let t = ((c - a) / (b - a)).clamp(0.0, 1.0);
                    t * t * (3.0 - 2.0 * t)
                }
            }
            // `a` is the edge, `b` the input.
            Op::Step => {
                if b < a {
                    0.0
                } else {
                    1.0
                }
            }
            Op::SmoothMin(k) => {
                let k = f64::from(k);
                let h = ((k - (a - b).abs()) / k).clamp(0.0, 1.0);
                a.min(b) - h * h * k * 0.25
            }
            Op::SmoothMax(k) => {
                let k = f64::from(k);
                let h = ((k - (a - b).abs()) / k).clamp(0.0, 1.0);
                a.max(b) + h * h * k * 0.25
            }
            Op::ScaleBias { scale, bias } => a * f64::from(scale) + f64::from(bias),
            Op::Curve(index) => curve_eval(&self.curves[index as usize], a),
        }
    }
}

fn clamp_height(raw: f64) -> i32 {
    let value = if raw.is_finite() { raw } else { 0.0 };
    (value.round() as i64).clamp(i64::from(WORLD_MIN_Y + 1), i64::from(WORLD_MAX_Y - 4)) as i32
}

fn curve_eval(curve: &Curve, x: f64) -> f64 {
    let xs = &curve.xs;
    if x <= f64::from(xs[0]) {
        return f64::from(curve.ys[0]);
    }
    let last = xs.len() - 1;
    if x >= f64::from(xs[last]) {
        return f64::from(curve.ys[last]);
    }
    let mut low = 0usize;
    while low + 1 < xs.len() && f64::from(xs[low + 1]) < x {
        low += 1;
    }
    // Validated points have strictly increasing x. Tiny positive spans are valid.
    let span = f64::from(xs[low + 1]) - f64::from(xs[low]);
    let t = ((x - f64::from(xs[low])) / span).clamp(0.0, 1.0);
    let smooth = t * t * (3.0 - 2.0 * t);
    f64::from(curve.ys[low]) + (f64::from(curve.ys[low + 1]) - f64::from(curve.ys[low])) * smooth
}

// ---------------------------------------------------------------------------
// Noise
// ---------------------------------------------------------------------------

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn hash01(ix: i64, iz: i64, salt: u64) -> f64 {
    let value = splitmix64(
        (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (iz as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ salt,
    );
    (value >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
}

fn smooth01(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

fn value_noise(x: f64, z: f64, salt: u64) -> f64 {
    // Explicit-coordinate graph nodes can exceed the integer lattice range.
    // Mark them invalid instead of overflowing neighbour arithmetic.
    const LIMIT: f64 = i64::MAX as f64 - 4096.0;
    if !x.is_finite() || !z.is_finite() || x.abs() >= LIMIT || z.abs() >= LIMIT {
        return f64::NAN;
    }
    let x0 = x.floor();
    let z0 = z.floor();
    let tx = x - x0;
    let tz = z - z0;
    let ix = x0 as i64;
    let iz = z0 as i64;
    let u = smooth01(tx);
    let v = smooth01(tz);
    let a = hash01(ix, iz, salt);
    let b = hash01(ix + 1, iz, salt);
    let c = hash01(ix, iz + 1, salt);
    let d = hash01(ix + 1, iz + 1, salt);
    let ab = a + (b - a) * u;
    let cd = c + (d - c) * u;
    ab + (cd - ab) * v
}

fn noise2(spec: &NoiseSpec, x: f64, z: f64, seed: u64) -> f32 {
    let mut frequency = f64::from(spec.frequency);
    let mut amplitude = 1.0f64;
    let mut sum = 0.0f64;
    let mut norm = 0.0f64;
    let base_salt = seed ^ spec.salt;
    for octave in 0..spec.octaves {
        let salt = base_salt ^ (u64::from(octave)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut value = value_noise(x * frequency, z * frequency, salt);
        if spec.ridged {
            let ridge = 1.0 - (2.0 * value - 1.0).abs();
            value = ridge * ridge;
        }
        sum += value * amplitude;
        norm += amplitude;
        amplitude *= f64::from(spec.gain);
        frequency *= f64::from(spec.lacunarity);
    }
    if norm > 0.0 { (sum / norm) as f32 } else { 0.0 }
}

// ---------------------------------------------------------------------------
// Caves: seeded 3D value noise carving density below the surface
// ---------------------------------------------------------------------------

/// World blocks per lattice cell of the lowest cave octave.
const CAVE_FREQUENCY: f64 = 0.02;
const CAVE_OCTAVES: u8 = 3;
/// fBm band that carves: below `CAVE_LO` nothing is removed, above `CAVE_HI`
/// the full `CAVE_CARVE` is subtracted.
const CAVE_LO: f64 = 0.62;
const CAVE_HI: f64 = 0.8;
/// Maximum density removed by a cave cell. Exceeds the i8 range so a fully
/// developed cave opens even inside saturated stone.
const CAVE_CARVE: f64 = 200.0;
/// Carving fades in over this many blocks below the surface so the terrain
/// skin is never swiss cheese.
const CAVE_DEPTH_FADE: f64 = 4.0;
const CAVE_SALT: u64 = 0xCA7E_5EED_CA7E_5EED;

fn hash3(ix: i64, iy: i64, iz: i64, salt: u64) -> f64 {
    let value = splitmix64(
        (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ (iy as u64).wrapping_mul(0x85EB_CA6B_8F3B_5B4D)
            ^ (iz as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
            ^ salt,
    );
    (value >> 11) as f64 * (1.0 / ((1u64 << 53) as f64))
}

fn value_noise3(x: f64, y: f64, z: f64, salt: u64) -> f64 {
    const LIMIT: f64 = i64::MAX as f64 - 4096.0;
    if !x.is_finite()
        || !y.is_finite()
        || !z.is_finite()
        || x.abs() >= LIMIT
        || y.abs() >= LIMIT
        || z.abs() >= LIMIT
    {
        return f64::NAN;
    }
    let x0 = x.floor();
    let y0 = y.floor();
    let z0 = z.floor();
    let u = smooth01(x - x0);
    let v = smooth01(y - y0);
    let w = smooth01(z - z0);
    let (ix, iy, iz) = (x0 as i64, y0 as i64, z0 as i64);
    let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
    let z0_face = lerp(
        lerp(
            hash3(ix, iy, iz, salt),
            hash3(ix + 1, iy, iz, salt),
            u,
        ),
        lerp(
            hash3(ix, iy + 1, iz, salt),
            hash3(ix + 1, iy + 1, iz, salt),
            u,
        ),
        v,
    );
    let z1_face = lerp(
        lerp(
            hash3(ix, iy, iz + 1, salt),
            hash3(ix + 1, iy, iz + 1, salt),
            u,
        ),
        lerp(
            hash3(ix, iy + 1, iz + 1, salt),
            hash3(ix + 1, iy + 1, iz + 1, salt),
            u,
        ),
        v,
    );
    lerp(z0_face, z1_face, w)
}

fn cave_fbm(x: f64, y: f64, z: f64, seed: u64) -> f64 {
    let mut frequency = CAVE_FREQUENCY;
    let mut amplitude = 1.0f64;
    let mut sum = 0.0f64;
    let mut norm = 0.0f64;
    let base_salt = seed ^ CAVE_SALT;
    for octave in 0..CAVE_OCTAVES {
        let salt = base_salt ^ (u64::from(octave)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        sum += value_noise3(x * frequency, y * frequency, z * frequency, salt) * amplitude;
        norm += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    sum / norm
}

/// Upper bound of `cave_fbm` over the world box `[x, x+size) x [y, y+size) x
/// [z, z+size)`. Value noise is a convex blend of lattice-corner hashes, so
/// per octave the maximum corner hash over the covering lattice bounds the
/// field everywhere inside. Lets generation keep the uniform-stone fast path
/// only where no cave can reach the chunk.
fn cave_fbm_max(x: i32, y: i32, z: i32, size: i32, seed: u64) -> f64 {
    let mut frequency = CAVE_FREQUENCY;
    let mut amplitude = 1.0f64;
    let mut bound = 0.0f64;
    let mut norm = 0.0f64;
    let base_salt = seed ^ CAVE_SALT;
    for octave in 0..CAVE_OCTAVES {
        let salt = base_salt ^ (u64::from(octave)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let x0 = (f64::from(x) * frequency).floor() as i64;
        let x1 = (f64::from(x + size - 1) * frequency).floor() as i64;
        let y0 = (f64::from(y) * frequency).floor() as i64;
        let y1 = (f64::from(y + size - 1) * frequency).floor() as i64;
        let z0 = (f64::from(z) * frequency).floor() as i64;
        let z1 = (f64::from(z + size - 1) * frequency).floor() as i64;
        let mut octave_max = 0.0f64;
        for iz in z0..=z1 + 1 {
            for iy in y0..=y1 + 1 {
                for ix in x0..=x1 + 1 {
                    octave_max = octave_max.max(hash3(ix, iy, iz, salt));
                }
            }
        }
        bound += octave_max * amplitude;
        norm += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    bound / norm
}

/// Signed terrain density at one world voxel: a ramp crossing zero at
/// `raw_height`, saturated before caves carve so tunnels open at any depth.
/// `shelter` is the lowest surface height in the column's neighborhood: cave
/// depth is measured from it so a cave deep under a plateau cannot breach a
/// hillside whose own surface is lower.
fn terrain_density(x: i64, y: i64, z: i64, raw_height: f32, shelter: f32, seed: u64) -> i8 {
    let base = ((f64::from(raw_height) - y as f64) * 64.0)
        .max(f64::from(DENSITY_AIR))
        .min(f64::from(DENSITY_SOLID));
    let depth = f64::from(shelter) - CAVE_DEPTH_FADE - y as f64;
    if base <= f64::from(DENSITY_AIR) || depth <= 0.0 {
        return base as i8;
    }
    let band = (cave_fbm(x as f64, y as f64, z as f64, seed) - CAVE_LO) / (CAVE_HI - CAVE_LO);
    if band <= 0.0 {
        return base as i8;
    }
    let carve = smooth01(band.min(1.0)) * CAVE_CARVE * (depth / CAVE_DEPTH_FADE).min(1.0);
    (base - carve).max(f64::from(DENSITY_AIR)) as i8
}

// ---------------------------------------------------------------------------
// Column sampling and chunk filling
// ---------------------------------------------------------------------------

pub(crate) struct ColumnGrid {
    samples: Vec<TerrainSample>,
}

impl ColumnGrid {
    const WIDTH: usize = (CHUNK_SIZE + 2) as usize;

    pub(crate) fn at(&self, x: i32, z: i32) -> TerrainSample {
        self.samples[(z + 1) as usize * Self::WIDTH + (x + 1) as usize]
    }

    pub(crate) fn interior_bounds(&self) -> (i32, i32) {
        let mut minimum = i32::MAX;
        let mut maximum = i32::MIN;
        for z in 0..CHUNK_SIZE {
            for x in 0..CHUNK_SIZE {
                let height = self.at(x, z).height;
                minimum = minimum.min(height);
                maximum = maximum.max(height);
            }
        }
        (minimum, maximum)
    }

    pub(crate) fn slope(&self, x: i32, z: i32) -> i32 {
        let height = self.at(x, z).height;
        [
            self.at(x - 1, z).height,
            self.at(x + 1, z).height,
            self.at(x, z - 1).height,
            self.at(x, z + 1).height,
        ]
        .iter()
        .map(|neighbour| (neighbour - height).abs())
        .max()
        .unwrap_or(0)
    }
}

/// Per-column material bands resolved once per chunk: `cap` is the topmost
/// voxel that can hold solid density (`ceil(raw_height) - 1`), `soil_bottom`
/// the first y of the stone layer.
#[derive(Clone, Copy)]
struct Column {
    raw_height: f32,
    cap: i32,
    soil_bottom: i32,
    surface: u8,
    subsurface: u8,
}

// Shelter field: the lowest raw surface within ~20 blocks, sampled on a
// stride-4 lattice. Caves measure depth from this, not the column's own
// surface, so tunnels under high ground cannot open through lower slopes.
// The 3-block margin covers the strided sample missing the true minimum.
const SHELTER_RADIUS: i64 = 20;
const SHELTER_STRIDE: i64 = 4;
const SHELTER_MARGIN: f32 = 3.0;
const SHELTER_W: usize = (2 * SHELTER_RADIUS / SHELTER_STRIDE + 1) as usize;

/// Raw surface height on the stride-4 shelter lattice covering the column's
/// ±SHELTER_RADIUS neighborhood. Column-scoped: it depends only on
/// `(cx, cz, seed, generator)`.
fn shelter_field(
    cx: i32,
    cz: i32,
    generator: &TerrainGenerator,
    seed: u64,
) -> [f32; SHELTER_W * SHELTER_W] {
    let base_x = i64::from(cx) * i64::from(CHUNK_SIZE);
    let base_z = i64::from(cz) * i64::from(CHUNK_SIZE);
    let mut shelter_grid = [f32::INFINITY; SHELTER_W * SHELTER_W];
    for sz in 0..SHELTER_W {
        for sx in 0..SHELTER_W {
            shelter_grid[sz * SHELTER_W + sx] = generator
                .sample(
                    base_x + (sx as i64 * SHELTER_STRIDE - SHELTER_RADIUS),
                    base_z + (sz as i64 * SHELTER_STRIDE - SHELTER_RADIUS),
                    seed,
                )
                .raw_height;
        }
    }
    shelter_grid
}

/// Lowest raw surface within `SHELTER_RADIUS` of `(x, z)` — the column's own
/// surface included — less `SHELTER_MARGIN`.
fn shelter_at(
    shelter_grid: &[f32; SHELTER_W * SHELTER_W],
    x: i64,
    z: i64,
    own: f32,
) -> f32 {
    let mut low = own;
    for sz in 0..SHELTER_W {
        for sx in 0..SHELTER_W {
            let dx = sx as i64 * SHELTER_STRIDE - SHELTER_RADIUS - x;
            let dz = sz as i64 * SHELTER_STRIDE - SHELTER_RADIUS - z;
            if dx * dx + dz * dz <= SHELTER_RADIUS * SHELTER_RADIUS {
                low = low.min(shelter_grid[sz * SHELTER_W + sx]);
            }
        }
    }
    low - SHELTER_MARGIN
}

/// All column-scoped generation state for chunk column `(cx, cz)`: the
/// sampled surface grid, the cave shelter field, the scatter instances
/// grouped by destination chunk row, and the generator's stone slope.
/// Prepare it once with [`TerrainGenerator::prepare_column`] and reuse it for
/// the column's survey and every interested chunk row.
pub struct PreparedColumn {
    grid: ColumnGrid,
    shelter: [f32; SHELTER_W * SHELTER_W],
    scatter: BTreeMap<i32, Vec<ScatterInstance>>,
    stone_slope: f32,
}

impl PreparedColumn {
    /// Exact minimum and maximum generated surface heights across the
    /// column's 32x32 interior.
    pub fn bounds(&self) -> (i32, i32) {
        self.grid.interior_bounds()
    }

    /// Deterministic scatter instances whose surface block sits in chunk row
    /// `y`, in the same order [`TerrainGenerator::scatter_chunk`] emits them.
    pub fn scatter(&self, y: i32) -> Vec<ScatterInstance> {
        self.scatter.get(&y).map_or_else(Vec::new, |v| v.to_vec())
    }
}

/// Rows outside the generated world collapse to uniform storage without
/// sampling the column.
fn boundary_chunk(coord: IVec3) -> Option<Chunk> {
    if coord.y < crate::MIN_CHUNK_Y {
        return Some(Chunk {
            revision: 0,
            storage: Storage::Uniform(STONE),
            runs_cache: Mutex::new(None),
        });
    }
    if coord.y > crate::MAX_CHUNK_Y {
        return Some(Chunk {
            revision: 0,
            storage: Storage::Uniform(AIR),
            runs_cache: Mutex::new(None),
        });
    }
    None
}

/// Chunks whose density is provably saturated and uncarved collapse to
/// uniform storage without touching individual cells.
fn saturated_chunk(coord: IVec3, seed: u64, min_height: i32, max_height: i32) -> Option<Chunk> {
    let base_y = coord.y * CHUNK_SIZE;
    let top_y = base_y + CHUNK_SIZE - 1;
    // raw_height < height + 0.5, so three blocks above the tallest column the
    // density ramp has saturated at DENSITY_AIR and caves never reach.
    if base_y >= max_height + 3 {
        return Some(Chunk {
            revision: 0,
            storage: Storage::Uniform(AIR),
            runs_cache: Mutex::new(None),
        });
    }
    // Uniform stone requires every voxel saturated solid (top_y at least two
    // below the lowest raw surface), below every soil band, and beyond the
    // reach of any cave cell.
    if top_y < min_height - i32::from(MAX_SOIL_DEPTH) - 1
        && cave_fbm_max(
            coord.x * CHUNK_SIZE,
            base_y,
            coord.z * CHUNK_SIZE,
            CHUNK_SIZE,
            seed,
        ) <= CAVE_LO
    {
        return Some(Chunk {
            revision: 0,
            storage: Storage::Uniform(STONE),
            runs_cache: Mutex::new(None),
        });
    }
    None
}

/// Write every voxel of `coord`'s chunk from prepared column state.
fn fill_chunk(
    coord: IVec3,
    seed: u64,
    grid: &ColumnGrid,
    shelter_grid: &[f32; SHELTER_W * SHELTER_W],
    stone_slope: f32,
) -> Chunk {
    let base_y = coord.y * CHUNK_SIZE;
    let mut columns = [Column {
        raw_height: 0.0,
        cap: 0,
        soil_bottom: 0,
        surface: AIR,
        subsurface: AIR,
    }; (CHUNK_SIZE * CHUNK_SIZE) as usize];
    for local_z in 0..CHUNK_SIZE {
        for local_x in 0..CHUNK_SIZE {
            let sample = grid.at(local_x, local_z);
            let (surface, subsurface, soil) = if grid.slope(local_x, local_z) as f32 >= stone_slope
            {
                (STONE, STONE, 0i32)
            } else {
                (sample.surface, sample.subsurface, i32::from(sample.soil))
            };
            // `cap` is the topmost voxel that can hold solid density; clamping
            // to `height` keeps the surface band at the world ceiling when the
            // raw graph height overshoots the clamp.
            let cap = ((f64::from(sample.raw_height).ceil() - 1.0) as i32).min(sample.height);
            columns[(local_z * CHUNK_SIZE + local_x) as usize] = Column {
                raw_height: sample.raw_height,
                cap,
                soil_bottom: cap.saturating_sub(soil),
                surface,
                subsurface,
            };
        }
    }
    let base_x = i64::from(coord.x) * i64::from(CHUNK_SIZE);
    let base_z = i64::from(coord.z) * i64::from(CHUNK_SIZE);
    // The shelter value depends only on (x, z, column height), not y, so
    // compute one value per column instead of rescanning the lattice for
    // every voxel in the chunk.
    let mut shelter_columns = [0.0f32; (CHUNK_SIZE * CHUNK_SIZE) as usize];
    for local_z in 0..CHUNK_SIZE {
        for local_x in 0..CHUNK_SIZE {
            let idx = (local_z * CHUNK_SIZE + local_x) as usize;
            shelter_columns[idx] = shelter_at(
                shelter_grid,
                i64::from(local_x),
                i64::from(local_z),
                columns[idx].raw_height,
            );
        }
    }
    let mut chunk = Chunk {
        revision: 0,
        storage: Storage::Uniform(AIR),
        runs_cache: Mutex::new(None),
    };
    for local_y in 0..CHUNK_SIZE {
        let world_y = base_y + local_y;
        for local_z in 0..CHUNK_SIZE {
            for local_x in 0..CHUNK_SIZE {
                let column = columns[(local_z * CHUNK_SIZE + local_x) as usize];
                let density = terrain_density(
                    base_x + i64::from(local_x),
                    i64::from(world_y),
                    base_z + i64::from(local_z),
                    column.raw_height,
                    shelter_columns[(local_z * CHUNK_SIZE + local_x) as usize],
                    seed,
                );
                let material = if density <= 0 {
                    AIR
                } else if world_y >= column.cap {
                    column.surface
                } else if world_y >= column.soil_bottom {
                    column.subsurface
                } else {
                    STONE
                };
                chunk.set_voxel(
                    crate::index(IVec3::new(local_x, local_y, local_z)),
                    Voxel {
                        material,
                        density,
                        placed: false,
                    },
                );
            }
        }
    }
    chunk.compact();
    chunk
}

/// Fill one chunk from a compiled generator.
pub(crate) fn generate_chunk(coord: IVec3, seed: u64, generator: &TerrainGenerator) -> Chunk {
    if let Some(chunk) = boundary_chunk(coord) {
        return chunk;
    }
    let grid = generator.column_grid(coord.x, coord.z, seed);
    let (min_height, max_height) = grid.interior_bounds();
    if let Some(chunk) = saturated_chunk(coord, seed, min_height, max_height) {
        return chunk;
    }
    let shelter = shelter_field(coord.x, coord.z, generator, seed);
    fill_chunk(coord, seed, &grid, &shelter, generator.stone_slope())
}

/// Fill one chunk of a column already prepared with
/// [`TerrainGenerator::prepare_column`]. Chunks whose density is provably
/// saturated and uncarved collapse to uniform storage without touching
/// individual cells; everything else writes full voxel state.
pub(crate) fn generate_chunk_from_column(
    coord: IVec3,
    seed: u64,
    column: &PreparedColumn,
) -> Chunk {
    if let Some(chunk) = boundary_chunk(coord) {
        return chunk;
    }
    let (min_height, max_height) = column.bounds();
    if let Some(chunk) = saturated_chunk(coord, seed, min_height, max_height) {
        return chunk;
    }
    fill_chunk(
        coord,
        seed,
        &column.grid,
        &column.shelter,
        column.stone_slope,
    )
}

// ---------------------------------------------------------------------------
// Built-in default graph (native mirror of `packages/terrain/server.scm`)
// ---------------------------------------------------------------------------

fn default_spec() -> TerrainSpec {
    use TerrainExpr as E;

    fn fbm(frequency: f64, octaves: u8, lacunarity: f64, gain: f64, salt: u64) -> E {
        E::Noise {
            spec: NoiseSpec {
                frequency: frequency as f32,
                octaves,
                lacunarity: lacunarity as f32,
                gain: gain as f32,
                salt,
                ridged: false,
            },
            x: Box::new(E::X),
            z: Box::new(E::Z),
        }
    }

    fn ridged(frequency: f64, octaves: u8, lacunarity: f64, gain: f64, salt: u64) -> E {
        E::Noise {
            spec: NoiseSpec {
                frequency: frequency as f32,
                octaves,
                lacunarity: lacunarity as f32,
                gain: gain as f32,
                salt,
                ridged: true,
            },
            x: Box::new(E::X),
            z: Box::new(E::Z),
        }
    }

    fn constant(value: f64) -> E {
        E::Constant(value as f32)
    }

    fn add(l: E, r: E) -> E {
        E::Add(Box::new(l), Box::new(r))
    }

    fn sub(l: E, r: E) -> E {
        E::Subtract(Box::new(l), Box::new(r))
    }

    fn mul(l: E, r: E) -> E {
        E::Multiply(Box::new(l), Box::new(r))
    }

    fn clamp(value: E, low: E, high: E) -> E {
        E::Clamp {
            value: Box::new(value),
            low: Box::new(low),
            high: Box::new(high),
        }
    }

    fn smoothstep(edge0: E, edge1: E, x: E) -> E {
        E::Smoothstep {
            edge0: Box::new(edge0),
            edge1: Box::new(edge1),
            x: Box::new(x),
        }
    }

    fn scale_bias(value: E, scale: f64, bias: f64) -> E {
        E::ScaleBias {
            value: Box::new(value),
            scale: scale as f32,
            bias: bias as f32,
        }
    }

    fn pow(value: E, exponent: f64) -> E {
        E::Pow(Box::new(value), exponent as f32)
    }

    // Height: broad gentle plains, concentrated ridged mountain regions, and a
    // connected valley network carved where the ridged field's complement is high.
    let base = constant(44.0);
    let plains = fbm(0.010416667, 3, 2.0, 0.5, 102);
    let plains_relief = mul(scale_bias(plains, 1.0, -0.5), constant(12.0));
    let mask = smoothstep(
        constant(0.52),
        constant(0.74),
        fbm(0.0013020833, 3, 2.0, 0.55, 103),
    );
    let ridge = pow(ridged(0.004464286, 5, 2.0, 0.5, 104), 2.0);
    let relief = mul(mask, ridge);
    let mountains = mul(relief.clone(), constant(340.0));
    let valley = smoothstep(
        constant(0.42),
        constant(0.78),
        sub(constant(1.0), ridged(0.003125, 4, 2.0, 0.5, 105)),
    );
    let carve = mul(mul(valley, sub(constant(1.0), relief)), constant(38.0));
    let height = clamp(
        sub(add(add(base, plains_relief), mountains), carve),
        constant(2.0),
        constant(350.0),
    );

    // Temperature: a broad climate field cooled by the column's actual altitude,
    // so high ground is cold rather than following the mountain mask directly.
    let temperature = clamp(
        sub(
            fbm(0.0011111111, 3, 2.0, 0.5, 106),
            mul(
                smoothstep(constant(80.0), constant(260.0), height.clone()),
                constant(0.5),
            ),
        ),
        constant(0.0),
        constant(1.0),
    );
    let moisture = clamp(
        fbm(0.0015625, 3, 2.0, 0.5, 107),
        constant(0.0),
        constant(1.0),
    );
    let soil = clamp(
        add(
            mul(
                scale_bias(fbm(0.0078125, 3, 2.0, 0.5, 108), 1.0, -0.5),
                constant(0.6),
            ),
            constant(0.5),
        ),
        constant(0.0),
        constant(1.0),
    );

    TerrainSpec {
        identity: "jtech-terrain-v1".into(),
        version: 1,
        stone_slope: 4.0,
        height,
        temperature,
        moisture,
        soil,
        biomes: vec![
            BiomeSpec {
                name: "shore".into(),
                surface: SAND,
                subsurface: SAND,
                depth: 3,
                temperature: (-2.0, 3.0),
                moisture: (-2.0, 3.0),
                height: (-1000.0, 6.0),
            },
            BiomeSpec {
                name: "desert".into(),
                surface: SAND,
                subsurface: SAND,
                depth: 5,
                temperature: (0.62, 3.0),
                moisture: (-2.0, 0.34),
                height: (-1000.0, 1000.0),
            },
            BiomeSpec {
                name: "mountains".into(),
                surface: STONE,
                subsurface: STONE,
                depth: 1,
                temperature: (-2.0, 3.0),
                moisture: (-2.0, 3.0),
                height: (150.0, 5000.0),
            },
            BiomeSpec {
                name: "tundra".into(),
                surface: GRASS,
                subsurface: DIRT,
                depth: 2,
                temperature: (-2.0, 0.3),
                moisture: (-2.0, 3.0),
                height: (-1000.0, 5000.0),
            },
            BiomeSpec {
                name: "plains".into(),
                surface: GRASS,
                subsurface: DIRT,
                depth: 4,
                temperature: (-2.0, 3.0),
                moisture: (-2.0, 3.0),
                height: (-1000.0, 5000.0),
            },
        ],
        // Biome order is shore, desert, mountains, tundra, plains.
        scatter: ScatterSpec {
            species: vec![
                SpeciesSpec {
                    name: "oak".into(),
                    model: "oak.glb".into(),
                    spacing: 6.0,
                    density: 0.5,
                    slope_max: 0.6,
                    altitude: (0.0, 120.0),
                    moisture: (0.25, 1.0),
                    scale: (0.8, 1.4),
                    sink: 0.15,
                    cluster: (0.012, 0.52),
                },
                SpeciesSpec {
                    name: "pine".into(),
                    model: "pine.glb".into(),
                    spacing: 5.0,
                    density: 0.45,
                    slope_max: 0.8,
                    altitude: (0.0, 200.0),
                    moisture: (0.15, 1.0),
                    scale: (0.9, 1.6),
                    sink: 0.15,
                    cluster: (0.014, 0.5),
                },
                SpeciesSpec {
                    name: "boulder".into(),
                    model: "boulder.glb".into(),
                    spacing: 9.0,
                    density: 0.3,
                    slope_max: 1.5,
                    altitude: (0.0, 400.0),
                    moisture: (0.0, 1.0),
                    scale: (0.6, 1.4),
                    sink: 0.35,
                    cluster: (0.0, 0.0),
                },
            ],
            biomes: vec![
                vec![2],
                vec![2],
                vec![2],
                vec![1, 2],
                vec![0, 2],
            ],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant(value: f32) -> TerrainExpr {
        TerrainExpr::Constant(value)
    }
    fn spec(expr: TerrainExpr) -> TerrainSpec {
        TerrainSpec {
            identity: "test".into(),
            version: 1,
            stone_slope: 4.0,
            height: expr.clone(),
            temperature: constant(0.5),
            moisture: constant(0.5),
            soil: constant(0.5),
            biomes: vec![BiomeSpec {
                name: "test".into(),
                surface: GRASS,
                subsurface: DIRT,
                depth: 4,
                temperature: (-2.0, 3.0),
                moisture: (-2.0, 3.0),
                height: (-1000.0, 5000.0),
            }],
            scatter: ScatterSpec::default(),
        }
    }

    #[test]
    fn default_generator_is_within_world_and_deterministic() {
        let generator = TerrainGenerator::default();
        assert_eq!(generator.identity(), "jtech-terrain-v1");
        assert_eq!(generator.version(), 1);
        assert!(generator.node_count() <= MAX_GRAPH_NODES);
        for seed in [0u64, 1, 42] {
            for x in [-4096i64, -33, 0, 17, 4096] {
                for z in [-2048i64, 0, 513, 9000] {
                    let first = generator.sample(x, z, seed);
                    let second = generator.sample(x, z, seed);
                    assert_eq!(first, second, "sampling is not deterministic");
                    assert!(
                        (WORLD_MIN_Y..=WORLD_MAX_Y).contains(&first.height),
                        "height {} outside world",
                        first.height
                    );
                    assert!(first.soil <= 5);
                }
            }
        }
    }

    #[test]
    fn sampling_is_order_independent() {
        let generator = TerrainGenerator::default();
        let coords: Vec<(i64, i64)> = (0..64).map(|i| (i * 97 - 3000, i * 131 - 5000)).collect();
        let forward: Vec<_> = coords
            .iter()
            .map(|&(x, z)| generator.sample(x, z, 9))
            .collect();
        let mut reverse: Vec<_> = coords
            .iter()
            .rev()
            .map(|&(x, z)| generator.sample(x, z, 9))
            .collect();
        reverse.reverse();
        assert_eq!(forward, reverse);
    }

    #[test]
    fn seeds_change_the_terrain() {
        let generator = TerrainGenerator::default();
        let changed = (0..256i64).any(|i| {
            let x = i * 53 - 4000;
            let z = i * 91 - 2000;
            generator.sample(x, z, 1).height != generator.sample(x, z, 2).height
        });
        assert!(changed, "seed does not influence the default graph");
    }

    #[test]
    fn column_bounds_match_generated_surface_heights() {
        let generator = TerrainGenerator::default();
        for &(cx, cz) in &[(0, 0), (-3, 7), (12, -5), (-40, -40)] {
            let (low, high) = generator.column_bounds(cx, cz, 5);
            let mut expected_low = i32::MAX;
            let mut expected_high = i32::MIN;
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    let height = generator
                        .sample(
                            i64::from(cx) * i64::from(CHUNK_SIZE) + i64::from(x),
                            i64::from(cz) * i64::from(CHUNK_SIZE) + i64::from(z),
                            5,
                        )
                        .height;
                    expected_low = expected_low.min(height);
                    expected_high = expected_high.max(height);
                }
            }
            assert_eq!((low, high), (expected_low, expected_high));
        }
    }

    #[test]
    fn chunk_layering_is_slope_aware_and_has_no_wood() {
        let generator = TerrainGenerator::default();
        let probe = generator.sample(5, 5, 3);
        let probe_cap = (probe.raw_height.ceil() as i32 - 1).min(probe.height);
        let coord = IVec3::new(0, probe_cap.div_euclid(CHUNK_SIZE), 0);
        let chunk = Chunk::generate_with(coord, 3, &generator);
        let mut checked_surface = false;
        for local_z in 0..CHUNK_SIZE {
            for local_x in 0..CHUNK_SIZE {
                let world_x = i64::from(local_x);
                let world_z = i64::from(local_z);
                let sample = generator.sample(world_x, world_z, 3);
                let cap = (sample.raw_height.ceil() as i32 - 1).min(sample.height);
                // The topmost solid voxel is cap or cap - 1: a raw_height
                // just above an integer truncates the cap's ramp density to 0.
                let mut top = None;
                for local_y in (0..CHUNK_SIZE).rev() {
                    let local = IVec3::new(local_x, local_y, local_z);
                    if chunk.density(local) > 0 {
                        top = Some((local_y, local));
                        break;
                    }
                }
                let Some((top_y, top_local)) = top else {
                    continue;
                };
                let world_top = coord.y * CHUNK_SIZE + top_y;
                assert!(
                    world_top >= cap - 1,
                    "top solid at {world_top} fell below cap - 1 = {}",
                    cap - 1
                );
                let density = chunk.density(top_local);
                assert!(
                    density < DENSITY_SOLID,
                    "surface density {density} is saturated, not a ramp value"
                );
                assert_ne!(chunk.get(top_local), AIR, "surface column missing a block");
                for local_y in top_y + 1..CHUNK_SIZE {
                    let above = IVec3::new(local_x, local_y, local_z);
                    assert!(chunk.density(above) <= 0);
                    assert_eq!(chunk.get(above), AIR, "air must sit above the surface");
                }
                checked_surface = true;
            }
        }
        assert!(
            checked_surface,
            "no surface column landed in the test chunk"
        );
        for y in 0..CHUNK_SIZE {
            for z in 0..CHUNK_SIZE {
                for x in 0..CHUNK_SIZE {
                    assert_ne!(chunk.get(IVec3::new(x, y, z)), crate::WOOD);
                }
            }
        }
    }

    #[test]
    fn chunk_seams_follow_world_coordinates_across_all_axes() {
        let ramp = TerrainExpr::Add(Box::new(TerrainExpr::X), Box::new(TerrainExpr::Z));
        let generator = TerrainGenerator::compile(spec(ramp)).unwrap();
        for cz in -1..=0 {
            for cx in -1..=0 {
                for cy in -3..=1 {
                    let coord = IVec3::new(cx, cy, cz);
                    let chunk = Chunk::generate_with(coord, 11, &generator);
                    for z in [0, 1, 30, 31] {
                        for x in [0, 1, 30, 31] {
                            let world_x = i64::from(cx * CHUNK_SIZE + x);
                            let world_z = i64::from(cz * CHUNK_SIZE + z);
                            let raw = (world_x + world_z) as f32;
                            let cap = raw.ceil() as i32 - 1;
                            for y in 0..CHUNK_SIZE {
                                let wy = cy * CHUNK_SIZE + y;
                                let local = IVec3::new(x, y, z);
                                // Above the carve fade the density is the pure
                                // height ramp; deeper down caves may subtract.
                                let density = chunk.density(local);
                                if wy >= cap - 3 {
                                    let expected = ((raw - wy as f32) * 64.0)
                                        .clamp(f32::from(DENSITY_AIR), f32::from(DENSITY_SOLID))
                                        as i8;
                                    assert_eq!(
                                        density, expected,
                                        "coord={coord} local=({x},{y},{z}) raw={raw}"
                                    );
                                }
                                let expected = if density <= 0 {
                                    AIR
                                } else if wy >= cap {
                                    GRASS
                                } else if wy >= cap - 2 {
                                    DIRT
                                } else {
                                    STONE
                                };
                                assert_eq!(
                                    chunk.get(local),
                                    expected,
                                    "coord={coord} local=({x},{y},{z}) raw={raw}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn default_terrain_has_plains_valleys_and_mountains() {
        let generator = TerrainGenerator::default();
        let mut heights = Vec::new();
        let mut biomes = std::collections::HashSet::new();
        for z in 0..128i64 {
            for x in 0..128i64 {
                let sample = generator.sample(x * 256 - 16_384, z * 256 - 16_384, 2024);
                heights.push(sample.height);
                biomes.insert(sample.biome.0);
            }
        }
        heights.sort_unstable();
        let minimum = heights[0];
        let maximum = heights[heights.len() - 1];
        let median = heights[heights.len() / 2];
        assert!(maximum - minimum > 120, "terrain is not varied enough");
        assert!(maximum > 150, "no mountains were produced: max {maximum}");
        assert!(median < 90, "plains are not dominant: median {median}");
        assert!(
            heights.iter().filter(|&&h| (25..=90).contains(&h)).count() * 2 > heights.len(),
            "less than half the sampled terrain is lowland"
        );
        assert!(biomes.len() >= 2, "expected several biomes, got {biomes:?}");
    }

    #[test]
    fn compile_rejects_bounded_graph_violations() {
        let deep = (0..MAX_GRAPH_DEPTH + 4).fold(constant(1.0), |value, _| {
            TerrainExpr::Add(Box::new(value), Box::new(constant(0.0)))
        });
        assert!(TerrainGenerator::compile(spec(deep)).is_err());

        let mut wide = constant(0.0);
        for _ in 0..MAX_GRAPH_NODES {
            wide = TerrainExpr::Add(Box::new(wide), Box::new(constant(1.0)));
        }
        assert!(TerrainGenerator::compile(spec(wide)).is_err());

        let non_finite = TerrainExpr::Pow(Box::new(constant(-1.0)), 0.5);
        assert!(TerrainGenerator::compile(spec(non_finite)).is_err());

        let mut bad_biome = spec(constant(40.0));
        bad_biome.biomes[0].surface = crate::WOOD;
        assert!(TerrainGenerator::compile(bad_biome).is_err());

        let mut no_biomes = spec(constant(40.0));
        no_biomes.biomes.clear();
        assert!(TerrainGenerator::compile(no_biomes).is_err());

        let bad_noise = TerrainExpr::Noise {
            spec: NoiseSpec {
                frequency: 0.0,
                octaves: 0,
                lacunarity: 2.0,
                gain: 0.5,
                salt: 0,
                ridged: false,
            },
            x: Box::new(TerrainExpr::X),
            z: Box::new(TerrainExpr::Z),
        };
        assert!(TerrainGenerator::compile(spec(bad_noise)).is_err());
    }

    fn constant_graph_builds_a_flat_world() {
        let generator = TerrainGenerator::compile(spec(constant(40.0))).unwrap();
        let sample = generator.sample(-9, 12345, 0);
        assert_eq!(sample.height, 40);
        assert_eq!(sample.raw_height, 40.0);
        assert_eq!(sample.surface, GRASS);
        assert_eq!(sample.subsurface, DIRT);
        // raw_height 40 puts the density zero-crossing exactly on y=40: the
        // cap voxel at y=39 carries the surface material and a half-ramp
        // density, y=40 is air.
        let chunk = Chunk::generate_with(IVec3::new(0, 1, 0), 0, &generator);
        assert_eq!(chunk.get(IVec3::new(0, 8, 0)), AIR);
        assert_eq!(chunk.density(IVec3::new(0, 8, 0)), 0);
        assert_eq!(chunk.get(IVec3::new(0, 7, 0)), GRASS);
        assert_eq!(chunk.density(IVec3::new(0, 7, 0)), 64);
        assert_eq!(chunk.get(IVec3::new(0, 6, 0)), DIRT);
        assert_eq!(chunk.get(IVec3::new(0, 5, 0)), DIRT);
        assert_eq!(chunk.get(IVec3::new(0, 4, 0)), STONE);
    }
    #[test]
    fn mask_primitives_have_correct_edges_and_midpoints() {
        for (x, expected) in [
            (-1.0, 0.0),
            (0.0, 0.0),
            (0.25, 0.15625),
            (0.5, 0.5),
            (1.0, 1.0),
            (2.0, 1.0),
        ] {
            let expression = TerrainExpr::Smoothstep {
                edge0: Box::new(constant(0.0)),
                edge1: Box::new(constant(1.0)),
                x: Box::new(constant(x)),
            };
            let mut description = spec(constant(30.0));
            description.temperature = expression;
            let generator = TerrainGenerator::compile(description).unwrap();
            assert_eq!(generator.sample(0, 0, 0).temperature, expected);
        }
        for (x, expected) in [(0.0, 0.0), (0.5, 1.0), (1.0, 1.0)] {
            let mut description = spec(constant(30.0));
            description.temperature = TerrainExpr::Step {
                edge: Box::new(constant(0.5)),
                x: Box::new(constant(x)),
            };
            let generator = TerrainGenerator::compile(description).unwrap();
            assert_eq!(generator.sample(0, 0, 0).temperature, expected);
        }
    }

    #[test]
    fn soil_crosses_vertical_chunk_boundaries() {
        for height in [-32.0, 0.0, 32.0, 64.0] {
            let generator = TerrainGenerator::compile(spec(constant(height))).unwrap();
            let coord = IVec3::new(0, height as i32 / CHUNK_SIZE - 1, 0);
            let chunk = Chunk::generate_with(coord, 0, &generator);
            // Integer raw_height puts the cap voxel at height - 1; the soil
            // band sits directly under it.
            assert_eq!(chunk.get(IVec3::new(0, 31, 0)), GRASS);
            assert_eq!(chunk.get(IVec3::new(0, 30, 0)), DIRT);
            assert_eq!(chunk.get(IVec3::new(0, 29, 0)), DIRT);
            assert_eq!(chunk.get(IVec3::new(0, 28, 0)), STONE);
        }
    }

    #[test]
    fn caves_carve_air_pockets_below_the_surface() {
        let generator = TerrainGenerator::default();
        let mut found = false;
        'outer: for seed in [0u64, 1, 7, 42, 2024] {
            for coord in [
                IVec3::new(0, -1, 0),
                IVec3::new(0, -2, 0),
                IVec3::new(1, -1, 0),
                IVec3::new(-1, -2, 1),
                IVec3::new(2, -3, -1),
            ] {
                let chunk = Chunk::generate_with(coord, seed, &generator);
                if matches!(chunk.storage, Storage::Uniform(_)) {
                    continue;
                }
                for local_z in 0..CHUNK_SIZE {
                    for local_x in 0..CHUNK_SIZE {
                        let sample = generator.sample(
                            i64::from(coord.x * CHUNK_SIZE + local_x),
                            i64::from(coord.z * CHUNK_SIZE + local_z),
                            seed,
                        );
                        let cap = sample.raw_height.ceil() as i32 - 1;
                        for local_y in 0..CHUNK_SIZE {
                            let world_y = coord.y * CHUNK_SIZE + local_y;
                            // Air strictly below the cap is a carved pocket:
                            // without caves every voxel under the cap is solid.
                            if world_y < cap
                                && chunk.get(IVec3::new(local_x, local_y, local_z)) == AIR
                            {
                                assert!(
                                    chunk.density(IVec3::new(local_x, local_y, local_z)) <= 0
                                );
                                found = true;
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }
        assert!(found, "no cave pocket found across seeds and chunks");
    }

    #[test]
    fn tiny_curve_spans_and_extreme_coordinates_are_defined() {
        let curve = Curve {
            xs: vec![0.0, 5e-7, 1.0].into(),
            ys: vec![0.0, 1.0, 0.0].into(),
        };
        assert_eq!(curve_eval(&curve, 5e-7), 1.0);
        assert!(value_noise(1e30, 0.0, 0).is_nan());
        let generator = TerrainGenerator::default();
        assert!(Chunk::generate_with(IVec3::new(0, i32::MAX, 0), 0, &generator).is_empty());
        assert_eq!(
            Chunk::generate_with(IVec3::new(0, i32::MIN, 0), 0, &generator).get(IVec3::ZERO),
            STONE
        );
    }
    #[test]
    fn far_coordinates_above_f32_precision_stay_distinct() {
        // (x - 2^24) + 300 stays in world range while every tracked coordinate is
        // at or above 2^24, where f32 cannot represent x and x + 1 distinctly.
        let height = TerrainExpr::Add(
            Box::new(TerrainExpr::Subtract(
                Box::new(TerrainExpr::X),
                Box::new(constant(16_777_216.0)),
            )),
            Box::new(constant(300.0)),
        );
        let generator = TerrainGenerator::compile(spec(height)).unwrap();
        assert_eq!(16_777_216i64 as f32, (16_777_216i64 + 1) as f32);
        let mut previous = None;
        for offset in 0..8i64 {
            let x = 16_777_216 + offset;
            let sample = generator.sample(x, -7, 3);
            assert_eq!(sample.height, 300 + offset as i32, "height lost x={x}");
            if let Some(previous) = previous {
                assert_ne!(
                    sample.height, previous,
                    "adjacent far columns collapsed at x={x}"
                );
            }
            previous = Some(sample.height);
        }
    }

    #[test]
    fn weighted_biome_transitions_blend_soil_and_materials_deterministically() {
        fn rule(
            name: &str,
            surface: u8,
            subsurface: u8,
            depth: u8,
            temperature: (f32, f32),
        ) -> BiomeSpec {
            BiomeSpec {
                name: name.into(),
                surface,
                subsurface,
                depth,
                temperature,
                moisture: (-2.0, 3.0),
                height: (-1000.0, 5000.0),
            }
        }

        fn recipe(temperature: f32) -> TerrainGenerator {
            let mut description = spec(constant(50.0));
            description.temperature = constant(temperature);
            description.soil = constant(1.0);
            description.biomes = vec![
                rule("low", GRASS, DIRT, 8, (0.0, 0.5)),
                rule("high", SAND, SAND, 2, (0.4, 0.9)),
            ];
            TerrainGenerator::compile(description).unwrap()
        }

        // In the overlap both rules contribute: the depth lands strictly between
        // the two declared depths and every column keeps a declared material pair.
        let transition = recipe(0.47);
        let probe = transition.sample(1234, -99, 5);
        assert_eq!(probe, transition.sample(1234, -99, 5));
        assert!(
            probe.soil > 2 && probe.soil < 8,
            "soil {} is not blended",
            probe.soil
        );
        assert!(matches!(probe.surface, GRASS | SAND));
        assert_eq!(
            probe.subsurface,
            if probe.surface == GRASS { DIRT } else { SAND }
        );

        // Material choice is stable and both weighted picks appear over the map.
        let mut surfaces = std::collections::HashSet::new();
        for i in 0..512i64 {
            let x = i * 31 - 4000;
            let z = i * 17 + 250;
            let sample = transition.sample(x, z, 5);
            assert_eq!(sample, transition.sample(x, z, 5));
            assert!(matches!(sample.surface, GRASS | SAND));
            surfaces.insert(sample.surface);
        }
        assert_eq!(
            surfaces,
            std::collections::HashSet::from([GRASS, SAND]),
            "weighted material selection did not mix"
        );

        // Above the high range the last rule owns every column.
        let hot = recipe(0.85);
        for i in 0..64i64 {
            let sample = hot.sample(i * 29 - 900, i * 13 + 40, 5);
            assert_eq!(sample.surface, SAND);
            assert_eq!(sample.subsurface, SAND);
        }

        // At the cold end the low rule leads but the weighted fallback still
        // contributes, which is the documented priority order.
        let cold = recipe(0.05);
        let mut grass = 0usize;
        let mut sand = 0usize;
        for i in 0..512i64 {
            match cold.sample(i * 31 - 40, i * 17 + 25, 5).surface {
                GRASS => grass += 1,
                SAND => sand += 1,
                other => panic!("unexpected surface {other}"),
            }
        }
        assert!(
            grass > sand,
            "cold end should favour grass: {grass} vs {sand}"
        );
    }

    #[test]
    fn landscape_measurement_report() {
        let generator = TerrainGenerator::default();
        let mut heights = Vec::new();
        let mut biomes = std::collections::BTreeMap::new();
        for z in 0..256i64 {
            for x in 0..256i64 {
                let sample = generator.sample(x * 128 - 16_384, z * 128 - 16_384, 2024);
                heights.push(sample.height);
                *biomes
                    .entry(generator.biome_name(sample.biome).to_string())
                    .or_insert(0usize) += 1;
            }
        }
        heights.sort_unstable();
        let at = |p: f64| heights[((heights.len() - 1) as f64 * p) as usize];
        eprintln!(
            "landscape min={} p01={} p05={} p10={} p25={} p50={} p75={} p90={} p99={} max={}",
            heights[0],
            at(0.01),
            at(0.05),
            at(0.10),
            at(0.25),
            at(0.50),
            at(0.75),
            at(0.90),
            at(0.99),
            heights[heights.len() - 1]
        );
        for (name, count) in &biomes {
            eprintln!(
                "biome {name}: {:.2}%",
                *count as f64 / heights.len() as f64 * 100.0
            );
        }
        eprintln!(
            "lowland 25..=90 {:.1}%  high >=150 {:.2}%  valley <30 {:.2}%  valley <10 {:.2}%  floor<=2 {:.2}%  >=250 {:.3}%",
            heights.iter().filter(|&&h| (25..=90).contains(&h)).count() as f64
                / heights.len() as f64
                * 100.0,
            heights.iter().filter(|&&h| h >= 150).count() as f64 / heights.len() as f64 * 100.0,
            heights.iter().filter(|&&h| h < 30).count() as f64 / heights.len() as f64 * 100.0,
            heights.iter().filter(|&&h| h < 10).count() as f64 / heights.len() as f64 * 100.0,
            heights.iter().filter(|&&h| h <= 2).count() as f64 / heights.len() as f64 * 100.0,
            heights.iter().filter(|&&h| h >= 250).count() as f64 / heights.len() as f64 * 100.0
        );
        // Guard the shipped landscape shape: broad dominant plains, high but
        // concentrated mountains, and valley channels that avoid the floor clamp.
        assert!(
            heights
                .iter()
                .all(|&h| (WORLD_MIN_Y..=WORLD_MAX_Y).contains(&h))
        );
        let maximum = heights[heights.len() - 1];
        let lowland = heights.iter().filter(|&&h| (25..=90).contains(&h)).count() as f64
            / heights.len() as f64;
        let high = heights.iter().filter(|&&h| h >= 150).count() as f64 / heights.len() as f64;
        let floor = heights.iter().filter(|&&h| h <= 2).count() as f64 / heights.len() as f64;
        assert!(maximum >= 300, "mountainous regions are too low: {maximum}");
        assert!(high < 0.08, "mountains are not concentrated: {high}");
        assert!(lowland > 0.5, "plains are not dominant: {lowland}");
        assert!(floor < 0.02, "valley floors are over-clamped: {floor}");
    }

    fn species(name: &str) -> SpeciesSpec {
        SpeciesSpec {
            name: name.into(),
            model: format!("{name}.glb"),
            spacing: 6.0,
            density: 1.0,
            slope_max: 100.0,
            altitude: (-1000.0, 5000.0),
            moisture: (-2.0, 3.0),
            scale: (1.0, 1.0),
            sink: 0.0,
            cluster: (0.0, 0.0),
        }
    }

    fn scatter_generator(biomes: Vec<Vec<u16>>) -> TerrainGenerator {
        let mut spec = default_spec();
        spec.scatter = ScatterSpec {
            species: vec![species("oak")],
            biomes,
        };
        TerrainGenerator::compile(spec).expect("scatter spec is valid")
    }

    #[test]
    fn scatter_is_deterministic_and_seam_free() {
        let generator = scatter_generator(vec![vec![0]; 5]);
        let seed = 11;
        let mut all = Vec::new();
        for cx in -3..=3 {
            for cz in -3..=3 {
                // Cover the full vertical stack: an instance belongs to exactly
                // the chunk holding its surface block.
                for cy in -3..=4 {
                    let coord = IVec3::new(cx, cy, cz);
                    let first = generator.scatter_chunk(coord, seed);
                    assert_eq!(
                        first,
                        generator.scatter_chunk(coord, seed),
                        "scatter is not deterministic"
                    );
                    // Every anchor lies inside the chunk that produced it, so
                    // the union over chunks has no duplicates and no seam gaps.
                    for instance in &first {
                        let (lo_x, hi_x) = (cx as f32 * 32.0, (cx as f32 + 1.0) * 32.0);
                        let (lo_z, hi_z) = (cz as f32 * 32.0, (cz as f32 + 1.0) * 32.0);
                        assert!((lo_x..hi_x).contains(&instance.x), "anchor escaped chunk x");
                        assert!((lo_z..hi_z).contains(&instance.z), "anchor escaped chunk z");
                    }
                    all.extend(first);
                }
            }
        }
        assert!(!all.is_empty(), "scatter produced nothing");
        let mut keys: Vec<(i64, i64)> = all
            .iter()
            .map(|i| ((i.x * 4096.0) as i64, (i.z * 4096.0) as i64))
            .collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(total, keys.len(), "duplicate instances across chunks");
    }

    #[test]
    fn scatter_respects_biome_membership_and_ground() {
        // Only the plains biome (index 4) admits the species.
        let generator = scatter_generator(vec![vec![], vec![], vec![], vec![], vec![0]]);
        let seed = 5;
        let mut placed = 0;
        for cx in -8..=8 {
            for cz in -8..=8 {
                for cy in -3..=4 {
                    for instance in generator.scatter_chunk(IVec3::new(cx, cy, cz), seed) {
                        let sample = generator.sample(
                            instance.x.floor() as i64,
                            instance.z.floor() as i64,
                            seed,
                        );
                        assert_eq!(generator.biome_name(sample.biome), "plains");
                        assert_eq!(instance.y, sample.raw_height);
                        assert_eq!(sample.height.div_euclid(32), cy);
                        placed += 1;
                    }
                }
            }
        }
        assert!(placed > 0, "no plains instances were placed");
    }

    #[test]
    fn compile_rejects_invalid_scatter() {
        let mut spec = default_spec();
        spec.scatter.species.push(SpeciesSpec {
            spacing: 0.0,
            ..species("bad")
        });
        assert!(TerrainGenerator::compile(spec).is_err(), "zero spacing accepted");

        let mut spec = default_spec();
        spec.scatter.biomes[0] = vec![99];
        assert!(
            TerrainGenerator::compile(spec).is_err(),
            "unknown species index accepted"
        );

        let mut spec = default_spec();
        spec.scatter.species.push(species("oak"));
        assert!(
            TerrainGenerator::compile(spec).is_err(),
            "duplicate species name accepted"
        );

        let mut spec = default_spec();
        spec.scatter.species[0].sink = -1.0;
        assert!(
            TerrainGenerator::compile(spec).is_err(),
            "negative sink accepted"
        );
    }

    #[test]
    fn sink_buries_the_base_below_the_surface() {
        let mut spec = default_spec();
        spec.scatter.species[0].sink = 0.5;
        spec.scatter.biomes = vec![vec![0]; 5];
        let generator = TerrainGenerator::compile(spec).expect("sink spec is valid");
        let mut placed = 0;
        for cx in -4..=4 {
            for cz in -4..=4 {
                for cy in -2..=3 {
                    for instance in generator.scatter_chunk(IVec3::new(cx, cy, cz), 3) {
                        let sample = generator.sample(
                            instance.x.floor() as i64,
                            instance.z.floor() as i64,
                            3,
                        );
                        assert_eq!(instance.y, sample.raw_height - 0.5);
                        placed += 1;
                    }
                }
            }
        }
        assert!(placed > 0, "no instances to check");
    }
}
