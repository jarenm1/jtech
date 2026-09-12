//! Local pause overlay. Networking and neutral movement prediction keep running.
use bevy::{
    app::AppExit,
    prelude::*,
    window::{CursorGrabMode, CursorOptions},
};

use crate::{ClientSession, Options, game_hud::GameplayHud};

#[derive(Resource, Default)]
pub(crate) struct PauseMenu {
    pub open: bool,
    suppress_frame: bool,
    wait_for_release: bool,
}

impl PauseMenu {
    pub fn blocks_gameplay(&self) -> bool {
        self.open || self.suppress_frame || self.wait_for_release
    }

    fn resume(&mut self) {
        self.open = false;
        self.suppress_frame = true;
        self.wait_for_release = true;
    }

    /// Extend the gameplay gate until the mouse is released. Overlays that close
    /// without opening the menu use this so their click cannot reach gameplay.
    pub(crate) fn hold_for_mouse_release(&mut self) {
        self.wait_for_release = true;
    }
}

#[derive(Component)]
pub(crate) struct PausePanel;

#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuAction {
    Resume,
    Power,
    Quit,
}

#[derive(Component)]
pub(crate) struct PowerText;

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
            BackgroundColor(Color::srgba(0.025, 0.035, 0.045, 0.66)),
            GlobalZIndex(100),
            bevy::ui::FocusPolicy::Block,
            PausePanel,
        ))
        .with_children(|overlay| {
            overlay
                .spawn((
                    Node {
                        width: px(360),
                        max_width: percent(85),
                        padding: UiRect::all(px(30)),
                        row_gap: px(12),
                        flex_direction: FlexDirection::Column,
                        border: UiRect::top(px(3)),
                        ..default()
                    },
                    BorderColor::all(Color::srgb(0.9, 0.73, 0.4)),
                    BackgroundColor(Color::srgba(0.065, 0.08, 0.09, 0.86)),
                ))
                .with_children(|panel| {
                    panel.spawn((
                        Text::new("PAUSED"),
                        TextFont {
                            font_size: 34.0,
                            ..default()
                        },
                        TextColor(Color::srgb(0.96, 0.94, 0.87)),
                        Node {
                            margin: UiRect::bottom(px(20)),
                            ..default()
                        },
                    ));
                    for (action, label) in [
                        (MenuAction::Resume, "Resume"),
                        (MenuAction::Power, "Bow power"),
                        (MenuAction::Quit, "Quit game"),
                    ] {
                        panel
                            .spawn((
                                Button,
                                action,
                                Node {
                                    width: percent(100),
                                    min_height: px(52),
                                    padding: UiRect::horizontal(px(18)),
                                    align_items: AlignItems::Center,
                                    ..default()
                                },
                                BackgroundColor(button_color(Interaction::None)),
                            ))
                            .with_children(|button| {
                                let mut text = button.spawn((
                                    Text::new(label),
                                    TextFont {
                                        font_size: 19.0,
                                        ..default()
                                    },
                                    TextColor(Color::srgb(0.96, 0.94, 0.87)),
                                ));
                                if matches!(action, MenuAction::Power) {
                                    text.insert(PowerText);
                                }
                            });
                    }
                });
        });
}

pub(crate) fn input(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    window: Single<&Window>,
    options: Res<Options>,
    mut menu: ResMut<PauseMenu>,
) {
    menu.suppress_frame = false;
    if !buttons.any_pressed([MouseButton::Left, MouseButton::Right]) {
        menu.wait_for_release = false;
    }
    if keys.just_pressed(KeyCode::Escape) && window.focused {
        if menu.open {
            menu.resume();
        } else {
            menu.open = true;
        }
        menu.suppress_frame = true;
    }
    if !window.focused && !options.bot {
        menu.open = true;
    }
}

pub(crate) fn actions(
    buttons: Query<(&Interaction, &MenuAction), Changed<Interaction>>,
    mut menu: ResMut<PauseMenu>,
    mut session: ResMut<ClientSession>,
    mut exit: MessageWriter<AppExit>,
) {
    if !menu.open {
        return;
    }
    for (interaction, action) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            MenuAction::Resume => menu.resume(),
            MenuAction::Power => session.bow_power = session.bow_power.next(),
            MenuAction::Quit => {
                exit.write(AppExit::Success);
            }
        }
    }
}

pub(crate) fn button_color(interaction: Interaction) -> Color {
    match interaction {
        Interaction::Pressed => Color::srgb(0.39, 0.32, 0.2),
        Interaction::Hovered => Color::srgb(0.24, 0.26, 0.25),
        Interaction::None => Color::srgba(0.14, 0.17, 0.18, 0.95),
    }
}

#[allow(clippy::too_many_arguments)] // Synchronize independent cursor and UI components.
pub(crate) fn sync(
    menu: Res<PauseMenu>,
    options: Res<Options>,
    session: Res<ClientSession>,
    mut cursor: Single<&mut CursorOptions>,
    mut panel: Single<&mut Node, With<PausePanel>>,
    mut hud: Query<&mut Visibility, With<GameplayHud>>,
    mut buttons: Query<(&Interaction, &mut BackgroundColor), With<MenuAction>>,
    mut power: Single<&mut Text, With<PowerText>>,
) {
    cursor.visible = menu.open || options.bot;
    cursor.grab_mode = if cursor.visible {
        CursorGrabMode::None
    } else {
        CursorGrabMode::Locked
    };
    panel.display = if menu.open {
        Display::Flex
    } else {
        Display::None
    };
    for mut visibility in &mut hud {
        *visibility = if menu.open {
            Visibility::Hidden
        } else {
            Visibility::Inherited
        };
    }
    for (interaction, mut color) in &mut buttons {
        color.0 = button_color(*interaction);
    }
    power.0 = format!("Bow power   {}", session.bow_power.label());
}

pub(crate) fn gameplay_enabled(menu: Res<PauseMenu>) -> bool {
    !menu.blocks_gameplay()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::input::mouse::AccumulatedMouseMotion;

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<PauseMenu>()
            .init_resource::<ClientSession>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .init_resource::<AccumulatedMouseMotion>()
            .insert_resource(Options {
                server: "127.0.0.1:4000".parse().unwrap(),
                bot: false,
                frames: None,
                screenshot: None,
                lighting: crate::lighting::DayCycle::default(),
            })
            .add_message::<AppExit>()
            .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
            .add_systems(Update, (input, actions, sync, crate::controls).chain());
        app.world_mut().spawn((
            Window {
                focused: true,
                ..default()
            },
            CursorOptions::default(),
        ));
        app.world_mut()
            .spawn((Node::default(), GameplayHud, Visibility::Inherited));
        app.update();
        app
    }

    fn press_action(app: &mut App, action: MenuAction) {
        let entity = app
            .world_mut()
            .query::<(Entity, &MenuAction)>()
            .iter(app.world())
            .find(|(_, value)| **value == action)
            .unwrap()
            .0;
        *app.world_mut().get_mut::<Interaction>(entity).unwrap() = Interaction::Pressed;
    }

    #[test]
    fn escape_blocks_look_and_selection_and_resume_does_not_click_through() {
        let mut app = app();
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::Escape);
            keys.press(KeyCode::Digit6);
        }
        let yaw = app.world().resource::<ClientSession>().yaw;
        app.world_mut()
            .resource_mut::<AccumulatedMouseMotion>()
            .delta = Vec2::splat(50.0);
        app.update();
        assert!(app.world().resource::<PauseMenu>().open);
        assert_eq!(app.world().resource::<ClientSession>().selected, 3);
        assert_eq!(app.world().resource::<ClientSession>().yaw, yaw);
        let cursor = app
            .world_mut()
            .query::<&CursorOptions>()
            .single(app.world())
            .unwrap();
        assert!(cursor.visible);
        assert_eq!(cursor.grab_mode, CursorGrabMode::None);
        let visibility = app
            .world_mut()
            .query_filtered::<&Visibility, With<GameplayHud>>()
            .single(app.world())
            .unwrap();
        assert_eq!(*visibility, Visibility::Hidden);

        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = default();
        app.world_mut()
            .resource_mut::<ButtonInput<MouseButton>>()
            .press(MouseButton::Left);
        press_action(&mut app, MenuAction::Resume);
        app.update();
        assert!(!app.world().resource::<PauseMenu>().open);
        assert!(app.world().resource::<PauseMenu>().blocks_gameplay());
        app.update();
        assert!(app.world().resource::<PauseMenu>().blocks_gameplay());
        *app.world_mut().resource_mut::<ButtonInput<MouseButton>>() = default();
        app.update();
        assert!(!app.world().resource::<PauseMenu>().blocks_gameplay());
    }

    #[test]
    fn focus_loss_opens_menu_and_menu_actions_change_power_and_exit() {
        let mut app = app();
        app.world_mut()
            .query::<&mut Window>()
            .single_mut(app.world_mut())
            .unwrap()
            .focused = false;
        app.update();
        assert!(app.world().resource::<PauseMenu>().open);
        let power = app.world().resource::<ClientSession>().bow_power;
        press_action(&mut app, MenuAction::Power);
        app.update();
        assert_eq!(
            app.world().resource::<ClientSession>().bow_power,
            power.next()
        );
        press_action(&mut app, MenuAction::Quit);
        app.update();
        assert_eq!(app.world().resource::<Messages<AppExit>>().len(), 1);
    }
}
