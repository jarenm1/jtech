use bevy::prelude::*;

use crate::{
    ClientSession,
    game_hud::{GameplayHud, palette},
};

#[derive(Component)]
pub(crate) struct HealthLabel;

#[derive(Component)]
pub(crate) struct HealthFill;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: px(22),
                left: px(20),
                width: px(210),
                padding: UiRect::axes(px(10), px(7)),
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                border: UiRect::all(px(1)),
                ..default()
            },
            BackgroundColor(palette::PANEL),
            BorderColor::all(palette::BORDER),
            BorderRadius::all(px(6)),
            GlobalZIndex(10),
            GameplayHud,
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new(""),
                TextFont {
                    font_size: 13.0,
                    ..default()
                },
                TextColor(palette::IVORY),
                TextShadow::default(),
                HealthLabel,
            ));
            panel
                .spawn((
                    Node {
                        width: percent(100),
                        height: px(10),
                        border: UiRect::all(px(1)),
                        ..default()
                    },
                    BackgroundColor(palette::HEALTH_TRACK),
                    BorderColor::all(palette::BORDER),
                    BorderRadius::all(px(5)),
                ))
                .with_children(|track| {
                    track.spawn((
                        Node {
                            width: percent(100),
                            height: percent(100),
                            ..default()
                        },
                        BackgroundColor(palette::HEALTH_FILL),
                        BorderRadius::all(px(4)),
                        HealthFill,
                    ));
                });
        });
}

pub(crate) fn update(
    session: Res<ClientSession>,
    mut label: Single<&mut Text, With<HealthLabel>>,
    mut fill: Single<(&mut Node, &mut BackgroundColor), With<HealthFill>>,
) {
    if !session.is_changed() {
        return;
    }
    let health = session.health;
    let fraction = f32::from(health.current()) / f32::from(health.maximum());
    **label = Text::new(format!("HP {}/{}", health.current(), health.maximum()));
    let (node, color) = &mut *fill;
    node.width = percent(100.0 * fraction);
    color.0 = if fraction <= 0.3 {
        palette::HEALTH_LOW
    } else {
        palette::HEALTH_FILL
    };
}
