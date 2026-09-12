//! Client-local presentation clock: hours 6/18 are sunrise/sunset.
use bevy::{light::CascadeShadowConfigBuilder, prelude::*};

pub struct LightingPlugin;

#[derive(Resource, Clone, Copy)]
pub struct DayCycle {
    pub hour: f64,
    /// Real seconds per full day; zero holds the current hour.
    pub day_seconds: f64,
}

impl Default for DayCycle {
    fn default() -> Self {
        Self {
            hour: 9.0,
            day_seconds: 1200.0,
        }
    }
}

impl DayCycle {
    fn advance(&mut self, seconds: f64) {
        if self.day_seconds > 0.0 {
            self.hour = (self.hour + seconds * 24.0 / self.day_seconds).rem_euclid(24.0);
        }
    }
}

#[derive(Component)]
struct CelestialLight {
    moon: bool,
}

impl Plugin for LightingPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DayCycle>()
            .init_resource::<AmbientLight>()
            .init_resource::<ClearColor>()
            .add_systems(Startup, setup)
            .add_systems(Update, update);
    }
}

fn setup(mut commands: Commands) {
    for moon in [false, true] {
        commands.spawn((
            CelestialLight { moon },
            DirectionalLight {
                illuminance: 0.0,
                shadows_enabled: true,
                ..default()
            },
            CascadeShadowConfigBuilder {
                first_cascade_far_bound: 16.0,
                maximum_distance: 192.0,
                ..default()
            }
            .build(),
            Transform::default(),
        ));
    }
}

fn smooth(low: f32, high: f32, value: f32) -> f32 {
    let t = ((value - low) / (high - low)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn color(a: Vec3, b: Vec3, t: f32) -> Color {
    let c = a.lerp(b, t);
    Color::srgb(c.x, c.y, c.z)
}

struct Lighting {
    sun: Vec3,
    daylight: f32,
    sun_strength: f32,
    moon_strength: f32,
    sun_color: Color,
    sky: Color,
}

fn sample(hour: f64) -> Lighting {
    let angle = ((hour.rem_euclid(24.0) - 6.0) / 24.0 * std::f64::consts::TAU) as f32;
    // Tilt the orbit so even noon produces angled shadows (maximum elevation 60 degrees).
    let sun = Vec3::new(angle.cos(), angle.sin() * 0.8660254, angle.sin() * 0.5);
    let daylight = smooth(-0.16, 0.22, sun.y);
    let high_sun = smooth(0.0, 0.5, sun.y);
    let dusk = Vec3::new(0.55, 0.24, 0.16);
    let day = Vec3::new(0.48, 0.69, 0.88);
    Lighting {
        sun,
        daylight,
        // Fade before crossing the horizon, avoiding light through terrain from below.
        sun_strength: smooth(0.0, 0.18, sun.y),
        moon_strength: smooth(0.0, 0.25, -sun.y),
        sun_color: color(
            Vec3::new(1.0, 0.43, 0.18),
            Vec3::new(1.0, 0.97, 0.9),
            high_sun,
        ),
        sky: color(
            Vec3::new(0.008, 0.014, 0.035),
            dusk.lerp(day, high_sun),
            daylight,
        ),
    }
}

fn update(
    time: Res<Time>,
    mut cycle: ResMut<DayCycle>,
    mut ambient: ResMut<AmbientLight>,
    mut clear: ResMut<ClearColor>,
    mut lights: Query<(&CelestialLight, &mut DirectionalLight, &mut Transform)>,
) {
    cycle.advance(time.delta_secs_f64());
    let lighting = sample(cycle.hour);
    clear.0 = lighting.sky;
    ambient.color = color(Vec3::new(0.35, 0.45, 0.75), Vec3::ONE, lighting.daylight);
    ambient.brightness = 35.0 + 215.0 * lighting.daylight;
    for (celestial, mut light, mut transform) in &mut lights {
        let (direction, strength) = if celestial.moon {
            light.color = Color::srgb(0.48, 0.62, 1.0);
            (-lighting.sun, lighting.moon_strength)
        } else {
            light.color = lighting.sun_color;
            (lighting.sun, lighting.sun_strength)
        };
        light.illuminance = strength * if celestial.moon { 350.0 } else { 12_000.0 };
        light.shadows_enabled = strength > 0.0;
        *transform = Transform::default().looking_to(-direction, Vec3::Y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_wraps_and_can_be_frozen() {
        let mut cycle = DayCycle::default();
        cycle.advance(1200.0);
        assert!((cycle.hour - 9.0).abs() < 1e-9);
        cycle.hour = 23.0;
        cycle.advance(100.0);
        assert!((cycle.hour - 1.0).abs() < 1e-9);
        cycle.day_seconds = 0.0;
        cycle.advance(100.0);
        assert_eq!(cycle.hour, 1.0);
    }

    #[test]
    fn sun_and_moon_alternate_without_below_ground_light() {
        for step in 0..2400 {
            let lighting = sample(step as f64 / 100.0);
            assert!((lighting.sun.length() - 1.0).abs() < 1e-5);
            assert!(lighting.sun_strength == 0.0 || lighting.sun.y > 0.0);
            assert!(lighting.moon_strength == 0.0 || lighting.sun.y < 0.0);
            assert!(lighting.sun_strength == 0.0 || lighting.moon_strength == 0.0);
            assert!(
                Transform::default()
                    .looking_to(-lighting.sun, Vec3::Y)
                    .rotation
                    .is_finite()
            );
        }
        assert!(sample(9.0).sun.x > 0.0);
        assert!(sample(15.0).sun.x < 0.0);
        assert_eq!(sample(12.0).daylight, 1.0);
        assert_eq!(sample(0.0).daylight, 0.0);
        assert!(sample(12.0).sun.y < 0.9);
    }

    #[test]
    fn plugin_updates_lights_and_environment() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(LightingPlugin);
        app.insert_resource(DayCycle {
            hour: 12.0,
            day_seconds: 0.0,
        });
        app.update();
        let world = app.world_mut();
        let mut query = world.query::<(&CelestialLight, &DirectionalLight)>();
        assert_eq!(query.iter(world).count(), 2);
        for (celestial, light) in query.iter(world) {
            assert_eq!(light.shadows_enabled, !celestial.moon);
            assert_eq!(
                light.illuminance,
                if celestial.moon { 0.0 } else { 12_000.0 }
            );
        }
        world.resource_mut::<DayCycle>().hour = 0.0;
        app.update();
        assert_eq!(app.world().resource::<AmbientLight>().brightness, 35.0);
        let world = app.world_mut();
        for (celestial, light) in query.iter(world) {
            assert_eq!(light.shadows_enabled, celestial.moon);
        }
    }
}
