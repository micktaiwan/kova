//! The tab color palette, shared by the renderer (which paints it) and the IPC
//! transport (which validates `set-tab-color` indices against its length).
//! Kept apart from both so the transport does not depend on the renderer.

/// Predefined tab color palette (macOS Finder-style tags).
/// Each entry is [R, G, B] in 0.0–1.0.
pub const TAB_COLORS: [[f32; 3]; 6] = [
    [0.82, 0.22, 0.22], // Red
    [0.90, 0.55, 0.15], // Orange
    [0.85, 0.75, 0.15], // Yellow
    [0.30, 0.70, 0.30], // Green
    [0.25, 0.50, 0.85], // Blue
    [0.60, 0.35, 0.75], // Violet
];
