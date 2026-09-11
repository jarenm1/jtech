use crate::{
    CharacterBody, CharacterIntent, CharacterState, DynamicCollider, FIXED_DT, MovementProfile,
    step_character,
};
use bevy_app::{App, FixedUpdate, Plugin};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::{InternedScheduleLabel, ScheduleLabel};
use voxel_world::VoxelWorld;

/// Hosts update snapshot colliders before the motor. No extrapolation is performed.
#[derive(Resource, Default)]
pub struct ObservedBodies(pub Vec<DynamicCollider>);

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControllerSet {
    Intent,
    Motor,
}

/// Headless ECS path for scripted and learned actors. Human prediction calls the
/// same motor explicitly, once per acknowledged/replayed command.
/// Run FixedUpdate at 60Hz, or manually once per FIXED_DT in a headless experiment.
pub struct ControllerPlugin {
    schedule: InternedScheduleLabel,
}
impl Default for ControllerPlugin {
    fn default() -> Self {
        Self::on(FixedUpdate)
    }
}
impl ControllerPlugin {
    /// Choose a host schedule that runs once per 1/60 simulation second.
    pub fn on(schedule: impl ScheduleLabel) -> Self {
        Self {
            schedule: schedule.intern(),
        }
    }
}
impl Plugin for ControllerPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<physics::PhysicsPlugin>() {
            app.add_plugins(physics::PhysicsPlugin);
        }
        app.init_resource::<VoxelWorld>()
            .init_resource::<ObservedBodies>()
            .configure_sets(
                self.schedule,
                (
                    physics::PhysicsSet::Input,
                    physics::PhysicsSet::Movement,
                    physics::PhysicsSet::Output,
                )
                    .chain(),
            )
            .configure_sets(
                self.schedule,
                (ControllerSet::Intent, ControllerSet::Motor)
                    .chain()
                    .in_set(physics::PhysicsSet::Movement),
            )
            .add_systems(self.schedule, step_actors.in_set(ControllerSet::Motor));
    }
}
fn step_actors(
    world: Res<VoxelWorld>,
    bodies: Res<ObservedBodies>,
    mut actors: Query<(
        &mut CharacterState,
        &CharacterBody,
        &MovementProfile,
        &mut CharacterIntent,
    )>,
) {
    for (mut state, body, profile, mut intent) in &mut actors {
        step_character(
            &world,
            &mut state,
            body,
            profile,
            &mut intent,
            FIXED_DT,
            &bodies.0,
        );
    }
}
