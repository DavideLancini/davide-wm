use std::ffi::{c_char, c_void};

pub type xkb_context = c_void;
pub type xkb_keymap = c_void;
pub type xkb_state = c_void;
pub type xkb_keycode_t = u32;
pub type xkb_keysym_t = u32;
pub type xkb_mod_mask_t = u32;
pub type xkb_mod_index_t = u32;
pub type xkb_layout_index_t = u32;

#[repr(C)]
pub struct xkb_rule_names {
    pub rules: *const c_char,
    pub model: *const c_char,
    pub layout: *const c_char,
    pub variant: *const c_char,
    pub options: *const c_char,
}

pub const KEYMAP_FORMAT_TEXT_V1: i32 = 1;
pub const KEY_UP: i32 = 0;
pub const KEY_DOWN: i32 = 1;
pub const STATE_MODS_DEPRESSED: i32 = 1 << 0;
pub const STATE_MODS_LATCHED: i32 = 1 << 1;
pub const STATE_MODS_LOCKED: i32 = 1 << 2;
pub const STATE_MODS_EFFECTIVE: i32 = 1 << 3;
pub const STATE_LAYOUT_EFFECTIVE: i32 = 1 << 7;
pub const MOD_INVALID: xkb_mod_index_t = 0xffff_ffff;

#[link(name = "xkbcommon")]
extern "C" {
    pub fn xkb_context_new(flags: i32) -> *mut xkb_context;
    pub fn xkb_context_unref(ctx: *mut xkb_context);
    pub fn xkb_keymap_new_from_names(ctx: *mut xkb_context, names: *const xkb_rule_names, flags: i32) -> *mut xkb_keymap;
    pub fn xkb_keymap_unref(km: *mut xkb_keymap);
    pub fn xkb_keymap_get_as_string(km: *mut xkb_keymap, format: i32) -> *mut c_char;
    pub fn xkb_keymap_mod_get_index(km: *mut xkb_keymap, name: *const c_char) -> xkb_mod_index_t;
    pub fn xkb_keymap_key_get_syms_by_level(
        km: *mut xkb_keymap,
        key: xkb_keycode_t,
        layout: xkb_layout_index_t,
        level: u32,
        syms_out: *mut *const xkb_keysym_t,
    ) -> i32;
    pub fn xkb_state_new(km: *mut xkb_keymap) -> *mut xkb_state;
    pub fn xkb_state_unref(st: *mut xkb_state);
    pub fn xkb_state_update_key(st: *mut xkb_state, key: xkb_keycode_t, direction: i32) -> i32;
    pub fn xkb_state_serialize_mods(st: *mut xkb_state, components: i32) -> xkb_mod_mask_t;
    pub fn xkb_state_serialize_layout(st: *mut xkb_state, components: i32) -> xkb_layout_index_t;
    pub fn xkb_state_key_get_one_sym(st: *mut xkb_state, key: xkb_keycode_t) -> xkb_keysym_t;
    pub fn xkb_state_key_get_utf32(st: *mut xkb_state, key: xkb_keycode_t) -> u32;
}
