//! All settings of davide-wm. Changing anything here means rebuilding.

/// Modifier bits used by key bindings.
pub mod mods {
    pub const SHIFT: u32 = 1 << 0;
    pub const CTRL: u32 = 1 << 1;
    pub const ALT: u32 = 1 << 2;
    pub const SUPER: u32 = 1 << 3;
    /// Placeholder resolved at runtime: `SUPER` on real hardware, `ALT` when
    /// nested, so bindings do not collide with the host compositor.
    pub const MAIN: u32 = 1 << 31;
}

/// The keysyms used below (values from `xkbcommon-keysyms.h`).
pub mod keys {
    pub const E: u32 = 0x0065;
    pub const Q: u32 = 0x0071;
    pub const T: u32 = 0x0074;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
}

pub use wm::Direction;

#[derive(Clone, Copy, Debug)]
pub enum Action {
    Spawn(&'static str),
    CloseWindow,
    Focus(Direction),
    Exit,
}

pub struct Bind {
    pub mods: u32,
    /// Keysym of the key at shift level 0, so `MAIN+SHIFT+E` matches `e`.
    pub key: u32,
    pub action: Action,
}

use mods::{MAIN, SHIFT};

pub const BINDS: &[Bind] = &[
    Bind { mods: MAIN, key: keys::T, action: Action::Spawn(TERMINAL) },
    Bind { mods: MAIN, key: keys::Q, action: Action::CloseWindow },
    Bind { mods: MAIN, key: keys::LEFT, action: Action::Focus(Direction::Left) },
    Bind { mods: MAIN, key: keys::RIGHT, action: Action::Focus(Direction::Right) },
    Bind { mods: MAIN, key: keys::UP, action: Action::Focus(Direction::Up) },
    Bind { mods: MAIN, key: keys::DOWN, action: Action::Focus(Direction::Down) },
    Bind { mods: MAIN | SHIFT, key: keys::E, action: Action::Exit },
];

pub const TERMINAL: &str = "foot";

pub const KEYBOARD_LAYOUT: &str = "it";
pub const REPEAT_RATE: i32 = 25;
pub const REPEAT_DELAY: i32 = 600;

/// Background behind the windows, as premultiplied RGBA.
pub const BACKGROUND: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// DRM render node used by the nested backend.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";
/// Size of the nested window before the host configures it.
pub const NESTED_SIZE: (i32, i32) = (1280, 800);
