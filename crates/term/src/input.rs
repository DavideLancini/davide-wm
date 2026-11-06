//! Keyboard, paste, mouse and focus encoding (xterm), using the current modes.

use crate::terminal::{MouseEncoding, MouseMode, Terminal};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// Text produced by the keymap (already shifted).
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// 1..=12.
    F(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    /// xterm modifier parameter, 1 when none.
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

impl MouseButton {
    fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            MouseButton::WheelUp => 64,
            MouseButton::WheelDown => 65,
            MouseButton::WheelLeft => 66,
            MouseButton::WheelRight => 67,
        }
    }

    fn is_wheel(self) -> bool {
        self.code() >= 64
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    /// Pointer moved; `held` is the button down, if any.
    Motion(Option<MouseButton>),
}

/// Cell coordinates are 0-based screen positions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MouseEvent {
    pub action: MouseAction,
    pub col: usize,
    pub row: usize,
    pub mods: Mods,
}

impl Terminal {
    pub fn encode_key(&self, key: Key, mods: Mods) -> Option<Vec<u8>> {
        let m = mods.param();
        let alt = |mut v: Vec<u8>| {
            if mods.alt {
                v.insert(0, 0x1b);
            }
            Some(v)
        };
        let cursor = |c: u8| {
            Some(if mods.any() {
                format!("\x1b[1;{m}{}", c as char).into_bytes()
            } else if self.modes.app_cursor {
                vec![0x1b, b'O', c]
            } else {
                vec![0x1b, b'[', c]
            })
        };
        let tilde = |n: u8| Some(if mods.any() { format!("\x1b[{n};{m}~") } else { format!("\x1b[{n}~") }.into_bytes());
        match key {
            Key::Char(c) => {
                let mut b = [0; 4];
                let v = match (mods.ctrl, ctrl_code(c)) {
                    (true, Some(code)) => vec![code],
                    _ => c.encode_utf8(&mut b).as_bytes().to_vec(),
                };
                alt(v)
            }
            Key::Enter => alt(if self.modes.newline { b"\r\n".to_vec() } else { b"\r".to_vec() }),
            Key::Tab if mods.shift => Some(b"\x1b[Z".to_vec()),
            Key::Tab => alt(b"\t".to_vec()),
            Key::Backspace => alt(vec![if mods.ctrl { 0x08 } else { 0x7f }]),
            Key::Escape => alt(vec![0x1b]),
            Key::Up => cursor(b'A'),
            Key::Down => cursor(b'B'),
            Key::Right => cursor(b'C'),
            Key::Left => cursor(b'D'),
            Key::Home => cursor(b'H'),
            Key::End => cursor(b'F'),
            Key::Insert => tilde(2),
            Key::Delete => tilde(3),
            Key::PageUp => tilde(5),
            Key::PageDown => tilde(6),
            Key::F(n @ 1..=4) => {
                let c = b"PQRS"[n as usize - 1];
                Some(if mods.any() { format!("\x1b[1;{m}{}", c as char).into_bytes() } else { vec![0x1b, b'O', c] })
            }
            Key::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
            Key::F(_) => None,
        }
    }

    /// Pasted text, bracketed when the application asked (2004). Without
    /// brackets, newlines become CR as if typed.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        if self.modes.bracketed_paste {
            let mut v = b"\x1b[200~".to_vec();
            v.extend_from_slice(text.replace("\x1b[201~", "").as_bytes());
            v.extend_from_slice(b"\x1b[201~");
            v
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
        }
    }

    /// Whether the application tracks the mouse; if not, the compositor
    /// handles it (selection, view scrolling, `encode_mouse` for wheel on the
    /// alternate screen).
    pub fn wants_mouse(&self) -> bool {
        self.modes.mouse != MouseMode::Off
    }

    /// Mouse report for the application, or None if its modes do not want
    /// this event. Without tracking, the wheel on the alternate screen sends
    /// one arrow key per event (1007). Motion within the same cell is
    /// reported once.
    pub fn encode_mouse(&mut self, ev: MouseEvent) -> Option<Vec<u8>> {
        let mode = self.modes.mouse;
        if mode == MouseMode::Off {
            return match ev.action {
                MouseAction::Press(b @ (MouseButton::WheelUp | MouseButton::WheelDown)) if self.alt_active && self.modes.alt_scroll => {
                    self.encode_key(if b == MouseButton::WheelUp { Key::Up } else { Key::Down }, Mods::default())
                }
                _ => None,
            };
        }
        let cell = (ev.col, ev.row);
        let (code, release) = match ev.action {
            MouseAction::Press(b) => (b.code(), false),
            MouseAction::Release(b) if !b.is_wheel() && mode != MouseMode::X10 => (b.code(), true),
            MouseAction::Release(_) => return None,
            MouseAction::Motion(held) => {
                let ok = match mode {
                    MouseMode::Motion => true,
                    MouseMode::Drag => held.is_some(),
                    _ => false,
                };
                if !ok || self.mouse_last == Some(cell) {
                    return None;
                }
                (32 + held.map_or(3, MouseButton::code), false)
            }
        };
        self.mouse_last = Some(cell);
        let code = if mode == MouseMode::X10 {
            code
        } else {
            code | (ev.mods.shift as u32) << 2 | (ev.mods.alt as u32) << 3 | (ev.mods.ctrl as u32) << 4
        };
        let (x, y) = (ev.col as u32 + 1, ev.row as u32 + 1);
        Some(match self.modes.mouse_encoding {
            MouseEncoding::Sgr => format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes(),
            MouseEncoding::Urxvt => format!("\x1b[{};{x};{y}M", 32 + if release { 3 } else { code }).into_bytes(),
            enc => {
                let b = 32 + if release { 3 | (code & !3 & !64) } else { code };
                let mut v = b"\x1b[M".to_vec();
                for n in [b, 32 + x, 32 + y] {
                    if enc == MouseEncoding::Utf8 {
                        let mut buf = [0; 4];
                        v.extend_from_slice(char::from_u32(n.min(2047)).unwrap_or(' ').encode_utf8(&mut buf).as_bytes());
                    } else {
                        v.push(n.min(255) as u8);
                    }
                }
                v
            }
        })
    }

    /// Focus change report when the application asked for it (1004).
    pub fn encode_focus(&self, focused: bool) -> Option<&'static [u8]> {
        self.modes.focus_events.then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
    }
}

/// Control code for Ctrl+`c`, xterm style.
fn ctrl_code(c: char) -> Option<u8> {
    Some(match c {
        '@' | ' ' | '2' | '`' => 0,
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' | '[' | '\\' | ']' | '^' | '_' => c as u8 & 0x1f,
        '3' => 0x1b,
        '4' => 0x1c,
        '5' => 0x1d,
        '6' => 0x1e,
        '7' | '/' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}
