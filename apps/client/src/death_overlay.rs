//! Local death overlay and respawn action.
//!
//! Networking, terrain streaming and remote presentation keep running while the
//! player is dead. Only local movement, look, fire, edit and noclip input is
//! gated. The server owns revival: this module never restores local health, it
//! only asks the server to respawn at the life counter currently observed.

use bevy::{
    prelude::*,
    ui::FocusPolicy,
    window::{CursorGrabMode, CursorOptions},
};

use crate::{
    ClientSession, Options,
    game_hud::{GameplayHud, palette},
    pause_menu::{self, PauseMenu},
};

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

pub(crate) fn spawn(commands: &mut Commands) {
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
            overlay
                .spawn((
                    Node {
                        width: px(370),
                        max_width: percent(85),
                        padding: UiRect::all(px(30)),
                        row_gap: px(12),
                        flex_direction: FlexDirection::Column,
                        align_items: AlignItems::Center,
                        border: UiRect::top(px(3)),
                        ..default()
                    },
                    BorderColor::all(palette::DANGER),
                    BackgroundColor(Color::srgba(0.065, 0.08, 0.09, 0.9)),
                ))
                .with_children(|panel| {
                    panel.spawn((
                        Text::new("YOU DIED"),
                        TextFont {
                            font_size: 36.0,
                            ..default()
                        },
                        TextColor(palette::DANGER),
                        Node {
                            margin: UiRect::bottom(px(2)),
                            ..default()
                        },
                    ));
                    panel.spawn((
                        Text::new("Press Enter or click Respawn"),
                        TextFont {
                            font_size: 15.0,
                            ..default()
                        },
                        TextColor(palette::MUTED),
                        Node {
                            margin: UiRect::bottom(px(16)),
                            ..default()
                        },
                    ));
                    panel
                        .spawn((
                            Button,
                            DeathAction::Respawn,
                            Node {
                                width: percent(100),
                                min_height: px(52),
                                padding: UiRect::horizontal(px(18)),
                                align_items: AlignItems::Center,
                                justify_content: JustifyContent::Center,
                                ..default()
                            },
                            BackgroundColor(pause_menu::button_color(Interaction::None)),
                        ))
                        .with_children(|button| {
                            button.spawn((
                                Text::new("Respawn"),
                                TextFont {
                                    font_size: 19.0,
                                    ..default()
                                },
                                TextColor(palette::IVORY),
                            ));
                        });
                });
        });
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
    options: Res<Options>,
    session: Res<ClientSession>,
    mut menu: ResMut<PauseMenu>,
    mut overlay: ResMut<DeathOverlay>,
    mut cursor: Single<&mut CursorOptions>,
    mut panel: Single<&mut Node, With<DeathPanel>>,
    mut hud: Query<&mut Visibility, With<GameplayHud>>,
    mut respawn_buttons: Query<(&Interaction, &mut BackgroundColor), With<DeathAction>>,
) {
    let dead = session.health.is_depleted();
    if overlay.dead && !dead && buttons.any_pressed([MouseButton::Left, MouseButton::Right]) {
        menu.hold_for_mouse_release();
    }
    overlay.dead = dead;
    cursor.visible = dead || menu.open || options.bot;
    cursor.grab_mode = if cursor.visible {
        CursorGrabMode::None
    } else {
        CursorGrabMode::Locked
    };
    panel.display = if dead { Display::Flex } else { Display::None };
    for mut visibility in &mut hud {
        *visibility = if menu.open || dead {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
    }
    for (interaction, mut color) in &mut respawn_buttons {
        color.0 = pause_menu::button_color(*interaction);
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
        .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
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
