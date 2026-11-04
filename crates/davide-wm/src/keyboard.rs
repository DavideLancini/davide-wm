//! xkbcommon keymap and state, shared by all clients.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr::null;

use sys::xkb;

pub struct Keyboard {
    ctx: *mut xkb::xkb_context,
    keymap: *mut xkb::xkb_keymap,
    state: *mut xkb::xkb_state,
    keymap_fd: OwnedFd,
    keymap_size: u32,
    /// Modifier indices for shift, ctrl, alt, super, in `config::mods` order.
    mod_index: [xkb::xkb_mod_index_t; 4],
    pressed: Vec<u32>,
}

/// Evdev keycodes are offset by 8 in xkb.
const XKB_OFFSET: u32 = 8;

impl Keyboard {
    pub fn new(layout: &str) -> io::Result<Keyboard> {
        let layout = CString::new(layout).unwrap();
        unsafe {
            let ctx = xkb::xkb_context_new(0);
            if ctx.is_null() {
                return Err(io::Error::other("xkb_context_new failed"));
            }
            let names = xkb::xkb_rule_names { rules: null(), model: null(), layout: layout.as_ptr(), variant: null(), options: null() };
            let keymap = xkb::xkb_keymap_new_from_names(ctx, &names, 0);
            if keymap.is_null() {
                xkb::xkb_context_unref(ctx);
                return Err(io::Error::other("cannot compile the keymap"));
            }
            let text = xkb::xkb_keymap_get_as_string(keymap, xkb::KEYMAP_FORMAT_TEXT_V1);
            let bytes = CStr::from_ptr(text).to_bytes_with_nul().to_vec();
            libc::free(text.cast());
            let keymap_fd = sealed_memfd(&bytes)?;
            let mod_index = [c"Shift", c"Control", c"Mod1", c"Mod4"].map(|n| xkb::xkb_keymap_mod_get_index(keymap, n.as_ptr()));
            Ok(Keyboard {
                ctx,
                keymap,
                state: xkb::xkb_state_new(keymap),
                keymap_fd,
                keymap_size: bytes.len() as u32,
                mod_index,
                pressed: Vec::new(),
            })
        }
    }

    pub fn keymap_fd(&self) -> io::Result<OwnedFd> {
        self.keymap_fd.try_clone()
    }

    pub fn keymap_size(&self) -> u32 {
        self.keymap_size
    }

    /// Evdev keycodes currently held.
    pub fn pressed(&self) -> &[u32] {
        &self.pressed
    }

    /// Feeds a key event; returns true when the serialised modifiers changed.
    pub fn update(&mut self, keycode: u32, pressed: bool) -> bool {
        if pressed {
            if !self.pressed.contains(&keycode) {
                self.pressed.push(keycode);
            }
        } else {
            self.pressed.retain(|k| *k != keycode);
        }
        let before = self.wire_mods();
        let dir = if pressed { xkb::KEY_DOWN } else { xkb::KEY_UP };
        unsafe { xkb::xkb_state_update_key(self.state, keycode + XKB_OFFSET, dir) };
        before != self.wire_mods()
    }

    /// Forgets every held key, e.g. when the nested window loses focus.
    pub fn reset(&mut self) {
        unsafe {
            xkb::xkb_state_unref(self.state);
            self.state = xkb::xkb_state_new(self.keymap);
        }
        self.pressed.clear();
    }

    /// (depressed, latched, locked, group) as sent in `wl_keyboard.modifiers`.
    pub fn wire_mods(&self) -> (u32, u32, u32, u32) {
        unsafe {
            (
                xkb::xkb_state_serialize_mods(self.state, xkb::STATE_MODS_DEPRESSED),
                xkb::xkb_state_serialize_mods(self.state, xkb::STATE_MODS_LATCHED),
                xkb::xkb_state_serialize_mods(self.state, xkb::STATE_MODS_LOCKED),
                xkb::xkb_state_serialize_layout(self.state, xkb::STATE_LAYOUT_EFFECTIVE),
            )
        }
    }

    /// Active modifiers as `config::mods` bits.
    pub fn bind_mods(&self) -> u32 {
        let effective = unsafe { xkb::xkb_state_serialize_mods(self.state, xkb::STATE_MODS_EFFECTIVE) };
        let mut out = 0;
        for (bit, idx) in self.mod_index.iter().enumerate() {
            if *idx != xkb::MOD_INVALID && effective & (1 << idx) != 0 {
                out |= 1 << bit;
            }
        }
        out
    }

    /// Keysym with the current modifiers applied (Shift gives `E`).
    pub fn sym(&self, keycode: u32) -> u32 {
        unsafe { xkb::xkb_state_key_get_one_sym(self.state, keycode + XKB_OFFSET) }
    }

    /// The character the key types with the current modifiers, if any.
    pub fn text(&self, keycode: u32) -> Option<char> {
        char::from_u32(unsafe { xkb::xkb_state_key_get_utf32(self.state, keycode + XKB_OFFSET) }).filter(|c| *c != '\0')
    }

    /// Keysym of the key at layout 0, level 0 (ignores Shift).
    pub fn base_sym(&self, keycode: u32) -> Option<u32> {
        let mut syms: *const u32 = null();
        let n = unsafe { xkb::xkb_keymap_key_get_syms_by_level(self.keymap, keycode + XKB_OFFSET, 0, 0, &mut syms) };
        (n > 0).then(|| unsafe { *syms })
    }
}

impl Drop for Keyboard {
    fn drop(&mut self) {
        unsafe {
            xkb::xkb_state_unref(self.state);
            xkb::xkb_keymap_unref(self.keymap);
            xkb::xkb_context_unref(self.ctx);
        }
    }
}

/// A read-only memfd holding `data`, safe to share with clients.
fn sealed_memfd(data: &[u8]) -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::memfd_create(c"davide-wm-keymap".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(fd);
        let mut written = 0;
        while written < data.len() {
            let n = libc::write(fd.as_raw_fd(), data[written..].as_ptr().cast(), data.len() - written);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            written += n as usize;
        }
        let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
        libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, seals);
        Ok(fd)
    }
}
