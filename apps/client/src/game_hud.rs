//! Gameplay HUD: centered hotbar, crosshair and connection status.
//!
//! Replaces the previous debug/tutorial readout with a minimal game HUD built
//! from native Bevy UI geometry and the bundled default font. The package
//! panel lives in its own module with a higher global z-index so a menu
//! backdrop can cover the gameplay HUD while the panel stays readable.

use bevy::{
    diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin},
    prelude::*,
    text::LineBreak,
};
use crate::{ClientSession, package_hud::ServerPackages};

/// Marks top-level gameplay HUD nodes (game, health and launcher widgets) so the
/// menu can hide them with a single query. The package panel deliberately
/// omits this marker.
#[derive(Component)]
pub(crate) struct GameplayHud;

/// Charcoal panels with warm ivory text and amber accents.
pub(crate) mod palette {
    use bevy::prelude::Color;

    pub(crate) const PANEL: Color = Color::srgba(0.086, 0.090, 0.110, 0.74);
    pub(crate) const SLOT: Color = Color::srgba(0.130, 0.135, 0.160, 0.72);
    pub(crate) const SLOT_SELECTED: Color = Color::srgba(0.220, 0.190, 0.140, 0.94);
    pub(crate) const HOVER: Color = Color::srgba(0.220, 0.200, 0.170, 0.92);
    pub(crate) const PRESSED: Color = Color::srgba(0.310, 0.260, 0.190, 0.96);
    pub(crate) const BORDER: Color = Color::srgba(0.960, 0.930, 0.860, 0.12);
    pub(crate) const IVORY: Color = Color::srgb(0.960, 0.930, 0.860);
    pub(crate) const MUTED: Color = Color::srgba(0.960, 0.930, 0.860, 0.55);
    pub(crate) const AMBER: Color = Color::srgb(0.980, 0.720, 0.300);
    pub(crate) const CONNECTING: Color = Color::srgb(0.980, 0.800, 0.440);
    pub(crate) const DANGER: Color = Color::srgb(1.000, 0.500, 0.420);
    pub(crate) const HEALTH_TRACK: Color = Color::srgba(0.260, 0.110, 0.110, 0.88);
    pub(crate) const HEALTH_FILL: Color = Color::srgb(0.560, 0.740, 0.440);
    pub(crate) const HEALTH_LOW: Color = Color::srgb(0.980, 0.600, 0.300);
}

const SLOT_SIZE: f32 = 58.0;
const SLOT_GAP: f32 = 8.0;

#[derive(Component)]
pub(crate) struct HotbarSlot(u8);

impl HotbarSlot {
    /// Zero-based index into `ClientSession::hotbar`.
    pub(crate) fn index(&self) -> usize {
        usize::from(self.0) - 1
    }
}

#[derive(Component)]
pub(crate) struct SelectedLabel;

#[derive(Component)]
pub(crate) struct ConnectionStatus;

/// Material swatch inside a hotbar tile; dimmed while the stack is empty.
#[derive(Component)]
pub(crate) struct Swatch(u8);

impl Swatch {
    /// Zero-based index into `ClientSession::hotbar`.
    fn index(&self) -> usize {
        usize::from(self.0) - 1
    }
}

/// Stack count label inside a hotbar tile.
#[derive(Component)]
pub(crate) struct SlotCount(u8);

impl SlotCount {
    /// Zero-based index into `ClientSession::hotbar`.
    fn index(&self) -> usize {
        usize::from(self.0) - 1
    }
}


#[derive(Component)]
pub(crate) struct FpsCounter;
pub(crate) fn spawn(commands: &mut Commands) {
    spawn_crosshair(commands);
    spawn_status(commands);
    spawn_hotbar(commands);
    commands.spawn((
        Text::new("FPS --"),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(palette::IVORY),
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(16),
            left: px(16),
            padding: UiRect::axes(px(10), px(6)),
            ..default()
        },
        BackgroundColor(palette::PANEL),
        BorderRadius::all(px(4)),
        GlobalZIndex(110),
        FpsCounter,
    ));
}

pub(crate) fn update_fps(
    diagnostics: Res<DiagnosticsStore>,
    time: Res<Time>,
    mut next_update: Local<f64>,
    mut counter: Single<&mut Text, With<FpsCounter>>,
) {
    let now = time.elapsed_secs_f64();
    if now < *next_update {
        return;
    }
    *next_update = now + 0.25;
    if let Some(fps) = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|value| value.smoothed())
    {
        counter.0 = format!("{fps:.0} FPS");
    }
}
fn spawn_crosshair(commands: &mut Commands) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: percent(50),
            top: percent(50),
            width: px(4),
            height: px(4),
            margin: UiRect {
                left: px(-2),
                top: px(-2),
                ..default()
            },
            ..default()
        },
        BackgroundColor(Color::WHITE),
        BorderRadius::all(px(2)),
        GlobalZIndex(10),
        GameplayHud,
    ));
}

fn spawn_status(commands: &mut Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(palette::CONNECTING),
        TextLayout::new_with_linebreak(LineBreak::WordOrCharacter),
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(56),
            left: px(16),
            max_width: percent(52),
            padding: UiRect::axes(px(12), px(7)),
            border: UiRect::all(px(1)),
            display: Display::None,
            ..default()
        },
        BackgroundColor(palette::PANEL),
        BorderColor::all(palette::BORDER),
        BorderRadius::all(px(6)),
        GlobalZIndex(10),
        GameplayHud,
        ConnectionStatus,
    ));
}

fn spawn_hotbar(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: px(18),
                left: px(0),
                right: px(0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(7),
                ..default()
            },
            GlobalZIndex(10),
            GameplayHud,
        ))
        .with_children(|root| {
            root.spawn((
                Text::new(""),
                TextFont {
                    font_size: 15.0,
                    ..default()
                },
                TextColor(palette::IVORY),
                TextShadow::default(),
                SelectedLabel,
            ));
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Row,
                    column_gap: px(SLOT_GAP),
                    padding: UiRect::all(px(6)),
                    border: UiRect::all(px(1)),
                    ..default()
                },
                BackgroundColor(palette::PANEL),
                BorderColor::all(palette::BORDER),
                BorderRadius::all(px(8)),
            ))
            .with_children(|row| {
                for slot in 1..=10 {
                    row.spawn((
                        Button,
                        Node {
                            width: px(SLOT_SIZE),
                            height: px(SLOT_SIZE),
                            border: UiRect::all(px(2)),
                            align_items: AlignItems::Center,
                            justify_content: JustifyContent::Center,
                            ..default()
                        },
                        BackgroundColor(palette::SLOT),
                        BorderColor::all(palette::BORDER),
                        BorderRadius::all(px(6)),
                        HotbarSlot(slot),
                    ))
                    .with_children(|tile| {
                        tile.spawn((
                            Node {
                                width: percent(70),
                                height: percent(70),
                                ..default()
                            },
                            BackgroundColor(palette::SLOT),
                            Swatch(slot),
                            BorderRadius::all(px(3)),
                        ));
                        tile.spawn((
                            Text::new(""),
                            TextFont {
                                font_size: 13.0,
                                ..default()
                            },
                            TextColor(palette::IVORY),
                            TextShadow::default(),
                            Node {
                                position_type: PositionType::Absolute,
                                bottom: px(2),
                                right: px(5),
                                ..default()
                            },
                            SlotCount(slot),
                        ));
                    });
                }
            });
        });
}

pub(crate) fn update(
    session: Res<ClientSession>,
    mut slots: Query<
        (&HotbarSlot, &mut BackgroundColor, &mut BorderColor),
        Without<Swatch>,
    >,
    mut selected_label: Single<
        &mut Text,
        (
            With<SelectedLabel>,
            Without<ConnectionStatus>,
            Without<SlotCount>,
        ),
    >,
    mut status: Single<(&mut Node, &mut Text, &mut TextColor), With<ConnectionStatus>>,
    mut counts: Query<
        (&SlotCount, &mut Text),
        (
            With<SlotCount>,
            Without<SelectedLabel>,
            Without<ConnectionStatus>,
        ),
    >,
    mut swatches: Query<
        (&Swatch, &mut BackgroundColor),
        (With<Swatch>, Without<HotbarSlot>),
    >,
) {
    for (slot, mut background, mut border) in &mut slots {
        let selected = slot.0 == session.selected;
        background.0 = if selected {
            palette::SLOT_SELECTED
        } else {
            palette::SLOT
        };
        border.set_all(if selected {
            palette::AMBER
        } else {
            palette::BORDER
        });
    }
    **selected_label = Text::new(item_name(&session, session.held_item()));
    for (slot, mut text) in &mut counts {
        // Equipment is not a stack; its tile shows no count.
        let item = session.hotbar[slot.index()];
        let stack = item.is_some_and(|item| !session.packages.is_equipment(item));
        let count = if stack {
            item.map(|item| session.inventory.count(item)).unwrap_or(0)
        } else {
            0
        };
        text.0 = if count == 0 {
            String::new()
        } else {
            count.to_string()
        };
    }
    for (swatch, mut color) in &mut swatches {
        color.0 = match session.hotbar[swatch.index()] {
            // Equipment is always usable; only stacks dim when spent.
            Some(item) if session.packages.is_equipment(item) => {
                item_swatch(&session.packages, item, false)
            }
            Some(item) => {
                item_swatch(&session.packages, item, session.inventory.count(item) == 0)
            }
            None => palette::SLOT,
        };
    }

    let (node, text, color) = &mut *status;
    let connected = session.transport.is_some() && session.id.is_some();
    node.display = if connected {
        Display::None
    } else {
        Display::Flex
    };
    if !connected {
        text.0 = session.status.clone();
        color.0 = if session.status.starts_with("Disconnected") {
            palette::DANGER
        } else {
            palette::CONNECTING
        };
    }
}

/// Display name for an item id; replicated weapon names win, unknown ids read
/// as a numbered unknown.
pub(crate) fn item_name(session: &ClientSession, item: u32) -> String {
    if let Some(name) = session.packages.melee_name(item) {
        return name.to_string();
    }
    if let Some(name) = session.packages.launcher_name(item) {
        return name.to_string();
    }
    match u8::try_from(item) {
        Ok(voxel_world::GRASS) => "Grass".into(),
        Ok(voxel_world::DIRT) => "Dirt".into(),
        Ok(voxel_world::STONE) => "Stone".into(),
        Ok(voxel_world::SAND) => "Sand".into(),
        Ok(voxel_world::WOOD) => "Wood".into(),
        _ => format!("Item {item}"),
    }
}
/// Empty stacks keep their material hue at low alpha so the tile reads as spent.
fn item_swatch(packages: &ServerPackages, item: u32, empty: bool) -> Color {
    let color = if packages.is_launcher(item) {
        Color::srgb(0.45, 0.30, 0.15)
    } else {
        item_color(item)
    };
    if empty { color.with_alpha(0.25) } else { color }
}

/// Base swatch color for an item id.
pub(crate) fn item_color(item: u32) -> Color {
    match u8::try_from(item) {
        Ok(voxel_world::GRASS) => Color::srgb(0.36, 0.60, 0.26),
        Ok(voxel_world::DIRT) => Color::srgb(0.45, 0.30, 0.19),
        Ok(voxel_world::STONE) => Color::srgb(0.55, 0.56, 0.58),
        Ok(voxel_world::SAND) => Color::srgb(0.82, 0.73, 0.48),
        Ok(voxel_world::WOOD) => Color::srgb(0.55, 0.38, 0.21),
        // Melee weapons: cool steel hues distinct from terrain materials.
        Ok(7) => Color::srgb(0.70, 0.72, 0.78),
        Ok(8) => Color::srgb(0.48, 0.42, 0.55),
        Ok(9) => Color::srgb(0.60, 0.55, 0.40),
        _ => palette::AMBER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_hud() -> App {
        let mut app = App::new();
        app.init_resource::<ClientSession>()
            .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
            .add_systems(Update, update);
        app.update();
        app
    }

    #[test]
    fn hotbar_accents_exactly_the_selected_slot() {
        let mut app = app_with_hud();
        for selected in [3, 6, 1] {
            app.world_mut().resource_mut::<ClientSession>().selected = selected;
            app.update();
            let world = app.world_mut();
            let mut slots = world.query::<(&HotbarSlot, &BackgroundColor, &BorderColor)>();
            let mut count = 0;
            let mut accented = Vec::new();
            for (slot, background, border) in slots.iter(world) {
                count += 1;
                if border.top == palette::AMBER {
                    accented.push((slot.0, background.0));
                }
            }
            assert_eq!(count, 10);
            assert_eq!(accented, vec![(selected, palette::SLOT_SELECTED)]);
        }
    }

    #[test]
    fn status_panel_is_visible_until_connected_and_reports_disconnects() {
        let mut app = app_with_hud();
        let world = app.world_mut();
        let mut status =
            world.query_filtered::<(&Node, &Text, &TextColor), With<ConnectionStatus>>();
        let (node, text, color) = status.single(world).unwrap();
        assert_eq!(node.display, Display::Flex);
        assert_eq!(text.0, world.resource::<ClientSession>().status);
        assert_eq!(color.0, palette::CONNECTING);

        app.world_mut()
            .resource_mut::<ClientSession>()
            .disconnect("test link");
        app.update();
        let world = app.world_mut();
        let mut status =
            world.query_filtered::<(&Node, &Text, &TextColor), With<ConnectionStatus>>();
        let (node, text, color) = status.single(world).unwrap();
        assert_eq!(node.display, Display::Flex);
        assert_eq!(text.0, world.resource::<ClientSession>().status);
        assert_eq!(color.0, palette::DANGER);
    }
}
