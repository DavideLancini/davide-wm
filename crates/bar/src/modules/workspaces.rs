//! Workspaces as the compositor reports them, and the notification-to-workspace
//! matcher.

use crate::settings;
use crate::{Client, WsEntry};

/// Text shown in the bar: special workspaces lose the `special:` prefix,
/// high-numbered ones become letters (31 -> "A"), named ones keep their name,
/// and the rest show their number.
pub fn label(ws: &WsEntry) -> String {
    if let Some(bare) = ws.name.strip_prefix("special:") {
        if !bare.is_empty() {
            return bare.to_string();
        }
    }
    if let Some(letter) = letter_label(ws.id) {
        return letter;
    }
    if !ws.name.is_empty() {
        return ws.name.clone();
    }
    ws.id.to_string()
}

/// Letter for a high-numbered workspace: `WORKSPACE_LETTER_FROM` maps to 'A',
/// the next id to 'B', and so on. None when the id is below the threshold or
/// would run past 'Z'.
fn letter_label(id: i32) -> Option<String> {
    let offset = id.checked_sub(settings::WORKSPACE_LETTER_FROM)?;
    if !(0..=25).contains(&offset) {
        return None;
    }
    Some(char::from(b'A' + offset as u8).to_string())
}

/// Normalises a name for comparison: lowercase, letters and digits only.
///
/// App names and app ids agree only loosely — "Betterbird" against
/// `eu.betterbird.Betterbird`, "Telegram Desktop" against
/// `io.github.tdesktop_x64.TDesktop` — so punctuation and case are dropped
/// before comparing.
fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The workspace holding the window that belongs to `app`, or None when
/// nothing matches.
///
/// Matching goes from strict to loose and stops at the first level that finds
/// something, so an exact app id match is never diluted by a loose title hit:
/// the app id equals the app name, the app id contains it (reversed-domain
/// ids), or finally the window title mentions it.
///
/// Apps with several windows are the normal case, not the exception — Telegram
/// keeps a detached chat next to its main window, browsers spread over half a
/// dozen workspaces — and the notification says nothing about which one raised
/// it. The tie goes to the **most recently used** window, which is where the
/// conversation you are following lives; the alternative, giving up, would
/// silently drop exactly the apps that notify most.
pub fn workspace_of_app(app: &str, clients: &[Client]) -> Option<i32> {
    let wanted = normalise(app);
    if wanted.is_empty() {
        return None;
    }

    for level in 0..3 {
        let best = clients
            .iter()
            .filter(|c| {
                let app_id = normalise(&c.app_id);
                match level {
                    0 => app_id == wanted,
                    1 => !app_id.is_empty() && (app_id.contains(&wanted) || wanted.contains(&app_id)),
                    _ => normalise(&c.title).contains(&wanted),
                }
            })
            .min_by_key(|c| c.focus_age);
        if let Some(c) = best {
            return Some(c.workspace);
        }
    }
    None
}

#[cfg(test)]
pub fn entry(id: i32, name: &str) -> WsEntry {
    WsEntry { id, name: name.to_string(), active: false, urgent: false }
}

/// Renders the workspace list as one string, a compact way for the tests to
/// check labels; the focused one is marked by **colour**, not by brackets.
#[cfg(test)]
pub fn render(list: &[WsEntry]) -> String {
    list.iter().map(label).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The focused workspace is not marked in the text: colour does that.
    #[test]
    fn labels_and_rendering() {
        let ws = [entry(-1337, "chat"), entry(-98, "special:resize"), entry(31, "31"), entry(32, "32")];
        assert_eq!(render(&ws), "chat resize A B");
    }

    /// High-numbered workspaces become letters, mirroring the Waybar setup;
    /// low-numbered ones keep their number.
    #[test]
    fn high_ids_are_lettered() {
        let ws = [entry(1, "1"), entry(30, "30"), entry(31, "31"), entry(33, "33"), entry(41, "41")];
        assert_eq!(render(&ws), "1 30 A C K");
    }

    /// Named workspaces keep their name even above the threshold, and ids past
    /// 'Z' fall back to the number rather than producing garbage.
    #[test]
    fn letter_mapping_edges() {
        assert_eq!(letter_label(30), None);
        assert_eq!(letter_label(31).as_deref(), Some("A"));
        assert_eq!(letter_label(56).as_deref(), Some("Z"));
        assert_eq!(letter_label(57), None);
        assert_eq!(letter_label(i32::MIN), None);

        assert_eq!(render(&[entry(35, "work")]), "E", "id mapping wins over a plain name");
        assert_eq!(render(&[entry(7, "")]), "7", "no name: the id");
    }

    fn client(app_id: &str, title: &str, workspace: i32) -> Client {
        aged(app_id, title, workspace, 0)
    }

    fn aged(app_id: &str, title: &str, workspace: i32, focus_age: i32) -> Client {
        Client {
            app_id: app_id.to_string(),
            title: title.to_string(),
            workspace,
            focus_age,
        }
    }

    /// App names never match app ids literally, so the comparison has to
    /// survive reversed-domain ids, suffixes and spacing.
    #[test]
    fn app_names_match_their_windows() {
        let clients = vec![
            client("eu.betterbird.Betterbird", "Posta in arrivo", 7),
            client("footclient", "davidelancini@dawn:~", 3),
            client("Thunar", "DVR FORMAT - Thunar", 9),
        ];
        assert_eq!(workspace_of_app("Betterbird", &clients), Some(7));
        assert_eq!(workspace_of_app("Thunar", &clients), Some(9));
        assert_eq!(workspace_of_app("thunar", &clients), Some(9), "case is ignored");
        assert_eq!(workspace_of_app("Signal", &clients), None);
        assert_eq!(workspace_of_app("", &clients), None);
    }

    /// An app spread over several workspaces resolves to its most recently used
    /// window, whatever order the compositor lists them in.
    #[test]
    fn several_windows_resolve_to_the_recent_one() {
        let spread = vec![
            aged("vivaldi-stable", "WhatsApp - Vivaldi", 2, 9),
            aged("vivaldi-stable", "ONECPAS - Vivaldi", 4, 1),
            aged("vivaldi-stable", "Home - Vivaldi", 7, 4),
        ];
        assert_eq!(workspace_of_app("Vivaldi", &spread), Some(4));

        // Telegram, the case that prompted this. It announces itself with its
        // reversed-domain id rather than a display name — captured off the bus,
        // not guessed: "io.github.tdesktop_x64.TDesktop", exactly the app id.
        // A detached chat puts a second window elsewhere, and the more recent
        // one wins.
        let telegram = vec![
            aged("io.github.tdesktop_x64.TDesktop", "64Gram", -1337, 12),
            aged("io.github.tdesktop_x64.TDesktop", "Francesco", 33, 1),
        ];
        let app = "io.github.tdesktop_x64.TDesktop";
        assert_eq!(workspace_of_app(app, &telegram), Some(33));

        // With only the main window left, it resolves to the chat workspace.
        let single = vec![aged(app, "64Gram (7)", -1337, 9)];
        assert_eq!(workspace_of_app(app, &single), Some(-1337));
    }

    /// An exact app id match wins over a window that merely mentions the app in
    /// its title, which would otherwise make most notifications ambiguous.
    #[test]
    fn app_id_matches_beat_title_matches() {
        // The title match is the more recent window, and still must not win.
        let clients = vec![
            aged("Thunar", "home", 9, 5),
            aged("footclient", "man thunar", 3, 0),
        ];
        assert_eq!(workspace_of_app("Thunar", &clients), Some(9));

        // Without a window of its own, the title is the only clue left.
        let only_title = vec![client("footclient", "man thunar", 3)];
        assert_eq!(workspace_of_app("Thunar", &only_title), Some(3));
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert_eq!(render(&[]), "");
        assert_eq!(workspace_of_app("Thunar", &[]), None);
    }
}
