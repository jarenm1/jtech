//! Inventory pane: a scrollable list of owned items grouped into categories,
//! with the character preview and stats beside it, plus drag-and-drop hotbar
//! assignment.
//!
//! The pane is a local view of the replicated `ClientSession::inventory`; the
//! server never sees hotbar assignments. Dragging a row onto a hotbar slot
//! stores the item id in `ClientSession::hotbar`, which `held_item` resolves
//! into `PlayerInput::selected`. Dragging a hotbar slot onto another swaps the
//! two assignments; dropping it on an inventory row clears the slot.
//!
//! The explosive bow is a pseudo-item: it is not a carried stack, so the list
//! prepends a synthetic row for it and every client may hotbar it.

use std::fmt::Write as _;

use bevy::{prelude::*, ui::FocusPolicy};
use protocol::EXPLOSIVE_BOW_ITEM;

use crate::{
    ClientSession, book, character_ui, game_hud,
    item_icon::{self, ItemIcons},
    pane,
    pause_menu::PauseMenu,
    ui_theme::palette,
};

const PANE_SIZE: Vec2 = Vec2::new(768.0, 384.0);
const ROW_HEIGHT: f32 = 26.0;
const ITEM_PREVIEW: f32 = 200.0;

/// Local inventory pane state. `shown` caches the last rendered inventory and
/// the package revision that produced the weapon names, so the list only
/// rebuilds when the replicated contents change.
#[derive(Resource, Default)]
pub(crate) struct InventoryUi {
    pub open: bool,
    shown: Option<Shown>,
}

/// Snapshot of what the list renders: owned stacks plus the package revision
/// that names them. `ServerPackages::receive` bumps `revision` whenever
/// `melee_weapons` is replaced, so it keys the name lookups too.
struct Shown {
    inventory: gameplay::Inventory,
    revision: Option<u64>,
    category: Category,
}

/// Where a dragged stack came from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragSource {
    /// An inventory row (or the synthetic bow row).
    List,
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
pub(crate) struct InventoryList;

/// One list row; carries the item id it renders.
#[derive(Component)]
pub(crate) struct InventoryRow(u32);

/// A row's icon tile; carries the item id whose icon it shows.
#[derive(Component)]
pub(crate) struct ItemIcon(u32);

/// One category switcher tab.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CategoryTab(Category);

/// Category the list shows.
#[derive(Resource)]
pub(crate) struct SelectedCategory(pub Category);

impl Default for SelectedCategory {
    fn default() -> Self {
        Self(Category::Weapons)
    }
}

/// The item preview tile; its background is the item's colour swatch.
#[derive(Component)]
pub(crate) struct ItemPreviewTile;

/// The floating tile that follows the cursor during a drag.
#[derive(Component)]
pub(crate) struct DragGhost;

/// The floating item-detail tooltip root.
#[derive(Component)]
pub(crate) struct ItemTooltip;

#[derive(Component)]
pub(crate) struct TooltipName;

#[derive(Component)]
pub(crate) struct TooltipBody;

/// Sections the list sorts items into, in display order.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    Weapons,
    Materials,
    Other,
}

const CATEGORIES: [Category; 3] = [Category::Weapons, Category::Materials, Category::Other];

impl Category {
    fn label(self) -> &'static str {
        match self {
            Category::Weapons => "WEAPONS",
            Category::Materials => "MATERIALS",
            Category::Other => "OTHER",
        }
    }
}

/// Which section an item sorts into: the bow and equipment are weapons, block
/// materials are materials, anything else falls through to other.
fn category(session: &ClientSession, item: u32) -> Category {
    if item == EXPLOSIVE_BOW_ITEM || session.packages.is_equipment(item) {
        Category::Weapons
    } else if u8::try_from(item)
        .is_ok_and(|id| (voxel_world::GRASS..=voxel_world::WOOD).contains(&id))
    {
        Category::Materials
    } else {
        Category::Other
    }
}

/// Build the pane, its page UI trees and its tooltip. Returns the pane entity.
pub(crate) fn spawn(
    commands: &mut Commands,
    preview: Option<Handle<Image>>,
    item_preview: &item_icon::ItemPreview,
    book: &book::BookView,
) -> Entity {
    // Pane root: shows the rendered book and holds the close button.
    let panel = commands
        .spawn((pane::root(PANE_SIZE), InventoryPanel))
        .id();
    commands.entity(panel).with_children(|panel| {
        panel.spawn((
            Node {
                width: percent(100),
                height: percent(100),
                ..default()
            },
            ImageNode {
                image: book.texture.clone(),
                image_mode: NodeImageMode::Stretch,
                ..default()
            },
        ));
        pane::spawn_close_button(panel);
    });
    // Left page: category tabs, then the item preview beside the list.
    commands
        .spawn((
            Node {
                width: px(book::PAGE_SIZE.x),
                height: px(book::PAGE_SIZE.y),
                flex_direction: FlexDirection::Column,
                row_gap: px(10),
                padding: UiRect::all(px(16)),
                ..default()
            },
            UiTargetCamera(book.page_camera(book::PageSide::Left)),
        ))
        .with_children(|page| {
            page.spawn(Node {
                width: percent(100),
                flex_direction: FlexDirection::Row,
                column_gap: px(6),
                ..default()
            })
            .with_children(|tabs| {
                for category in CATEGORIES {
                    spawn_tab(tabs, category);
                }
            });
            page.spawn(Node {
                width: percent(100),
                flex_grow: 1.0,
                flex_direction: FlexDirection::Row,
                column_gap: px(12),
                ..default()
            })
            .with_children(|row| {
                row.spawn((
                    Node {
                        width: px(ITEM_PREVIEW),
                        height: percent(100),
                        border: UiRect::all(px(1)),
                        ..default()
                    },
                    BackgroundColor(palette::SURFACE),
                    BorderColor::all(palette::BORDER),
                    BorderRadius::all(px(palette::INNER_RADIUS)),
                    ImageNode {
                        image: item_preview.texture.clone(),
                        image_mode: NodeImageMode::Stretch,
                        ..default()
                    },
                    ItemPreviewTile,
                ));
                row.spawn((
                    Node {
                        flex_grow: 1.0,
                        flex_basis: px(0.0),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(2),
                        overflow: Overflow::scroll_y(),
                        ..default()
                    },
                    ScrollPosition::default(),
                    // Track hover so drops on empty list space still unbind.
                    Interaction::None,
                    InventoryList,
                ));
            });
        });
    // Right page: the character preview and stats.
    commands
        .spawn((
            Node {
                width: px(book::PAGE_SIZE.x),
                height: px(book::PAGE_SIZE.y),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(16)),
                ..default()
            },
            UiTargetCamera(book.page_camera(book::PageSide::Right)),
        ))
        .with_children(|page| character_ui::spawn(page, preview));
    spawn_tooltip(commands);
    panel
}

/// One category switcher tab.
fn spawn_tab(tabs: &mut ChildSpawnerCommands, category: Category) {
    tabs.spawn((
        Button,
        Node {
            flex_grow: 1.0,
            height: px(26),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border: UiRect::all(px(1)),
            ..default()
        },
        BackgroundColor(palette::SLOT),
        BorderColor::all(palette::BORDER),
        BorderRadius::all(px(palette::INNER_RADIUS)),
        CategoryTab(category),
    ))
    .with_children(|tab| {
        tab.spawn((
            Text::new(category.label()),
            TextFont {
                font_size: 11.0,
                ..default()
            },
            TextColor(palette::TEXT_MUTED),
        ));
    });
}

fn spawn_tooltip(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                flex_direction: FlexDirection::Row,
                column_gap: px(12),
                padding: UiRect::all(px(10)),
                border: UiRect::all(px(1)),
                max_width: px(260),
                display: Display::None,
                ..default()
            },
            BackgroundColor(palette::BACKGROUND),
            BorderColor::all(palette::BORDER_STRONG),
            BorderRadius::all(px(palette::INNER_RADIUS)),
            GlobalZIndex(130),
            // The tooltip must not steal hover from the row beneath it.
            FocusPolicy::Pass,
            ItemTooltip,
        ))
        .with_children(|tooltip| {
            // One detail column today; an item-comparison column can be added
            // beside it later without changing the tooltip root.
            tooltip
                .spawn(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(4),
                    ..default()
                })
                .with_children(|detail| {
                    detail.spawn((
                        Text::new(""),
                        TextFont {
                            font_size: 14.0,
                            ..default()
                        },
                        TextColor(palette::TEXT_STRONG),
                        TextShadow::default(),
                        TooltipName,
                    ));
                    detail.spawn((
                        Text::new(""),
                        TextFont {
                            font_size: 12.0,
                            ..default()
                        },
                        TextColor(palette::TEXT_MUTED),
                        TextShadow::default(),
                        TooltipBody,
                    ));
                });
        });
}

/// **Tab** toggles the pane. Escape is handled by `pause_menu::input` so it can
/// close the pane before opening the menu.
pub(crate) fn input(
    keys: Res<ButtonInput<KeyCode>>,
    window: Single<&Window>,
    mut ui: ResMut<InventoryUi>,
    mut menu: ResMut<PauseMenu>,
) {
    if keys.just_pressed(KeyCode::Tab) && window.focused && !menu.open {
        ui.open = !ui.open;
        if !ui.open {
            // A held click must not reach gameplay the frame the pane closes.
            menu.hold_for_mouse_release();
        }
    }
}

/// Show or hide the pane and rebuild the list when the replicated contents
/// change.
pub(crate) fn sync(
    mut commands: Commands,
    session: Res<ClientSession>,
    selected: Res<SelectedCategory>,
    mut ui: ResMut<InventoryUi>,
    mut preview: ResMut<item_icon::PreviewItem>,
    mut panel: Single<&mut Node, With<InventoryPanel>>,
    list: Single<Entity, With<InventoryList>>,
    rows: Query<Entity, With<InventoryRow>>,
) {
    let desired = if ui.open {
        Display::Flex
    } else {
        Display::None
    };
    if panel.display != desired {
        panel.display = desired;
    }
    if !ui.open {
        ui.shown = None;
        return;
    }
    let revision = session.packages.revision;
    let unchanged = ui.shown.as_ref().is_some_and(|shown| {
        shown.inventory == session.inventory
            && shown.revision == revision
            && shown.category == selected.0
    });
    if unchanged {
        return;
    }
    ui.shown = Some(Shown {
        inventory: session.inventory.clone(),
        revision,
        category: selected.0,
    });
    for row in &rows {
        commands.entity(row).despawn();
    }
    let mut group: Vec<(u32, Option<u32>)> = Vec::new();
    // The bow is usable by everyone; show it ahead of carried stacks.
    if selected.0 == Category::Weapons {
        group.push((EXPLOSIVE_BOW_ITEM, None));
    }
    for &(item, count) in session.inventory.entries() {
        if category(&session, item) != selected.0 {
            continue;
        }
        // Equipment is unique; its row shows no stack count.
        let count = (!session.packages.is_equipment(item)).then_some(count);
        group.push((item, count));
    }
    group.sort_by_cached_key(|&(item, _)| game_hud::item_name(&session, item).into_owned());
    // Keep the preview on a listed item: default to the first, and drop it when
    // the category no longer contains it.
    if !group.iter().any(|&(item, _)| Some(item) == preview.0) {
        preview.0 = group.first().map(|&(item, _)| item);
    }
    commands.entity(*list).with_children(|list| {
        for &(item, count) in &group {
            spawn_row(list, item, game_hud::item_name(&session, item), count);
        }
    });
}

/// Switch the visible category when a tab is pressed.
pub(crate) fn select_category(
    tabs: Query<(&Interaction, &CategoryTab), Changed<Interaction>>,
    mut selected: ResMut<SelectedCategory>,
) {
    for (interaction, tab) in &tabs {
        if *interaction == Interaction::Pressed && selected.0 != tab.0 {
            selected.0 = tab.0;
        }
    }
}

/// Accent the active category tab.
pub(crate) fn sync_tabs(
    selected: Res<SelectedCategory>,
    mut tabs: Query<(&CategoryTab, &mut BackgroundColor, &mut BorderColor)>,
) {
    for (tab, mut background, mut border) in &mut tabs {
        let active = tab.0 == selected.0;
        let fill = if active {
            palette::SLOT_HOVER
        } else {
            palette::SLOT
        };
        let edge = if active {
            palette::BORDER_STRONG
        } else {
            palette::BORDER
        };
        if background.0 != fill {
            background.0 = fill;
        }
        if border.top != edge {
            *border = BorderColor::all(edge);
        }
    }
}

/// Show the hovered item's colour behind the preview model.
pub(crate) fn sync_preview_tile(
    wanted: Res<item_icon::PreviewItem>,
    mut tiles: Query<&mut BackgroundColor, With<ItemPreviewTile>>,
) {
    let color = wanted.0.map_or(palette::SURFACE, game_hud::item_color);
    for mut background in &mut tiles {
        if background.0 != color {
            background.0 = color;
        }
    }
}

fn spawn_row(
    list: &mut ChildSpawnerCommands,
    item: u32,
    name: impl Into<String>,
    count: Option<u32>,
) {
    list.spawn((
        Button,
        Node {
            width: percent(100),
            height: px(ROW_HEIGHT),
            align_items: AlignItems::Center,
            column_gap: px(8),
            padding: UiRect::axes(px(6), px(0)),
            border: UiRect::all(px(1)),
            ..default()
        },
        BackgroundColor(palette::SLOT),
        BorderColor::all(palette::BORDER),
        BorderRadius::all(px(palette::INNER_RADIUS)),
        InventoryRow(item),
    ))
    .with_children(|row| {
        row.spawn((
            Node {
                width: px(18),
                height: px(18),
                ..default()
            },
            // The colour swatch is the 2D fallback; a rendered 3D icon covers it
            // once `item_icon` has one for this item.
            BackgroundColor(game_hud::item_color(item)),
            ImageNode {
                image_mode: NodeImageMode::Stretch,
                ..default()
            },
            BorderRadius::all(px(palette::INNER_RADIUS)),
            ItemIcon(item),
        ));
        row.spawn((
            Text::new(name),
            TextFont {
                font_size: 12.0,
                ..default()
            },
            TextColor(palette::TEXT),
            Node {
                flex_grow: 1.0,
                ..default()
            },
        ));
        if let Some(count) = count {
            row.spawn((
                Text::new(count.to_string()),
                TextFont {
                    font_size: 12.0,
                    ..default()
                },
                TextColor(palette::TEXT_MUTED),
            ));
        }
    });
}

/// Tint rows by interaction state so hover and drag-start read clearly.
pub(crate) fn highlight(
    mut rows: Query<(&Interaction, &mut BackgroundColor, &mut BorderColor), With<InventoryRow>>,
) {
    for (interaction, mut background, mut border) in &mut rows {
        let (fill, edge) = match interaction {
            Interaction::Pressed => (palette::SLOT_PRESSED, palette::BORDER_STRONG),
            Interaction::Hovered => (palette::SLOT_HOVER, palette::BORDER_STRONG),
            Interaction::None => (palette::SLOT, palette::BORDER),
        };
        if background.0 != fill {
            background.0 = fill;
        }
        if border.top != edge {
            *border = BorderColor::all(edge);
        }
    }
}

/// Point each row's icon tile at its rendered texture, falling back to the
/// transparent default so the colour swatch shows through.
pub(crate) fn apply_icons(
    icons: Res<ItemIcons>,
    mut tiles: Query<(&ItemIcon, &mut ImageNode)>,
) {
    let fallback = ImageNode::default().image;
    for (icon, mut node) in &mut tiles {
        let desired = icons
            .texture(icon.0)
            .cloned()
            .unwrap_or_else(|| fallback.clone());
        if node.image != desired {
            node.image = desired;
        }
    }
}

/// Inputs that determine the tooltip text. Compared against the session so the
/// text is rebuilt only when the hovered item, the package revision, or the
/// stack count changes — not every frame.
#[derive(Default)]
pub(crate) struct TooltipShown {
    item: Option<u32>,
    revision: Option<u64>,
    count: u32,
}

/// Show the hovered item's detail near the cursor. Equipment lists its combat
/// stats; stacks list their count. Hidden while the pane is closed or a drag is
/// in progress.
pub(crate) fn tooltip(
    session: Res<ClientSession>,
    ui: Res<InventoryUi>,
    buttons: Res<ButtonInput<MouseButton>>,
    window: Single<&Window>,
    rows: Query<(&Interaction, &InventoryRow)>,
    mut tooltip: Single<&mut Node, With<ItemTooltip>>,
    mut name: Single<&mut Text, (With<TooltipName>, Without<TooltipBody>)>,
    mut body: Single<&mut Text, (With<TooltipBody>, Without<TooltipName>)>,
    mut shown: Local<TooltipShown>,
    mut preview: ResMut<item_icon::PreviewItem>,
) {
    let hovered = ui
        .open
        .then(|| {
            rows.iter()
                .find_map(|(interaction, row)| (*interaction == Interaction::Hovered).then_some(row.0))
        })
        .flatten()
        .filter(|_| !buttons.pressed(MouseButton::Left));
    let Some(item) = hovered else {
        tooltip.display = Display::None;
        shown.item = None;
        return;
    };
    if preview.0 != Some(item) {
        preview.0 = Some(item);
    }
    tooltip.display = Display::Flex;
    let revision = session.packages.revision;
    let count = session.inventory.count(item);
    if shown.item != Some(item) || shown.revision != revision || shown.count != count {
        shown.item = Some(item);
        shown.revision = revision;
        shown.count = count;
        let (title, detail) = item_detail(&session, item);
        if name.0 != title {
            name.0 = title;
        }
        if body.0 != detail {
            body.0 = detail;
        }
    }
    if let Some(position) = window.cursor_position() {
        tooltip.left = px(position.x + 18.0);
        tooltip.top = px(position.y + 12.0);
    }
}

/// Title and stat lines for one item. Equipment (unique weapons) shows its
/// combat stats; the bow shows its fire rate; everything else shows its count.
/// This is the seam an item-comparison view extends.
fn item_detail(session: &ClientSession, item: u32) -> (String, String) {
    let title = game_hud::item_name(session, item).into_owned();
    if let Some(weapon) = session
        .packages
        .melee_weapons
        .iter()
        .find(|weapon| weapon.item == item)
    {
        let mut detail = String::new();
        let _ = write!(detail, "Damage  {}", weapon.damage);
        let _ = write!(detail, "\nRange  {:.1} m", weapon.range);
        if weapon.cooldown_ticks > 0 {
            let _ = write!(detail, "\nSpeed  {:.1}/s", 60.0 / weapon.cooldown_ticks as f32);
        }
        let _ = write!(detail, "\nKnockback  {:.0}", weapon.knockback);
        detail.push_str("\nUnique");
        return (title, detail);
    }
    if item == EXPLOSIVE_BOW_ITEM {
        return (
            title,
            format!("Ranged\nShots/s  {}", session.packages.bow_shots_per_second),
        );
    }
    (title, format!("Quantity  {}", session.inventory.count(item)))
}

/// Begin a drag from a list row or a filled hotbar slot, keep the ghost under
/// the cursor, and resolve the drop when the mouse releases.
#[allow(clippy::too_many_arguments)] // Drag touches rows, slots, cursor and ghost.
pub(crate) fn drag(
    mut commands: Commands,
    buttons: Res<ButtonInput<MouseButton>>,
    window: Single<&Window>,
    ui: Res<InventoryUi>,
    mut session: ResMut<ClientSession>,
    mut drag: Local<Option<Drag>>,
    pressed_rows: Query<(&Interaction, &InventoryRow), Changed<Interaction>>,
    rows: Query<&Interaction, With<InventoryRow>>,
    panel: Query<&Interaction, With<InventoryPanel>>,
    lists: Query<&Interaction, With<InventoryList>>,
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
    for (interaction, row) in &pressed_rows {
        if *interaction == Interaction::Pressed {
            *drag = Some(Drag {
                item: row.0,
                source: DragSource::List,
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
    // drag landing anywhere on the pane unbinds it; anything else cancels.
    let target = slots.iter().find_map(|(interaction, slot)| {
        (*interaction == Interaction::Hovered).then_some(slot.index())
    });
    let over_pane = panel
        .iter()
        .chain(rows.iter())
        .chain(lists.iter())
        .any(|interaction| *interaction == Interaction::Hovered);
    match (active.source, target) {
        (_, Some(slot)) => {
            let previous = session.hotbar[slot].replace(active.item);
            if let DragSource::Hotbar(source) = active.source {
                session.hotbar[source] = previous;
            }
        }
        (DragSource::Hotbar(source), None) if over_pane => {
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
        BorderColor::all(palette::TEXT_STRONG),
        BorderRadius::all(px(palette::INNER_RADIUS)),
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

    /// App with the real pane, pause menu and cursor plumbing.
    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<InventoryUi>()
            .init_resource::<SelectedCategory>()
            .init_resource::<ItemIcons>()
            .init_resource::<item_icon::PreviewItem>()
            .init_resource::<Assets<Image>>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<PauseMenu>()
            .init_resource::<ClientSession>()
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .insert_resource(crate::Options {
                server: "127.0.0.1:4000".parse().unwrap(),
                ..default()
            })
            .add_systems(Startup, |mut commands: Commands,
                                   mut images: ResMut<Assets<Image>>,
                                   mut meshes: ResMut<Assets<Mesh>>,
                                   mut materials: ResMut<Assets<StandardMaterial>>| {
                let item_preview = item_icon::spawn_preview(&mut commands, &mut images);
                let book = book::spawn(&mut commands, &mut images, &mut meshes, &mut materials);
                spawn(&mut commands, None, &item_preview, &book);
                commands.insert_resource(item_preview);
                commands.insert_resource(book);
                crate::pause_menu::spawn(&mut commands);
            })
            .add_systems(
                Update,
                (
                    input,
                    select_category,
                    sync,
                    sync_tabs,
                    sync_preview_tile,
                    character_ui::sync_stats,
                    highlight,
                    apply_icons,
                    tooltip,
                    drag,
                )
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
    fn tab_opens_the_pane_and_releases_the_cursor() {
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
        // The list shows the synthetic bow row even with an empty inventory.
        let rows = app
            .world_mut()
            .query::<&InventoryRow>()
            .iter(app.world())
            .count();
        assert_eq!(rows, 1);

        // Escape peels the pane without opening the pause menu.
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

    fn listed_items(app: &mut App) -> Vec<u32> {
        app.world_mut()
            .query::<&InventoryRow>()
            .iter(app.world())
            .map(|row| row.0)
            .collect()
    }

    #[test]
    fn category_tabs_switch_the_list() {
        let mut app = app();
        let mut inventory = gameplay::Inventory::new();
        inventory.add(u32::from(voxel_world::WOOD), 7);
        inventory.add(u32::from(voxel_world::STONE), 40);
        inventory.add(7, 1);
        let mut session = app.world_mut().resource_mut::<ClientSession>();
        session.inventory = inventory;
        session.packages.receive(
            1,
            vec![],
            10,
            vec![protocol::MeleeWeaponInfo {
                item: 7,
                package: "melee".into(),
                name: "Knife".into(),
                kind: protocol::ItemKind::Equipment,
                range: 2.5,
                damage: 8,
                cooldown_ticks: 18,
                knockback: 200.0,
                model: None,
            }],
        );
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Tab);
        app.update();
        // Weapons first (bow, then the knife), sorted by name.
        assert_eq!(listed_items(&mut app), vec![EXPLOSIVE_BOW_ITEM, 7]);
        // Switching category swaps the list, materials by name. Clear the key
        // input first: without the input plugin `just_pressed` never resets.
        *app.world_mut().resource_mut::<ButtonInput<KeyCode>>() = default();
        app.world_mut().resource_mut::<SelectedCategory>().0 = Category::Materials;
        app.update();
        assert_eq!(
            listed_items(&mut app),
            vec![u32::from(voxel_world::STONE), u32::from(voxel_world::WOOD)]
        );
    }

    #[test]
    fn rendered_icon_replaces_the_swatch() {
        let mut app = app();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Tab);
        app.update();
        let tile = app
            .world_mut()
            .query_filtered::<Entity, With<ItemIcon>>()
            .single(app.world())
            .unwrap();
        // With no rendered icon the tile keeps the transparent default image,
        // so the colour swatch shows through.
        let fallback = app.world().get::<ImageNode>(tile).unwrap().image.clone();
        let texture = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::default());
        assert_ne!(texture, fallback);
        app.world_mut()
            .resource_mut::<ItemIcons>()
            .set(EXPLOSIVE_BOW_ITEM, texture.clone());
        app.update();
        assert_eq!(app.world().get::<ImageNode>(tile).unwrap().image, texture);
    }

    #[test]
    fn equipment_detail_lists_combat_stats_and_stacks_list_counts() {
        let mut session = ClientSession::default();
        session.packages.receive(
            1,
            vec![],
            10,
            vec![protocol::MeleeWeaponInfo {
                item: 7,
                package: "melee".into(),
                name: "Knife".into(),
                kind: protocol::ItemKind::Equipment,
                range: 2.5,
                damage: 8,
                cooldown_ticks: 18,
                knockback: 200.0,
                model: None,
            }],
        );
        session.inventory.add(u32::from(voxel_world::STONE), 40);
        let (title, detail) = item_detail(&session, 7);
        assert_eq!(title, "Knife");
        assert!(detail.contains("Damage  8"), "{detail}");
        assert!(detail.contains("Range  2.5 m"), "{detail}");
        assert!(detail.contains("Speed  3.3/s"), "{detail}");
        assert!(detail.contains("Unique"), "{detail}");
        let (title, detail) = item_detail(&session, u32::from(voxel_world::STONE));
        assert_eq!(title, "Stone");
        assert_eq!(detail, "Quantity  40");
    }
}
