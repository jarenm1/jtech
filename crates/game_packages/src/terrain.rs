//! Scheme-authored native terrain packages.
//!
//! `load_terrain` evaluates `directory/terrain/server.scm` once inside the same
//! bounded sandbox and watchdog used for weapon packages, converts the declared
//! expression graph and biome table into `voxel_world::terrain` values, and
//! compiles them into an immutable [`TerrainGenerator`]. Generation never runs
//! Scheme.

use crate::{LOAD_BUDGET, Vm, diagnostic, read_source};
use std::path::Path;
use steel::{SteelVal, steel_vm::engine::Engine};
use voxel_world::terrain::{
    BiomeSpec, MAX_BIOMES, MAX_CURVE_POINTS, MAX_GRAPH_DEPTH, MAX_GRAPH_NODES, NoiseSpec,
    TERRAIN_API_VERSION, TerrainExpr, TerrainGenerator, TerrainSpec,
};
use voxel_world::{DIRT, GRASS, SAND, STONE};

/// Directory name of the terrain package inside the packages root.
pub const TERRAIN_PACKAGE: &str = "terrain";

/// Composition primitives exposed to `server.scm`. Each returns a plain list so
/// the loader can compile the description without evaluating scheme closures.
const API: &str = r#"
(define (terrain-x) '(x))
(define (terrain-z) '(z))
(define (constant v) (list 'const v))
(define (fbm freq octaves lacunarity gain salt)
  (list 'fbm freq octaves lacunarity gain salt (terrain-x) (terrain-z)))
(define (fbm-xy freq octaves lacunarity gain salt x z)
  (list 'fbm freq octaves lacunarity gain salt x z))
(define (ridged freq octaves lacunarity gain salt)
  (list 'ridged freq octaves lacunarity gain salt (terrain-x) (terrain-z)))
(define (ridged-xy freq octaves lacunarity gain salt x z)
  (list 'ridged freq octaves lacunarity gain salt x z))
(define (tadd a b) (list 'add a b))
(define (tsub a b) (list 'sub a b))
(define (tmul a b) (list 'mul a b))
(define (tdiv a b) (list 'div a b))
(define (tmin a b) (list 'min a b))
(define (tmax a b) (list 'max a b))
(define (tabs v) (list 'abs v))
(define (tneg v) (list 'neg v))
(define (tsqrt v) (list 'sqrt v))
(define (tpow v e) (list 'pow v e))
(define (tclamp v lo hi) (list 'clamp v lo hi))
(define (tmix a b t) (list 'mix a b t))
(define (tsmoothstep e0 e1 x) (list 'smoothstep e0 e1 x))
(define (tstep edge x) (list 'step edge x))
(define (tsmooth-min a b k) (list 'smooth-min a b k))
(define (tsmooth-max a b k) (list 'smooth-max a b k))
(define (tscale-bias v scale bias) (list 'scale-bias v scale bias))
(define (tcurve x points) (list 'curve x points))
(define (biome name surface subsurface depth
                min-temperature max-temperature
                min-moisture max-moisture min-height max-height)
  (list 'biome name surface subsurface depth
        min-temperature max-temperature
        min-moisture max-moisture min-height max-height))
"#;

/// Evaluate `directory/terrain/server.scm` once and compile its terrain graph.
pub fn load_terrain(directory: &Path) -> Result<TerrainGenerator, String> {
    let path = directory.join(TERRAIN_PACKAGE).join("server.scm");
    let source = read_source(&path)?;
    let mut vm = Vm::new()?;
    vm.run(LOAD_BUDGET, |engine| {
        engine.run(API).map_err(|error| diagnostic(engine, error))?;
        engine
            .compile_and_run_raw_program_with_path(source, path.clone())
            .map_err(|error| diagnostic(engine, error))?;
        Ok(())
    })?;
    let engine = &vm.engine;
    let api = integer(engine, "terrain-api-version")?;
    if api != i64::from(TERRAIN_API_VERSION) {
        return Err(format!("terrain-api-version must be {TERRAIN_API_VERSION}"));
    }
    let version = integer(engine, "terrain-version")?;
    if !(1..=i64::from(u32::MAX)).contains(&version) {
        return Err("terrain-version must be a positive 32-bit integer".into());
    }
    let identity = string(engine, "generator-identity")?;
    let stone_slope = number(engine, "stone-slope")? as f32;
    let mut graph_nodes = MAX_GRAPH_NODES;
    let height = expression(engine, "terrain-height", &mut graph_nodes)?;
    let temperature = expression(engine, "terrain-temperature", &mut graph_nodes)?;
    let moisture = expression(engine, "terrain-moisture", &mut graph_nodes)?;
    let soil = expression(engine, "terrain-soil", &mut graph_nodes)?;
    let biomes = biome_list(engine, "biomes")?;
    TerrainGenerator::compile(TerrainSpec {
        identity,
        version: version as u32,
        stone_slope,
        height,
        temperature,
        moisture,
        soil,
        biomes,
    })
}

fn extract(engine: &Engine, name: &str) -> Result<SteelVal, String> {
    engine
        .extract_value(name)
        .map_err(|error| error.to_string())
}

fn number(engine: &Engine, name: &str) -> Result<f64, String> {
    number_value(&extract(engine, name)?, name)
}

fn integer(engine: &Engine, name: &str) -> Result<i64, String> {
    let value = number(engine, name)?;
    if value.fract() != 0.0 {
        return Err(format!("{name} must be an integer"));
    }
    Ok(value as i64)
}

fn string(engine: &Engine, name: &str) -> Result<String, String> {
    match extract(engine, name)? {
        SteelVal::StringV(value) => Ok(value.to_string()),
        _ => Err(format!("{name} must be a string")),
    }
}

fn expression(engine: &Engine, name: &str, budget: &mut usize) -> Result<TerrainExpr, String> {
    expression_from_steel(&extract(engine, name)?, 0, budget)
}

fn number_value(value: &SteelVal, name: &str) -> Result<f64, String> {
    match value {
        SteelVal::IntV(value) => Ok(*value as f64),
        SteelVal::NumV(value) => Ok(*value),
        _ => Err(format!("{name} must be a number")),
    }
}

fn arguments<'a>(value: &'a SteelVal, name: &str) -> Result<Vec<&'a SteelVal>, String> {
    match value {
        // The biome table is the widest valid list. Keep one excess item so all
        // callers reject oversized lists without allocating in proportion to input.
        SteelVal::ListV(items) => Ok(items.iter().take(MAX_BIOMES + 1).collect()),
        _ => Err(format!("{name} must be a list")),
    }
}

fn expect(args: &[&SteelVal], count: usize, name: &str) -> Result<(), String> {
    if args.len() != count {
        Err(format!(
            "{name} expects {count} arguments, received {}",
            args.len()
        ))
    } else {
        Ok(())
    }
}

fn byte_argument(value: &SteelVal, name: &str) -> Result<u8, String> {
    let number = number_value(value, name)?;
    if number.fract() != 0.0 || !(0.0..=255.0).contains(&number) {
        return Err(format!("{name} must be an integer in 0..=255"));
    }
    Ok(number as u8)
}

/// Exclusive float bound for salt conversion. `u64::MAX as f64` rounds up to
/// 2^64, so finite integral floats at or above this value must be rejected
/// rather than silently saturating the `as u64` conversion.
const SALT_FLOAT_LIMIT: f64 = 18_446_744_073_709_551_616.0;

fn salt_argument(value: &SteelVal, name: &str) -> Result<u64, String> {
    match value {
        SteelVal::IntV(value) if *value >= 0 => Ok(*value as u64),
        SteelVal::NumV(value)
            if *value >= 0.0 && value.fract() == 0.0 && *value < SALT_FLOAT_LIMIT =>
        {
            Ok(*value as u64)
        }
        _ => Err(format!("{name} must be a non-negative integer below 2^64")),
    }
}

fn expression_from_steel(
    value: &SteelVal,
    depth: usize,
    budget: &mut usize,
) -> Result<TerrainExpr, String> {
    if depth > MAX_GRAPH_DEPTH {
        return Err(format!(
            "terrain expression exceeds the {MAX_GRAPH_DEPTH} depth limit"
        ));
    }
    if *budget == 0 {
        return Err(format!(
            "terrain expression exceeds the {MAX_GRAPH_NODES} node limit"
        ));
    }
    *budget -= 1;
    match value {
        SteelVal::IntV(value) => Ok(TerrainExpr::Constant(*value as f32)),
        SteelVal::NumV(value) => Ok(TerrainExpr::Constant(*value as f32)),
        SteelVal::ListV(_) => {
            let args = arguments(value, "terrain expression")?;
            let Some(SteelVal::SymbolV(head)) = args.first() else {
                return Err("terrain expression must start with a primitive symbol".into());
            };
            let head = head.to_string();
            let args = &args[1..];
            let binary = |op: fn(Box<TerrainExpr>, Box<TerrainExpr>) -> TerrainExpr,
                          budget: &mut usize|
             -> Result<TerrainExpr, String> {
                expect(args, 2, &head)?;
                Ok(op(
                    Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                    Box::new(expression_from_steel(args[1], depth + 1, budget)?),
                ))
            };
            match head.as_str() {
                "x" => {
                    expect(args, 0, &head)?;
                    Ok(TerrainExpr::X)
                }
                "z" => {
                    expect(args, 0, &head)?;
                    Ok(TerrainExpr::Z)
                }
                "const" => {
                    expect(args, 1, &head)?;
                    Ok(TerrainExpr::Constant(number_value(args[0], "const")? as f32))
                }
                "fbm" | "ridged" => {
                    expect(args, 7, &head)?;
                    Ok(TerrainExpr::Noise {
                        spec: NoiseSpec {
                            frequency: number_value(args[0], "noise frequency")? as f32,
                            octaves: byte_argument(args[1], "noise octaves")?,
                            lacunarity: number_value(args[2], "noise lacunarity")? as f32,
                            gain: number_value(args[3], "noise gain")? as f32,
                            salt: salt_argument(args[4], "noise salt")?,
                            ridged: head == "ridged",
                        },
                        x: Box::new(expression_from_steel(args[5], depth + 1, budget)?),
                        z: Box::new(expression_from_steel(args[6], depth + 1, budget)?),
                    })
                }
                "add" => binary(TerrainExpr::Add, budget),
                "sub" => binary(TerrainExpr::Subtract, budget),
                "mul" => binary(TerrainExpr::Multiply, budget),
                "div" => binary(TerrainExpr::Divide, budget),
                "min" => binary(TerrainExpr::Min, budget),
                "max" => binary(TerrainExpr::Max, budget),
                "abs" => {
                    expect(args, 1, &head)?;
                    Ok(TerrainExpr::Abs(Box::new(expression_from_steel(
                        args[0],
                        depth + 1,
                        budget,
                    )?)))
                }
                "neg" => {
                    expect(args, 1, &head)?;
                    Ok(TerrainExpr::Negate(Box::new(expression_from_steel(
                        args[0],
                        depth + 1,
                        budget,
                    )?)))
                }
                "sqrt" => {
                    expect(args, 1, &head)?;
                    Ok(TerrainExpr::Sqrt(Box::new(expression_from_steel(
                        args[0],
                        depth + 1,
                        budget,
                    )?)))
                }
                "pow" => {
                    expect(args, 2, &head)?;
                    Ok(TerrainExpr::Pow(
                        Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        number_value(args[1], "pow exponent")? as f32,
                    ))
                }
                "clamp" => {
                    expect(args, 3, &head)?;
                    Ok(TerrainExpr::Clamp {
                        value: Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        low: Box::new(expression_from_steel(args[1], depth + 1, budget)?),
                        high: Box::new(expression_from_steel(args[2], depth + 1, budget)?),
                    })
                }
                "mix" => {
                    expect(args, 3, &head)?;
                    Ok(TerrainExpr::Mix {
                        a: Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        b: Box::new(expression_from_steel(args[1], depth + 1, budget)?),
                        t: Box::new(expression_from_steel(args[2], depth + 1, budget)?),
                    })
                }
                "smoothstep" => {
                    expect(args, 3, &head)?;
                    Ok(TerrainExpr::Smoothstep {
                        edge0: Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        edge1: Box::new(expression_from_steel(args[1], depth + 1, budget)?),
                        x: Box::new(expression_from_steel(args[2], depth + 1, budget)?),
                    })
                }
                "step" => {
                    expect(args, 2, &head)?;
                    Ok(TerrainExpr::Step {
                        edge: Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        x: Box::new(expression_from_steel(args[1], depth + 1, budget)?),
                    })
                }
                "smooth-min" | "smooth-max" => {
                    expect(args, 3, &head)?;
                    let a = Box::new(expression_from_steel(args[0], depth + 1, budget)?);
                    let b = Box::new(expression_from_steel(args[1], depth + 1, budget)?);
                    let k = number_value(args[2], "smooth k")? as f32;
                    Ok(if head == "smooth-min" {
                        TerrainExpr::SmoothMin { a, b, k }
                    } else {
                        TerrainExpr::SmoothMax { a, b, k }
                    })
                }
                "scale-bias" => {
                    expect(args, 3, &head)?;
                    Ok(TerrainExpr::ScaleBias {
                        value: Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        scale: number_value(args[1], "scale")? as f32,
                        bias: number_value(args[2], "bias")? as f32,
                    })
                }
                "curve" => {
                    expect(args, 2, &head)?;
                    Ok(TerrainExpr::Curve {
                        x: Box::new(expression_from_steel(args[0], depth + 1, budget)?),
                        points: curve_points(args[1])?,
                    })
                }
                other => Err(format!("unknown terrain primitive: {other}")),
            }
        }
        _ => Err("terrain expression must be a number, coordinate or primitive list".into()),
    }
}

fn curve_points(value: &SteelVal) -> Result<Vec<(f32, f32)>, String> {
    let points = arguments(value, "curve points")?;
    if points.len() > MAX_CURVE_POINTS {
        return Err(format!(
            "curve has more than {MAX_CURVE_POINTS} control points"
        ));
    }
    let mut result = Vec::with_capacity(points.len());
    for point in points {
        let pair = arguments(point, "curve point")?;
        expect(&pair, 2, "curve point")?;
        result.push((
            number_value(pair[0], "curve x")? as f32,
            number_value(pair[1], "curve y")? as f32,
        ));
    }
    Ok(result)
}

fn biome_list(engine: &Engine, name: &str) -> Result<Vec<BiomeSpec>, String> {
    let value = extract(engine, name)?;
    let items = arguments(&value, name)?;
    if items.len() > MAX_BIOMES {
        return Err(format!("terrain declares more than {MAX_BIOMES} biomes"));
    }
    let mut biomes = Vec::with_capacity(items.len());
    for item in items {
        biomes.push(biome_from_steel(item)?);
    }
    Ok(biomes)
}

fn block_argument(value: &SteelVal, name: &str) -> Result<u8, String> {
    match value {
        SteelVal::SymbolV(symbol) => match symbol.to_string().as_str() {
            "grass" => Ok(GRASS),
            "dirt" => Ok(DIRT),
            "stone" => Ok(STONE),
            "sand" => Ok(SAND),
            other => Err(format!("{name} has unknown block: {other}")),
        },
        _ => byte_argument(value, name),
    }
}

fn biome_from_steel(value: &SteelVal) -> Result<BiomeSpec, String> {
    let args = arguments(value, "biome")?;
    let Some(SteelVal::SymbolV(head)) = args.first() else {
        return Err("biome must start with the biome symbol".into());
    };
    if head.to_string() != "biome" {
        return Err(format!("expected biome, found {}", head));
    }
    let args = &args[1..];
    expect(args, 10, "biome")?;
    let name = match args[0] {
        SteelVal::StringV(value) => value.to_string(),
        _ => return Err("biome name must be a string".into()),
    };
    Ok(BiomeSpec {
        name,
        surface: block_argument(args[1], "biome surface")?,
        subsurface: block_argument(args[2], "biome subsurface")?,
        depth: byte_argument(args[3], "biome depth")?,
        temperature: (
            number_value(args[4], "biome min temperature")? as f32,
            number_value(args[5], "biome max temperature")? as f32,
        ),
        moisture: (
            number_value(args[6], "biome min moisture")? as f32,
            number_value(args[7], "biome max moisture")? as f32,
        ),
        height: (
            number_value(args[8], "biome min height")? as f32,
            number_value(args[9], "biome max height")? as f32,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use steel::steel_vm::engine::Engine;

    fn engine_with(source: &str) -> Engine {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut engine = Engine::new_sandboxed();
        engine
            .run(API)
            .map_err(|error| error.to_string())
            .expect("terrain API loads");
        let path = std::env::temp_dir().join(format!(
            "jtech-terrain-test-{}-{}.scm",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, source).expect("write test source");
        let result = engine.compile_and_run_raw_program_with_path(source.to_owned(), path.clone());
        let _ = std::fs::remove_file(&path);
        result
            .map_err(|error| error.to_string())
            .expect("test source loads");
        engine
    }

    fn exponential_description(levels: usize) -> String {
        let mut source = String::from("(define d0 (constant 1.0))\n");
        for level in 1..=levels {
            source.push_str(&format!(
                "(define d{level} (tadd d{} d{}))\n",
                level - 1,
                level - 1
            ));
        }
        source
    }

    #[test]
    fn compact_shared_description_is_rejected_by_node_budget() {
        let engine = engine_with(&exponential_description(60));
        let value = engine
            .extract_value("d60")
            .map_err(|error| error.to_string())
            .expect("description is defined");
        let mut budget = MAX_GRAPH_NODES;
        let started = Instant::now();
        let error = expression_from_steel(&value, 0, &mut budget).unwrap_err();
        assert!(error.contains("node limit"), "unexpected error: {error}");
        assert_eq!(budget, 0, "the shared budget must be exhausted");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a compact shared description must fail without expanding"
        );
    }

    #[test]
    fn excessive_biomes_are_rejected_before_conversion() {
        let mut source = String::from("(define biomes (list ");
        for index in 0..=MAX_BIOMES {
            source.push_str(&format!(
                "(biome \"b{index}\" 0 1 1 0.0 1.0 0.0 1.0 0.0 1.0) "
            ));
        }
        source.push_str("))\n");
        let engine = engine_with(&source);
        let error = biome_list(&engine, "biomes").unwrap_err();
        assert!(
            error.contains(&MAX_BIOMES.to_string()),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn excessive_curve_points_are_rejected_before_conversion() {
        let mut points = String::new();
        for index in 0..=MAX_CURVE_POINTS {
            points.push_str(&format!("(list {index}.0 0.0) "));
        }
        let source = format!("(define curve (tcurve (constant 0.0) (list {points})))\n");
        let engine = engine_with(&source);
        let value = engine
            .extract_value("curve")
            .map_err(|error| error.to_string())
            .expect("curve is defined");
        let mut budget = MAX_GRAPH_NODES;
        let error = expression_from_steel(&value, 0, &mut budget).unwrap_err();
        assert!(
            error.contains("control points"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn salt_rejects_finite_integral_floats_at_or_above_u64() {
        assert_eq!(salt_argument(&SteelVal::IntV(7), "noise salt").unwrap(), 7);
        assert_eq!(
            salt_argument(&SteelVal::NumV(42.0), "noise salt").unwrap(),
            42
        );
        assert_eq!(
            salt_argument(&SteelVal::NumV(18_446_744_073_709_549_568.0), "noise salt").unwrap(),
            18_446_744_073_709_549_568
        );
        for value in [
            u64::MAX as f64,
            2f64.powi(64),
            2f64.powi(65),
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            assert!(
                salt_argument(&SteelVal::NumV(value), "noise salt").is_err(),
                "{value} must be rejected"
            );
        }
    }
}
