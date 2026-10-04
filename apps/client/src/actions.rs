//! Input bindings and action resolution.
//!
//! Physical keys map to semantic actions — the same interface a scripted or RL
//! policy drives — and the held item's kind decides what the attack action does.
//! `bindings` produces the action state; `resolve` turns it into effects.
use std::time::Instant;

use bevy::prelude::*;
use bevy::window::CursorOptions;
use controller::BasicAttackKind;
use physics::{EYE_HEIGHT, look_direction};
use protocol::{ClientMessage, EXPLOSIVE_BOW_ITEM};
use voxel_world::VoxelWorld;

use crate::{ClientSession, Options, chunk_coord, pause_menu::PauseMenu};

/// Semantic action state for one frame, produced by the key bindings.
#[derive(Resource, Default)]
pub(crate) struct Actions {
    /// Body-relative movement axes: +X right, +Y forward.
    pub movement: [f32; 2],
    pub jump: bool,
    pub descend: bool,
    pub sprint: bool,
    pub crouch: bool,
    /// Held attack action: the general attack keybind.
    pub attack: bool,
    pub ability: [bool; 3],
    pub noclip: bool,
    /// Hotbar slot selected this frame, if any.
    pub slot: Option<u8>,
}

/// What the held item is, for resolving the attack action.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    /// A block or an empty hand: the attack action mines.
    Mine,
    /// A weapon: its basic attack kind decides.
    Weapon(BasicAttackKind),
}

/// Bind physical keys to semantic actions. Movement, look and slot selection are
/// held or edge state; the attack action is held so a ranged weapon can charge.
pub(crate) fn bindings(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Option<Single<&CursorOptions>>,
    options: Res<Options>,
    menu: Res<PauseMenu>,
    mut actions: ResMut<Actions>,
) {
    let cursor_visible = cursor.is_some_and(|cursor| cursor.visible);
    let blocked = menu.blocks_gameplay() || cursor_visible;
    let held = |key| keys.pressed(key);
    actions.movement = if blocked || options.bot {
        [0.0; 2]
    } else {
        [
            f32::from(u8::from(held(KeyCode::KeyD))) - f32::from(u8::from(held(KeyCode::KeyA))),
            f32::from(u8::from(held(KeyCode::KeyW))) - f32::from(u8::from(held(KeyCode::KeyS))),
        ]
    };
    actions.jump = !blocked && keys.pressed(KeyCode::Space);
    actions.descend = !blocked && (held(KeyCode::ControlLeft) || held(KeyCode::ControlRight));
    actions.sprint = !blocked && (held(KeyCode::ShiftLeft) || held(KeyCode::ShiftRight));
    actions.crouch = !blocked && held(KeyCode::KeyC);
    actions.attack = !blocked && buttons.pressed(MouseButton::Left);
    actions.ability = if blocked {
        [false; 3]
    } else {
        [
            keys.just_pressed(KeyCode::KeyQ),
            keys.just_pressed(KeyCode::KeyE),
            keys.just_pressed(KeyCode::KeyR),
        ]
    };
    actions.noclip = !blocked && !options.bot && keys.just_pressed(KeyCode::KeyV);
    actions.slot = if blocked {
        None
    } else {
        selected_slot(&keys)
    };
}

/// Resolve the attack action against the held item: a weapon attacks, anything
/// else mines. The motor owns the weapon's cadence and charge, so the client
/// only decides which channel the action takes.
pub(crate) fn resolve(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Option<Single<&CursorOptions>>,
    world: Res<VoxelWorld>,
    actions: Res<Actions>,
    mut session: ResMut<ClientSession>,
    menu: Res<PauseMenu>,
) {
    let cursor_visible = cursor.is_some_and(|cursor| cursor.visible);
    if menu.blocks_gameplay()
        || cursor_visible
        || session.transport.is_none()
        || session.id.is_none()
        || session.health.is_depleted()
    {
        session.attack_held = false;
        return;
    }
    let strike = keys.just_pressed(KeyCode::KeyF);
    let held = held_kind(&session);
    // A weapon attacks on the held action; the animation plays on the press so
    // the held item always reacts. Blocks and empty hands mine instead.
    let attacking = actions.attack && matches!(held, Held::Weapon(_));
    session.attack_held = attacking;
    if attacking && buttons.just_pressed(MouseButton::Left) {
        session.swing_at = Some(Instant::now());
    }
    let mining = (actions.attack && !attacking) || strike;
    if let Some(message) = block_action(&mut session, &world, mining, strike) {
        session.send(message);
    }
}

/// Classify the held item for the attack action.
fn held_kind(session: &ClientSession) -> Held {
    let item = session.held_item();
    if item == EXPLOSIVE_BOW_ITEM {
        return Held::Weapon(BasicAttackKind::Admin);
    }
    session
        .packages
        .melee_weapons
        .iter()
        .find(|weapon| weapon.item == item)
        .map_or(Held::Mine, |weapon| Held::Weapon(weapon.attack_kind))
}

/// Hotbar slot selected this frame, if any; the last pressed digit wins.
fn selected_slot(keys: &ButtonInput<KeyCode>) -> Option<u8> {
    [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
        KeyCode::Digit0,
    ]
    .into_iter()
    .enumerate()
    .filter(|(_, key)| keys.just_pressed(*key))
    .map(|(index, _)| index as u8 + 1)
    .next_back()
}

/// Raycast the crosshair and build the mining request. `strike` fractures the
/// block outright; `mine` starts a normal edit.
fn block_action(
    session: &mut ClientSession,
    world: &VoxelWorld,
    mine: bool,
    strike: bool,
) -> Option<ClientMessage> {
    if !mine && !strike {
        return None;
    }
    let origin = session.state.motion.position + Vec3::Y * EYE_HEIGHT;
    let direction = look_direction(session.yaw, session.pitch);
    let hit = world.raycast(origin, direction, 6.0)?;
    let expected_revision = world.chunks.get(&chunk_coord(hit.block))?.revision;
    session.request += 1;
    let request = session.request;
    Some(if strike {
        ClientMessage::Strike {
            request,
            target: hit.block,
            expected_revision,
        }
    } else {
        ClientMessage::Edit {
            request,
            target: hit.block,
            block: 0,
            expected_revision,
        }
    })
}
