// ============================================================================
// Color Theme (Tokyo Night)
// ============================================================================

use ratatui::style::Color;

pub const BG: Color = Color::Rgb(0x1a, 0x1b, 0x26); // dark navy
pub const SURFACE: Color = Color::Rgb(0x24, 0x28, 0x3b); // panel bg
pub const PRIMARY: Color = Color::Rgb(0xc0, 0xca, 0xf5); // main text
pub const SECONDARY: Color = Color::Rgb(0x56, 0x5f, 0x89); // dim text
pub const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7); // blue highlight
pub const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a); // running/active
pub const YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68); // pending/idle
pub const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e); // failed/error
pub const CYAN: Color = Color::Rgb(0x73, 0xda, 0xca); // info
pub const PURPLE: Color = Color::Rgb(0xbb, 0x9a, 0xf7); // special
pub const BORDER: Color = Color::Rgb(0x41, 0x48, 0x68); // borders
