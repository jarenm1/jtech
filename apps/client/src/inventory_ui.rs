//! Inventory panel: a scrollable grid of owned stacks plus drag-and-drop
//! hotbar assignment.
//!
//! The panel is a local view of the replicated `ClientSession::inventory`; the
//! server never sees hotbar assignments. Dragging a cell onto a hotbar slot
//! stores the item id in `ClientSession::hotbar`, which `held_item` resolves
//! into `PlayerInput::selected`. Dragging a hotbar slot onto another swaps the
//! two assignments; dropping it on an inventory cell clears the slot.
//!
//! The explosive bow is a pseudo-item: it is not a carried stack, so the grid
//! prepends a synthetic cell for it and every client may hotbar it.

use bevy::{prelude::*, ui::FocusPolicy};
use protocol::EXPLOSIVE_BOW_ITEM;

use crate::{ClientSession, game_hud, pause_menu::PauseMenu};

/// Local inventory panel state. `shown` caches the last rendered inventory and
/// weapon set so the grid only rebuilds when the replicated contents change.
#[derive(Resource, Default)]
pub(crate) struct InventoryUi {
    pub open: bool,
    shown: Option<(gameplay::Inventory, Vec<protocol::MeleeWeaponInfo>)>,
}

/// Where a dragged stack came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragSource {
    /// An inventory grid cell (or the synthetic bow cell).
    Grid,
    /// Hotbar slot index 0..10.
    Hotbar(usize),
}

#[derive(Clone, Copy)]
pub(crate) struct Drag {
    item: u32,
    source: DragSource,
}

#[derive(Component)]
pub(crate) struct InventoryPanel;

#[derive(Component)]
pub(crate) struct InventoryGrid;

/// One grid cell; carries the item id it renders.
#[derive(Component)]
pub(crate) struct InventoryCell(u32);

/// The floating tile that follows the cursor during a drag.
#[derive(Component)]
pub(crate) struct DragGhost;

const CELL: f32 = 64.0;
const CELL_GAP: f32 = 6.0;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: percent(50),
                width: px(340),
                height: px(320),
                margin: UiRect {
                    left: px(-170),
                    top: px(-160),
                    ..default()
                },
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(12)),
                row_gap: px(8),
                border: UiRect::all(px(1)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(game_hud::palette::PANEL),
            BorderColor::all(game_hud::palette::BORDER),
            BorderRadius::all(px(8)),
            GlobalZIndex(90),
            FocusPolicy::Block,
            // Track hover so hotbar drags can unbind anywhere over the panel.
            Interaction::None,
            InventoryPanel,
        ))
        .with_children(|panel| {
            panel.spawn((
                Text::new("Inventory"),
                TextFont {
                    font_size: 15.0,
                    ..default()
                },
                TextColor(game_hud::palette::IVORY),
            ));
            panel.spawn((
                Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Row,
                    flex_wrap: FlexWrap::Wrap,
                    align_content: AlignContent::FlexStart,
                    row_gap: px(CELL_GAP),
                    column_gap: px(CELL_GAP),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollPosition::default(),
                // Track hover so drops on empty grid space still unbind.
                Interaction::None,
                InventoryGrid,
            ));
        });
}

/// Tab toggles the panel. Escape is handled by `pause_menu::input` so it can
/// close the panel before opening the menu.
pub(crate) fn input(
    keys: Res<ButtonInput<KeyCode>>,
    window: Single<&Window>,
    mut ui: ResMut<InventoryUi>,
    mut menu: ResMut<PauseMenu>,
) {
    if keys.just_pressed(KeyCode::Tab) && window.focused && !menu.open {
        ui.open = !ui.open;
        if !ui.open {
            // A held click must not reach gameplay the frame the panel closes.
            menu.hold_for_mouse_release();
        }
    }
}

/// Show or hide the panel and rebuild the grid when contents change.
pub(crate) fn sync(
    mut commands: Commands,
    session: Res<ClientSession>,
    mut ui: ResMut<InventoryUi>,
    mut panel: Single<&mut Node, With<InventoryPanel>>,
    grid: Single<Entity, With<InventoryGrid>>,
    cells: Query<Entity, With<InventoryCell>>,
) {
    panel.display = if ui.open {
        Display::Flex
    } else {
        Display::None
    };
    if !ui.open {
        ui.shown = None;
        return;
    }
    let shown = (session.inventory.clone(), session.packages.melee_weapons.clone());
    if ui.shown.as_ref() == Some(&shown) {
        return;
    }
    ui.shown = Some(shown);
    for cell in &cells {
        commands.entity(cell).despawn();
    }
    commands.entity(*grid).with_children(|grid| {
        // The bow is usable by everyone; show it ahead of carried stacks.
        spawn_cell(grid, EXPLOSIVE_BOW_ITEM, game_hud::item_name(&session, EXPLOSIVE_BOW_ITEM), None);
        for &(item, count) in session.inventory.entries() {
            // Equipment is unique; its cell shows no stack count.
            let count = (!session.packages.is_equipment(item)).then_some(count);
            spawn_cell(grid, item, game_hud::item_name(&session, item), count);
        }
    });
}

fn spawn_cell(grid: &mut ChildSpawnerCommands, item: u32, name: String, count: Option<u32>) {
    grid.spawn((
        Button,
        Node {
            width: px(CELL),
            height: px(CELL),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(px(1)),
            ..default()
        },
        BackgroundColor(game_hud::palette::SLOT),
        BorderColor::all(game_hud::palette::BORDER),
        BorderRadius::all(px(6)),
        InventoryCell(item),
    ))
    .with_children(|cell| {
        cell.spawn((
            Node {
                width: px(26),
                height: px(26),
                ..default()
            },
            BackgroundColor(game_hud::item_color(item)),
            BorderRadius::all(px(3)),
        ));
        cell.spawn((
            Text::new(name),
            TextFont {
                font_size: 10.0,
                ..default()
            },
            TextColor(game_hud::palette::MUTED),
        ));
        if let Some(count) = count {
            cell.spawn((
                Text::new(count.to_string()),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(game_hud::palette::IVORY),
                TextShadow::default(),
                Node {
                    position_type: PositionType::Absolute,
                    bottom: px(2),
                    right: px(5),
                    ..default()
                },
            ));
        }
    });
}

/// Begin a drag from a grid cell or a filled hotbar slot, keep the ghost under
/// the cursor, and resolve the drop when the mouse releases.
#[allow(clippy::too_many_arguments)] // Drag touches cells, slots, cursor and ghost.
pub(crate) fn drag(
    mut commands: Commands,
    buttons: Res<ButtonInput<MouseButton>>,
    window: Single<&Window>,
    ui: Res<InventoryUi>,
    mut session: ResMut<ClientSession>,
    mut drag: Local<Option<Drag>>,
    pressed_cells: Query<(&Interaction, &InventoryCell), Changed<Interaction>>,
    cells: Query<&Interaction, With<InventoryCell>>,
    panel: Query<&Interaction, With<InventoryPanel>>,
    grids: Query<&Interaction, With<InventoryGrid>>,
    slots: Query<(&Interaction, &game_hud::HotbarSlot)>,
    ghosts: Query<Entity, With<DragGhost>>,
    mut ghost_nodes: Query<&mut Node, With<DragGhost>>,
) {
    if !ui.open {
        if drag.is_some() {
            *drag = None;
            for ghost in &ghosts {
                commands.entity(ghost).despawn();
            }
        }
        return;
    }
    for (interaction, cell) in &pressed_cells {
        if *interaction == Interaction::Pressed {
            *drag = Some(Drag {
                item: cell.0,
                source: DragSource::Grid,
            });
        }
    }
    for (interaction, slot) in &slots {
        if *interaction == Interaction::Pressed
            && let Some(item) = session.hotbar[slot.index()]
        {
            *drag = Some(Drag {
                item,
                source: DragSource::Hotbar(slot.index()),
            });
        }
    }
    let Some(active) = *drag else {
        return;
    };
    if ghosts.is_empty() {
        spawn_ghost(&mut commands, active.item);
    }
    if let Some(position) = window.cursor_position() {
        for mut node in &mut ghost_nodes {
            node.left = px(position.x - 16.0);
            node.top = px(position.y - 16.0);
        }
    }
    if !buttons.just_released(MouseButton::Left) {
        return;
    }
    // Drop resolution: a hovered hotbar slot takes the item; a hotbar-sourced
    // drag landing anywhere on the panel unbinds it; anything else cancels.
    let target = slots.iter().find_map(|(interaction, slot)| {
        (*interaction == Interaction::Hovered).then_some(slot.index())
    });
    let over_panel = panel
        .iter()
        .chain(cells.iter())
        .chain(grids.iter())
        .any(|interaction| *interaction == Interaction::Hovered);
    match (active.source, target) {
        (_, Some(slot)) => {
            let previous = session.hotbar[slot].replace(active.item);
            if let DragSource::Hotbar(source) = active.source {
                session.hotbar[source] = previous;
            }
        }
        (DragSource::Hotbar(source), None) if over_panel => {
            session.hotbar[source] = None;
        }
        _ => {}
    }
    *drag = None;
    for ghost in &ghosts {
        commands.entity(ghost).despawn();
    }
}

fn spawn_ghost(commands: &mut Commands, item: u32) {
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            width: px(32),
            height: px(32),
            border: UiRect::all(px(1)),
            ..default()
        },
        BackgroundColor(game_hud::item_color(item)),
        BorderColor::all(game_hud::palette::IVORY),
        BorderRadius::all(px(4)),
        GlobalZIndex(120),
        // The ghost must not steal hover from the drop targets beneath it.
        FocusPolicy::Pass,
        DragGhost,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::window::{CursorGrabMode, CursorOptions};

    /// App with the real panel, pause menu and cursor plumbing.
    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<InventoryUi>()
            .init_resource::<PauseMenu>()
            .init_resource::<ClientSession>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .insert_resource(crate::Options {
                server: "127.0.0.1:4000".parse().unwrap(),
                bot: false,
                frames: None,
                screenshot: None,
                lighting: crate::lighting::DayCycle::default(),
            })
            .add_systems(Startup, |mut commands: Commands| {
                spawn(&mut commands);
                crate::pause_menu::spawn(&mut commands);
            })
            .add_systems(
                Update,
                (input, sync, drag)
                    .chain()
                    .before(crate::pause_menu::sync),
            )
            .add_systems(
                Update,
                (
                    crate::pause_menu::input,
                    crate::pause_menu::actions,
                    crate::pause_menu::sync,
                    crate::pause_menu::sync_cursor,
                )
                    .chain(),
            );
        app.world_mut().spawn((
            Window {
                focused: true,
                ..default()
            },
            CursorOptions {
                visible: false,
                grab_mode: CursorGrabMode::Locked,
                ..default()
            },
        ));
        app.world_mut()
            .spawn((Node::default(), game_hud::GameplayHud, Visibility::Inherited));
        app.update();
        app
    }

    #[test]
    fn tab_opens_the_panel_and_releases_the_cursor() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Tab);
        app.update();
        assert!(app.world().resource::<InventoryUi>().open);
        let cursor = app
            .world_mut()
            .query::<&CursorOptions>()
            .single(app.world())
            .unwrap();
        assert!(cursor.visible);
        assert_eq!(cursor.grab_mode, CursorGrabMode::None);
        // The grid shows the synthetic bow cell even with an empty inventory.
        let cells = app
            .world_mut()
            .query::<&InventoryCell>()
            .iter(app.world())
            .count();
        assert_eq!(cells, 1);

        // Escape peels the panel without opening the pause menu.
        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = default();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        assert!(!app.world().resource::<InventoryUi>().open);
        assert!(!app.world().resource::<PauseMenu>().open);
        let cursor = app
            .world_mut()
            .query::<&CursorOptions>()
            .single(app.world())
            .unwrap();
        assert!(!cursor.visible);
        assert_eq!(cursor.grab_mode, CursorGrabMode::Locked);
    }

    #[test]
    fn grid_rebuilds_with_replicated_stacks() {
        let mut app = app();
        let mut inventory = gameplay::Inventory::new();
        inventory.add(u32::from(voxel_world::STONE), 40);
        inventory.add(u32::from(voxel_world::WOOD), 7);
        app.world_mut().resource_mut::<ClientSession>().inventory = inventory;
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Tab);
        app.update();
        let items: Vec<u32> = app
            .world_mut()
            .query::<&InventoryCell>()
            .iter(app.world())
            .map(|cell| cell.0)
            .collect();
        assert_eq!(
            items,
            vec![EXPLOSIVE_BOW_ITEM, u32::from(voxel_world::STONE), u32::from(voxel_world::WOOD)]
        );
    }
}
