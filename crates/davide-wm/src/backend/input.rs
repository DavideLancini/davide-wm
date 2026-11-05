//! libinput on top of logind: devices are opened through `TakeDevice`.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::io;
use std::os::fd::{IntoRawFd, RawFd};

use sys::input as li;

use super::logind::{device_numbers, Logind};
use super::Event;

struct Ctx {
    logind: *mut Logind,
    devices: HashMap<RawFd, (u32, u32)>,
}

unsafe extern "C" fn open_restricted(path: *const c_char, _flags: i32, data: *mut c_void) -> i32 {
    let ctx = &mut *(data as *mut Ctx);
    let path = CStr::from_ptr(path).to_string_lossy().into_owned();
    let logind = &mut *ctx.logind;
    match logind.take_device(&path) {
        Ok(fd) => {
            let fd = fd.into_raw_fd();
            let fl = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
            if let Ok(n) = device_numbers(&path) {
                ctx.devices.insert(fd, n);
            }
            fd
        }
        Err(e) => {
            log!("cannot open {path}: {e}");
            -libc::EACCES
        }
    }
}

unsafe extern "C" fn close_restricted(fd: i32, data: *mut c_void) {
    let ctx = &mut *(data as *mut Ctx);
    if let Some((major, minor)) = ctx.devices.remove(&fd) {
        (*ctx.logind).release_device(major, minor);
    }
    libc::close(fd);
}

static INTERFACE: li::libinput_interface = li::libinput_interface { open_restricted, close_restricted };

pub struct Input {
    li: *mut li::libinput,
    udev: *mut li::udev,
    _ctx: Box<Ctx>,
    /// The swipe in progress: fingers and distance so far.
    swipe: (i32, f64, f64),
}

impl Input {
    /// `logind` must outlive the returned value.
    pub fn new(logind: *mut Logind) -> io::Result<Input> {
        let mut ctx = Box::new(Ctx { logind, devices: HashMap::new() });
        unsafe {
            let udev = li::udev_new();
            if udev.is_null() {
                return Err(io::Error::other("udev_new failed"));
            }
            let ptr = &mut *ctx as *mut Ctx as *mut c_void;
            let l = li::libinput_udev_create_context(&INTERFACE, ptr, udev);
            if l.is_null() {
                li::udev_unref(udev);
                return Err(io::Error::other("libinput context failed"));
            }
            if li::libinput_udev_assign_seat(l, c"seat0".as_ptr()) != 0 {
                li::libinput_unref(l);
                li::udev_unref(udev);
                return Err(io::Error::other("libinput cannot assign seat0"));
            }
            Ok(Input { li: l, udev, _ctx: ctx, swipe: (0, 0.0, 0.0) })
        }
    }

    pub fn raw_fd(&self) -> RawFd {
        unsafe { li::libinput_get_fd(self.li) }
    }

    pub fn suspend(&mut self) {
        unsafe { li::libinput_suspend(self.li) };
    }

    pub fn resume(&mut self) {
        unsafe { li::libinput_resume(self.li) };
    }

    pub fn dispatch(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        unsafe {
            li::libinput_dispatch(self.li);
            loop {
                let ev = li::libinput_get_event(self.li);
                if ev.is_null() {
                    break;
                }
                match li::libinput_event_get_type(ev) {
                    li::EVENT_DEVICE_ADDED => configure(li::libinput_event_get_device(ev)),
                    li::EVENT_KEYBOARD_KEY => {
                        let k = li::libinput_event_get_keyboard_event(ev);
                        let pressed = li::libinput_event_keyboard_get_key_state(k) == li::KEY_STATE_PRESSED;
                        out.push(Event::Key(li::libinput_event_keyboard_get_key(k), pressed));
                    }
                    li::EVENT_POINTER_MOTION => {
                        let p = li::libinput_event_get_pointer_event(ev);
                        out.push(Event::PointerMotionRelative(li::libinput_event_pointer_get_dx(p), li::libinput_event_pointer_get_dy(p)));
                    }
                    li::EVENT_POINTER_BUTTON => {
                        let p = li::libinput_event_get_pointer_event(ev);
                        let pressed = li::libinput_event_pointer_get_button_state(p) == li::BUTTON_STATE_PRESSED;
                        out.push(Event::PointerButton(li::libinput_event_pointer_get_button(p), pressed));
                    }
                    kind @ (li::EVENT_POINTER_SCROLL_WHEEL | li::EVENT_POINTER_SCROLL_FINGER | li::EVENT_POINTER_SCROLL_CONTINUOUS) => {
                        let p = li::libinput_event_get_pointer_event(ev);
                        for (axis, wl_axis) in [(li::POINTER_AXIS_SCROLL_VERTICAL, 0), (li::POINTER_AXIS_SCROLL_HORIZONTAL, 1)] {
                            if li::libinput_event_pointer_has_axis(p, axis) != 0 {
                                let mut v = li::libinput_event_pointer_get_scroll_value(p, axis);
                                if kind == li::EVENT_POINTER_SCROLL_WHEEL {
                                    // 120 units per detent; clients expect about 15 per detent.
                                    v = v / 120.0 * 15.0;
                                }
                                out.push(Event::PointerAxis(wl_axis, v));
                            }
                        }
                    }
                    li::EVENT_GESTURE_SWIPE_BEGIN => {
                        let g = li::libinput_event_get_gesture_event(ev);
                        self.swipe = (li::libinput_event_gesture_get_finger_count(g), 0.0, 0.0);
                    }
                    li::EVENT_GESTURE_SWIPE_UPDATE => {
                        let g = li::libinput_event_get_gesture_event(ev);
                        self.swipe.1 += li::libinput_event_gesture_get_dx_unaccelerated(g);
                        self.swipe.2 += li::libinput_event_gesture_get_dy_unaccelerated(g);
                    }
                    li::EVENT_GESTURE_SWIPE_END => {
                        let g = li::libinput_event_get_gesture_event(ev);
                        let (fingers, dx, dy) = std::mem::take(&mut self.swipe);
                        // A deliberate horizontal three-finger swipe.
                        if li::libinput_event_gesture_get_cancelled(g) == 0 && fingers == 3 && dx.abs() > 2.0 * dy.abs() && dx.abs() > config::SWIPE_DISTANCE {
                            out.push(Event::Swipe(if dx < 0.0 { 1 } else { -1 }));
                        }
                    }
                    li::EVENT_SWITCH_TOGGLE => {
                        let sw = li::libinput_event_get_switch_event(ev);
                        if li::libinput_event_switch_get_switch(sw) == li::SWITCH_LID {
                            out.push(Event::Lid(li::libinput_event_switch_get_switch_state(sw) == li::SWITCH_STATE_ON));
                        }
                    }
                    _ => {}
                }
                li::libinput_event_destroy(ev);
            }
        }
        out
    }
}

/// Flat acceleration, natural scrolling and tap-to-click, like the Hyprland setup.
unsafe fn configure(dev: *mut li::libinput_device) {
    let name = CStr::from_ptr(li::libinput_device_get_name(dev)).to_string_lossy();
    log!("input device: {name}");
    if li::libinput_device_config_accel_is_available(dev) != 0 {
        li::libinput_device_config_accel_set_profile(dev, li::CONFIG_ACCEL_PROFILE_FLAT);
        li::libinput_device_config_accel_set_speed(dev, 0.0);
    }
    if li::libinput_device_config_tap_get_finger_count(dev) > 0 {
        li::libinput_device_config_tap_set_enabled(dev, 1);
        if li::libinput_device_config_scroll_has_natural_scroll(dev) != 0 {
            li::libinput_device_config_scroll_set_natural_scroll_enabled(dev, 1);
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        unsafe {
            li::libinput_unref(self.li);
            li::udev_unref(self.udev);
        }
    }
}
