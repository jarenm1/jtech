//! Shared neutral-gray theme for the floating inventory and character panes.
//!
//! Every color the panes draw is a constant here so the look can be retuned in
//! one place.

pub(crate) mod palette {
    use bevy::prelude::Color;

    /// Pane background.
    pub(crate) const BACKGROUND: Color = Color::srgba(0.110, 0.110, 0.118, 1.0);
    /// Inset surfaces: preview viewport, stats block, tooltip.
    pub(crate) const SURFACE: Color = Color::srgba(0.145, 0.145, 0.157, 1.0);
    /// Item cell at rest.
    pub(crate) const SLOT: Color = Color::srgba(0.196, 0.196, 0.212, 1.0);
    /// Item cell under the cursor.
    pub(crate) const SLOT_HOVER: Color = Color::srgba(0.278, 0.278, 0.298, 1.0);
    /// Item cell while pressed (drag start).
    pub(crate) const SLOT_PRESSED: Color = Color::srgba(0.333, 0.333, 0.357, 1.0);
    /// Hairline separators and cell outlines.
    pub(crate) const BORDER: Color = Color::srgba(0.353, 0.353, 0.376, 1.0);
    /// Emphasized outline: hovered cell, tooltip frame.
    pub(crate) const BORDER_STRONG: Color = Color::srgba(0.588, 0.588, 0.620, 1.0);
    /// Primary text.
    pub(crate) const TEXT: Color = Color::srgb(0.902, 0.902, 0.918);
    /// Headings and the hovered item's name.
    pub(crate) const TEXT_STRONG: Color = Color::srgb(1.0, 1.0, 1.0);
    /// Secondary text: labels, quantities, the guild line.
    pub(crate) const TEXT_MUTED: Color = Color::srgba(0.902, 0.902, 0.918, 0.55);
    /// Neutral highlight for stat values and the selected hotbar slot.
    pub(crate) const ACCENT: Color = Color::srgb(0.780, 0.780, 0.820);
    /// Preview viewport clear color.
    pub(crate) const PREVIEW_BG: Color = Color::srgb(0.118, 0.118, 0.129);
    /// Book page background.
    pub(crate) const PAGE: Color = Color::srgb(0.086, 0.086, 0.098);
    /// Corner radius for cells, insets and the title bar.
    pub(crate) const INNER_RADIUS: f32 = 3.0;
}
