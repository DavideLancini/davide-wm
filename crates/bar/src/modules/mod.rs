//! The sources of text. Each module returns a string of its own; `layout`
//! assembles them into the bar's two halves, deciding order, colour and what
//! a click does.

pub mod backlight;
pub mod battery;
pub mod bluetooth;
pub mod clock;
pub mod cpu;
pub mod cpu_freq;
pub mod helpers;
pub mod layout;
pub mod memory;
pub mod mega;
pub mod network;
pub mod notifications;
pub mod pulseaudio;
pub mod spotify;
pub mod temp;
pub mod timer;
pub mod workspaces;
