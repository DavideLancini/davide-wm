//! libinput (with its udev backend) and the two libudev calls it needs.

use std::ffi::{c_char, c_void};

pub type libinput = c_void;
pub type libinput_event = c_void;
pub type libinput_device = c_void;
pub type udev = c_void;

#[repr(C)]
pub struct libinput_interface {
    pub open_restricted: unsafe extern "C" fn(path: *const c_char, flags: i32, user_data: *mut c_void) -> i32,
    pub close_restricted: unsafe extern "C" fn(fd: i32, user_data: *mut c_void),
}

pub const EVENT_DEVICE_ADDED: u32 = 1;
pub const EVENT_KEYBOARD_KEY: u32 = 300;
pub const EVENT_POINTER_MOTION: u32 = 400;
pub const EVENT_POINTER_BUTTON: u32 = 402;
pub const EVENT_POINTER_SCROLL_WHEEL: u32 = 404;
pub const EVENT_POINTER_SCROLL_FINGER: u32 = 405;
pub const EVENT_POINTER_SCROLL_CONTINUOUS: u32 = 406;
pub const EVENT_SWITCH_TOGGLE: u32 = 900;
pub const EVENT_GESTURE_SWIPE_BEGIN: u32 = 800;
pub const EVENT_GESTURE_SWIPE_UPDATE: u32 = 801;
pub const EVENT_GESTURE_SWIPE_END: u32 = 802;

pub const POINTER_AXIS_SCROLL_VERTICAL: u32 = 0;
pub const POINTER_AXIS_SCROLL_HORIZONTAL: u32 = 1;

pub const KEY_STATE_PRESSED: u32 = 1;
pub const BUTTON_STATE_PRESSED: u32 = 1;

pub const CONFIG_ACCEL_PROFILE_FLAT: u32 = 1;
pub const SWITCH_LID: u32 = 1;
pub const SWITCH_STATE_ON: u32 = 1;

#[link(name = "udev")]
extern "C" {
    pub fn udev_new() -> *mut udev;
    pub fn udev_unref(u: *mut udev) -> *mut udev;
}

#[link(name = "input")]
extern "C" {
    pub fn libinput_udev_create_context(iface: *const libinput_interface, user_data: *mut c_void, udev: *mut udev) -> *mut libinput;
    pub fn libinput_udev_assign_seat(li: *mut libinput, seat: *const c_char) -> i32;
    pub fn libinput_unref(li: *mut libinput) -> *mut libinput;
    pub fn libinput_get_fd(li: *mut libinput) -> i32;
    pub fn libinput_dispatch(li: *mut libinput) -> i32;
    pub fn libinput_get_event(li: *mut libinput) -> *mut libinput_event;
    pub fn libinput_suspend(li: *mut libinput);
    pub fn libinput_resume(li: *mut libinput) -> i32;

    pub fn libinput_event_get_type(ev: *mut libinput_event) -> u32;
    pub fn libinput_event_destroy(ev: *mut libinput_event);
    pub fn libinput_event_get_device(ev: *mut libinput_event) -> *mut libinput_device;

    pub fn libinput_event_get_keyboard_event(ev: *mut libinput_event) -> *mut libinput_event;
    pub fn libinput_event_keyboard_get_key(ev: *mut libinput_event) -> u32;
    pub fn libinput_event_keyboard_get_key_state(ev: *mut libinput_event) -> u32;

    pub fn libinput_event_get_pointer_event(ev: *mut libinput_event) -> *mut libinput_event;
    pub fn libinput_event_pointer_get_dx(ev: *mut libinput_event) -> f64;
    pub fn libinput_event_pointer_get_dy(ev: *mut libinput_event) -> f64;
    pub fn libinput_event_pointer_get_button(ev: *mut libinput_event) -> u32;
    pub fn libinput_event_pointer_get_button_state(ev: *mut libinput_event) -> u32;
    pub fn libinput_event_pointer_has_axis(ev: *mut libinput_event, axis: u32) -> i32;
    pub fn libinput_event_pointer_get_scroll_value(ev: *mut libinput_event, axis: u32) -> f64;

    pub fn libinput_event_get_switch_event(ev: *mut libinput_event) -> *mut libinput_event;
    pub fn libinput_event_get_gesture_event(ev: *mut libinput_event) -> *mut libinput_event;
    pub fn libinput_event_gesture_get_finger_count(ev: *mut libinput_event) -> i32;
    pub fn libinput_event_gesture_get_dx_unaccelerated(ev: *mut libinput_event) -> f64;
    pub fn libinput_event_gesture_get_dy_unaccelerated(ev: *mut libinput_event) -> f64;
    pub fn libinput_event_gesture_get_cancelled(ev: *mut libinput_event) -> i32;
    pub fn libinput_event_switch_get_switch(ev: *mut libinput_event) -> u32;
    pub fn libinput_event_switch_get_switch_state(ev: *mut libinput_event) -> u32;

    pub fn libinput_device_config_accel_is_available(dev: *mut libinput_device) -> i32;
    pub fn libinput_device_config_accel_set_profile(dev: *mut libinput_device, profile: u32) -> u32;
    pub fn libinput_device_config_accel_set_speed(dev: *mut libinput_device, speed: f64) -> u32;
    pub fn libinput_device_config_scroll_has_natural_scroll(dev: *mut libinput_device) -> i32;
    pub fn libinput_device_config_scroll_set_natural_scroll_enabled(dev: *mut libinput_device, enable: i32) -> u32;
    pub fn libinput_device_config_tap_get_finger_count(dev: *mut libinput_device) -> i32;
    pub fn libinput_device_config_tap_set_enabled(dev: *mut libinput_device, enable: i32) -> u32;
    pub fn libinput_device_get_name(dev: *mut libinput_device) -> *const c_char;
}
