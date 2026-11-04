//! Now playing: "artist - title (year), album".
//!
//! Three of the four fields come from MPRIS, which Spotify serves over D-Bus
//! and pushes on every track change — no polling, the same shape as the
//! bluetooth and notification widgets (ADR-015).
//!
//! The **year does not**. MPRIS has a standard field for it,
//! `xesam:contentCreated`, and Spotify leaves it unset — verified against
//! several tracks. So the release date is fetched once per track from the Web
//! API, using the track id MPRIS already carries, and cached: an id maps to a
//! year forever, so a track heard twice costs one lookup.

use crate::dbus::{DBus, Value};
use std::collections::HashMap;

const PLAYER: &str = "org.mpris.MediaPlayer2.spotify";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Credentials, compiled in: the bar reads no config file of its own, and
/// depending on another program's would break when that program is removed.
///
/// These authenticate the *application*, not a user — catalogue data is
/// public, so there is no browser redirect and no refresh token to keep. The
/// secret is still a secret: publishing this source means rotating it.
const CLIENT_ID: &str = "ce6a15ada15e491aaef04d1cd26fbc12";
const CLIENT_SECRET: &str = "9fa59106aeb34911aabbd1d27953a985";

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

/// Longest track line the bar will draw.
///
/// The full line runs past sixty characters — "Jon Lajoie - Everyday Normal
/// Guy 2 (2009), You Want Some of This?" — which crowds out the workspaces.
/// The cut keeps the artist, which is what identifies the track at a glance.
///
/// Raised from 20 once the status widgets gave back 534 px (ADR-041): at 20
/// only the artist survived, which answered one of the four things asked for.
/// Raised again to 70 when the clipboard widget was removed and gave back the
/// room it occupied (ADR-053).
const MAX_LEN: usize = 70;

/// How often the API may be asked while the desktop client is closed.
///
/// Ten seconds is the compromise: a track change shows up within one poll, and
/// six calls a minute sit far under Spotify's limit. There is no push here —
/// the API has no equivalent of the MPRIS signal — so the number is a direct
/// trade between latency and traffic.
const REMOTE_POLL: std::time::Duration = std::time::Duration::from_secs(10);

/// Ceiling on any single HTTP request, connection included.
///
/// **Every one of these calls is synchronous inside the event loop**, which is
/// single-threaded: while a request is out, the bar draws nothing and answers
/// no clicks. `ureq` 2.x defaults to a 30 s connect timeout and **no read
/// timeout at all**, so a server that accepts a connection and then goes quiet
/// freezes the bar indefinitely — with nothing in the journal, because from
/// the process's point of view nothing went wrong. Three seconds is ten times
/// the measured round trip (~80 ms) and still short enough to pass unnoticed.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// An HTTP agent that gives up rather than waiting for ever.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .timeout_connect(HTTP_TIMEOUT)
        .build()
}

/// What is playing, as far as the bar is concerned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    pub artist: String,
    pub title: String,
    pub album: String,
    /// Empty until the lookup answers, so the bar can show the rest at once
    /// rather than waiting on the network.
    pub year: String,
    /// Spotify's id, the key for both the cache and the lookup.
    pub id: String,
}

impl Track {
    /// The bar's line: "year - artist - title - album".
    ///
    /// One separator throughout, and every field but the title is optional: an
    /// absent one takes its separator with it rather than leaving a dangling
    /// dash. The year is missing for local files, and for the moment between a
    /// track change and the lookup that fills it in.
    ///
    /// The year leads because the line is cut from the right when it does not
    /// fit (`MAX_LEN`): whatever is first is what survives.
    pub fn text(&self) -> String {
        if self.title.is_empty() {
            return String::new();
        }
        [&self.year, &self.artist, &self.title, &self.album]
            .iter()
            .filter(|f| !f.is_empty())
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(" - ")
    }
}

/// Reads the metadata Spotify publishes over MPRIS.
pub fn now_playing(bus: &mut DBus) -> Option<Track> {
    let body = bus.get_property_raw(PLAYER, PATH, PLAYER_IFACE, "Metadata")?;
    parse_metadata(&body)
}

/// Pulls the four fields out of the `a{sv}` metadata dictionary.
fn parse_metadata(body: &[u8]) -> Option<Track> {
    let mut r = crate::dbus::Reader::new(body);
    // The reply is a variant wrapping the dictionary.
    let sig = r.signature()?;
    if sig != "a{sv}" {
        return None;
    }
    let entries = r.dict()?;

    let find = |key: &str| -> String {
        entries
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    };

    let title = find("xesam:title");
    if title.is_empty() {
        return None; // nothing playing, or a reply we cannot use
    }
    Some(Track {
        artist: find("xesam:artist"),
        title,
        album: find("xesam:album"),
        year: String::new(),
        // "/com/spotify/track/<id>" — the bare id is what the API wants.
        id: find("mpris:trackid")
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string(),
    })
}

/// Where the refresh token lives, written once by `tools/spotify-authorize.py`.
///
/// The only file the bar reads at runtime. It has to be a file rather than a
/// compiled-in constant because Spotify may rotate the token, and a rotated
/// secret has to be written back somewhere.
fn token_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".local/state/topbar/spotify-token"))
}

/// Asks Spotify what the *account* is playing, on any device.
///
/// This is what MPRIS cannot answer: MPRIS is the local client talking about
/// itself, so it goes quiet when the desktop app is closed. The trade is a
/// network round trip (~80 ms) and a user token, against an answer that
/// follows you to the phone.
fn now_playing_remote(token: &str) -> Option<Track> {
    let response = agent()
        .get("https://api.spotify.com/v1/me/player/currently-playing")
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?;
    // 204: authenticated fine, simply nothing playing anywhere.
    if response.status() == 204 {
        return Some(Track::default());
    }
    parse_currently_playing(&response.into_string().ok()?)
}

/// Pulls a track out of the `currently-playing` payload.
///
/// The fields are nested and their names repeat: `album` carries a `"name"` of
/// its own, which appears *before* the track's. Reading the first `"name"` in
/// the document — which is what a flat scan does — yields the album title as
/// the song title, so each field is taken from its own object.
fn parse_currently_playing(json: &str) -> Option<Track> {
    let item = crate::json::object_field(json, "item")?;
    // Narrow to the album before reading its name, then take the track's own
    // name from what is left once the album object is excluded.
    let album_obj = crate::json::object_field(item, "album")?;
    // The album object nests an `artists` array of its own, whose `"name"`
    // precedes the album's — the same trap one level down.
    let album_flat = strip_nested(album_obj);
    let album = crate::json::string_field(&album_flat, "name").unwrap_or_default();
    let year = crate::json::string_field(&album_flat, "release_date")
        .map(|d| d.chars().take(4).collect())
        .unwrap_or_default();

    let artist = crate::json::array_first(item, "artists")
        .and_then(|a| crate::json::string_field(a, "name"))
        .unwrap_or_default();

    // The track's own fields are what is left once the nested objects are
    // excluded: `album` has a `"name"` and an `"id"`, and so does every entry
    // of `artists`. Cutting the tail after the album is not enough — the
    // artist list sits between the two and would be read instead.
    let item_flat = strip_nested(item);
    let title = crate::json::string_field(&item_flat, "name").unwrap_or_default();
    if title.is_empty() {
        return None;
    }

    Some(Track {
        artist,
        title,
        album,
        year,
        id: crate::json::string_field(&item_flat, "id").unwrap_or_default(),
    })
}

/// Removes every nested object and array from a JSON object's body, leaving
/// only its own scalar fields.
///
/// The track's `"name"`, `"id"` and `"duration_ms"` sit alongside an `album`
/// object and an `artists` array that carry fields of the same names. Reading
/// the outer object means ignoring what is inside the inner ones, and the
/// simplest way to do that is to take them out first.
fn strip_nested(object: &str) -> String {
    let mut out = String::new();
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for c in object.chars() {
        if escaped {
            if depth <= 1 {
                out.push(c);
            }
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => {
                escaped = true;
                if depth <= 1 {
                    out.push(c);
                }
            }
            '"' => {
                in_string = !in_string;
                if depth <= 1 {
                    out.push(c);
                }
            }
            '{' | '[' if !in_string => {
                depth += 1;
                // The outermost brace belongs to the object itself.
                if depth == 1 {
                    out.push(c);
                }
            }
            '}' | ']' if !in_string => {
                if depth == 1 {
                    out.push(c);
                }
                depth = depth.saturating_sub(1);
            }
            _ if depth <= 1 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Renews an access token from the stored refresh token.
fn refresh_access_token(refresh: &str) -> Option<String> {
    let basic = base64(format!("{CLIENT_ID}:{CLIENT_SECRET}").as_bytes());
    let body = agent()
        .post(TOKEN_URL)
        .set("Authorization", &format!("Basic {basic}"))
        .send_form(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])
        .ok()?
        .into_string()
        .ok()?;
    crate::json::string_field(&body, "access_token")
}

/// The years already looked up, so a track heard twice costs one request.
#[derive(Default)]
pub struct Years {
    cache: HashMap<String, String>,
    token: Option<String>,
}

impl Years {
    /// Year for a track id, from the cache or the API.
    ///
    /// Returns None when the lookup fails: the bar then shows the line without
    /// its year rather than blocking or showing a placeholder.
    pub fn get(&mut self, id: &str) -> Option<String> {
        if id.is_empty() {
            return None;
        }
        if let Some(year) = self.cache.get(id) {
            return Some(year.clone());
        }
        // A token lasts an hour; on failure it is dropped and fetched again.
        if self.token.is_none() {
            self.token = fetch_token();
        }
        let token = self.token.clone()?;
        let year = match release_year(&token, id) {
            Some(y) => y,
            None => {
                // Most likely an expired token: drop it so the next call
                // retries with a fresh one.
                self.token = None;
                return None;
            }
        };
        self.cache.insert(id.to_string(), year.clone());
        Some(year)
    }
}

/// Fetches an application token via the Client Credentials flow.
fn fetch_token() -> Option<String> {
    let basic = base64(format!("{CLIENT_ID}:{CLIENT_SECRET}").as_bytes());
    let body = agent()
        .post(TOKEN_URL)
        .set("Authorization", &format!("Basic {basic}"))
        .send_form(&[("grant_type", "client_credentials")])
        .ok()?
        .into_string()
        .ok()?;
    crate::json::string_field(&body, "access_token")
}

/// Release year of a track, from the Web API.
fn release_year(token: &str, track_id: &str) -> Option<String> {
    let url = format!("https://api.spotify.com/v1/tracks/{track_id}");
    let body = agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    parse_year(&body)
}

/// Takes the year out of a track reply's `release_date`.
///
/// The date is the *album's*, which is what makes remasters read correctly: a
/// 2016 remaster of a 1989 record reports 1989, the recording rather than the
/// edition. Precision varies — "1970-09-23", "1970-09" or "1970" — and the
/// first four characters are the year in every case.
fn parse_year(json: &str) -> Option<String> {
    let date = crate::json::string_field(json, "release_date")?;
    let year: String = date.chars().take(4).collect();
    if year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()) {
        Some(year)
    } else {
        None
    }
}

/// Minimal base64, to avoid a crate for sixty bytes of credentials.
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - i * 6)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Base64 must match what HTTP Basic expects, padding included, or the
    /// token request is rejected before it reaches the API.
    #[test]
    fn base64_matches_the_reference() {
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"abc"), "YWJj");
        assert_eq!(base64(b"id:secret"), "aWQ6c2VjcmV0");
    }

    /// The line reads as asked: "artist - title (year), album".
    #[test]
    fn the_line_has_the_shape_asked_for() {
        let t = Track {
            artist: "Santana".into(),
            title: "Samba Pa Ti".into(),
            album: "Abraxas".into(),
            year: "1970".into(),
            id: "x".into(),
        };
        assert_eq!(t.text(), "1970 - Santana - Samba Pa Ti - Abraxas");
    }

    /// Every field but the title is optional, and a missing one takes its
    /// punctuation with it — an empty "()" would look like a bug.
    #[test]
    fn missing_fields_take_their_punctuation() {
        let mut t = Track {
            artist: "Santana".into(),
            title: "Samba Pa Ti".into(),
            album: "Abraxas".into(),
            year: String::new(),
            id: String::new(),
        };
        assert_eq!(t.text(), "Santana - Samba Pa Ti - Abraxas", "year not yet known");

        t.album.clear();
        assert_eq!(t.text(), "Santana - Samba Pa Ti");

        t.artist.clear();
        assert_eq!(t.text(), "Samba Pa Ti");

        // No title means nothing is playing: the widget disappears.
        t.title.clear();
        assert_eq!(t.text(), "");
    }

    /// Release dates come at three precisions and the year is the first four
    /// characters of each.
    #[test]
    fn the_year_survives_every_date_precision() {
        assert_eq!(parse_year(r#"{"release_date":"1970-09-23"}"#).as_deref(), Some("1970"));
        assert_eq!(parse_year(r#"{"release_date":"1970-09"}"#).as_deref(), Some("1970"));
        assert_eq!(parse_year(r#"{"release_date":"1970"}"#).as_deref(), Some("1970"));
        // Nothing usable rather than a wrong guess.
        assert_eq!(parse_year(r#"{"release_date":""}"#), None);
        assert_eq!(parse_year(r#"{"name":"x"}"#), None);
        assert_eq!(parse_year("not json"), None);
    }

    /// The cache answers without a token, which is what makes a repeated
    /// track free — and an empty id must never reach the network.
    #[test]
    fn the_cache_answers_offline() {
        let mut years = Years::default();
        years.cache.insert("abc".into(), "1970".into());
        assert_eq!(years.get("abc").as_deref(), Some("1970"));
        assert!(years.token.is_none(), "a cache hit must not fetch a token");
        assert_eq!(years.get(""), None, "an empty id is not a lookup");
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Reads what is playing right now, if Spotify is up.
    #[test]
    fn reports_what_is_playing() {
        let Some(mut bus) = DBus::session() else {
            return;
        };
        let Some(mut track) = now_playing(&mut bus) else {
            println!("nothing playing");
            return;
        };
        println!("MPRIS: {track:?}");
        assert!(!track.title.is_empty());
        assert!(!track.id.is_empty(), "the track id is needed for the year");

        // And the year, which is the part MPRIS does not carry.
        let mut years = Years::default();
        if let Some(year) = years.get(&track.id) {
            track.year = year;
        }
        println!("line: {}", track.text());
    }
}

/// The widget's state: the track on screen, the years already looked up, and
/// the bus connection the push events arrive on.
#[derive(Default)]
pub struct NowPlaying {
    bus: Option<DBus>,
    years: Years,
    track: Track,
    /// Refresh token, read once at startup. Absent when the authorisation
    /// script has not been run: the widget then works locally only.
    refresh: Option<String>,
    /// Current access token, renewed when the API rejects it.
    access: Option<String>,
    /// When the remote endpoint was last asked, to keep the polling honest.
    last_poll: Option<std::time::Instant>,
}

impl NowPlaying {
    /// Connects and asks for the player's property changes.
    ///
    /// MPRIS pushes a `PropertiesChanged` carrying the whole metadata
    /// dictionary on every track change, so there is nothing to poll.
    pub fn connect(&mut self) {
        if self.bus.is_some() {
            return;
        }
        let Some(mut bus) = DBus::session() else {
            return;
        };
        bus.add_match(&format!(
            "type='signal',interface='org.freedesktop.DBus.Properties',\
             member='PropertiesChanged',path='{PATH}'"
        ));
        self.bus = Some(bus);
        self.refresh = token_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self.reread();
    }

    /// Consumes pending signals; true when the track changed.
    pub fn drain_events(&mut self) -> bool {
        let Some(bus) = self.bus.as_mut() else {
            return false;
        };
        if !bus.drain_signals() {
            return false;
        }
        // The signal says *something* changed — position included, which fires
        // constantly. Re-reading and comparing is what keeps the bar still.
        self.reread()
    }

    /// Re-reads the metadata; true when the line on screen has to change.
    ///
    /// MPRIS first, the API only when it says nothing. The local client is
    /// instant and free, so it is worth preferring whenever it is running;
    /// the network is the fallback that keeps the widget alive when Spotify
    /// is closed, showing what plays on the phone instead.
    fn reread(&mut self) -> bool {
        let local = self.bus.as_mut().and_then(now_playing);
        let fresh = match local {
            Some(track) => {
                let mut track = track;
                // MPRIS never carries the year; the cached lookup fills it.
                if let Some(year) = self.years.get(&track.id) {
                    track.year = year;
                }
                track
            }
            None => self.poll_remote().unwrap_or_default(),
        };

        if fresh.id == self.track.id && fresh.title == self.track.title {
            return false; // same track: nothing to redraw
        }
        self.track = fresh;
        true
    }

    /// Asks the API, at most once per `REMOTE_POLL`.
    ///
    /// Rate limiting is not the reason — Spotify allows far more than this —
    /// the round trip is: every call costs ~80 ms, and the event loop must
    /// not spend that on every tick.
    fn poll_remote(&mut self) -> Option<Track> {
        let refresh = self.refresh.clone()?;
        if let Some(last) = self.last_poll {
            if last.elapsed() < REMOTE_POLL {
                // Too soon: keep showing what was there rather than blanking.
                return Some(self.track.clone());
            }
        }
        self.last_poll = Some(std::time::Instant::now());

        if self.access.is_none() {
            self.access = refresh_access_token(&refresh);
        }
        let token = self.access.clone()?;
        match now_playing_remote(&token) {
            Some(track) => Some(track),
            None => {
                // Most likely an hour has passed and the token expired: drop
                // it so the next poll renews before asking again.
                self.access = None;
                Some(self.track.clone())
            }
        }
    }

    /// The line for the bar, trimmed to fit and empty when nothing is playing.
    pub fn text(&self) -> String {
        super::helpers::shorten(&self.track.text(), MAX_LEN)
    }

    /// Polls the API when the desktop client is not running.
    ///
    /// Driven by a timer rather than by MPRIS: with Spotify closed there is no
    /// signal to wake on, and something has to ask. Returns true when the line
    /// changed. Cheap when MPRIS is alive — it answers first and the poll is
    /// never reached.
    pub fn tick(&mut self) -> bool {
        // Nothing to poll for: no token, or the local client is answering.
        if self.refresh.is_none() {
            return false;
        }
        self.reread()
    }

    /// The interval its timer should use.
    pub const fn poll_interval() -> std::time::Duration {
        REMOTE_POLL
    }

    /// Whether the bus connection died. A closed descriptor stays readable
    /// for ever under level-triggered epoll, so the loop must drop the source
    /// instead of polling it (ADR-049).
    pub fn bus_closed(&self) -> bool {
        self.bus.as_ref().is_some_and(|b| b.is_closed())
    }

    /// Forces the track shown, for tests that need a known line without a
    /// running Spotify.
    #[cfg(test)]
    pub fn set_track_for_test(&mut self, title: &str) {
        self.track = Track {
            title: title.to_string(),
            ..Track::default()
        };
    }

    pub fn raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        self.bus.as_ref().map(|b| b.stream().as_raw_fd())
    }
}

#[cfg(test)]
mod render_check {
    /// The whole left zone must fit the screen and every glyph must be
    /// drawable: a character missing from the subset renders as nothing, which
    /// would silently swallow the track name.
    #[test]
    fn the_track_line_is_drawable() {
        let font = crate::draw::load_font().expect("embedded font");
        let line = "\u{f001} Rings of Saturn - Unhallowed (2017), Ultu Ulla";
        let mut missing = Vec::new();
        for ch in line.chars() {
            if ch.is_control() {
                continue;
            }
            let (m, _) = font.rasterize(ch, crate::settings::FONT_SIZE);
            if m.width == 0 && ch != ' ' {
                missing.push(ch);
            }
        }
        assert!(missing.is_empty(), "glyphs missing from the subset: {missing:?}");
        println!("line width: {} px", crate::draw::text_width(&font, line));
    }
}

#[cfg(test)]
mod live_limit {
    use super::*;

    /// What the bar will actually draw for the current track.
    #[test]
    fn shows_the_trimmed_line() {
        let Some(mut bus) = DBus::session() else { return };
        let Some(track) = now_playing(&mut bus) else {
            println!("nothing playing");
            return;
        };
        let full = track.text();
        let cut = crate::modules::helpers::shorten(&full, MAX_LEN);
        println!("full ({:2}): {full}", full.chars().count());
        println!("on the bar ({:2}): {cut}", cut.chars().count());
        assert!(cut.chars().count() <= MAX_LEN);
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;

    /// A trimmed copy of a real `currently-playing` payload. The shape is what
    /// matters: `album` carries a `"name"` and a `"release_date"`, and the
    /// track's own `"name"` comes *after* it.
    const PAYLOAD: &str = r#"{
      "timestamp": 1786000000000,
      "device": {"id": "abc", "name": "iPhone", "type": "Smartphone"},
      "is_playing": true,
      "item": {
        "album": {
          "album_type": "album",
          "artists": [{"name": "Kelly Clarkson", "type": "artist"}],
          "name": "Breakaway",
          "release_date": "2004-01-17",
          "release_date_precision": "day"
        },
        "artists": [{"name": "Kelly Clarkson", "id": "xyz", "type": "artist"}],
        "duration_ms": 188160,
        "id": "1TfqLAPs4K3s2rJMoCokcS",
        "name": "Since U Been Gone",
        "type": "track"
      }
    }"#;

    /// Every field must come from its own object. Reading the first `"name"`
    /// in the document yields "Breakaway" — the album — as the song title,
    /// which is exactly the bug the nesting invites.
    #[test]
    fn fields_come_from_the_right_object() {
        let track = parse_currently_playing(PAYLOAD).expect("a track");
        assert_eq!(track.title, "Since U Been Gone", "album name leaked in");
        assert_eq!(track.album, "Breakaway");
        assert_eq!(track.artist, "Kelly Clarkson");
        assert_eq!(track.year, "2004");
        assert_eq!(track.id, "1TfqLAPs4K3s2rJMoCokcS");
        assert_eq!(
            track.text(),
            "2004 - Kelly Clarkson - Since U Been Gone - Breakaway"
        );
    }

    /// Nothing usable rather than a half-filled line.
    #[test]
    fn broken_payloads_yield_nothing() {
        assert!(parse_currently_playing("").is_none());
        assert!(parse_currently_playing("{}").is_none(), "no item");
        assert!(parse_currently_playing(r#"{"item":null}"#).is_none());
        assert!(parse_currently_playing("not json").is_none());
    }

    /// A title containing a brace must not end the object early — the span
    /// scanner has to ignore braces inside strings.
    #[test]
    fn braces_in_titles_survive() {
        let tricky = PAYLOAD.replace("Since U Been Gone", "Since {U} Been Gone");
        let track = parse_currently_playing(&tricky).expect("a track");
        assert_eq!(track.title, "Since {U} Been Gone");
        assert_eq!(track.album, "Breakaway", "the album must still be found");
    }
}

#[cfg(test)]
mod live_remote {
    use super::*;

    /// Asks the real account what it is playing, wherever that is. Skipped
    /// when the authorisation script has not been run.
    #[test]
    fn reads_the_account() {
        let Some(refresh) = token_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            println!("no refresh token: run tools/spotify-authorize.py");
            return;
        };
        let token = refresh_access_token(&refresh).expect("the refresh must work");
        match now_playing_remote(&token) {
            Some(t) if t.title.is_empty() => println!("nothing playing on the account"),
            Some(t) => println!("remote: {}", t.text()),
            None => panic!("unexpected API reply"),
        }
    }
}

#[cfg(test)]
mod year_probe {
    use super::*;

    /// Which year the widget shows for known editions.
    #[test]
    #[ignore = "hits the network"]
    fn what_year_do_we_show() {
        let Some(refresh) = token_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else { return };
        let token = refresh_access_token(&refresh).expect("token");
        for (id, note) in [
            ("40riOy7x9W7GXjyGp4pjAv", "Hotel California - 2013 Remaster (orig. 1976)"),
            ("3z8h0TU7ReDPLIbEnYhWZb", "Bohemian Rhapsody (orig. 1975)"),
            ("1FvDJ9KGxcqwv1utyPL3JZ", "Karma Chameleon on a compilation"),
        ] {
            let y = release_year(&token, id);
            println!("  {y:?}  {note}");
        }
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    /// A server that accepts the connection and then says nothing must not
    /// hold the bar: this is the freeze it caused, and `ureq`'s default read
    /// timeout is infinite.
    #[test]
    fn a_silent_server_does_not_hang_us() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::time::Instant;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        // Accept, read the request, then go quiet for longer than the timeout.
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = sock.read(&mut scratch);
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });

        let start = Instant::now();
        let result = agent().get(&format!("http://127.0.0.1:{port}/")).call();
        let elapsed = start.elapsed();

        assert!(result.is_err(), "a silent server should not answer");
        assert!(
            elapsed < HTTP_TIMEOUT * 2,
            "waited {elapsed:?}: the timeout did not fire"
        );
    }

    /// An unroutable address must fail on connect rather than sitting through
    /// the operating system's own, much longer, timeout.
    #[test]
    fn an_unreachable_host_fails_fast() {
        use std::time::Instant;
        let start = Instant::now();
        // TEST-NET-1 (RFC 5737): reserved, never routed.
        let _ = agent().get("http://192.0.2.1/").call();
        assert!(
            start.elapsed() < HTTP_TIMEOUT * 2,
            "waited {:?}",
            start.elapsed()
        );
    }
}
