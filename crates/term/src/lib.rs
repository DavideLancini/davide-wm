//! Terminal emulation engine for the compositor's built-in terminal: no I/O,
//! no rendering. The compositor feeds PTY output to [`Terminal::feed`],
//! writes [`Terminal::take_responses`] and the `encode_*` results back to the
//! PTY, and draws the rows [`Terminal::take_damage`] reports through
//! [`Terminal::visible_row`].
//!
//! Memory: a screen cell is 16 bytes; history lines are compressed to their
//! UTF-8 text plus attribute runs (`scrollback`), roughly 100 bytes for a
//! typical shell line. The alternate screen is allocated only while shown.

mod cell;
mod grid;
mod handler;
mod input;
mod parser;
mod persist;
mod scrollback;
mod select;
mod terminal;
#[cfg(test)]
mod tests;

pub use cell::{attr, Cell, Color, ColorKind, Rgb, Theme};
pub use input::{Key, Mods, MouseAction, MouseButton, MouseEvent};
pub use terminal::{CursorInfo, CursorShape, Damage, Modes, MouseEncoding, MouseMode, Point, SelectionMode, Terminal};
