//! The calx palette, "Alexandria": warm lamplight on old stone, with a few
//! mineral pigments. Each color means one thing in both views, so color
//! carries meaning rather than decoration:
//!
//! - amber: calx itself: headings, panel titles, the prompt, open trifecta legs
//! - verdigris: tools, and key hints
//! - plum: you, the user
//! - sage: success, and added lines
//! - clay: waiting on you, steering, and a refused plan
//! - brick: errors, removed lines, and all three trifecta legs open
//! - lapis: measures, such as context bars
//!
//! They are 256-color indices rather than 24-bit colors, so they look the
//! same in Terminal.app, Windows Terminal, and tmux.

use ratatui::style::Color;

pub const AMBER: Color = Color::Indexed(179); // #d7af5f
pub const VERDIGRIS: Color = Color::Indexed(73); // #5fafaf
pub const PLUM: Color = Color::Indexed(139); // #af87af
pub const SAGE: Color = Color::Indexed(108); // #87af87
pub const CLAY: Color = Color::Indexed(173); // #d7875f
pub const BRICK: Color = Color::Indexed(167); // #d75f5f
pub const LAPIS: Color = Color::Indexed(68); // #5f87d7
/// A quieter lapis and a slate, for the Context panel's other parts.
pub const SLATE: Color = Color::Indexed(60); // #5f5f87
pub const STONE: Color = Color::Indexed(244); // #808080

/// The same colors as ANSI escapes, for the plain view.
pub mod ansi {
    pub const AMBER: &str = "\x1b[38;5;179m";
    pub const VERDIGRIS: &str = "\x1b[38;5;73m";
    pub const PLUM: &str = "\x1b[38;5;139m";
    pub const SAGE: &str = "\x1b[38;5;108m";
    pub const CLAY: &str = "\x1b[38;5;173m";
    pub const BRICK: &str = "\x1b[38;5;167m";
    pub const LAPIS: &str = "\x1b[38;5;68m";
}
