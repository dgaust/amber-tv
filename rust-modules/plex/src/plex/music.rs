//! Amber: music from the server's artist libraries (`docs/music-design.md`).
//!
//! Shapes follow `docs/plex-openapi.json` and the video side of this client; a music item is read
//! leniently from the JSON (`serde_json::Value`), like the Live TV guide, so this module owns its
//! own small type and never touches the video `Metadata` row.
//!
//! - `GET /library/sections` -> `Directory[]`, music libraries have `type=artist`;
//! - `GET /library/sections/{id}/all?type=8|9|10` -> artists / albums / tracks, paged by
//!   `X-Plex-Container-Start` / `-Size`, `MediaContainer.totalSize` the whole count;
//! - `GET /library/metadata/{rk}/children` -> an artist's albums, an album's tracks;
//! - `GET /library/metadata/{rk}/grandchildren` -> every track of an artist;
//! - `GET /hubs/sections/{id}` -> the library's shelves (Recently Played, Recently Added, ...);
//! - `PUT /:/rate?key={rk}&identifier=com.plexapp.plugins.library&rating=0..10`;
//! - the audio bytes are the track's Part (`Media[0].Part[0].key`), fetched as they are.

use super::Client;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MusicKind {
    #[default]
    Other,
    Artist,
    Album,
    Track,
    Playlist,
}

impl MusicKind {
    pub fn from_wire(s: &str) -> MusicKind {
        match s {
            "artist" => MusicKind::Artist,
            "album" => MusicKind::Album,
            "track" => MusicKind::Track,
            "playlist" => MusicKind::Playlist,
            _ => MusicKind::Other,
        }
    }
    /// The `type=` a library listing asks for.
    pub fn listing_type(self) -> i32 {
        match self {
            MusicKind::Artist => 8,
            MusicKind::Album => 9,
            _ => 10,
        }
    }
}

/// One artist, album, track or playlist, whichever the server sent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicItem {
    pub kind: MusicKind,
    pub rk: String,
    pub title: String,
    /// A track's album / an album's artist.
    pub parent_title: String,
    pub parent_rk: String,
    /// A track's artist.
    pub grandparent_title: String,
    pub grandparent_rk: String,
    pub thumb: String,
    pub parent_thumb: String,
    pub grandparent_thumb: String,
    pub art: String,
    pub summary: String,
    pub year: i32,
    /// A track's number on its disc.
    pub index: i32,
    /// A track's disc.
    pub disc: i32,
    pub duration_ms: i64,
    /// The viewer's rating, 0..10 (`-1` = none).
    pub user_rating: f32,
    pub view_count: i32,
    /// An album's track count / an artist's album count / a playlist's item count.
    pub leaf_count: i32,
    pub added_at: i64,
    pub genres: Vec<String>,
    pub studio: String,
    // the audio, tracks only
    pub codec: String,
    pub container: String,
    pub bitrate_kbps: i32,
    pub channels: i32,
    pub sample_rate: i32,
    pub bit_depth: i32,
    pub part_key: String,
    pub part_size: i64,
    /// Plex's track gain / album gain in dB and the album peak (loudness levelling); `None` when absent.
    pub gain: Option<f32>,
    pub album_gain: Option<f32>,
}

impl MusicItem {
    /// The artwork for a square tile: an album's cover, a track's album cover, an artist's portrait.
    pub fn cover(&self) -> &str {
        if !self.thumb.is_empty() {
            &self.thumb
        } else if !self.parent_thumb.is_empty() {
            &self.parent_thumb
        } else {
            &self.grandparent_thumb
        }
    }
    /// "Artist" for an album or track.
    pub fn artist(&self) -> &str {
        if self.kind == MusicKind::Track { &self.grandparent_title } else { &self.parent_title }
    }
    /// Lossless in the sense a badge means it.
    pub fn lossless(&self) -> bool {
        matches!(self.codec.as_str(), "flac" | "alac" | "wav" | "pcm" | "ape" | "wavpack" | "aiff")
            || self.codec.starts_with("pcm")
    }
    /// "FLAC 24-bit / 96 kHz", "MP3 320 kbps" and so on, for the Now Playing chip.
    pub fn quality_label(&self) -> String {
        let name = self.codec.to_uppercase();
        if self.lossless() {
            if self.bit_depth > 0 && self.sample_rate > 0 {
                format!("{name} {}-bit / {} kHz", self.bit_depth, trim_khz(self.sample_rate))
            } else {
                name
            }
        } else if self.bitrate_kbps > 0 {
            format!("{name} {} kbps", self.bitrate_kbps)
        } else {
            name
        }
    }
}

fn trim_khz(hz: i32) -> String {
    let k = hz as f32 / 1000.0;
    if (k - k.round()).abs() < 0.01 { format!("{}", k.round() as i32) } else { format!("{k:.1}") }
}

/// One library of music.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicSection {
    pub key: i64,
    pub title: String,
}

/// A page of a listing and how many the whole listing holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicPage {
    pub items: Vec<MusicItem>,
    pub total: usize,
    pub start: usize,
}

/// One shelf of a library's hubs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicHub {
    pub id: String,
    pub title: String,
    pub items: Vec<MusicItem>,
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn n(v: &Value, k: &str) -> i64 {
    match v.get(k) {
        Some(Value::Number(x)) => x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(x)) => x.parse().unwrap_or(0),
        _ => 0,
    }
}

fn f(v: &Value, k: &str) -> Option<f32> {
    match v.get(k) {
        Some(Value::Number(x)) => x.as_f64().map(|f| f as f32),
        Some(Value::String(x)) => x.parse().ok(),
        _ => None,
    }
}

fn arr<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    v.pointer(path).and_then(|x| x.as_array()).map_or(&[], |a| a.as_slice())
}

pub fn parse_item(m: &Value) -> MusicItem {
    let media = m.pointer("/Media/0");
    let part = media.and_then(|x| x.pointer("/Part/0"));
    // the audio stream of the part carries the rate, depth and gain
    let stream = part
        .and_then(|p| p.get("Stream").and_then(|x| x.as_array()))
        .and_then(|a| a.iter().find(|st| n(st, "streamType") == 2).or_else(|| a.first()));
    let mut item = MusicItem {
        kind: MusicKind::from_wire(&s(m, "type")),
        rk: s(m, "ratingKey"),
        title: s(m, "title"),
        parent_title: s(m, "parentTitle"),
        parent_rk: s(m, "parentRatingKey"),
        grandparent_title: s(m, "grandparentTitle"),
        grandparent_rk: s(m, "grandparentRatingKey"),
        thumb: { let th = s(m, "thumb"); if th.is_empty() { s(m, "composite") } else { th } },
        parent_thumb: s(m, "parentThumb"),
        grandparent_thumb: s(m, "grandparentThumb"),
        art: s(m, "art"),
        summary: s(m, "summary"),
        year: n(m, "year") as i32,
        index: n(m, "index") as i32,
        disc: n(m, "parentIndex") as i32,
        duration_ms: n(m, "duration"),
        user_rating: f(m, "userRating").unwrap_or(-1.0),
        view_count: n(m, "viewCount") as i32,
        // an artist's count is its albums (`childCount`); an album's or playlist's is its tracks (`leafCount`)
        leaf_count: (if s(m, "type") == "artist" { n(m, "childCount") } else { n(m, "leafCount") }) as i32,
        added_at: n(m, "addedAt"),
        genres: m.get("Genre").and_then(|g| g.as_array()).map_or_else(Vec::new, |a| {
            a.iter().map(|g| s(g, "tag")).filter(|t| !t.is_empty()).collect()
        }),
        studio: s(m, "studio"),
        ..Default::default()
    };
    // a playlist's own duration and size are on the playlist; a track's disc is `parentIndex`
    if item.kind != MusicKind::Track {
        item.disc = 0;
    }
    if let Some(media) = media {
        item.codec = s(media, "audioCodec");
        item.container = s(media, "container");
        item.bitrate_kbps = n(media, "bitrate") as i32;
        item.channels = n(media, "audioChannels") as i32;
        if item.duration_ms == 0 {
            item.duration_ms = n(media, "duration");
        }
    }
    if let Some(part) = part {
        item.part_key = s(part, "key");
        item.part_size = n(part, "size");
    }
    if let Some(st) = stream {
        item.sample_rate = n(st, "samplingRate") as i32;
        item.bit_depth = n(st, "bitDepth") as i32;
        if item.channels == 0 {
            item.channels = n(st, "channels") as i32;
        }
        item.gain = f(st, "gain");
        item.album_gain = f(st, "albumGain");
    }
    item
}

pub fn parse_items(body: &[u8]) -> MusicPage {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return MusicPage::default() };
    let items: Vec<MusicItem> = arr(&v, "/MediaContainer/Metadata")
        .iter()
        .map(parse_item)
        .filter(|i| !i.rk.is_empty() && i.kind != MusicKind::Other)
        .collect();
    let total = v.pointer("/MediaContainer/totalSize").map_or(0, |x| match x {
        Value::Number(n) => n.as_u64().unwrap_or(0) as usize,
        Value::String(t) => t.parse().unwrap_or(0),
        _ => 0,
    });
    let start = v.pointer("/MediaContainer/offset").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
    MusicPage { total: total.max(items.len() + start), start, items }
}

pub fn parse_sections(body: &[u8]) -> Vec<MusicSection> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    arr(&v, "/MediaContainer/Directory")
        .iter()
        .filter(|d| s(d, "type") == "artist")
        .filter_map(|d| {
            let key = s(d, "key").parse::<i64>().ok()?;
            Some(MusicSection { key, title: s(d, "title") })
        })
        .collect()
}

pub fn parse_hubs(body: &[u8]) -> Vec<MusicHub> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    arr(&v, "/MediaContainer/Hub")
        .iter()
        .map(|h| MusicHub {
            id: s(h, "hubIdentifier"),
            title: s(h, "title"),
            items: h
                .get("Metadata")
                .and_then(|x| x.as_array())
                .map_or_else(Vec::new, |a| a.iter().map(parse_item).filter(|i| !i.rk.is_empty() && i.kind != MusicKind::Other).collect()),
        })
        .filter(|h| !h.items.is_empty())
        .collect()
}

/// The path of a section listing page. `sort` is the server's own key (`titleSort`, `addedAt:desc`...).
pub fn list_path(section: i64, kind: MusicKind, start: usize, size: usize, sort: &str) -> String {
    let sort = if sort.is_empty() { String::new() } else { format!("&sort={}", urlenc(sort)) };
    format!(
        "/library/sections/{section}/all?type={}{sort}&X-Plex-Container-Start={start}&X-Plex-Container-Size={size}",
        kind.listing_type()
    )
}

fn urlenc(x: &str) -> String {
    let mut out = String::new();
    for b in x.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The 0..10 rating the server stores for a star count out of five, in half stars.
pub fn rating_of_half_stars(half_stars: i32) -> i32 {
    half_stars.clamp(0, 10)
}

impl Client {
    pub fn music_sections(&self) -> Option<Vec<MusicSection>> {
        let body = self.body_2xx_bulk("/library/sections", &[crate::http::ACCEPT_JSON])?;
        Some(parse_sections(&body))
    }

    pub fn music_list(&self, section: i64, kind: MusicKind, start: usize, size: usize, sort: &str) -> Option<MusicPage> {
        let body = self.body_2xx_bulk(&list_path(section, kind, start, size, sort), &[crate::http::ACCEPT_JSON])?;
        Some(parse_items(&body))
    }

    /// An artist's albums, an album's tracks, a playlist's items.
    pub fn music_children(&self, rk: &str, playlist: bool) -> Option<MusicPage> {
        let path = if playlist {
            format!("/playlists/{rk}/items")
        } else {
            format!("/library/metadata/{rk}/children?X-Plex-Container-Start=0&X-Plex-Container-Size=500")
        };
        let body = self.body_2xx_bulk(&path, &[crate::http::ACCEPT_JSON])?;
        Some(parse_items(&body))
    }

    /// The audio playlists.
    pub fn music_playlists(&self) -> Option<MusicPage> {
        let body = self.body_2xx_bulk("/playlists?playlistType=audio", &[crate::http::ACCEPT_JSON])?;
        Some(parse_items(&body))
    }

    /// Every track of an artist.
    pub fn music_artist_tracks(&self, rk: &str) -> Option<MusicPage> {
        let path = format!("/library/metadata/{rk}/grandchildren?X-Plex-Container-Start=0&X-Plex-Container-Size=500");
        let body = self.body_2xx_bulk(&path, &[crate::http::ACCEPT_JSON])?;
        Some(parse_items(&body))
    }

    /// One item with its own detail (an artist's summary, an album's studio, a track's streams).
    pub fn music_item(&self, rk: &str) -> Option<MusicItem> {
        let body = self.body_2xx_bulk(&format!("/library/metadata/{rk}"), &[crate::http::ACCEPT_JSON])?;
        parse_items(&body).items.into_iter().next()
    }

    pub fn music_hubs(&self, section: i64) -> Option<Vec<MusicHub>> {
        let body = self.body_2xx_bulk(&format!("/hubs/sections/{section}?count=20"), &[crate::http::ACCEPT_JSON])?;
        Some(parse_hubs(&body))
    }

    /// An artist's most played tracks.
    pub fn music_popular(&self, section: i64, artist_rk: &str, count: usize) -> Option<MusicPage> {
        let path = format!(
            "/library/sections/{section}/all?type=10&artist.id={artist_rk}&sort=ratingCount%3Adesc&X-Plex-Container-Start=0&X-Plex-Container-Size={count}"
        );
        let body = self.body_2xx_bulk(&path, &[crate::http::ACCEPT_JSON])?;
        Some(parse_items(&body))
    }

    /// Search one library for artists, albums and tracks.
    pub fn music_search(&self, section: i64, query: &str, kind: MusicKind, size: usize) -> Option<MusicPage> {
        let path = format!(
            "/library/sections/{section}/all?type={}&title={}&X-Plex-Container-Start=0&X-Plex-Container-Size={size}",
            kind.listing_type(),
            urlenc(query)
        );
        let body = self.body_2xx_bulk(&path, &[crate::http::ACCEPT_JSON])?;
        Some(parse_items(&body))
    }

    /// Tracks sonically near `rk`: the Plex Pass "radio". `None` when the server has no analysis.
    pub fn music_nearest(&self, rk: &str, count: usize) -> Option<MusicPage> {
        let path = format!("/library/metadata/{rk}/nearest?limit={count}&maxDistance=0.25");
        let body = self.body_2xx_bulk(&path, &[crate::http::ACCEPT_JSON])?;
        let page = parse_items(&body);
        (!page.items.is_empty()).then_some(page)
    }

    /// Rate an item, `half_stars` 0..10 (0 clears it). True when the server took it.
    pub fn music_rate(&self, rk: &str, half_stars: i32) -> bool {
        let r = rating_of_half_stars(half_stars);
        self.put(&format!("/:/rate?key={rk}&identifier=com.plexapp.plugins.library&rating={r}")) / 100 == 2
    }

    /// `from..=to` of the audio file (a `Range` read). A server that ignores the range sends the whole file, which
    /// the caller sees by the length.
    pub fn music_fetch_range(&self, part_key: &str, from: usize, to: usize) -> Result<Vec<u8>, i32> {
        let range = format!("Range: bytes={from}-{to}");
        match self.send_bulk(part_key, &[range.as_str()]) {
            Some(r) if r.ok() => Ok(r.body),
            Some(r) => Err(r.status),
            None => Err(0),
        }
    }

    /// `path` on this server as a plain-HTTP address carrying the token, for a device on the home network that
    /// fetches the file itself (a Sonos speaker). A secure `plex.direct` name is turned back into the address it
    /// stands for (`10-0-0-5.<hash>.plex.direct` is `10.0.0.5`); the server answers plain HTTP there too.
    pub fn music_lan_url(&self, path: &str) -> Option<String> {
        let mut host = self.origin.host().to_owned();
        if host.ends_with(".plex.direct") {
            let first = host.split('.').next().unwrap_or("");
            if first.split('-').count() == 4 && first.split('-').all(|p| p.parse::<u8>().is_ok()) {
                host = first.replace('-', ".");
            } else {
                return None;
            }
        }
        // a plain-http link carries the server's token: it goes only where a speaker on the home network can reach it
        if !is_home_host(&host) {
            return None;
        }
        let host = if host.contains(':') { format!("[{host}]") } else { host };
        Some(format!("http://{host}:{}{}", self.origin.port(), self.with_token(path)))
    }

    /// The whole audio file of a track, as the server stores it.
    pub fn music_fetch_part(&self, part_key: &str) -> Option<Vec<u8>> {
        self.body_2xx_bulk(part_key, &[])
    }

    /// Report a track's play state to the server (`state` = playing, paused, stopped; times in ms).
    pub fn music_timeline(&self, rk: &str, key: &str, state: &str, time_ms: i64, duration_ms: i64) -> bool {
        let path = format!(
            "/:/timeline?ratingKey={rk}&key={}&state={state}&time={time_ms}&duration={duration_ms}&identifier=com.plexapp.plugins.library&hasMDE=1",
            urlenc(key)
        );
        self.post_ok(&path)
    }
}

/// A private, loopback or link-local address, or a `.local` name: somewhere a home network reaches and the internet
/// does not.
fn is_home_host(host: &str) -> bool {
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_private() || v4.is_loopback() || v4.is_link_local();
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        let s = v6.segments()[0];
        return v6.is_loopback() || (s & 0xfe00) == 0xfc00 || (s & 0xffc0) == 0xfe80;
    }
    host.to_ascii_lowercase().ends_with(".local")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_only_goes_to_the_home_network() {
        for h in ["10.0.0.5", "192.168.1.20", "172.16.4.2", "172.31.255.1", "169.254.3.4", "127.0.0.1", "fd12:3456::1", "fe80::1", "plex.local"] {
            assert!(is_home_host(h), "{h}");
        }
        for h in ["8.8.8.8", "172.32.0.1", "203.0.113.9", "2001:db8::1", "plex.example.com", "192-168-1-5.abc.plex.direct", ""] {
            assert!(!is_home_host(h), "{h}");
        }
    }

    const TRACKS: &str = r#"{"MediaContainer":{"size":2,"totalSize":42,"offset":0,"Metadata":[
      {"ratingKey":"101","type":"track","title":"Glass Harbour","parentTitle":"Night Shift","parentRatingKey":"90",
       "grandparentTitle":"Harbour Lights","grandparentRatingKey":"7","parentThumb":"/library/metadata/90/thumb/1",
       "index":1,"parentIndex":1,"duration":222000,"userRating":8.0,
       "Media":[{"audioCodec":"flac","container":"flac","bitrate":1411,"audioChannels":2,"Part":[{"key":"/library/parts/5/1/file.flac","size":31000000,
         "Stream":[{"streamType":2,"samplingRate":96000,"bitDepth":24,"gain":-6.5,"albumGain":-7.25}]}]}]},
      {"ratingKey":"102","type":"track","title":"Paper Moons","index":"2","duration":"245000"}]}}"#;

    #[test]
    fn a_track_row_reads_its_tags_and_its_audio() {
        let page = parse_items(TRACKS.as_bytes());
        assert_eq!(page.total, 42);
        assert_eq!(page.items.len(), 2);
        let t = &page.items[0];
        assert_eq!(t.kind, MusicKind::Track);
        assert_eq!((t.title.as_str(), t.artist(), t.parent_title.as_str()), ("Glass Harbour", "Harbour Lights", "Night Shift"));
        assert_eq!((t.index, t.disc, t.duration_ms), (1, 1, 222_000));
        assert_eq!((t.codec.as_str(), t.sample_rate, t.bit_depth), ("flac", 96000, 24));
        assert_eq!(t.part_key, "/library/parts/5/1/file.flac");
        assert_eq!(t.gain, Some(-6.5));
        assert!(t.lossless());
        assert_eq!(t.quality_label(), "FLAC 24-bit / 96 kHz");
        assert_eq!(t.cover(), "/library/metadata/90/thumb/1", "a track's cover is its album's");
        // a sparse row, with numbers sent as strings
        assert_eq!((page.items[1].index, page.items[1].duration_ms), (2, 245_000));
        assert_eq!(page.items[1].user_rating, -1.0);
    }

    #[test]
    fn an_artist_counts_albums_and_an_album_counts_tracks() {
        let page = parse_items(br#"{"MediaContainer":{"Metadata":[
          {"ratingKey":"1","type":"artist","title":"A","leafCount":96,"childCount":8},
          {"ratingKey":"2","type":"album","title":"B","leafCount":11,"childCount":0}]}}"#);
        assert_eq!(page.items[0].leaf_count, 8);
        assert_eq!(page.items[1].leaf_count, 11);
    }

    #[test]
    fn music_sections_are_the_artist_libraries_only() {
        let body = br#"{"MediaContainer":{"Directory":[{"key":"1","type":"movie","title":"Films"},{"key":"4","type":"artist","title":"Music"}]}}"#;
        assert_eq!(parse_sections(body), vec![MusicSection { key: 4, title: "Music".into() }]);
    }

    #[test]
    fn hubs_keep_only_shelves_with_music_in_them() {
        let body = br#"{"MediaContainer":{"Hub":[{"hubIdentifier":"home.recent","title":"Recently Added","Metadata":[{"ratingKey":"9","type":"album","title":"A"}]},
          {"hubIdentifier":"empty","title":"Nothing","Metadata":[]}]}}"#;
        let hubs = parse_hubs(body);
        assert_eq!(hubs.len(), 1);
        assert_eq!((hubs[0].title.as_str(), hubs[0].items[0].kind), ("Recently Added", MusicKind::Album));
    }

    #[test]
    fn a_listing_path_names_the_type_the_sort_and_the_page() {
        assert_eq!(
            list_path(4, MusicKind::Album, 100, 50, "addedAt:desc"),
            "/library/sections/4/all?type=9&sort=addedAt%3Adesc&X-Plex-Container-Start=100&X-Plex-Container-Size=50"
        );
        assert_eq!(list_path(4, MusicKind::Artist, 0, 10, ""), "/library/sections/4/all?type=8&X-Plex-Container-Start=0&X-Plex-Container-Size=10");
    }

    #[test]
    fn quality_labels_for_a_lossy_track_name_the_bitrate() {
        let t = MusicItem { codec: "mp3".into(), bitrate_kbps: 320, ..Default::default() };
        assert_eq!(t.quality_label(), "MP3 320 kbps");
        assert!(!t.lossless());
    }
}
