// The SoundCloud pill (Linux): SoundCloud has no desktop app, it plays in a
// browser tab. Browsers publish that tab on the session bus as an MPRIS player
// under a name of their own (org.mpris.MediaPlayer2.plasma-browser-integration,
// …chromium.instanceN, …), so the player is found by what it plays: the one
// whose xesam:url is on soundcloud.com. Chrome's own MPRIS gives no URL;
// Plasma Browser Integration (KDE) does, and is what this relies on.
//
// The state, the metadata parsing, the D-Bus values and the cover checks are
// spotify.rs's. Nothing runs on a timer: one thread blocks on the bus while
// the pill is declared and reads the players again on every MPRIS signal.

// Only reached from the Linux client and the tests.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use tauri::AppHandle;

use crate::spotify::PlayerState;

pub const PILL_ID: &str = "integration_soundcloud";
/// "Open SoundCloud" when no tab plays it.
#[cfg(target_os = "linux")]
const HOME_URL: &str = "https://soundcloud.com/discover";
const STATE_EVENT: &str = "soundcloud";
const ARTWORK_EVENT: &str = "soundcloud-artwork";

/// Whether an xesam:url is a SoundCloud page.
pub fn is_soundcloud(url: &str) -> bool {
    reqwest::Url::parse(url).ok().is_some_and(|u| {
        let host = u.host_str().unwrap_or("").to_ascii_lowercase();
        u.scheme() == "https" && (host == "soundcloud.com" || host.ends_with(".soundcloud.com"))
    })
}

#[tauri::command]
pub async fn soundcloud_refresh(app: AppHandle) -> Option<PlayerState> {
    #[cfg(target_os = "linux")]
    {
        tauri::async_runtime::spawn_blocking(move || linux::refresh(&app)).await.ok().flatten()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        None
    }
}

/// Same actions as spotify_control: `playPause`, `next`, `previous`, `seek`
/// (seconds), `shuffle` / `repeat` (1 on, 0 off), `volume` (0…100).
#[tauri::command]
pub async fn soundcloud_control(app: AppHandle, action: String, value: Option<f64>) -> bool {
    #[cfg(target_os = "linux")]
    {
        tauri::async_runtime::spawn_blocking(move || linux::control(&app, &action, value)).await.unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (app, action, value);
        false
    }
}

/// Brings the playing tab's browser forward, or opens SoundCloud.
#[tauri::command]
pub async fn soundcloud_open() -> bool {
    #[cfg(target_os = "linux")]
    {
        tauri::async_runtime::spawn_blocking(linux::open).await.unwrap_or(false)
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Starts the listener when the pill is declared, stops it when it is not.
pub fn sync(app: &AppHandle, active_integrations: &[String]) {
    let on = active_integrations.iter().any(|id| id == PILL_ID);
    #[cfg(target_os = "linux")]
    linux::set_active(app, on);
    #[cfg(not(target_os = "linux"))]
    let _ = (app, on);
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::sync::{LazyLock, Mutex};
    use std::time::Duration;

    use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
    use dbus::blocking::Connection;
    use dbus::message::MessageType;
    use dbus::Message;
    use tauri::Emitter;

    use crate::spotify::linux::props_of;
    use crate::spotify::{artwork_source, data_url, now_ms, ArtSource, Value, ARTWORK_LIMIT};

    const PREFIX: &str = "org.mpris.MediaPlayer2.";
    const PATH: &str = "/org/mpris/MediaPlayer2";
    const ROOT: &str = "org.mpris.MediaPlayer2";
    const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
    const DBUS: &str = "org.freedesktop.DBus";
    const CALL: Duration = Duration::from_millis(1500);
    const IDLE_WAIT: Duration = Duration::from_secs(3600);

    const RULES: [&str; 3] = [
        "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',member='NameOwnerChanged',arg0namespace='org.mpris.MediaPlayer2'",
        "type='signal',path='/org/mpris/MediaPlayer2',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',arg0='org.mpris.MediaPlayer2.Player'",
        "type='signal',path='/org/mpris/MediaPlayer2',interface='org.mpris.MediaPlayer2.Player',member='Seeked'",
    ];

    struct Shared {
        generation: u64,
        active: bool,
        listener: Option<String>,
        /// The bus name of the player playing SoundCloud, for the controls.
        player: Option<String>,
        state: PlayerState,
        /// The cover last sent, so it is read once per track.
        art: Option<String>,
    }

    static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| {
        Mutex::new(Shared { generation: 0, active: false, listener: None, player: None, state: idle(), art: None })
    });

    /// A website is always there to open.
    fn idle() -> PlayerState {
        PlayerState { installed: true, ..PlayerState::default() }
    }

    pub fn set_active(app: &AppHandle, on: bool) {
        let wake = {
            let mut s = SHARED.lock().unwrap();
            if s.active == on {
                return;
            }
            s.active = on;
            s.generation += 1;
            s.player = None;
            s.art = None;
            s.state = idle();
            s.listener.take()
        };
        if on {
            let generation = SHARED.lock().unwrap().generation;
            let app = app.clone();
            if let Err(err) = std::thread::Builder::new().name("coucou-soundcloud".into()).spawn(move || listen(app, generation)) {
                crate::log::line(format!("soundcloud: no listener thread: {err}"));
            }
        } else {
            if let Some(name) = wake {
                // Any message to its unique name gets the listener off the bus.
                if let (Ok(conn), Ok(mut msg)) =
                    (Connection::new_session(), Message::new_method_call(name, "/", "org.freedesktop.DBus.Peer", "Ping"))
                {
                    msg.set_no_reply(true);
                    let _ = conn.channel().send(msg);
                    conn.channel().flush();
                }
            }
            let _ = app.emit_to(crate::island::WINDOW_LABEL, STATE_EVENT, idle());
        }
    }

    fn current(generation: u64) -> bool {
        let s = SHARED.lock().unwrap();
        s.active && s.generation == generation
    }

    fn listen(app: AppHandle, generation: u64) {
        let conn = match Connection::new_session() {
            Ok(c) => c,
            Err(err) => {
                crate::log::line(format!("soundcloud: no session bus: {err}"));
                return;
            }
        };
        {
            let mut s = SHARED.lock().unwrap();
            if !(s.active && s.generation == generation) {
                return;
            }
            s.listener = Some(conn.unique_name().to_string());
        }
        for rule in RULES {
            if let Err(err) = conn.add_match_no_cb(rule) {
                crate::log::line(format!("soundcloud: match refused: {err}"));
            }
        }
        update(&conn, &app, generation, false);
        while current(generation) {
            match conn.channel().blocking_pop_message(IDLE_WAIT) {
                Ok(Some(msg)) if msg.msg_type() == MessageType::Signal => {
                    update(&conn, &app, generation, false);
                }
                Ok(_) => {}
                Err(err) => {
                    crate::log::line(format!("soundcloud: bus lost: {err}"));
                    break;
                }
            }
        }
    }

    /// Every MPRIS player on the bus playing a soundcloud.com page, as
    /// (bus name, Player properties); the one playing first.
    fn find(conn: &Connection) -> Option<(String, Vec<(String, Value)>)> {
        let proxy = conn.with_proxy(DBUS, "/org/freedesktop/DBus", CALL);
        let (names,): (Vec<String>,) = proxy.method_call(DBUS, "ListNames", ()).ok()?;
        let mut found: Vec<(String, Vec<(String, Value)>)> = names
            .into_iter()
            .filter(|n| n.starts_with(PREFIX))
            .filter_map(|n| {
                let props = props_of(&conn.with_proxy(n.as_str(), PATH, CALL).get_all(PLAYER).ok()?);
                plays_soundcloud(&props).then_some((n, props))
            })
            .collect();
        found.sort_by_key(|(_, props)| !matches!(prop(props, "PlaybackStatus"), Some(Value::Str(s)) if s == "Playing"));
        found.into_iter().next()
    }

    fn prop<'a>(props: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
        props.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The SoundCloud tab on this session bus, if any (the live test).
    #[cfg(test)]
    pub(super) fn find_now() -> Option<(String, PlayerState)> {
        let conn = Connection::new_session().ok()?;
        find(&conn).map(|(name, props)| (name, state_of(&props, now_ms())))
    }

    pub(super) fn plays_soundcloud(props: &[(String, Value)]) -> bool {
        let Some(Value::Map(meta)) = prop(props, "Metadata") else { return false };
        matches!(prop(meta, "xesam:url"), Some(Value::Str(url)) if is_soundcloud(url))
    }

    /// The pill's state from a player's properties. SoundCloud titles are
    /// often "Artist - Title": kept whole, not shortened as Spotify's are.
    pub(super) fn state_of(props: &[(String, Value)], now: f64) -> PlayerState {
        let mut state = PlayerState { running: true, ..idle() };
        state.apply(props, now);
        if let (Some(track), Some(Value::Map(meta))) = (state.track.as_mut(), prop(props, "Metadata")) {
            if let Some(Value::Str(title)) = prop(meta, "xesam:title") {
                track.title = title.trim().to_string();
            }
        }
        state
    }

    /// Reads the players again; tells the island when something it shows moved.
    fn update(conn: &Connection, app: &AppHandle, generation: u64, force: bool) -> Option<PlayerState> {
        let found = find(conn);
        let now = now_ms();
        let next = match &found {
            Some((_, props)) => state_of(props, now),
            None => idle(),
        };
        let (changed, art) = {
            let mut s = SHARED.lock().unwrap();
            if !(s.active && s.generation == generation) {
                return None;
            }
            let old = &s.state;
            let drift = (old.position_at_time(now) - next.position_at_time(now)).abs();
            let changed = force
                || old.track != next.track
                || old.playing != next.playing
                || old.running != next.running
                || old.volume != next.volume
                || old.shuffle != next.shuffle
                || old.repeat != next.repeat
                || drift > 1.5;
            s.player = found.map(|(name, _)| name);
            s.state = next.clone();
            let art = next.track.as_ref().and_then(|t| t.art_url.clone()).filter(|u| s.art.as_ref() != Some(u));
            if art.is_some() {
                s.art = art.clone();
            }
            (changed, art)
        };
        if changed {
            let _ = app.emit_to(crate::island::WINDOW_LABEL, STATE_EVENT, &next);
        }
        if let Some(url) = art {
            send_artwork(app, url);
        }
        Some(next)
    }

    /// Plasma Browser Integration saves the tab's cover to a local file; only
    /// that is read (a remote cover would need SoundCloud's CDN allowed).
    // ponytail: local covers only; add i1.sndcdn.com to artwork_source if a browser gives https ones.
    fn send_artwork(app: &AppHandle, url: String) {
        let Some(ArtSource::File(path)) = artwork_source(&url) else { return };
        let data = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file() && m.len() as usize <= ARTWORK_LIMIT)
            .and_then(|_| std::fs::read(&path).ok())
            .and_then(|bytes| data_url(&bytes));
        if let Some(data_url) = data {
            let payload = serde_json::json!({ "artUrl": url, "dataUrl": data_url });
            let _ = app.emit_to(crate::island::WINDOW_LABEL, ARTWORK_EVENT, payload);
        }
    }

    pub fn refresh(app: &AppHandle) -> Option<PlayerState> {
        let generation = {
            let mut s = SHARED.lock().unwrap();
            if !s.active {
                return None;
            }
            s.art = None;
            s.generation
        };
        let conn = Connection::new_session().ok()?;
        update(&conn, app, generation, true)
    }

    pub fn control(app: &AppHandle, action: &str, value: Option<f64>) -> bool {
        let (player, state, generation) = {
            let s = SHARED.lock().unwrap();
            if !s.active {
                return false;
            }
            (s.player.clone(), s.state.clone(), s.generation)
        };
        let (Some(player), Ok(conn)) = (player, Connection::new_session()) else { return false };
        let p = conn.with_proxy(player.as_str(), PATH, CALL);
        let v = value.unwrap_or(0.0);
        let done = match action {
            "playPause" => p.method_call(PLAYER, "PlayPause", ()),
            "next" => p.method_call(PLAYER, "Next", ()),
            "previous" => p.method_call(PLAYER, "Previous", ()),
            "seek" => match state.track.as_ref().map(|t| (dbus::Path::new(t.object_path.clone()), t.duration)) {
                Some((Ok(path), duration)) => {
                    let target = v.clamp(0.0, (duration - 1.0).max(0.0));
                    p.method_call(PLAYER, "SetPosition", (path, (target * 1_000_000.0) as i64))
                }
                _ => return false,
            },
            "shuffle" => p.set(PLAYER, "Shuffle", v != 0.0),
            "repeat" => p.set(PLAYER, "LoopStatus", if v != 0.0 { "Playlist" } else { "None" }),
            "volume" => p.set(PLAYER, "Volume", (v / 100.0).clamp(0.0, 1.0)),
            _ => return false,
        }
        .is_ok();
        // Browsers don't always signal a seek or a volume: read them back.
        if done {
            update(&conn, app, generation, false);
        }
        done
    }

    pub fn open() -> bool {
        let player = SHARED.lock().unwrap().player.clone();
        let raised = player.zip(Connection::new_session().ok()).is_some_and(|(name, conn)| {
            let p = conn.with_proxy(name.as_str(), PATH, CALL);
            p.get::<bool>(ROOT, "CanRaise").unwrap_or(false) && p.method_call::<(), _, _, _>(ROOT, "Raise", ()).is_ok()
        });
        if !raised {
            crate::platform::open_url(HOME_URL);
        }
        true
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::linux::{plays_soundcloud, state_of};
    use super::*;
    use crate::spotify::Value;

    fn props(url: &str) -> Vec<(String, Value)> {
        // What Plasma Browser Integration publishes for a SoundCloud tab.
        vec![
            (
                "Metadata".into(),
                Value::Map(vec![
                    ("mpris:trackid".into(), Value::Str("/org/kde/plasma/browser_integration/1337".into())),
                    ("mpris:length".into(), Value::Int(163_863_211)),
                    ("mpris:artUrl".into(), Value::Str("file:///tmp/plasma-browser-integration_artwork_X.jpg".into())),
                    ("xesam:title".into(), Value::Str("Don Miguelo - Y Que Fue? [Audio Oficial]".into())),
                    ("xesam:artist".into(), Value::Strs(vec!["Someone".into()])),
                    ("xesam:url".into(), Value::Str(url.into())),
                ]),
            ),
            ("PlaybackStatus".into(), Value::Str("Playing".into())),
            ("Position".into(), Value::Int(8_000_000)),
            ("Volume".into(), Value::Float(0.5)),
        ]
    }

    /// `cargo test soundcloud -- --ignored --nocapture` with soundcloud.com playing in a browser.
    #[test]
    #[ignore]
    fn live_finds_the_soundcloud_tab() {
        let (name, state) = super::linux::find_now().expect("no SoundCloud tab on the bus");
        println!("{name}: {state:?}");
        assert!(state.running && state.track.is_some());
    }

    #[test]
    fn only_soundcloud_pages_count() {
        assert!(is_soundcloud("https://soundcloud.com/discover"));
        assert!(is_soundcloud("https://m.soundcloud.com/a/b"));
        assert!(!is_soundcloud("https://soundcloud.com.evil.io/"));
        assert!(!is_soundcloud("http://soundcloud.com/"));
        assert!(!is_soundcloud("https://youtube.com/watch?v=soundcloud.com"));
        assert!(plays_soundcloud(&props("https://soundcloud.com/discover")));
        assert!(!plays_soundcloud(&props("https://www.youtube.com/watch")));
    }

    #[test]
    fn a_tab_becomes_the_pill_state_with_its_whole_title() {
        let s = state_of(&props("https://soundcloud.com/discover"), 1000.0);
        let track = s.track.expect("a track");
        assert!(s.running && s.installed && s.playing);
        assert_eq!(track.title, "Don Miguelo - Y Que Fue? [Audio Oficial]");
        assert_eq!(track.artist, "Someone");
        assert_eq!(track.duration.round(), 164.0);
        assert_eq!(s.position, 8.0);
        assert_eq!(s.volume, 50);
    }
}
