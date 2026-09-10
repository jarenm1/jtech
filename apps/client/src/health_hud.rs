use bevy::prelude::*;

use crate::ClientSession;

#[derive(Component)]
pub(crate) struct HealthLabel;

#[derive(Component)]
pub(crate) struct HealthFill;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: px(112),
                left: px(20),
                width: px(220),
                padding: UiRect::all(px(10)),
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                ..default()
            },
            BackgroundColor(Color::srgba(0.025, 0.04, 0.07, 0.8)),
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new(""),
                TextFont {
                    font_size: 18.0,
                    ..default()
                },
                TextColor(Color::srgb(0.94, 0.97, 1.0)),
                TextShadow::default(),
                HealthLabel,
            ));
            panel
                .spawn((
                    Node {
                        width: percent(100),
                        height: px(10),
                        ..default()
                    },
                    BackgroundColor(Color::srgb(0.25, 0.08, 0.08)),
                ))
                .with_children(|track| {
                    track.spawn((
                        Node {
                            width: percent(100),
                            height: percent(100),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.3, 0.85, 0.45)),
                        HealthFill,
                    ));
                });
        });
}

pub(crate) fn update(
    session: Res<ClientSession>,
    mut label: Single<&mut Text, With<HealthLabel>>,
    mut fill: Single<&mut Node, With<HealthFill>>,
) {
    if !session.is_changed() {
        return;
    }
    let health = session.health;
    **label = Text::new(format!(
        "HP {} / {}{}",
        health.current(),
        health.maximum(),
        if health.is_depleted() {
            "  DEPLETED"
        } else {
            ""
        },
    ));
    fill.width = percent(100.0 * f32::from(health.current()) / f32::from(health.maximum()));
}
