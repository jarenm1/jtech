//! Character column: the player's name and guild over an offscreen preview,
//! with a stats block beside it. Spawned as part of the inventory pane.

use bevy::prelude::*;

use crate::{ClientSession, ui_theme::palette};

/// Stats column width.
pub(crate) const STATS_WIDTH: f32 = 190.0;

/// Text fields refreshed from the session.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatField {
    Name,
    Guild,
    Health,
}

/// Spawn the name section, the preview and the stats column into `page`.
pub(crate) fn spawn(page: &mut ChildSpawnerCommands, preview: Option<Handle<Image>>) {
    // Name and guild in their own section above the preview.
    page.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Center,
        row_gap: px(2),
        margin: UiRect::bottom(px(10)),
        ..default()
    })
    .with_children(|name| {
        name.spawn((
            Text::new("Player"),
            TextFont {
                font_size: 24.0,
                ..default()
            },
            TextColor(palette::TEXT_STRONG),
            StatField::Name,
        ));
        name.spawn((
            Text::new("No Guild"),
            TextFont {
                font_size: 12.0,
                ..default()
            },
            TextColor(palette::TEXT_MUTED),
            StatField::Guild,
        ));
    });
    // Preview filling the rest, with the stats column down the right.
    page.spawn(Node {
        width: percent(100),
        flex_grow: 1.0,
        flex_direction: FlexDirection::Row,
        column_gap: px(12),
        ..default()
    })
    .with_children(|row| {
        let mut viewport = row.spawn((
            Node {
                flex_grow: 1.0,
                flex_basis: px(0.0),
                height: percent(100),
                border: UiRect::all(px(1)),
                ..default()
            },
            BackgroundColor(palette::SURFACE),
            BorderColor::all(palette::BORDER),
            BorderRadius::all(px(palette::INNER_RADIUS)),
        ));
        if let Some(image) = preview {
            viewport.insert(ImageNode {
                image,
                image_mode: NodeImageMode::Stretch,
                ..default()
            });
        }
        row.spawn((
            Node {
                width: px(STATS_WIDTH),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                ..default()
            },
            BackgroundColor(palette::SURFACE),
            BorderColor::all(palette::BORDER),
            BorderRadius::all(px(palette::INNER_RADIUS)),
        ))
        .with_children(|stats| {
            stats
                .spawn(Node {
                    width: percent(100),
                    flex_direction: FlexDirection::Row,
                    justify_content: JustifyContent::SpaceBetween,
                    ..default()
                })
                .with_children(|row| {
                    row.spawn((
                        Text::new("Health"),
                        TextFont {
                            font_size: 13.0,
                            ..default()
                        },
                        TextColor(palette::TEXT_MUTED),
                    ));
                    row.spawn((
                        Text::new("100 / 100"),
                        TextFont {
                            font_size: 13.0,
                            ..default()
                        },
                        TextColor(palette::ACCENT),
                        StatField::Health,
                    ));
                });
        });
    });
}

/// Last stat values written. Compared against the session so unchanged frames
/// neither allocate strings nor flag components changed (every write triggers a
/// relayout pass).
#[derive(Default)]
pub(crate) struct StatShown {
    id: Option<u64>,
    health: (u16, u16),
}

/// Refresh the name, guild and health fields from the session.
pub(crate) fn sync_stats(
    session: Res<ClientSession>,
    mut fields: Query<(&StatField, &mut Text)>,
    mut shown: Local<StatShown>,
) {
    let health = (session.health.current(), session.health.maximum());
    if shown.id == session.id && shown.health == health {
        return;
    }
    shown.id = session.id;
    shown.health = health;
    for (field, mut text) in &mut fields {
        let value = match field {
            StatField::Name => identity_name(&session),
            StatField::Guild => identity_guild(&session).unwrap_or_else(|| "No Guild".into()),
            StatField::Health => format!("{} / {}", health.0, health.1),
        };
        if text.0 != value {
            text.0 = value;
        }
    }
}

/// Placeholder identity. The protocol carries no player name or guild yet; swap
/// these for replicated fields when they exist — the pane already reserves the
/// space.
fn identity_name(session: &ClientSession) -> String {
    match session.id {
        Some(id) => format!("Player {id}"),
        None => "Player".into(),
    }
}

fn identity_guild(_session: &ClientSession) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_show_placeholder_identity_and_health() {
        let mut app = App::new();
        app.init_resource::<ClientSession>()
            .add_systems(Startup, |mut commands: Commands| {
                let row = commands.spawn(Node::default()).id();
                commands.entity(row).with_children(|row| spawn(row, None));
            })
            .add_systems(Update, sync_stats);
        app.update();
        let world = app.world_mut();
        let mut fields = world.query::<(&StatField, &Text)>();
        let mut name = None;
        let mut guild = None;
        let mut health = None;
        for (field, text) in fields.iter(world) {
            match field {
                StatField::Name => name = Some(text.0.clone()),
                StatField::Guild => guild = Some(text.0.clone()),
                StatField::Health => health = Some(text.0.clone()),
            }
        }
        assert_eq!(name.as_deref(), Some("Player"));
        assert_eq!(guild.as_deref(), Some("No Guild"));
        assert_eq!(health.as_deref(), Some("100 / 100"));
    }
}
