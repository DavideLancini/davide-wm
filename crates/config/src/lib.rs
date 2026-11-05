//! All settings of davide-wm. Changing anything here means rebuilding.

pub use wm::{Direction, WsId};

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
    pub const BACKSPACE: u32 = 0xff08;
    pub const APOSTROPHE: u32 = 0x0027;
    pub const BACKSLASH: u32 = 0x005c;
    pub const IGRAVE: u32 = 0x00ec;
    pub const DIGITS: [u32; 10] = [0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x30];
    pub const B: u32 = 0x0062;
    pub const C: u32 = 0x0063;
    pub const E: u32 = 0x0065;
    pub const F: u32 = 0x0066;
    pub const L: u32 = 0x006c;
    pub const M: u32 = 0x006d;
    pub const N: u32 = 0x006e;
    pub const P: u32 = 0x0070;
    pub const Q: u32 = 0x0071;
    pub const R: u32 = 0x0072;
    pub const S: u32 = 0x0073;
    pub const T: u32 = 0x0074;
    pub const W: u32 = 0x0077;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PRINT: u32 = 0xff61;
    pub const F12: u32 = 0xffc9;
    pub const AUDIO_LOWER_VOLUME: u32 = 0x1008ff11;
    pub const AUDIO_MUTE: u32 = 0x1008ff12;
    pub const AUDIO_RAISE_VOLUME: u32 = 0x1008ff13;
    pub const AUDIO_MIC_MUTE: u32 = 0x1008ffb2;
    pub const MON_BRIGHTNESS_UP: u32 = 0x1008ff02;
    pub const MON_BRIGHTNESS_DOWN: u32 = 0x1008ff03;
    /// F1; Fn is this plus n - 1.
    pub const F1: u32 = 0xffbe;
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Spawn(&'static str),
    CloseWindow,
    Focus(Direction),
    MoveWindow(Direction),
    Workspace(WsId),
    MoveToWorkspace(WsId),
    ToggleSpecial,
    ToggleMaximize,
    /// Hides or shows the bar, giving its space back to the windows.
    ToggleBar,
    SwitchVt(u32),
    /// Live upgrade to the binary on disk (README, ADR-014).
    Upgrade,
    /// The clipboard history picker.
    ClipboardHistory,
    /// The notification list.
    Notifications,
    /// Select a region, a window or the output; the PNG goes to the
    /// clipboard and to `SCREENSHOT_DIR`.
    Screenshot,
    /// Output volume change in points.
    Volume(i32),
    ToggleMute,
    ToggleMic,
    /// Brightness change in points, on every panel.
    Brightness(i32),
    Lock,
    /// The Wi-Fi network list.
    Wifi,
    Exit,
}

pub struct Bind {
    pub mods: u32,
    /// Keysym of the key at shift level 0, so `MAIN+SHIFT+E` matches `e`.
    pub key: u32,
    pub action: Action,
}

/// Falls back to a standalone foot when the server is not up yet.
pub const TERMINAL: &str = "footclient || foot";
const BROWSER: &str = "chromium --disable-sync";

/// Every key binding. Built at startup from the tables below.
pub fn binds() -> Vec<Bind> {
    use mods::{ALT, CTRL, MAIN, SHIFT};
    let b = |mods, key, action| Bind { mods, key, action };
    let mut v = vec![
        b(MAIN, keys::T, Action::Spawn(TERMINAL)),
        b(MAIN, keys::B, Action::Spawn(BROWSER)),
        b(MAIN, keys::C, Action::Spawn("code")),
        b(MAIN, keys::F, Action::Spawn("thunar")),
        b(MAIN, keys::W, Action::Wifi),
        b(MAIN, keys::N, Action::Notifications),
        b(MAIN, keys::P, Action::ClipboardHistory),
        b(MAIN, keys::S, Action::Spawn("~/7Scripts/tts --clipboard")),
        b(MAIN, keys::M, Action::ToggleMic),
        b(MAIN, keys::L, Action::Lock),
        b(0, keys::AUDIO_MIC_MUTE, Action::ToggleMic),
        b(0, keys::AUDIO_MUTE, Action::ToggleMute),
        b(0, keys::AUDIO_LOWER_VOLUME, Action::Volume(-VOLUME_STEP)),
        b(0, keys::AUDIO_RAISE_VOLUME, Action::Volume(VOLUME_STEP)),
        b(0, keys::MON_BRIGHTNESS_DOWN, Action::Brightness(-BRIGHTNESS_STEP)),
        b(0, keys::MON_BRIGHTNESS_UP, Action::Brightness(BRIGHTNESS_STEP)),
        b(0, keys::PRINT, Action::Screenshot),
        b(MAIN | CTRL | ALT | SHIFT, keys::F12, Action::ToggleBar),
        b(MAIN | SHIFT, keys::P, Action::Spawn("poweroff")),
        b(MAIN | SHIFT, keys::R, Action::Spawn("reboot")),
        b(MAIN, keys::Q, Action::CloseWindow),
        b(MAIN, keys::R, Action::ToggleSpecial),
        b(MAIN | SHIFT, keys::F, Action::ToggleMaximize),
        b(MAIN | SHIFT, keys::C, Action::Upgrade),
        b(MAIN | SHIFT, keys::E, Action::Exit),
    ];
    for (key, dir) in [(keys::LEFT, Direction::Left), (keys::RIGHT, Direction::Right), (keys::UP, Direction::Up), (keys::DOWN, Direction::Down)] {
        v.push(b(MAIN, key, Action::Focus(dir)));
        v.push(b(MAIN | SHIFT, key, Action::MoveWindow(dir)));
    }
    // Three banks of 14 workspaces: MAIN -> 1-10 + chat/31/32/33,
    // MAIN+CTRL -> 11-20 + 34-37, MAIN+ALT -> 21-30 + 38-41.
    let extra_keys = [keys::BACKSLASH, keys::APOSTROPHE, keys::IGRAVE, keys::BACKSPACE];
    let banks = [
        (0, 0, [WsId::Chat, WsId::Num(31), WsId::Num(32), WsId::Num(33)]),
        (CTRL, 10, [WsId::Num(34), WsId::Num(35), WsId::Num(36), WsId::Num(37)]),
        (ALT, 20, [WsId::Num(38), WsId::Num(39), WsId::Num(40), WsId::Num(41)]),
    ];
    for (prefix, base, extra) in banks {
        let targets = (1..=10).map(|i| WsId::Num(base + i)).zip(keys::DIGITS).chain(extra.into_iter().zip(extra_keys));
        for (ws, key) in targets {
            v.push(b(MAIN | prefix, key, Action::Workspace(ws)));
            v.push(b(MAIN | prefix | SHIFT, key, Action::MoveToWorkspace(ws)));
        }
    }
    for n in 1..=12 {
        v.push(b(CTRL | ALT, keys::F1 + n - 1, Action::SwitchVt(n)));
    }
    // Emergency exit, independent of the main modifier.
    v.push(b(CTRL | ALT, keys::BACKSPACE, Action::Exit));
    v
}

/// Window rules, matched on the exact app id.
pub struct Rule {
    pub app_id: &'static str,
    /// Ignore the client's own fullscreen/maximize requests.
    pub suppress_fullscreen: bool,
    /// Start fullscreen.
    pub fullscreen: bool,
    /// Keep the keyboard focus while mapped.
    pub stay_focused: bool,
}

pub const RULES: &[Rule] = &[
    Rule { app_id: "libreoffice-startcenter", suppress_fullscreen: true, fullscreen: false, stay_focused: false },
    // Black overlay of blk on panels that must not be switched off via DPMS.
    Rule { app_id: "blk-overlay", suppress_fullscreen: false, fullscreen: true, stay_focused: true },
];

/// Started once, in every mode.
pub const AUTOSTART: &[&str] = &["foot --server"];

pub const KEYBOARD_LAYOUT: &str = "it";
pub const REPEAT_RATE: i32 = 25;
pub const REPEAT_DELAY: i32 = 600;

/// Compiled into the binary by `crates/davide-wm/build.rs`; when the file is
/// missing at build time the background stays `BACKGROUND`.
pub const WALLPAPER: &str = "/wallpapers/1.jpg";
/// The wallpaper is halved at startup while it still covers this size.
pub const WALLPAPER_MIN_SIZE: (usize, usize) = (2560, 1600);

/// Background behind the windows when no wallpaper is compiled in, premultiplied RGBA.
pub const BACKGROUND: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
/// Dims the workspace below the special one.
pub const SPECIAL_DIM: [f32; 4] = [0.0, 0.0, 0.0, 0.4];

/// The laptop panel. With the lid closed it is never lit while another
/// output is connected.
pub const INTERNAL_OUTPUT: &str = "eDP-1";
/// Preferred modes by connector name or monitor model (a substring of the
/// EDID name, case-insensitive): (width, height, refresh in Hz), refresh 0 =
/// the highest the monitor offers at that size. Other outputs use their
/// preferred mode.
pub const OUTPUT_MODES: &[(&str, i32, i32, u32)] = &[("eDP-1", 2160, 1440, 60), ("273Q", 2560, 1440, 0)];
/// Monitors that, when connected, are the only lit output: the laptop panel
/// goes dark and every workspace shows there (the MSI MAG 273Q X24).
/// Any other external monitor mirrors the laptop panel.
pub const OUTPUT_EXCLUSIVE: &[&str] = &["273Q"];
/// Monitors blanked with black frames instead of being powered down: the MSI
/// loses the signal on DPMS off and re-handshakes itself back on, and being
/// OLED, black costs the same as off.
pub const OUTPUT_BLANK_BLACK: &[&str] = &["273Q"];
/// How long after a modeset of an exclusive monitor the kernel log is checked
/// for a failed DP link training (the monitor stays black while every API
/// reports it on); on failure the laptop panel comes back.
pub const LINK_CHECK_DELAY_MS: u64 = 3000;

/// Clipboard history: entries kept (newest first) and the largest one stored.
pub const CLIPBOARD_HISTORY: usize = 200;
pub const CLIPBOARD_MAX_BYTES: usize = 16 << 20;

/// Notifications: how long a popup stays when the app does not say, how many
/// popups show at once, and how many notifications the list keeps.
pub const NOTIFICATION_TIMEOUT_MS: u64 = 5000;
pub const NOTIFICATION_POPUPS: usize = 4;
/// The PAM stack the lock screen checks passwords with (Arch's local
/// login stack: pam_faillock, pam_unix).
pub const LOCK_PAM_SERVICE: &str = "system-local-login";

pub const VOLUME_STEP: i32 = 5;
pub const BRIGHTNESS_STEP: i32 = 5;
/// How long the volume/brightness indicator stays.
pub const OSD_TIMEOUT_MS: u64 = 1500;

/// Under `$HOME`; screenshots are also copied to the clipboard.
pub const SCREENSHOT_DIR: &str = "Screenshots";
/// Frame rate cap of screen casts.
pub const CAST_FPS: u32 = 30;

pub const NOTIFICATION_HISTORY: usize = 50;

/// Idle blanking: the timeout set automatically while an exclusive monitor is
/// connected (unless chosen by hand), and the cycle of the bar's click.
pub const SCREENSAVER_AUTO_TIMEOUT: u32 = 60;
pub const SCREENSAVER_CYCLE: &[u32] = &[60, 300, 1800];
/// Input is ignored this long after blanking, so the click or key that asked
/// for it (and its release) does not wake the screen straight away.
pub const BLANK_GRACE_MS: u64 = 700;

/// Programs started once with `--session`, to make the session known
/// to systemd and D-Bus activated services.
pub const SESSION_AUTOSTART: &[&str] = &[
    "dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP DAVIDE_WM_SOCKET XDG_SESSION_TYPE && systemctl --user start davide-wm-session.target",
];

/// DRM render node used by the nested backend.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";
/// Size of the nested window before the host configures it.
pub const NESTED_SIZE: (i32, i32) = (1280, 800);
