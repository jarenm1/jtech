//! Local death overlay and respawn action.
//!
//! Networking, terrain streaming and remote presentation keep running while the
//! player is dead. Only local movement, look, fire, edit and noclip input is
//! gated. The server owns revival: this module never restores local health, it
//! only asks the server to respawn at the life counter currently observed.

use bevy::{
    asset::RenderAssetUsages,
    image::Image,
    prelude::*,
    render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    ui::FocusPolicy,
};

use crate::{ClientSession, game_hud::GameplayHud, pause_menu::PauseMenu};

/// Death presentation state and the one-frame gameplay gate used around revival.
#[derive(Resource, Default)]
pub(crate) struct DeathOverlay {
    /// Last observed depletion, used to detect the revival transition.
    dead: bool,
    /// Life counter sent by the most recent local respawn request.
    pub(crate) requested_life: Option<u64>,
}

#[derive(Component)]
pub(crate) struct DeathPanel;

#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeathAction {
    Respawn,
}

/// Pre-rasterized copies of the SVG art in this directory. The PNGs are the
/// shipped form because resvg (the pure-Rust renderer) erodes the interior of
/// `feDisplacementMap` filters — librsvg and browsers render only ruffled
/// edges, so the PNGs are baked with `rsvg-convert` at 2× UI scale.
const DEATH_NOTICE_PNG: &[u8] = include_bytes!("../assets/death_notice.png");
const RESPAWN_BUTTON_PNG: &[u8] = include_bytes!("../assets/respawn_button.png");

pub(crate) fn spawn(commands: &mut Commands, images: &mut Assets<Image>) {
    let notice = images.add(load_png(DEATH_NOTICE_PNG));
    let button = images.add(load_png(RESPAWN_BUTTON_PNG));
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: percent(100),
                height: percent(100),
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.10, 0.015, 0.02, 0.72)),
            GlobalZIndex(90),
            FocusPolicy::Block,
            DeathPanel,
        ))
        .with_children(|overlay| {
            // Full-width band centered above the button; its child holds the
            // notice horizontally centered without magic margins.
            overlay
                .spawn(Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    right: px(0),
                    bottom: percent(58),
                    justify_content: JustifyContent::Center,
                    ..default()
                })
                .with_children(|notice_row| {
                    notice_row.spawn((
                        ImageNode::new(notice),
                        Node {
                            width: percent(75),
                            ..default()
                        },
                    ));
                });
            overlay.spawn((
                Button,
                DeathAction::Respawn,
                ImageNode::new(button),
                Node {
                    width: px(234),
                    ..default()
                },
            ));
        });
}

/// Decode an embedded RGBA PNG into a texture. The assets are part of the
/// binary, so a decode failure is a build defect: fail loudly at startup.
fn load_png(bytes: &[u8]) -> Image {
    let rgba = image::load_from_memory(bytes)
        .expect("embedded death overlay PNG must decode")
        .to_rgba8();
    Image::new(
        Extent3d {
            width: rgba.width(),
            height: rgba.height(),
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba.into_vec(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Multiplier on the button image: plain at rest, warm on hover, dark on press.
fn button_tint(interaction: Interaction) -> Color {
    match interaction {
        Interaction::Pressed => Color::srgb(0.55, 0.55, 0.55),
        Interaction::Hovered => Color::srgb(1.35, 1.25, 1.15),
        Interaction::None => Color::WHITE,
    }
}

/// Enter triggers the same respawn action as the button. Repeat presses are
/// harmless: each sends the life counter observed right now.
pub(crate) fn input(
    keys: Res<ButtonInput<KeyCode>>,
    menu: Res<PauseMenu>,
    mut session: ResMut<ClientSession>,
    mut overlay: ResMut<DeathOverlay>,
) {
    if menu.open || !session.health.is_depleted() {
        return;
    }
    if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter) {
        request(&mut session, &mut overlay);
    }
}

pub(crate) fn actions(
    buttons: Query<(&Interaction, &DeathAction), Changed<Interaction>>,
    menu: Res<PauseMenu>,
    mut session: ResMut<ClientSession>,
    mut overlay: ResMut<DeathOverlay>,
) {
    if menu.open || !session.health.is_depleted() {
        return;
    }
    for (interaction, _) in &buttons {
        if *interaction == Interaction::Pressed {
            request(&mut session, &mut overlay);
        }
    }
}

fn request(session: &mut ClientSession, overlay: &mut DeathOverlay) {
    overlay.requested_life = Some(session.life);
    let message = session.respawn_request();
    session.send(message);
}

/// Presentation and cursor ownership for the overlay. Runs after the pause menu
/// so death wins the cursor and HUD visibility. On revival the pause gate is held
/// until the mouse is released so the click that asked to respawn cannot fall
/// through to a gameplay action.
#[allow(clippy::too_many_arguments)] // Synchronize independent cursor and UI components.
pub(crate) fn sync(
    buttons: Res<ButtonInput<MouseButton>>,
    session: Res<ClientSession>,
    mut menu: ResMut<PauseMenu>,
    mut overlay: ResMut<DeathOverlay>,
    mut panel: Single<&mut Node, With<DeathPanel>>,
    mut hud: Query<&mut Visibility, With<GameplayHud>>,
    mut respawn_buttons: Query<(&Interaction, &mut ImageNode), With<DeathAction>>,
) {
    let dead = session.health.is_depleted();
    if overlay.dead && !dead && buttons.any_pressed([MouseButton::Left, MouseButton::Right]) {
        menu.hold_for_mouse_release();
    }
    overlay.dead = dead;
    panel.display = if dead { Display::Flex } else { Display::None };
    for mut visibility in &mut hud {
        *visibility = if menu.open || dead {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
    }
    for (interaction, mut image) in &mut respawn_buttons {
        image.color = button_tint(*interaction);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(health: crate::Health) -> App {
        let mut app = App::new();
        app.insert_resource(ClientSession {
            health,
            ..default()
        })
        .init_resource::<DeathOverlay>()
        .init_resource::<PauseMenu>()
        .init_resource::<ButtonInput<KeyCode>>()
        .init_resource::<Assets<Image>>()
        .add_systems(
            Startup,
            |mut commands: Commands, mut images: ResMut<Assets<Image>>| {
                spawn(&mut commands, &mut images);
            },
        )
        .add_systems(Update, (input, actions).chain());
        app.update();
        app
    }

    fn depleted() -> crate::Health {
        let mut health = crate::Health::default();
        health.damage(u16::MAX);
        health
    }

    #[test]
    fn enter_and_button_send_the_observed_life_and_repeat_safely() {
        for enter in [true, false] {
            let mut app = app(depleted());
            app.world_mut().resource_mut::<ClientSession>().life = 4;
            if enter {
                app.world_mut()
                    .resource_mut::<ButtonInput<KeyCode>>()
                    .press(KeyCode::Enter);
            } else {
                let button = app
                    .world_mut()
                    .query_filtered::<Entity, With<DeathAction>>()
                    .single(app.world())
                    .unwrap();
                *app.world_mut()
                    .entity_mut(button)
                    .get_mut::<Interaction>()
                    .unwrap() = Interaction::Pressed;
            }
            app.update();
            assert_eq!(
                app.world().resource::<DeathOverlay>().requested_life,
                Some(4)
            );
            // A second request with the same observed life is harmless.
            app.world_mut()
                .resource_mut::<ButtonInput<KeyCode>>()
                .clear();
            app.update();
            assert_eq!(
                app.world().resource::<DeathOverlay>().requested_life,
                Some(4)
            );
        }
    }

    #[test]
    fn healthy_players_do_not_trigger_respawn() {
        let mut app = app(crate::Health::default());
        app.world_mut().resource_mut::<ClientSession>().life = 2;
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Enter);
        app.update();
        assert_eq!(app.world().resource::<DeathOverlay>().requested_life, None);
    }
}
