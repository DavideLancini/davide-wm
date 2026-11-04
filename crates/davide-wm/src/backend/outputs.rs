//! Which outputs to light and how: the monitor policy of `monitor-manager`
//! (README, M4), as pure functions over the connected monitors.

use sys::drm::{self, ModeInfo};

/// A connected connector with at least one mode.
pub struct Monitor {
    pub connector: u32,
    pub name: String,
    /// Manufacturer and model from the EDID, e.g. "MSI MAG 273Q X24".
    pub model: String,
    pub modes: Vec<ModeInfo>,
}

/// Indices into the monitor list. The primary output holds the workspaces;
/// mirrors show a scaled copy of it.
#[derive(Debug, PartialEq)]
pub struct Layout {
    pub primary: usize,
    pub mirrors: Vec<usize>,
}

/// True when `pattern` names this connector or is part of its model.
pub fn matches(pattern: &str, name: &str, model: &str) -> bool {
    name == pattern || model.to_ascii_uppercase().contains(&pattern.to_ascii_uppercase())
}

fn any_match(patterns: &[&str], m: &Monitor) -> bool {
    patterns.iter().any(|p| matches(p, &m.name, &m.model))
}

pub fn is_exclusive(name: &str, model: &str) -> bool {
    config::OUTPUT_EXCLUSIVE.iter().any(|p| matches(p, name, model))
}

pub fn blanks_black(name: &str, model: &str) -> bool {
    config::OUTPUT_BLANK_BLACK.iter().any(|p| matches(p, name, model))
}

/// The policy: an exclusive monitor alone; otherwise the laptop panel with
/// every other monitor mirroring it. With the lid closed the panel only stays
/// when nothing else is connected (then it is lit or not by the lid switch
/// handling, not here). `failed` lists connectors whose link did not come up.
pub fn layout(monitors: &[Monitor], lid_closed: bool, failed: &[String]) -> Option<Layout> {
    let usable: Vec<usize> = (0..monitors.len()).filter(|i| !failed.contains(&monitors[*i].name)).collect();
    let internal = usable.iter().copied().find(|i| monitors[*i].name == config::INTERNAL_OUTPUT);
    let externals: Vec<usize> = usable.iter().copied().filter(|i| Some(*i) != internal).collect();
    if let Some(e) = externals.iter().copied().find(|i| any_match(config::OUTPUT_EXCLUSIVE, &monitors[*i])) {
        return Some(Layout { primary: e, mirrors: Vec::new() });
    }
    match internal {
        Some(i) if !lid_closed || externals.is_empty() => Some(Layout { primary: i, mirrors: externals }),
        _ => externals.split_first().map(|(p, rest)| Layout { primary: *p, mirrors: rest.to_vec() }),
    }
}

/// The configured mode for this monitor, else its preferred one.
pub fn pick_mode(m: &Monitor) -> Option<ModeInfo> {
    let wanted = config::OUTPUT_MODES.iter().find(|(p, ..)| matches(p, &m.name, &m.model));
    let sized = |w: i32, h: i32| m.modes.iter().copied().filter(move |x| x.hdisplay as i32 == w && x.vdisplay as i32 == h);
    wanted
        .and_then(|(_, w, h, hz)| match hz {
            0 => sized(*w, *h).max_by_key(|x| x.refresh_mhz()),
            hz => sized(*w, *h).min_by_key(|x| x.refresh_mhz().abs_diff(hz * 1000)),
        })
        .or(m.modes.iter().copied().find(|x| x.kind & drm::MODE_TYPE_PREFERRED != 0))
        .or(m.modes.first().copied())
}

/// "MFG model" from an EDID: the PNP manufacturer id and the monitor name
/// descriptor. Empty when the EDID is missing or malformed.
pub fn edid_model(edid: &[u8]) -> String {
    if edid.len() < 128 || edid[..8] != [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0] {
        return String::new();
    }
    let id = u16::from_be_bytes([edid[8], edid[9]]);
    let mfg: String = [10, 5, 0].iter().map(|s| (b'A' - 1 + ((id >> s) & 0x1f) as u8) as char).collect();
    let name = (0..4)
        .map(|i| &edid[54 + i * 18..72 + i * 18])
        .find(|d| d[..3] == [0, 0, 0] && d[3] == 0xfc)
        .map(|d| d[5..].iter().take_while(|b| **b != b'\n').map(|b| *b as char).collect::<String>())
        .unwrap_or_default();
    let name = name.trim();
    match name {
        "" => format!("{mfg} 0x{:04x}", u16::from_le_bytes([edid[10], edid[11]])),
        _ => format!("{mfg} {name}"),
    }
}

/// The EDID the kernel exposes for a connector of `card` ("/dev/dri/card1").
pub fn read_edid(card: &str, connector: &str) -> Vec<u8> {
    let card = card.rsplit('/').next().unwrap_or(card);
    std::fs::read(format!("/sys/class/drm/{card}-{connector}/edid")).unwrap_or_default()
}

/// True if the kernel logged a failed DP link training since `since` (epoch
/// seconds). amdgpu then still reports the output connected and lit at its
/// mode while the monitor stays black; only its log tells (`enabling link N
/// failed`, DC_FAIL_DP_LINK_TRAINING). Read through journalctl: /dev/kmsg is
/// restricted.
pub fn link_failed_since(since: u64) -> bool {
    let out = std::process::Command::new("journalctl")
        .args(["-k", "--since", &format!("@{since}"), "-g", "enabling link [0-9]+ failed", "-q", "--no-pager", "-o", "cat"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    out.is_ok_and(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
}

/// The lid state from ACPI, for startup: libinput only reports changes.
pub fn lid_closed_now() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc/acpi/button/lid") else { return false };
    dir.flatten().any(|e| std::fs::read_to_string(e.path().join("state")).is_ok_and(|s| s.contains("closed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(w: u16, h: u16, hz: u32, preferred: bool) -> ModeInfo {
        ModeInfo { hdisplay: w, vdisplay: h, vrefresh: hz, kind: if preferred { drm::MODE_TYPE_PREFERRED } else { 0 }, ..Default::default() }
    }

    fn mon(name: &str, model: &str) -> Monitor {
        Monitor { connector: 0, name: name.into(), model: model.into(), modes: vec![mode(1920, 1080, 60, true)] }
    }

    #[test]
    fn exclusive_monitor_alone() {
        let ms = [mon("eDP-1", "BOE 0x0893"), mon("DP-1", "MSI MAG 273Q X24")];
        assert_eq!(layout(&ms, false, &[]), Some(Layout { primary: 1, mirrors: vec![] }));
        assert_eq!(layout(&ms, true, &[]), Some(Layout { primary: 1, mirrors: vec![] }));
    }

    #[test]
    fn failed_exclusive_falls_back_to_the_panel() {
        let ms = [mon("eDP-1", ""), mon("DP-1", "MSI MAG 273Q X24")];
        assert_eq!(layout(&ms, false, &["DP-1".into()]), Some(Layout { primary: 0, mirrors: vec![] }));
    }

    #[test]
    fn other_monitors_mirror_the_panel() {
        let ms = [mon("HDMI-A-1", "HEC HISENSE"), mon("eDP-1", "")];
        assert_eq!(layout(&ms, false, &[]), Some(Layout { primary: 1, mirrors: vec![0] }));
    }

    #[test]
    fn closed_lid_hands_over_to_an_external() {
        let ms = [mon("eDP-1", ""), mon("HDMI-A-1", "HEC HISENSE")];
        assert_eq!(layout(&ms, true, &[]), Some(Layout { primary: 1, mirrors: vec![] }));
        assert_eq!(layout(&ms[..1], true, &[]), Some(Layout { primary: 0, mirrors: vec![] }));
        assert_eq!(layout(&[], false, &[]), None);
    }

    #[test]
    fn highest_refresh_when_unspecified() {
        let mut m = mon("DP-1", "MSI MAG 273Q X24");
        m.modes = vec![mode(2560, 1440, 60, true), mode(2560, 1440, 144, false), mode(1920, 1080, 240, false)];
        assert_eq!(pick_mode(&m).map(|x| x.refresh_mhz()), Some(144_000));
        let mut p = mon("eDP-1", "");
        p.modes = vec![mode(2160, 1440, 48, false), mode(2160, 1440, 60, false), mode(1080, 720, 60, true)];
        assert_eq!(pick_mode(&p).map(|x| x.refresh_mhz()), Some(60_000));
        let o = mon("HDMI-A-1", "");
        assert_eq!(pick_mode(&o).map(|x| x.hdisplay), Some(1920));
    }

    #[test]
    fn edid_name_and_manufacturer() {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        // "MSI": M=13, S=19, I=9.
        let id: u16 = (13 << 10) | (19 << 5) | 9;
        e[8..10].copy_from_slice(&id.to_be_bytes());
        e[72..77].copy_from_slice(&[0, 0, 0, 0xfc, 0]);
        e[77..90].copy_from_slice(b"MAG 273Q X24\n");
        assert_eq!(edid_model(&e), "MSI MAG 273Q X24");
        assert!(is_exclusive("DP-1", &edid_model(&e)));
        assert_eq!(edid_model(&[]), "");
    }
}
