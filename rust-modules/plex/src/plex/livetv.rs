//! Amber: Live TV from the server's DVR - its channels, the guide grid and tuning a channel.
//!
//! Shapes measured on a PMS 1.43.5 with one DVR (an HDHomeRun-style tuner and an XMLTV guide):
//! - `GET /livetv/dvrs` -> `MediaContainer.Dvr[]` with `key` and `epgIdentifier`;
//! - `GET /<epg>/lineups/dvr/channels` -> `MediaContainer.Channel[]` with `id`, `vcn`, `title`,
//!   `callSign`, `thumb`;
//! - `GET /<epg>/grid?type=1,4&sort=beginsAt&endsAt>=T0&beginsAt<=T1` -> `MediaContainer.Metadata[]`,
//!   one per airing, its `Media[0]` naming `channelIdentifier`, `beginsAt`, `endsAt`;
//! - `POST /livetv/dvrs/<key>/channels/<channel>/tune` -> a subscription whose grab operation's
//!   (`<channel>` is the guide's `channelIdentifier`, as Plex Web sends it; the POST must carry an
//!   `X-Plex-Session-Identifier` or PMS answers 400. Other forms are tried after it, `livetv_tune_any`)
//!   metadata `key` is the live session, `/livetv/sessions/<uuid>`, playable like a library item.

use super::Client;
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveDvr {
    pub key: String,
    pub epg: String,
    /// `Device[].ChannelMapping[]`: (the guide's channel id, the tuner's own channel, its key).
    pub mappings: Vec<(String, String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveChannel {
    pub id: String,
    pub number: String,
    pub grid_key: String,
    pub title: String,
    pub thumb: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Landscape art: the airing's fan art, its show's, else an episode's still.
    pub art: String,
    /// Amber: what a recording rule is made for - the film's own guid, or an episode's SHOW guid
    /// (`tv.plex.xmltv://show/<title>`). Empty from a guide cached before this existed.
    #[serde(default)]
    pub record_guid: String,
    /// Amber: the airing's own guid - what "record this episode" is made for (a film's is its
    /// `record_guid`).
    #[serde(default)]
    pub episode_guid: String,
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
        .map(|d| LiveDvr {
            key: s(d, "key"),
            epg: s(d, "epgIdentifier"),
            mappings: d.get("Device").and_then(|x| x.as_array()).map_or(&[][..], |a| a.as_slice()).iter()
                .flat_map(|dev| dev.get("ChannelMapping").and_then(|x| x.as_array()).map_or(&[][..], |a| a.as_slice()))
                .map(|m| (s(m, "lineupIdentifier"), s(m, "deviceIdentifier"), s(m, "channelKey")))
                .collect(),
        })
        .filter(|d| !d.key.is_empty() && !d.epg.is_empty())
        .collect()
}

pub fn parse_channels(body: &[u8]) -> Vec<LiveChannel> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    let mut out: Vec<LiveChannel> = arr(&v, "/MediaContainer/Channel").iter().map(|c| LiveChannel {
        id: s(c, "id"),
        number: s(c, "vcn"),
        grid_key: s(c, "gridKey"),
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
            let still = if episode { s(m, "thumb") } else { String::new() };
            let art = [s(m, "art"), s(m, "grandparentArt"), still]
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
                art,
                record_guid: if episode { s(m, "grandparentGuid") } else { s(m, "guid") },
                episode_guid: s(m, "guid"),
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

/// A fresh playback session id for one tune, uuid-shaped (the kernel's, where there is one).
fn session_id() -> String {
    if let Ok(u) = std::fs::read_to_string("/proc/sys/kernel/random/uuid") {
        let t = u.trim();
        if !t.is_empty() { return t.to_string(); }
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let h = format!("{:016x}{:016x}", t, n.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ t.rotate_left(17));
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

static SESSIONS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

fn remember_session(path: &str, id: &str) {
    let mut s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    s.retain(|(p, _)| p != path);
    s.push((path.to_owned(), id.to_owned()));
    let n = s.len();
    if n > 8 { s.drain(..n - 8); }
}

/// The playback session a tuned live session was opened under, so the stream that plays it joins
/// that session instead of starting another (PMS drops a stream whose session never tuned).
pub fn playback_session_for(path: &str) -> Option<String> {
    let s = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    s.iter().rev().find(|(p, _)| p == path).map(|(_, id)| id.clone())
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
    /// Tune the first of `candidates` (the forms one channel goes by: the tuner's channel, its
    /// number, the guide's id...) the server accepts. Every refusal is in the `Err`, in order.
    pub fn livetv_tune_any(&self, dvr: &str, candidates: &[String]) -> Result<String, String> {
        let mut why = Vec::new();
        let id = session_id();
        for c in candidates.iter().filter(|c| !c.is_empty()) {
            match self.livetv_tune_as(dvr, c, &id) {
                Ok(session) => {
                    remember_session(&session, &id);
                    if !why.is_empty() {
                        plx_base::eventlog::log(&format!("livetv: tune took channel form {c} after: {}", why.join(" | ")));
                    }
                    return Ok(session);
                }
                Err(e) => why.push(e),
            }
        }
        Err(why.join(" | "))
    }

    pub fn livetv_tune(&self, dvr: &str, channel: &str) -> Result<String, String> {
        self.livetv_tune_as(dvr, channel, &session_id())
    }

    /// Tune under the playback session `id`: the grab belongs to that session, and the stream
    /// that plays it must name the same one ([`playback_session_for`]).
    pub fn livetv_tune_as(&self, dvr: &str, channel: &str, id: &str) -> Result<String, String> {
        let path = format!("/livetv/dvrs/{}/channels/{}/tune", enc(dvr), enc(channel));
        // PMS answers a tune without a playback session id with a bare 400 (Plex Web sends
        // `X-Plex-Session-Identifier` on this POST, and the same id as the stream's session).
        let session = format!("X-Plex-Session-Identifier: {id}");
        let reply = self.send(&path, crate::http::Method::Post, &[crate::http::ACCEPT_JSON, &session])
            .ok_or_else(|| format!("no answer (channel {channel})"))?;
        let head = || String::from_utf8_lossy(&reply.body[..reply.body.len().min(160)]).replace('\n', " ");
        if !reply.ok() {
            return Err(format!("HTTP {} (channel {channel}): {}", reply.status, head()));
        }
        parse_tune(&reply.body).ok_or_else(|| format!("no session in the answer (channel {channel}): {}", head()))
    }
}

// ---- Amber: recording (the DVR) --------------------------------------------------------------------
// Shapes measured on the same PMS 1.43.5:
// - `GET /media/subscriptions/template?guid=<show or film guid>` -> `SubscriptionTemplate[0].
//   MediaSubscription[]`: `type` (2 = show, 1 = movie), `targetLibrarySectionID`, `parameters` (an
//   already-encoded query string to send back as it is), and `Setting[]` with each one's `id` and
//   default `value`;
// - `POST /media/subscriptions?...` creates a rule; `DELETE /media/subscriptions/<id>` cancels it;
// - `GET /media/subscriptions` lists the rules, `/media/subscriptions/scheduled` what will record.

/// What the server says a rule for one show or film is made of.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordTemplate {
    pub kind: i64,
    pub section: i64,
    /// Encoded `a=b&c=d`, the server's own, sent back untouched.
    pub parameters: String,
    /// `(setting id, default value)`.
    pub prefs: Vec<(String, String)>,
}

pub fn parse_template(body: &[u8]) -> Option<RecordTemplate> {
    let v = serde_json::from_slice::<Value>(body).ok()?;
    let subs = arr(&v, "/MediaContainer/SubscriptionTemplate/0/MediaSubscription");
    // the one the server marks selected, else the first
    let m = subs.iter().find(|m| m.get("selected").and_then(|x| x.as_bool()) == Some(true)).or_else(|| subs.first())?;
    let parameters = s(m, "parameters");
    if parameters.is_empty() { return None; }
    let prefs = m.get("Setting").and_then(|x| x.as_array()).map_or_else(Vec::new, |a| {
        a.iter().filter_map(|st| {
            let (id, value) = (s(st, "id"), s(st, "value"));
            // an empty value is the server's default; sending it adds nothing
            (!id.is_empty() && !value.is_empty()).then_some((id, value))
        }).collect()
    });
    Some(RecordTemplate { kind: n(m, "type"), section: n(m, "targetLibrarySectionID"), parameters, prefs })
}

/// The POST that creates a rule from its template.
pub fn create_path(t: &RecordTemplate) -> String {
    let mut path = format!(
        "/media/subscriptions?targetLibrarySectionID={}&targetSectionLocationID=-1&type={}&includeGrabs=1&{}",
        t.section, t.kind, t.parameters);
    for (id, value) in &t.prefs {
        path.push_str(&format!("&prefs%5B{}%5D={}", enc(id), enc(value)));
    }
    path
}

/// A recording rule the server holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveRule {
    /// The id `DELETE /media/subscriptions/<id>` takes.
    pub id: String,
    pub title: String,
    /// The show's or film's guid, when the server says it.
    pub guid: String,
}

pub fn parse_rules(body: &[u8]) -> Vec<LiveRule> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    arr(&v, "/MediaContainer/MediaSubscription").iter().filter_map(|m| {
        // `key` is the id, or the path ending in it
        let key = s(m, "key");
        let id = key.rsplit('/').next().unwrap_or("").to_string();
        if id.is_empty() { return None; }
        let target = m.get("Directory").or_else(|| m.get("Video")).unwrap_or(m);
        let title = [s(target, "title"), s(m, "title")].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
        Some(LiveRule { id, title, guid: s(target, "guid") })
    }).collect()
}

/// The id of the rule a create answered with.
pub fn parse_created(body: &[u8]) -> Option<String> {
    parse_rules(body).into_iter().next().map(|r| r.id)
}

impl Client {
    pub fn livetv_rules(&self) -> Option<Vec<LiveRule>> {
        let body = self.body_2xx_bulk("/media/subscriptions", &[crate::http::ACCEPT_JSON])?;
        Some(parse_rules(&body))
    }

    /// Record `guid` (a show's, or a film's): read the server's template for it, create a rule from
    /// it. `Ok` is the new rule's id. `Err` says why not, for the event log and the guide's note.
    /// THIS WRITES to the server.
    pub fn livetv_record(&self, guid: &str) -> Result<String, String> {
        let tpl_path = format!("/media/subscriptions/template?guid={}", enc(guid));
        let body = self.body_2xx_bulk(&tpl_path, &[crate::http::ACCEPT_JSON]).ok_or("no template from the server")?;
        let tpl = parse_template(&body).ok_or("the server's template was empty")?;
        let reply = self.send(&create_path(&tpl), crate::http::Method::Post, &[crate::http::ACCEPT_JSON])
            .ok_or("no answer to the record request")?;
        if !reply.ok() {
            let head = String::from_utf8_lossy(&reply.body[..reply.body.len().min(160)]).replace('\n', " ");
            return Err(format!("HTTP {}: {head}", reply.status));
        }
        Ok(parse_created(&reply.body).unwrap_or_default())
    }

    /// Cancel a rule. THIS WRITES to the server.
    pub fn livetv_cancel(&self, id: &str) -> Result<(), String> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err("not a rule id".into());
        }
        let reply = self.send(&format!("/media/subscriptions/{id}"), crate::http::Method::Delete, &[crate::http::ACCEPT_JSON])
            .ok_or("no answer to the cancel request")?;
        if reply.ok() { Ok(()) } else { Err(format!("HTTP {}", reply.status)) }
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

    #[test]
    fn a_template_becomes_the_create_request() {
        let body = br#"{"MediaContainer":{"size":1,"SubscriptionTemplate":[{"MediaSubscription":[{"parameters":"hints%5Bguid%5D=g&params%5BlibraryType%5D=2","selected":true,"type":2,"targetLibrarySectionID":3,
            "Setting":[{"id":"onlyNewAirings","value":"1"},{"id":"lineupChannel","value":""},{"id":"minVideoQuality","value":"0"}]}]}]}}"#;
        let t = parse_template(body).unwrap();
        assert_eq!((t.kind, t.section), (2, 3));
        assert_eq!(t.prefs, [("onlyNewAirings".to_string(), "1".to_string()), ("minVideoQuality".to_string(), "0".to_string())],
            "an empty value is left out");
        assert_eq!(create_path(&t),
            "/media/subscriptions?targetLibrarySectionID=3&targetSectionLocationID=-1&type=2&includeGrabs=1&hints%5Bguid%5D=g&params%5BlibraryType%5D=2&prefs%5BonlyNewAirings%5D=1&prefs%5BminVideoQuality%5D=0");
    }
    #[test]
    fn an_empty_template_is_none() {
        assert!(parse_template(br#"{"MediaContainer":{"size":0}}"#).is_none());
    }
    #[test]
    fn rules_read_with_their_ids() {
        let body = br#"{"MediaContainer":{"size":1,"MediaSubscription":[{"key":"/media/subscriptions/7","Directory":{"title":"Plebs","guid":"tv.plex.xmltv://show/Plebs"}}]}}"#;
        let r = parse_rules(body);
        assert_eq!(r, [LiveRule { id: "7".into(), title: "Plebs".into(), guid: "tv.plex.xmltv://show/Plebs".into() }]);
        assert!(parse_rules(br#"{"MediaContainer":{"size":0}}"#).is_empty());
    }
    #[test]
    fn a_grid_airing_carries_the_guid_a_rule_is_made_for() {
        let body = br#"{"MediaContainer":{"Metadata":[{"type":"episode","title":"E","grandparentTitle":"Plebs","guid":"tv.plex.xmltv://episode/Plebs/4/5","grandparentGuid":"tv.plex.xmltv://show/Plebs",
            "Media":[{"channelIdentifier":"C1","beginsAt":1,"endsAt":2}]},{"type":"movie","title":"M","guid":"tv.plex.xmltv://movie/M/1999",
            "Media":[{"channelIdentifier":"C1","beginsAt":3,"endsAt":4}]}]}}"#;
        let g = parse_grid(body);
        assert_eq!(g[0].record_guid, "tv.plex.xmltv://show/Plebs");
        assert_eq!(g[0].episode_guid, "tv.plex.xmltv://episode/Plebs/4/5");
        assert_eq!(g[1].record_guid, "tv.plex.xmltv://movie/M/1999");
    }
}
