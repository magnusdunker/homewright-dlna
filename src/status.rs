//! A renderer's state as read from AVTransport and RenderingControl, and
//! how it becomes a media_player state with Home Assistant's attribute names.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use crate::upnp::Scpd;
use crate::xml;

// media_player supported_features, as in Home Assistant
pub const PAUSE: u64 = 1;
pub const SEEK: u64 = 2;
pub const VOLUME_SET: u64 = 4;
pub const VOLUME_MUTE: u64 = 8;
pub const PREVIOUS_TRACK: u64 = 16;
pub const NEXT_TRACK: u64 = 32;
pub const PLAY_MEDIA: u64 = 512;
pub const VOLUME_STEP: u64 = 1024;
pub const STOP: u64 = 4096;
pub const PLAY: u64 = 16384;
pub const SHUFFLE_SET: u64 = 32768;
pub const REPEAT_SET: u64 = 262144;

/// What the renderer can do, from its service descriptions.
#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub avt: HashSet<String>,
    pub rc: HashSet<String>,
    pub volume_max: f64,
    pub play_modes: Vec<String>,
    /// ConnectionManager's sink protocols, e.g. `http-get:*:audio/mpeg:*`.
    pub sink: Vec<String>,
}

impl Caps {
    /// From the SCPDs; a missing one is assumed to have the usual actions.
    pub fn from(avt: Option<Scpd>, rc: Option<Scpd>, has_avt: bool, has_rc: bool) -> Self {
        let usual = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<HashSet<String>>();
        let avt_actions = match (&avt, has_avt) {
            (Some(s), _) => s.actions.clone(),
            (None, true) => usual(&["SetAVTransportURI", "GetMediaInfo", "GetTransportInfo", "GetPositionInfo", "Play", "Pause", "Stop", "Seek", "Next", "Previous"]),
            (None, false) => HashSet::new(),
        };
        let rc_actions = match (&rc, has_rc) {
            (Some(s), _) => s.actions.clone(),
            (None, true) => usual(&["GetVolume", "SetVolume", "GetMute", "SetMute"]),
            (None, false) => HashSet::new(),
        };
        let volume_max = rc.as_ref().and_then(|s| s.maximum.get("Volume").copied()).filter(|m| *m > 0.0).unwrap_or(100.0);
        let play_modes = avt.as_ref().and_then(|s| s.allowed.get("CurrentPlayMode").cloned()).unwrap_or_default();
        Self { avt: avt_actions, rc: rc_actions, volume_max, play_modes, sink: vec![] }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub transport_state: Option<String>,
    pub volume: Option<f64>,
    pub muted: Option<bool>,
    pub track_uri: String,
    pub track_meta: String,
    pub av_uri: String,
    pub av_meta: String,
    pub duration: Option<f64>,
    pub position: Option<f64>,
    /// When `position` was read.
    pub position_at: Option<chrono::DateTime<chrono::Utc>>,
    /// `None` when the renderer cannot say.
    pub actions: Option<Vec<String>>,
    pub play_mode: Option<String>,
}

impl State {
    pub fn playing(&self) -> bool {
        self.transport_state.as_deref() == Some("PLAYING")
    }

    /// Take a newly read position, but keep the old one while it still runs
    /// on time, so a poll does not send a new state every few seconds.
    pub fn set_position(&mut self, pos: Option<f64>, now: chrono::DateTime<chrono::Utc>) {
        let Some(p) = pos else {
            self.position = None;
            self.position_at = None;
            return;
        };
        if let (Some(old), Some(at)) = (self.position, self.position_at) {
            let expected = if self.playing() { old + (now - at).num_milliseconds() as f64 / 1000.0 } else { old };
            if (expected - p).abs() < 2.5 {
                return;
            }
        }
        self.position = Some(p);
        self.position_at = Some(now);
    }

    /// Apply a variable from a `LastChange` event or an action's answer.
    pub fn apply(&mut self, name: &str, channel: Option<&str>, val: &str, now: chrono::DateTime<chrono::Utc>) {
        match name {
            "TransportState" => {
                if self.transport_state.as_deref() != Some(val) {
                    // keep the position but restart its clock
                    if self.position.is_some() {
                        self.position_at = Some(now);
                    }
                    self.transport_state = Some(val.to_string());
                }
            }
            "CurrentTrackURI" => self.track_uri = val.to_string(),
            "CurrentTrackMetaData" => self.track_meta = not_implemented(val),
            "AVTransportURI" => self.av_uri = val.to_string(),
            "AVTransportURIMetaData" => self.av_meta = not_implemented(val),
            "CurrentTrackDuration" => {
                if let Some(d) = parse_time(val) {
                    self.duration = Some(d).filter(|d| *d > 0.0);
                }
            }
            "CurrentMediaDuration" if self.duration.is_none() => self.duration = parse_time(val).filter(|d| *d > 0.0),
            "RelativeTimePosition" => self.set_position(parse_time(val), now),
            "CurrentTransportActions" => self.actions = Some(split_actions(val)),
            "CurrentPlayMode" => self.play_mode = Some(val.to_string()),
            "Volume" if channel.is_none_or(|c| c == "Master") => {
                if let Ok(v) = val.parse() {
                    self.volume = Some(v);
                }
            }
            "Mute" if channel.is_none_or(|c| c == "Master") => self.muted = Some(val == "1" || val.eq_ignore_ascii_case("true")),
            _ => {}
        }
    }
}

fn not_implemented(v: &str) -> String {
    if v == "NOT_IMPLEMENTED" {
        String::new()
    } else {
        v.to_string()
    }
}

pub fn split_actions(v: &str) -> Vec<String> {
    v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// `H:MM:SS`, `H:MM:SS.fff` or `H:MM:SS.F0/F1` in seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() || s == "NOT_IMPLEMENTED" {
        return None;
    }
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut parts = s.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    let (sec, frac) = match rest.split_once('.') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };
    let mut secs: f64 = sec.parse().ok()?;
    if let Some((n, d)) = frac.split_once('/') {
        let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
        if d > 0.0 {
            secs += n / d;
        }
    } else if !frac.is_empty() {
        secs += format!("0.{frac}").parse::<f64>().ok()?;
    }
    let t = h * 3600.0 + m * 60.0 + secs;
    Some(if neg { -t } else { t })
}

pub fn format_time(secs: f64) -> String {
    let t = secs.max(0.0).round() as u64;
    format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

/// What a DIDL-Lite document says about the item in it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub track: Option<u64>,
    pub class: Option<String>,
    pub art: Option<String>,
    pub duration: Option<f64>,
}

pub fn parse_didl(text: &str) -> Meta {
    let mut m = Meta::default();
    if text.trim().is_empty() {
        return m;
    }
    let Some(root) = xml::parse(text) else { return m };
    let Some(item) = root.child("item").or(root.child("container")) else { return m };
    let t = |n: &str| item.text_of(n).map(String::from);
    m.title = t("title");
    m.album = t("album");
    m.class = t("class");
    m.art = t("albumArtURI");
    m.track = item.text_of("originalTrackNumber").and_then(|n| n.parse().ok());
    // upnp:artist with role AlbumArtist is the album's artist
    for a in item.children_named("artist") {
        let v = a.text.trim();
        if v.is_empty() {
            continue;
        }
        if a.attr("role").is_some_and(|r| r.eq_ignore_ascii_case("AlbumArtist")) {
            m.album_artist.get_or_insert_with(|| v.to_string());
        } else {
            m.artist.get_or_insert_with(|| v.to_string());
        }
    }
    if m.artist.is_none() {
        m.artist = t("creator");
    }
    m.duration = item.child("res").and_then(|r| r.attr("duration")).and_then(parse_time);
    m
}

fn content_type(class: Option<&str>, uri: &str) -> &'static str {
    let c = class.unwrap_or_default();
    if c.starts_with("object.item.audioItem") {
        return "music";
    }
    if c.starts_with("object.item.videoItem.movie") {
        return "movie";
    }
    if c.starts_with("object.item.videoItem") {
        return "video";
    }
    if c.starts_with("object.item.imageItem") {
        return "image";
    }
    if c.starts_with("object.container") {
        return "playlist";
    }
    match mime_for(uri, "") {
        m if m.starts_with("audio/") => "music",
        m if m.starts_with("image/") => "image",
        _ => "video",
    }
}

/// Whether the renderer currently allows a transport action.
pub fn can(caps: &Caps, st: &State, action: &str) -> bool {
    if !caps.avt.contains(action) {
        return false;
    }
    match &st.actions {
        Some(list) if caps.avt.contains("GetCurrentTransportActions") => list.iter().any(|a| a.eq_ignore_ascii_case(action)),
        _ => true,
    }
}

pub fn features(caps: &Caps, st: &State) -> u64 {
    let mut f = 0;
    for (action, flag) in [("Play", PLAY), ("Pause", PAUSE), ("Stop", STOP), ("Seek", SEEK), ("Next", NEXT_TRACK), ("Previous", PREVIOUS_TRACK)] {
        if can(caps, st, action) {
            f |= flag;
        }
    }
    if caps.avt.contains("SetAVTransportURI") {
        f |= PLAY_MEDIA;
    }
    if caps.rc.contains("SetVolume") {
        f |= VOLUME_SET | VOLUME_STEP;
    }
    if caps.rc.contains("SetMute") {
        f |= VOLUME_MUTE;
    }
    if caps.avt.contains("SetPlayMode") && !caps.play_modes.is_empty() {
        if caps.play_modes.iter().any(|m| m.contains("SHUFFLE") || m == "RANDOM") {
            f |= SHUFFLE_SET;
        }
        if caps.play_modes.iter().any(|m| m.contains("REPEAT")) {
            f |= REPEAT_SET;
        }
    }
    f
}

/// Shuffle and repeat from a play mode.
pub fn shuffle_repeat(mode: &str) -> (bool, &'static str) {
    match mode {
        "SHUFFLE" | "RANDOM" | "SHUFFLE_NOREPEAT" => (true, "off"),
        "SHUFFLE_REPEAT_ONE" => (true, "one"),
        "REPEAT_ALL" => (false, "all"),
        "REPEAT_ONE" | "REPEAT_TRACK" => (false, "one"),
        _ => (false, "off"),
    }
}

/// The play mode for a shuffle and repeat setting, among those allowed.
pub fn play_mode_for(shuffle: bool, repeat: &str, allowed: &[String]) -> Option<String> {
    let wanted: &[&str] = match (shuffle, repeat) {
        (false, "off") => &["NORMAL"],
        (false, "all") => &["REPEAT_ALL"],
        (false, "one") => &["REPEAT_ONE", "REPEAT_TRACK"],
        (true, "off") => &["SHUFFLE_NOREPEAT", "SHUFFLE", "RANDOM"],
        (true, "all") => &["SHUFFLE", "RANDOM"],
        (true, "one") => &["SHUFFLE_REPEAT_ONE"],
        _ => return None,
    };
    wanted.iter().find(|w| allowed.iter().any(|a| a == *w)).map(|w| w.to_string())
}

/// State and attributes for the media_player entity.
pub fn state_of(caps: &Caps, st: &State) -> (String, Map<String, Value>) {
    let mut a = Map::new();
    let state = match st.transport_state.as_deref() {
        None if caps.avt.is_empty() => "on",
        Some("PLAYING") => "playing",
        Some("TRANSITIONING") => "buffering",
        Some("PAUSED_PLAYBACK" | "PAUSED_RECORDING") => "paused",
        _ => "idle",
    };
    if let Some(v) = st.volume {
        a.insert("volume_level".into(), json!(((v / caps.volume_max).clamp(0.0, 1.0) * 1000.0).round() / 1000.0));
    }
    if let Some(m) = st.muted {
        a.insert("is_volume_muted".into(), json!(m));
    }
    let active = matches!(state, "playing" | "paused" | "buffering");
    if active {
        let meta = if st.track_meta.is_empty() { parse_didl(&st.av_meta) } else { parse_didl(&st.track_meta) };
        let uri = if st.track_uri.is_empty() { &st.av_uri } else { &st.track_uri };
        if !uri.is_empty() {
            a.insert("media_content_id".into(), json!(uri));
        }
        if !uri.is_empty() || meta.class.is_some() {
            a.insert("media_content_type".into(), json!(content_type(meta.class.as_deref(), uri)));
        }
        let title = meta.title.clone().or_else(|| (!uri.is_empty()).then(|| file_name(uri)));
        for (k, v) in [("media_title", title), ("media_artist", meta.artist), ("media_album_name", meta.album), ("media_album_artist", meta.album_artist), ("entity_picture", meta.art)] {
            if let Some(v) = v {
                a.insert(k.into(), json!(v));
            }
        }
        if let Some(t) = meta.track {
            a.insert("media_track".into(), json!(t));
        }
        if let Some(d) = st.duration.or(meta.duration) {
            a.insert("media_duration".into(), json!(d.round()));
        }
        if let (Some(p), Some(at)) = (st.position, st.position_at) {
            a.insert("media_position".into(), json!(p.round()));
            a.insert("media_position_updated_at".into(), json!(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)));
        }
    }
    if let Some(mode) = &st.play_mode {
        let f = features(caps, st);
        let (shuffle, repeat) = shuffle_repeat(mode);
        if f & SHUFFLE_SET != 0 {
            a.insert("shuffle".into(), json!(shuffle));
        }
        if f & REPEAT_SET != 0 {
            a.insert("repeat".into(), json!(repeat));
        }
    }
    a.insert("supported_features".into(), json!(features(caps, st)));
    (state.to_string(), a)
}

pub fn file_name(uri: &str) -> String {
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let name = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path);
    percent_decode(name)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A MIME type: `kind` if it is one, otherwise from the URL's extension,
/// otherwise from the kind (music, video, image).
pub fn mime_for(url: &str, kind: &str) -> String {
    if kind.contains('/') {
        return kind.to_string();
    }
    let path = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
    let by_ext = match ext {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "ts" => "video/mp2t",
        "m3u8" => "application/vnd.apple.mpegurl",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        _ => "",
    };
    if !by_ext.is_empty() {
        return by_ext.into();
    }
    match kind {
        "music" | "audio" | "podcast" | "track" => "audio/mpeg",
        "image" => "image/jpeg",
        _ => "video/mp4",
    }
    .into()
}

/// The protocolInfo for `mime`: the renderer's own entry for it when it has
/// one, so DLNA profile flags come along, otherwise a plain `http-get`.
pub fn protocol_for(mime: &str, sink: &[String]) -> String {
    for p in sink {
        let mut parts = p.splitn(4, ':');
        let (proto, _, m, extra) = (parts.next(), parts.next(), parts.next(), parts.next());
        if proto == Some("http-get") && m.is_some_and(|m| m.eq_ignore_ascii_case(mime)) {
            let extra = extra.unwrap_or("*");
            // the profile name only fits files in that exact profile
            let flags: Vec<&str> = extra.split(';').filter(|f| !f.starts_with("DLNA.ORG_PN=")).collect();
            let extra = if flags.is_empty() { "*".to_string() } else { flags.join(";") };
            return format!("http-get:*:{mime}:{extra}");
        }
    }
    format!("http-get:*:{mime}:*")
}

/// DIDL-Lite for SetAVTransportURI.
pub fn didl(url: &str, title: &str, mime: &str, protocol: &str, art: Option<&str>) -> String {
    let class = match mime.split('/').next().unwrap_or_default() {
        "audio" => "object.item.audioItem.musicTrack",
        "image" => "object.item.imageItem.photo",
        _ => "object.item.videoItem",
    };
    let art = art.map(|a| format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", xml::escape(a))).unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>{class}</upnp:class>{art}\
         <res protocolInfo=\"{}\">{}</res></item></DIDL-Lite>",
        xml::escape(title),
        xml::escape(protocol),
        xml::escape(url)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("0:03:25"), Some(205.0));
        assert_eq!(parse_time("1:00:00.500"), Some(3600.5));
        assert_eq!(parse_time("0:00:01.1/2"), Some(1.5));
        assert_eq!(parse_time("NOT_IMPLEMENTED"), None);
        assert_eq!(format_time(3725.4), "1:02:05");
    }

    #[test]
    fn didl_round_trip() {
        let d = didl("http://nas/a&b.mp3", "Sang & co", "audio/mpeg", "http-get:*:audio/mpeg:*", Some("http://nas/c.jpg"));
        let m = parse_didl(&d);
        assert_eq!(m.title.as_deref(), Some("Sang & co"));
        assert_eq!(m.class.as_deref(), Some("object.item.audioItem.musicTrack"));
        assert_eq!(m.art.as_deref(), Some("http://nas/c.jpg"));
    }

    #[test]
    fn protocol_from_sink() {
        let sink = vec!["http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_FLAGS=ED100000000000000000000000000000".to_string()];
        assert_eq!(protocol_for("audio/mpeg", &sink), "http-get:*:audio/mpeg:DLNA.ORG_FLAGS=ED100000000000000000000000000000");
        assert_eq!(protocol_for("video/mp4", &sink), "http-get:*:video/mp4:*");
        assert_eq!(mime_for("http://x/y.FLAC?z", "music"), "audio/flac");
        assert_eq!(mime_for("http://x/stream", "music"), "audio/mpeg");
    }

    #[test]
    fn features_follow_transport_actions() {
        let caps = Caps::from(None, None, true, true);
        let mut st = State { transport_state: Some("NO_MEDIA_PRESENT".into()), ..Default::default() };
        // without GetCurrentTransportActions everything the SCPD has is allowed
        assert_ne!(features(&caps, &st) & PLAY, 0);
        let mut caps = caps;
        caps.avt.insert("GetCurrentTransportActions".into());
        st.actions = Some(vec![]);
        let f = features(&caps, &st);
        assert_eq!(f & (PLAY | PAUSE), 0);
        assert_ne!(f & (PLAY_MEDIA | VOLUME_SET | VOLUME_MUTE), 0);
        st.actions = Some(split_actions("Play,Stop,Pause,Seek,X_DLNA_SeekTime"));
        assert_ne!(features(&caps, &st) & (PLAY | PAUSE | STOP | SEEK), 0);
    }

    #[test]
    fn state_and_position() {
        let caps = Caps::from(None, None, true, true);
        let now = chrono::Utc::now();
        let mut st = State::default();
        st.apply("TransportState", None, "PLAYING", now);
        st.apply("Volume", Some("Master"), "25", now);
        st.apply("Volume", Some("LF"), "90", now);
        st.apply("CurrentTrackURI", None, "http://nas/musik/Min%20sang.mp3", now);
        st.apply("RelativeTimePosition", None, "0:00:10", now);
        let (s, a) = state_of(&caps, &st);
        assert_eq!(s, "playing");
        assert_eq!(a["volume_level"], 0.25);
        assert_eq!(a["media_title"], "Min sang.mp3");
        assert_eq!(a["media_content_type"], "music");
        assert_eq!(a["media_position"], 10.0);
        // five seconds later at 15 s: on time, so the old reading stays
        st.set_position(Some(15.2), now + chrono::Duration::seconds(5));
        assert_eq!(st.position_at, Some(now));
        st.set_position(Some(60.0), now + chrono::Duration::seconds(5));
        assert_eq!(st.position, Some(60.0));
    }

    #[test]
    fn play_modes() {
        let allowed = vec!["NORMAL".to_string(), "SHUFFLE".into(), "REPEAT_ALL".into()];
        assert_eq!(play_mode_for(true, "off", &allowed).as_deref(), Some("SHUFFLE"));
        assert_eq!(play_mode_for(false, "one", &allowed), None);
        assert_eq!(shuffle_repeat("REPEAT_ALL"), (false, "all"));
    }
}
