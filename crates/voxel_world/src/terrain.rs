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

use crate::{AIR, CHUNK_SIZE, Chunk, DIRT, GRASS, SAND, STONE, Storage, WORLD_MAX_Y, WORLD_MIN_Y};
use glam::IVec3;
use std::sync::Arc;

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
/// Hard cap on blocks of soil below a surface block.
pub const MAX_SOIL_DEPTH: u8 = 16;

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

/// Fill one chunk from a compiled generator. Empty and solid chunks collapse to
/// a uniform storage without touching individual cells.
pub(crate) fn generate_chunk(coord: IVec3, seed: u64, generator: &TerrainGenerator) -> Chunk {
    if coord.y < crate::MIN_CHUNK_Y {
        return Chunk {
            revision: 0,
            storage: Storage::Uniform(STONE),
        };
    }
    if coord.y > crate::MAX_CHUNK_Y {
        return Chunk {
            revision: 0,
            storage: Storage::Uniform(AIR),
        };
    }
    let grid = generator.column_grid(coord.x, coord.z, seed);
    let (min_height, max_height) = grid.interior_bounds();
    let base_y = coord.y * CHUNK_SIZE;
    let top_y = base_y + CHUNK_SIZE - 1;
    if base_y > max_height {
        return Chunk {
            revision: 0,
            storage: Storage::Uniform(AIR),
        };
    }
    if top_y < min_height - i32::from(MAX_SOIL_DEPTH) {
        return Chunk {
            revision: 0,
            storage: Storage::Uniform(STONE),
        };
    }
    let stone_slope = generator.stone_slope();
    let mut chunk = Chunk {
        revision: 0,
        storage: Storage::Uniform(AIR),
    };
    for local_z in 0..CHUNK_SIZE {
        for local_x in 0..CHUNK_SIZE {
            let sample = grid.at(local_x, local_z);
            let (surface, subsurface, soil) = if grid.slope(local_x, local_z) as f32 >= stone_slope
            {
                (STONE, STONE, 0i32)
            } else {
                (sample.surface, sample.subsurface, i32::from(sample.soil))
            };
            let surface_y = sample.height;
            let soil_bottom = surface_y - soil;
            for local_y in 0..CHUNK_SIZE {
                let world_y = base_y + local_y;
                let block = if world_y > surface_y {
                    AIR
                } else if world_y == surface_y {
                    surface
                } else if world_y >= soil_bottom {
                    subsurface
                } else {
                    STONE
                };
                if block != AIR {
                    chunk.set(crate::index(IVec3::new(local_x, local_y, local_z)), block);
                }
            }
        }
    }
    chunk.compact();
    chunk
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
        let coord = IVec3::new(0, probe.height.div_euclid(CHUNK_SIZE), 0);
        let chunk = Chunk::generate_with(coord, 3, &generator);
        let mut checked_surface = false;
        for local_z in 0..CHUNK_SIZE {
            for local_x in 0..CHUNK_SIZE {
                let world_x = i64::from(local_x);
                let world_z = i64::from(local_z);
                let sample = generator.sample(world_x, world_z, 3);
                let chunk_local_y = sample.height - coord.y * CHUNK_SIZE;
                if !(0..CHUNK_SIZE).contains(&chunk_local_y) {
                    continue;
                }
                let surface = chunk.get(IVec3::new(local_x, chunk_local_y, local_z));
                assert_ne!(surface, AIR, "surface column missing a block");
                if chunk_local_y + 1 < CHUNK_SIZE {
                    assert_eq!(
                        chunk.get(IVec3::new(local_x, chunk_local_y + 1, local_z)),
                        AIR,
                        "air must sit above the surface"
                    );
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
                            let height = cx * CHUNK_SIZE + x + cz * CHUNK_SIZE + z;
                            for y in 0..CHUNK_SIZE {
                                let wy = cy * CHUNK_SIZE + y;
                                let expected = if wy > height {
                                    AIR
                                } else if wy == height {
                                    GRASS
                                } else if wy >= height - 2 {
                                    DIRT
                                } else {
                                    STONE
                                };
                                assert_eq!(
                                    chunk.get(IVec3::new(x, y, z)),
                                    expected,
                                    "coord={coord} local=({x},{y},{z}) height={height}"
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

    #[test]
    fn constant_graph_builds_a_flat_world() {
        let generator = TerrainGenerator::compile(spec(constant(40.0))).unwrap();
        let sample = generator.sample(-9, 12345, 0);
        assert_eq!(sample.height, 40);
        assert_eq!(sample.surface, GRASS);
        assert_eq!(sample.subsurface, DIRT);
        let chunk = Chunk::generate_with(IVec3::new(0, 1, 0), 0, &generator);
        assert_eq!(chunk.get(IVec3::new(0, 8, 0)), GRASS);
        assert_eq!(chunk.get(IVec3::new(0, 7, 0)), DIRT);
        assert_eq!(chunk.get(IVec3::new(0, 4, 0)), STONE);
        assert_eq!(chunk.get(IVec3::new(0, 9, 0)), AIR);
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
            assert_eq!(chunk.get(IVec3::new(0, 31, 0)), DIRT);
            assert_eq!(chunk.get(IVec3::new(0, 30, 0)), DIRT);
            assert_eq!(chunk.get(IVec3::new(0, 29, 0)), STONE);
        }
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
}
