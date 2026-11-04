//! Workspaces, window placement and the window-management actions.

use wm::{Direction, FsMode, WsId};

use super::*;

/// The workspace that receives new windows and keyboard actions.
pub fn focused_ws(s: &State) -> WsId {
    if s.special_shown {
        WsId::Special
    } else {
        s.active
    }
}

/// Tiles a new toplevel on the focused workspace (at its first commit).
pub fn add_window(s: &mut State, key: SKey) {
    let ws_id = focused_ws(s);
    let target = s.focus.filter(|f| s.window_ws.get(f) == Some(&ws_id));
    let fullscreen = s.rule(key).is_some_and(|r| r.fullscreen);
    let ws = s.workspaces.entry(ws_id).or_default();
    ws.tiling.insert(key, target);
    if fullscreen {
        ws.fullscreen = Some((key, FsMode::Fullscreen));
    }
    s.window_ws.insert(key, ws_id);
    s.relayout();
}

pub fn remove_window(s: &mut State, key: SKey) {
    let Some(ws_id) = s.window_ws.remove(&key) else { return };
    if let Some(ws) = s.workspaces.get_mut(&ws_id) {
        ws.remove(key);
    }
    s.urgent.remove(&key);
    if s.focus == Some(key) {
        s.focus = None;
        refocus(s);
    }
    s.relayout();
}

/// Gives the focus to the best window of the focused workspace.
pub fn refocus(s: &mut State) {
    let ws_id = focused_ws(s);
    let next = s.workspaces.get(&ws_id).and_then(|ws| {
        ws.fullscreen.map(|(k, _)| k).or(ws.focus.filter(|f| ws.tiling.contains(*f))).or(ws.tiling.windows().last().copied())
    });
    seat::set_focus(s, next);
}

pub fn switch(s: &mut State, target: WsId) {
    if target == WsId::Special {
        toggle_special(s);
        return;
    }
    s.special_shown = false;
    if s.active != target {
        s.active = target;
        s.workspaces.entry(target).or_default();
    }
    s.relayout();
    refocus(s);
}

pub fn toggle_special(s: &mut State) {
    s.special_shown = !s.special_shown;
    s.relayout();
    refocus(s);
}

/// Moves the focused window to another workspace and follows it.
pub fn move_to(s: &mut State, target: WsId) {
    let Some(key) = s.focus else { return };
    let Some(&from) = s.window_ws.get(&key) else { return };
    if from == target {
        return;
    }
    if let Some(ws) = s.workspaces.get_mut(&from) {
        ws.remove(key);
    }
    let ws = s.workspaces.entry(target).or_default();
    let split = ws.focus;
    ws.tiling.insert(key, split);
    ws.focus = Some(key);
    s.window_ws.insert(key, target);
    if target == WsId::Special {
        s.special_shown = true;
    } else {
        s.special_shown = false;
        s.active = target;
    }
    s.relayout();
    seat::set_focus(s, Some(key));
}

pub fn focus_dir(s: &mut State, dir: Direction) {
    if let Some(next) = s.focus.and_then(|f| wm::neighbor(&visible_tiles(s), f, dir)) {
        seat::set_focus(s, Some(next));
    }
}

/// Swaps the focused window with its neighbour in `dir`.
pub fn move_dir(s: &mut State, dir: Direction) {
    let Some(key) = s.focus else { return };
    let Some(other) = wm::neighbor(&visible_tiles(s), key, dir) else { return };
    let Some(ws_id) = s.window_ws.get(&key).copied() else { return };
    if let Some(ws) = s.workspaces.get_mut(&ws_id) {
        ws.tiling.swap(key, other);
    }
    s.relayout();
}

/// Tiles of the focused workspace only (the special one hides the rest).
fn visible_tiles(s: &State) -> Vec<(SKey, Rect)> {
    if s.special_shown {
        s.layout[s.special_start..].to_vec()
    } else {
        s.layout.clone()
    }
}

pub fn toggle_maximize(s: &mut State) {
    let Some(key) = s.focus else { return };
    let mode = if s.fs_mode(key).is_some() { None } else { Some(FsMode::Maximized) };
    set_fullscreen(s, key, mode);
}

pub fn set_fullscreen(s: &mut State, key: SKey, mode: Option<FsMode>) {
    let Some(ws_id) = s.window_ws.get(&key).copied() else { return };
    let Some(ws) = s.workspaces.get_mut(&ws_id) else { return };
    match mode {
        Some(m) => ws.fullscreen = Some((key, m)),
        None if ws.fullscreen.is_some_and(|(k, _)| k == key) => ws.fullscreen = None,
        None => {}
    }
    s.relayout();
}

pub fn mark_urgent(s: &mut State, key: SKey) {
    if s.focus != Some(key) && s.window_ws.contains_key(&key) {
        s.urgent.insert(key);
        s.needs_redraw = true;
    }
}
