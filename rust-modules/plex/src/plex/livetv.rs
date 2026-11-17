//! Amber: Live TV from the server's DVR - its channels, the guide grid and tuning a channel.
//!
//! Shapes measured on a PMS 1.43.5 with one DVR (an HDHomeRun-style tuner and an XMLTV guide):
//! - `GET /livetv/dvrs` -> `MediaContainer.Dvr[]` with `key` and `epgIdentifier`;
//! - `GET /<epg>/lineups/dvr/channels` -> `MediaContainer.Channel[]` with `id`, `vcn`, `title`,
//!   `callSign`, `thumb`;
//! - `GET /<epg>/grid?type=1,4&sort=beginsAt&endsAt>=T0&beginsAt<=T1` -> `MediaContainer.Metadata[]`,
//!   one per airing, its `Media[0]` naming `channelIdentifier`, `beginsAt`, `endsAt`;
//! - `POST /livetv/dvrs/<key>/channels/<channel id>/tune` -> a subscription whose grab operation's
//!   metadata `key` is the live session, `/livetv/sessions/<uuid>`, playable like a library item.

use super::Client;
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveDvr {
    pub key: String,
    pub epg: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveChannel {
    pub id: String,
    pub number: String,
    pub title: String,
    pub thumb: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveProgramme {
    pub channel: String,
    /// Unix seconds.
    pub begins: i64,
    pub ends: i64,
    /// The show (an episode's series) or the film.
    pub title: String,
    /// An episode's own title; empty for a film.
    pub subtitle: String,
    pub season: i32,
    pub episode: i32,
    pub year: i32,
    pub summary: String,
    pub rating: String,
    pub thumb: String,
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
        Some(Value::Number(x)) => x.as_i64().unwrap_or(0),
        Some(Value::String(x)) => x.parse().unwrap_or(0),
        _ => 0,
    }
}

fn arr<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    v.pointer(path).and_then(|x| x.as_array()).map_or(&[], |a| a.as_slice())
}

/// The path a playable item is addressed by: a library item's ratingKey becomes
/// `/library/metadata/<rk>`; a key that is already a path (a live session) is used as it is.
pub fn item_path(rating_key: &str) -> String {
    if rating_key.starts_with('/') { rating_key.to_owned() } else { format!("/library/metadata/{rating_key}") }
}

pub fn parse_dvrs(body: &[u8]) -> Vec<LiveDvr> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    arr(&v, "/MediaContainer/Dvr").iter()
        .map(|d| LiveDvr { key: s(d, "key"), epg: s(d, "epgIdentifier") })
        .filter(|d| !d.key.is_empty() && !d.epg.is_empty())
        .collect()
}

pub fn parse_channels(body: &[u8]) -> Vec<LiveChannel> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    let mut out: Vec<LiveChannel> = arr(&v, "/MediaContainer/Channel").iter().map(|c| LiveChannel {
        id: s(c, "id"),
        number: s(c, "vcn"),
        title: { let t = s(c, "title"); if t.is_empty() { s(c, "callSign") } else { t } },
        thumb: s(c, "thumb"),
    }).filter(|c| !c.id.is_empty()).collect();
    // channel-number order, numerically where the numbers are numbers
    out.sort_by(|a, b| {
        let (x, y) = (a.number.parse::<f64>(), b.number.parse::<f64>());
        match (x, y) {
            (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            _ => a.number.cmp(&b.number),
        }
    });
    out
}

pub fn parse_grid(body: &[u8]) -> Vec<LiveProgramme> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    let mut out = Vec::new();
    for m in arr(&v, "/MediaContainer/Metadata") {
        let kind = s(m, "type");
        let episode = kind == "episode";
        for media in m.get("Media").and_then(|x| x.as_array()).map_or(&[][..], |a| a.as_slice()) {
            let channel = s(media, "channelIdentifier");
            let (begins, ends) = (n(media, "beginsAt"), n(media, "endsAt"));
            if channel.is_empty() || ends <= begins { continue; }
            let title = if episode { s(m, "grandparentTitle") } else { s(m, "title") };
            let thumb = [s(m, "grandparentThumb"), s(m, "thumb"), s(m, "parentThumb")]
                .into_iter().find(|t| !t.is_empty()).unwrap_or_default();
            out.push(LiveProgramme {
                channel,
                begins,
                ends,
                title: if title.is_empty() { s(m, "title") } else { title },
                subtitle: if episode { s(m, "title") } else { String::new() },
                season: if episode { n(m, "parentIndex") as i32 } else { 0 },
                episode: if episode { n(m, "index") as i32 } else { 0 },
                year: n(m, "year") as i32,
                summary: s(m, "summary"),
                rating: s(m, "contentRating"),
                thumb,
            });
        }
    }
    out.sort_by(|a, b| a.channel.cmp(&b.channel).then(a.begins.cmp(&b.begins)));
    out.dedup_by(|a, b| a.channel == b.channel && a.begins == b.begins);
    out
}

/// The live session a tune answered with: the first `key` under `/livetv/sessions/` anywhere in it.
pub fn parse_tune(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    fn find(v: &Value) -> Option<String> {
        match v {
            Value::Object(o) => {
                if let Some(Value::String(k)) = o.get("key") {
                    if k.starts_with("/livetv/sessions/") {
                        // the session itself, not one of its stream files
                        let mut parts = k.trim_start_matches('/').split('/');
                        let (a, b, c) = (parts.next()?, parts.next()?, parts.next()?);
                        return Some(format!("/{a}/{b}/{c}"));
                    }
                }
                o.values().find_map(find)
            }
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    find(&v)
}

fn enc(x: &str) -> String {
    let mut out = String::new();
    for b in x.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Client {
    pub fn livetv_dvrs(&self) -> Option<Vec<LiveDvr>> {
        let body = self.body_2xx_bulk("/livetv/dvrs", &[crate::http::ACCEPT_JSON])?;
        Some(parse_dvrs(&body))
    }

    pub fn livetv_channels(&self, epg: &str) -> Option<Vec<LiveChannel>> {
        let body = self.body_2xx_bulk(&format!("/{epg}/lineups/dvr/channels"), &[crate::http::ACCEPT_JSON])?;
        Some(parse_channels(&body))
    }

    /// Every airing that overlaps `[from, to)` (unix seconds).
    pub fn livetv_grid(&self, epg: &str, from: i64, to: i64) -> Option<Vec<LiveProgramme>> {
        let path = format!("/{epg}/grid?type=1%2C4&sort=beginsAt&endsAt%3E={from}&beginsAt%3C={to}");
        let body = self.body_2xx_bulk(&path, &[crate::http::ACCEPT_JSON])?;
        Some(parse_grid(&body))
    }

    /// Tune `channel` on `dvr`: the live session to play, `/livetv/sessions/<uuid>`. `Err` says
    /// why not - the status and the head of the server's answer (never the request: the token is
    /// on it) - for the event log.
    pub fn livetv_tune(&self, dvr: &str, channel: &str) -> Result<String, String> {
        let path = format!("/livetv/dvrs/{}/channels/{}/tune", enc(dvr), enc(channel));
        let reply = self.send(&path, crate::http::Method::Post, &[crate::http::ACCEPT_JSON])
            .ok_or_else(|| format!("no answer (channel {channel})"))?;
        let head = || String::from_utf8_lossy(&reply.body[..reply.body.len().min(300)]).replace('\n', " ");
        if !reply.ok() {
            return Err(format!("HTTP {} (channel {channel}): {}", reply.status, head()));
        }
        parse_tune(&reply.body).ok_or_else(|| format!("no session in the answer (channel {channel}): {}", head()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_grid_airing_reads_as_a_programme_on_its_channel() {
        let body = br#"{"MediaContainer":{"Metadata":[{"type":"episode","title":"The Internet","grandparentTitle":"Outnumbered",
            "index":2,"parentIndex":3,"summary":"s","contentRating":"au","grandparentThumb":"/t",
            "Media":[{"channelIdentifier":"C2.146","beginsAt":100,"endsAt":200}]}]}}"#;
        let g = parse_grid(body);
        assert_eq!(g.len(), 1);
        assert_eq!((g[0].title.as_str(), g[0].subtitle.as_str(), g[0].season, g[0].episode), ("Outnumbered", "The Internet", 3, 2));
        assert_eq!((g[0].channel.as_str(), g[0].begins, g[0].ends), ("C2.146", 100, 200));
    }
    #[test]
    fn a_tune_answers_with_its_session() {
        let body = br#"{"MediaContainer":{"MediaSubscription":[{"MediaGrabOperation":[{"Metadata":{"key":"/livetv/sessions/abc/def/index.m3u8"}}]}]}}"#;
        assert_eq!(parse_tune(body).as_deref(), Some("/livetv/sessions/abc"));
    }
    #[test]
    fn a_session_path_is_its_own_item_path() {
        assert_eq!(item_path("123"), "/library/metadata/123");
        assert_eq!(item_path("/livetv/sessions/abc"), "/livetv/sessions/abc");
    }
}
