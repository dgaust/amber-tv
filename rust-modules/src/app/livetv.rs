//! Amber: Live TV's data - the server's DVR, its channels and the guide grid - and tuning, all off
//! the frame thread. The guide page reads a snapshot of it each frame (`Bridge::livetv_feed`); a
//! tune that lands is played by the frame loop (`take_ready`).

use plx_plex::plex::livetv::{LiveChannel, LiveDvr, LiveProgramme, LiveRule, LiveScheduled};
use plx_screens::livetv::{LiveRow, LiveSlot, LiveStatus, LiveTvModel};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

#[derive(Default)]
struct Shared {
    status: LiveStatus,
    dvr: Option<LiveDvr>,
    channels: Vec<LiveChannel>,
    grid: Vec<LiveProgramme>,
    loading: bool,
    /// Unix seconds of the last load that answered (or failed).
    loaded_at: i64,
    tuning: Option<String>,
    tune_failed: bool,
    ready: Option<plx_data::pms::PmsMovie>,
    /// The guide channel of the live session now playing, and the airing its title was last set
    /// from (`retitle`).
    playing: Option<(String, i64)>,
    /// Amber: the server's recording rules, a passing status line and when it goes, and whether a
    /// record or cancel is in flight.
    rules: Vec<LiveRule>,
    /// Amber: what the server has lined up to record.
    scheduled: Vec<LiveScheduled>,
    note: String,
    note_until: i64,
    recording: bool,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
/// Moves when the sidebar's question - is there Live TV? - may have a new answer.
static GEN: AtomicU32 = AtomicU32::new(0);

fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(from_cache))
}

/// The last guide that answered, kept on disk so a launch knows at once that there is Live TV (the
/// sidebar's entry, Home's On Now row in its place) instead of a few seconds later, when the first
/// load answers and both arrive and shove the page about. The live load still runs at once and
/// replaces it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Cached {
    saved_at: i64,
    dvr: Option<LiveDvr>,
    channels: Vec<LiveChannel>,
    grid: Vec<LiveProgramme>,
}

fn cache_path() -> std::path::PathBuf {
    let dev = std::path::Path::new("/media/developer");
    if dev.is_dir() {
        return dev.join(format!("{}-livetv.json", plx_base::paths::app_id()));
    }
    plx_base::paths::runtime_dir().join("amber-livetv.json")
}

fn from_cache() -> Shared {
    let Some(c) = std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice::<Cached>(&b).ok()) else {
        return Shared::default();
    };
    // a week-old guide is no answer to "is there Live TV here" any more
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    if c.dvr.is_none() || now - c.saved_at > 7 * 86_400 {
        return Shared::default();
    }
    plx_base::eventlog::log(&format!("livetv: guide from the last run — channels={} airings={}", c.channels.len(), c.grid.len()));
    Shared { status: LiveStatus::Ready, dvr: c.dvr, channels: c.channels, grid: c.grid, ..Shared::default() }
}

fn save_cache(now: i64) {
    let bytes = with(|s| serde_json::to_vec(&Cached {
        saved_at: now, dvr: s.dvr.clone(), channels: s.channels.clone(), grid: s.grid.clone(),
    }).ok());
    let Some(bytes) = bytes else { return };
    let target = cache_path();
    let tmp = target.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &target);
    }
}

/// Does the current server have a DVR? (`false` until the first load answers.)
pub(crate) fn available() -> bool {
    with(|s| s.dvr.is_some())
}

pub(crate) fn generation() -> u32 {
    GEN.load(Ordering::Relaxed)
}

/// Load (or refresh) the guide when it is due: at once the first time, then every five minutes
/// while `active` (the guide is on screen), every half hour otherwise; a failure retries after a
/// minute. Cheap when nothing is due.
pub(crate) fn tick(now: i64, active: bool) {
    let due = with(|s| {
        if s.loading { return false; }
        let age = now - s.loaded_at;
        let every = match (s.status, active) {
            (LiveStatus::Failed, _) => 60,
            (_, true) => 300,
            _ => 1800,
        };
        if s.loaded_at == 0 || age >= every {
            s.loading = true;
            true
        } else {
            false
        }
    });
    if !due { return; }
    let spawned = std::thread::Builder::new().name("livetv".into()).spawn(move || {
        let client = plx_plex::plex::client();
        let dvr = client.livetv_dvrs().map(|d| d.into_iter().next());
        let loaded = match dvr {
            None => Err(()),
            Some(None) => Ok((None, Vec::new(), Vec::new())),
            Some(Some(dvr)) => {
                reload_rules(&client);
                let channels = client.livetv_channels(&dvr.epg);
                let grid = client.livetv_grid(&dvr.epg, now - 3600, now + 6 * 3600);
                match (channels, grid) {
                    (Some(c), Some(g)) => Ok((Some(dvr), c, g)),
                    _ => Err(()),
                }
            }
        };
        let had = available();
        let answered = loaded.is_ok();
        with(|s| {
            s.loading = false;
            s.loaded_at = now;
            match loaded {
                Ok((dvr, channels, grid)) => {
                    s.status = if dvr.is_some() { LiveStatus::Ready } else { LiveStatus::None };
                    s.dvr = dvr;
                    s.channels = channels;
                    s.grid = grid;
                }
                Err(()) => {
                    if s.channels.is_empty() { s.status = LiveStatus::Failed; }
                }
            }
        });
        if answered { save_cache(now); }
        if had != available() { GEN.fetch_add(1, Ordering::Relaxed); }
        plx_base::eventlog::log(&format!("livetv: guide loaded — dvr={} channels={} airings={} first={}",
            available(), with(|s| s.channels.len()), with(|s| s.grid.len()),
            with(|s| s.channels.first().map(|c| format!("{}#{}", c.id, c.number)).unwrap_or_default())));
    });
    if spawned.is_err() {
        with(|s| s.loading = false);
    }
}

fn meta_of(p: &LiveProgramme) -> String {
    let mut parts = Vec::new();
    if p.season > 0 || p.episode > 0 { parts.push(format!("S{} E{}", p.season, p.episode)); }
    if p.year > 0 { parts.push(p.year.to_string()); }
    let mins = (p.ends - p.begins) / 60;
    if mins > 0 { parts.push(if mins >= 60 { format!("{}h {}m", mins / 60, mins % 60) } else { format!("{mins}m") }); }
    // `au/M` reads as `M`; a bare country code (`au`, the guide had no rating) reads as nothing
    let rating = p.rating.rsplit('/').next().unwrap_or("").trim();
    let bare_country = rating.len() == 2 && rating.chars().all(|c| c.is_ascii_lowercase());
    if !rating.is_empty() && !bare_country { parts.push(rating.to_string()); }
    parts.join("  \u{b7}  ")
}

/// A cheap fingerprint of everything [`model`] reads: the last load, the tune state and the minute.
/// The guide page rebuilds its snapshot only when this moves. Rebuilding it every frame (every
/// channel against every airing, every title and summary cloned, then compared whole) cost about
/// 8 ms a frame on the TV and held the guide's loop at ~42 Hz with nothing on screen moving.
pub(crate) fn model_stamp(now: i64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    with(|s| {
        s.loaded_at.hash(&mut h);
        (s.status as u8).hash(&mut h);
        s.channels.len().hash(&mut h);
        s.grid.len().hash(&mut h);
        s.tuning.hash(&mut h);
        s.tune_failed.hash(&mut h);
        plx_data::layout::generation().hash(&mut h);
        s.rules.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().hash(&mut h);
        s.scheduled.iter().map(|r| (r.title.as_str(), r.begins)).collect::<Vec<_>>().hash(&mut h);
        (now < s.note_until).hash(&mut h);
        s.note.hash(&mut h);
    });
    (now / 60).hash(&mut h);
    plx_plex::plex::current_server().raw().hash(&mut h);
    h.finish()
}

/// What the guide page draws: every channel with its airings from an hour ago on.
pub(crate) fn model(now: i64) -> LiveTvModel {
    with(|s| {
        let mut rows: Vec<LiveRow> = s.channels.iter().map(|c| LiveRow {
            id: c.id.clone(),
            number: c.number.clone(),
            title: c.title.clone(),
            logo: c.thumb.clone(),
            slots: s.grid.iter().filter(|p| p.channel == c.id && p.ends > now - 3600).map(|p| LiveSlot {
                begins: p.begins,
                ends: p.ends,
                title: p.title.clone(),
                subtitle: p.subtitle.clone(),
                meta: meta_of(p),
                summary: p.summary.clone(),
                art: p.art.clone(),
                poster: p.thumb.clone(),
                record_guid: p.record_guid.clone(),
                episode_guid: p.episode_guid.clone(),
            }).collect(),
        }).filter(|r| !r.slots.is_empty()).collect();
        // the viewer's own order (a stable sort: channels it does not name keep the server's order, after)
        let order = plx_data::layout::get().channel_order;
        if !order.is_empty() { rows.sort_by_key(|r| order.iter().position(|o| *o == r.id).unwrap_or(usize::MAX)); }
        LiveTvModel { status: s.status, rows, tuning: s.tuning.clone(), tune_failed: s.tune_failed,
            sid: plx_plex::plex::current_server().raw(),
            rules: s.rules.iter().map(|r| r.guid.clone()).collect(),
            recs: build_recs(&s.rules, &s.scheduled),
            note: if now < s.note_until { s.note.clone() } else { String::new() } }
    })
}

/// Amber: move the guide channel `id` one place up (`-1`) or down (`1`) among the channels the guide shows,
/// and keep the order.
pub(crate) fn move_channel(id: &str, dir: i32) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let shown: Vec<String> = model(now).rows.into_iter().map(|r| r.id).collect();
    plx_data::layout::update(|l| plx_data::layout::Layout::step(&mut l.channel_order, &shown, id, dir));
}

/// Tune `channel` and, when the server answers with a live session, hand it to the frame loop.
pub(crate) fn tune(channel: String) {
    let Some(dvr) = with(|s| {
        if s.tuning.is_some() { return None; }
        s.tuning = Some(channel.clone());
        s.tune_failed = false;
        s.dvr.clone()
    }) else { return };
    plx_data::layout::note_recent_channel(&channel);
    let title = with(|s| s.channels.iter().find(|c| c.id == channel).map(|c| c.title.clone()).unwrap_or_default());
    // The forms this channel goes by: the guide's id first (what Plex Web sends), then the tuner's.
    let candidates: Vec<String> = with(|s| {
        let mut out: Vec<String> = vec![channel.clone()];
        if let Some(d) = s.dvr.as_ref() {
            for (lineup, device, key) in &d.mappings {
                if *lineup == channel { out.push(device.clone()); out.push(key.clone()); }
            }
        }
        if let Some(c) = s.channels.iter().find(|c| c.id == channel) {
            out.push(c.number.clone());
            out.push(c.grid_key.clone());
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|c| !c.is_empty() && seen.insert(c.clone()));
        out
    });
    // the server is read HERE, on the frame thread: a worker reads no statics
    let sid = plx_plex::plex::current_server();
    let tuned = channel.clone();
    let spawned = std::thread::Builder::new().name("livetv-tune".into()).spawn(move || {
        let client = plx_plex::plex::client();
        let session = client.livetv_tune_any(&dvr.key, &candidates);
        // The session's own record carries the Media/Part the player is built from; a live
        // airing is played as a one-off item (no queue, no resume), addressed by its session path.
        if let Err(why) = &session {
            plx_base::eventlog::log(&format!("livetv: tune refused - {why}"));
        }
        let item = session.as_deref().ok().map(|path| {
            let mut m = client.metadata(path).map(|it| plx_data::pms::parse_item(&it, sid)).unwrap_or_default();
            m.sid = sid;
            m.rk = path.to_string();
            if m.part.is_empty() { m.part = path.to_string(); }
            m.kind = 0;
            m.resume_ms = 0;
            if !title.is_empty() {
                m.title = if m.title.is_empty() { title.clone() } else { format!("{} · {}", title, m.title) };
            }
            m
        });
        plx_base::eventlog::log(&format!("livetv: tune {} -> {}", if item.is_some() { "ok" } else { "failed" },
            item.as_ref().map_or("-".to_string(), |m| format!("session (part {})", if m.part == m.rk { "fallback" } else { "from record" }))));
        with(|s| {
            s.tuning = None;
            match item {
                Some(m) => {
                    s.ready = Some(m);
                    // no airing shown yet: the first `retitle` once it plays names the one on now
                    s.playing = Some((tuned, 0));
                }
                None => s.tune_failed = true,
            }
        });
    });
    if spawned.is_err() {
        with(|s| { s.tuning = None; s.tune_failed = true; });
    }
}

/// Amber: how much a channel's source must have buffered before its start plays, or `None` for a source
/// that needs no hold. A broadcast tuner (a plain channel number: "102") delivers in real time and has no
/// lead to spend, so a pause in its feed freezes the picture; a generated or streamed channel (ErsatzTV: ids
/// like `C2.146.ersatztv.org`) is served well ahead. The channel is the one just tuned (`playing`).
pub(crate) fn start_hold_ms() -> Option<u32> {
    with(|s| s.playing.as_ref().map(|(c, _)| source_hold_ms(c))).flatten()
}

fn source_hold_ms(channel_id: &str) -> Option<u32> {
    let broadcast = !channel_id.is_empty() && channel_id.chars().all(|c| c.is_ascii_digit() || c == '.') && !channel_id.contains("..");
    if broadcast { Some(10_000) } else { None }
}

/// A tuned session ready to play, as the row the player starts from.
pub(crate) fn take_ready() -> Option<plx_data::pms::PmsMovie> {
    with(|s| s.ready.take())
}

/// Amber: while a live channel plays, keep its title on what is airing. A channel runs on from one
/// programme to the next and the session does not tell the player; the guide does. When the airing
/// on now is not the one the title was last set from, the HUD's title becomes "<channel> · <show>"
/// and its context line the episode's title (or the airing's facts). Cheap: a lookup a frame,
/// a write only when the airing changes.
pub(crate) fn retitle(ps: &mut plx_media::route::PlaybackSession, now: i64) {
    let Some((rk, _)) = plx_media::route::timeline_identity() else { return };
    if !rk.starts_with("/livetv/sessions/") { return; }
    let change = with(|s| {
        let (channel, shown) = s.playing.clone()?;
        let p = s.grid.iter().find(|p| p.channel == channel && p.begins <= now && now < p.ends)?;
        if p.begins == shown { return None; }
        s.playing = Some((channel.clone(), p.begins));
        let name = s.channels.iter().find(|c| c.id == channel).map(|c| c.title.clone()).unwrap_or_default();
        let title = if name.is_empty() { p.title.clone() } else { format!("{name} \u{b7} {}", p.title) };
        let ctx = if p.subtitle.is_empty() { meta_of(p) } else { p.subtitle.clone() };
        let ctx = with_next(ctx, s.grid.iter().filter(|n| n.channel == channel && n.begins >= p.ends)
            .min_by_key(|n| n.begins).map(|n| n.title.as_str()));
        Some((title, ctx))
    });
    if let Some((title, ctx)) = change {
        plx_base::eventlog::log(&format!("livetv: now airing - {title}"));
        plx_media::route::retitle(ps, &title, &ctx);
    }
}

/// Amber: what is airing now on each channel, as Home's "On Now" row (`plx_data::pms::set_live_row`).
/// Called every frame; the data layer ignores a list equal to the one it holds.
pub(crate) fn publish_on_now(now: i64) {
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
    let recent_ids = plx_data::layout::get().recent_channels;
    let recent_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        recent_ids.hash(&mut h);
        (h.finish() >> 40) as i64
    };
    let stamp = now / 30 + i64::from(generation()) * 1_000_000_007 + recent_hash * 17_000_000_000_000;
    if LAST.swap(stamp, Ordering::Relaxed) == stamp { return; }
    let sid = plx_plex::plex::current_server();
    // every channel's airing now, with whether it is a film (no episode, a year) or an episode
    let cards: Vec<(bool, plx_data::pms::PmsMovie)> = with(|s| {
        s.channels.iter().filter_map(|c| {
            let p = s.grid.iter().find(|p| p.channel == c.id && p.begins <= now && now < p.ends)?;
            let film = p.season == 0 && p.episode == 0 && p.subtitle.is_empty() && p.year > 0;
            Some((film, plx_data::pms::PmsMovie {
                sid,
                rk: format!("{}{}", plx_data::pms::LIVE_KEY_PREFIX, c.id),
                title: if p.subtitle.is_empty() { p.title.clone() } else { format!("{} \u{b7} {}", p.title, p.subtitle) },
                summary: p.summary.clone(),
                year: p.year,
                rating: p.rating.clone(),
                thumb: if p.thumb.is_empty() { c.thumb.clone() } else { p.thumb.clone() },
                art: p.art.clone(),
                dur_ns: (p.ends - p.begins) * 1_000_000_000,
                ..Default::default()
            }))
        }).collect()
    });
    let mut extra = split_on_now(&cards);
    // Recent Channels: what is on each channel the viewer last tuned, the channel's logo as the card
    let recent: Vec<plx_data::pms::PmsMovie> = with(|s| {
        recent_ids.iter().filter_map(|id| {
            let c = s.channels.iter().find(|c| c.id == *id)?;
            let p = s.grid.iter().find(|p| p.channel == c.id && p.begins <= now && now < p.ends)
                .or_else(|| s.grid.iter().filter(|p| p.channel == c.id && p.begins > now).min_by_key(|p| p.begins))?;
            Some(plx_data::pms::PmsMovie {
                sid,
                rk: format!("{}{}", plx_data::pms::LIVE_KEY_PREFIX, c.id),
                title: if p.subtitle.is_empty() { p.title.clone() } else { format!("{} \u{b7} {}", p.title, p.subtitle) },
                summary: p.summary.clone(),
                year: p.year,
                rating: p.rating.clone(),
                thumb: if c.thumb.is_empty() { p.thumb.clone() } else { c.thumb.clone() },
                art: p.art.clone(),
                dur_ns: (p.ends - p.begins) * 1_000_000_000,
                ..Default::default()
            })
        }).collect()
    });
    if !recent.is_empty() {
        extra.insert(0, (plx_data::pms::LIVE_RECENT_ID.to_string(), "Recent Channels".to_string(), recent));
    }
    plx_data::pms::set_live_row(cards.into_iter().map(|(_, m)| m).collect());
    plx_data::pms::set_live_extra(extra);
}

/// Pure: the "Movies On Now" and "Shows On Now" rows out of what airs now (`film` marks a film).
/// A row needs two cards, and a row that would only repeat On Now whole is left out.
pub(crate) fn split_on_now(cards: &[(bool, plx_data::pms::PmsMovie)]) -> Vec<(String, String, Vec<plx_data::pms::PmsMovie>)> {
    let pick = |want: bool| cards.iter().filter(|(film, _)| *film == want).map(|(_, m)| m.clone()).collect::<Vec<_>>();
    let mut out = Vec::new();
    for (id, title, items) in [
        (plx_data::pms::LIVE_MOVIES_ID, "Movies On Now \u{b7} Live TV", pick(true)),
        (plx_data::pms::LIVE_SHOWS_ID, "Shows On Now \u{b7} Live TV", pick(false)),
    ] {
        if items.len() >= 2 && items.len() < cards.len() {
            out.push((id.to_string(), title.to_string(), items));
        }
    }
    out
}

#[cfg(test)]
mod split_tests {
    use super::split_on_now;
    use plx_data::pms::PmsMovie;
    fn card(film: bool, n: u32) -> (bool, PmsMovie) {
        (film, PmsMovie { rk: format!("livetv:{n}"), title: format!("t{n}"), ..Default::default() })
    }
    #[test]
    fn splits_films_from_shows() {
        let cards = [card(true, 1), card(false, 2), card(true, 3), card(false, 4), card(false, 5)];
        let rows = split_on_now(&cards);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].2.len(), 2, "two films");
        assert_eq!(rows[1].2.len(), 3, "three shows");
    }
    #[test]
    fn a_lone_card_or_a_whole_repeat_is_no_row() {
        assert!(split_on_now(&[card(true, 1), card(false, 2), card(false, 3)]).iter().all(|r| r.2.len() >= 2));
        assert!(split_on_now(&[card(false, 1), card(false, 2)]).is_empty(), "all shows: it would repeat On Now");
        assert!(split_on_now(&[card(true, 1), card(false, 2)]).is_empty(), "one each: no row of one");
    }
}

/// Amber: keep a playing live session alive. PMS ends a live transcode it hears nothing about for
/// about three minutes (measured: the stream closed at 184 s and 188 s and its reopen was a 400).
/// Plex Web pings the transcoder through playback; this does the same every 20 s while the player
/// is on a `/livetv/sessions/` item, and logs every answer.
pub(crate) fn keepalive(now_ms: u64) {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some((rk, session)) = plx_media::route::timeline_identity() else { return };
    if !rk.starts_with("/livetv/sessions/") { return; }
    let last = LAST.load(Ordering::Relaxed);
    if last != 0 && now_ms.saturating_sub(last) < 20_000 { return; }
    if BUSY.swap(true, Ordering::AcqRel) { return; }
    LAST.store(now_ms.max(1), Ordering::Relaxed);
    let encoder = plx_media::route::active_encoder_session();
    let client = plx_plex::plex::client();
    let spawned = std::thread::Builder::new().name("livetv-keepalive".into()).spawn(move || {
        let ping = |id: &str| client.transcode_session_present(id);
        let a = if encoder.is_empty() { None } else { ping(&encoder) };
        let b = if session.is_empty() || session == encoder { None } else { ping(&session) };
        plx_base::eventlog::log(&format!(
            "livetv: keepalive encoder={} -> {:?} session={} -> {:?}",
            short(&encoder), a, short(&session), b));
        BUSY.store(false, Ordering::Release);
    });
    if spawned.is_err() { BUSY.store(false, Ordering::Release); }
}

/// The HUD's context line with what airs next on the channel appended.
fn with_next(ctx: String, next: Option<&str>) -> String {
    match next.filter(|t| !t.is_empty()) {
        Some(t) if ctx.is_empty() => format!("Next: {t}"),
        Some(t) => format!("{ctx}  \u{b7}  Next: {t}"),
        None => ctx,
    }
}

/// Pure: the channel `dir` steps (+1 next, -1 previous) from `cur` in `ids`, wrapping at the ends.
/// `None` when `cur` is not among them or there is nowhere else to go.
pub(crate) fn step_channel(ids: &[String], cur: &str, dir: i32) -> Option<usize> {
    if ids.len() < 2 { return None; }
    let i = ids.iter().position(|c| c == cur)?;
    let n = ids.len() as i32;
    Some((i as i32 + dir.signum()).rem_euclid(n) as usize)
}

/// Is a live session what the player has now?
pub(crate) fn playing_live() -> bool {
    plx_media::route::timeline_identity().is_some_and(|(rk, _)| rk.starts_with("/livetv/sessions/"))
}

/// Amber: CH+ / CH- while a live channel plays - tune the next or previous channel that has an
/// airing now, in the guide's order, wrapping. The tune replaces the playing session when it lands
/// (`take_ready`); the tuning screen names the channel meanwhile. True when a change was started
/// or one is already in flight.
pub(crate) fn change_channel(dir: i32, now: i64) -> bool {
    if !playing_live() { return false; }
    let target = with(|s| {
        if s.tuning.is_some() { return Some(None); }
        let (cur, _) = s.playing.clone()?;
        let ids: Vec<String> = s.channels.iter()
            .filter(|c| s.grid.iter().any(|p| p.channel == c.id && p.begins <= now && now < p.ends))
            .map(|c| c.id.clone()).collect();
        let i = step_channel(&ids, &cur, dir)?;
        Some(Some(ids[i].clone()))
    });
    match target {
        Some(None) => true,
        Some(Some(channel)) => {
            plx_base::eventlog::log(&format!("livetv: channel {} -> {channel}", if dir > 0 { "up" } else { "down" }));
            tune(channel);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod channel_tests {
    use super::{step_channel, with_next};
    fn ids(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }
    #[test]
    fn steps_and_wraps() {
        let c = ids(&["a", "b", "c"]);
        assert_eq!(step_channel(&c, "a", 1), Some(1));
        assert_eq!(step_channel(&c, "c", 1), Some(0));
        assert_eq!(step_channel(&c, "a", -1), Some(2));
    }
    #[test]
    fn nowhere_to_go() {
        assert_eq!(step_channel(&ids(&["a"]), "a", 1), None);
        assert_eq!(step_channel(&ids(&["a", "b"]), "z", 1), None);
    }
    #[test]
    fn next_joins_the_context_line() {
        assert_eq!(with_next("S1 E2".into(), Some("News")), "S1 E2  \u{b7}  Next: News");
        assert_eq!(with_next(String::new(), Some("News")), "Next: News");
        assert_eq!(with_next("S1 E2".into(), None), "S1 E2");
    }
}

/// Pure: the Recordings view's lines - series, episodes and films by rule, then what is scheduled.
pub(crate) fn build_recs(rules: &[LiveRule], scheduled: &[LiveScheduled]) -> Vec<plx_screens::livetv::LiveRec> {
    use plx_screens::livetv::{LiveRec, RecKind};
    let kind = |r: &LiveRule| match r.kind { 4 => RecKind::Episode, 1 => RecKind::Film, _ => RecKind::Series };
    let mut out: Vec<LiveRec> = Vec::new();
    for want in [RecKind::Series, RecKind::Film, RecKind::Episode] {
        out.extend(rules.iter().filter(|r| kind(r) == want).map(|r| LiveRec {
            id: r.id.clone(), kind: want, title: r.title.clone(), detail: r.detail.clone(), begins: 0,
        }));
    }
    out.extend(scheduled.iter().map(|r| LiveRec {
        id: String::new(), kind: RecKind::Scheduled, title: r.title.clone(), detail: r.detail.clone(), begins: r.begins,
    }));
    out
}

/// Read the server's rules and what it has scheduled again.
fn reload_rules(client: &plx_plex::plex::Client) {
    if let Some(rules) = client.livetv_rules() { with(|s| s.rules = rules); }
    if let Some(sched) = client.livetv_scheduled() { with(|s| s.scheduled = sched); }
}

/// Amber: end a rule from the Recordings view. WRITES to the server, off the frame thread.
pub(crate) fn cancel_rule(id: String) {
    if with(|s| std::mem::replace(&mut s.recording, true)) { return; }
    let name = with(|s| s.rules.iter().find(|r| r.id == id).map(|r| r.title.clone())).unwrap_or_default();
    let client = plx_plex::plex::client();
    let spawned = std::thread::Builder::new().name("livetv-record".into()).spawn(move || {
        let note = match client.livetv_cancel(&id) {
            Ok(()) => { plx_base::eventlog::log(&format!("livetv: recording rule {id} cancelled")); format!("Cancelled {name}") }
            Err(why) => { plx_base::eventlog::log(&format!("livetv: cancel of rule {id} failed - {why}")); format!("Couldn't cancel: {why}") }
        };
        reload_rules(&client);
        with(|s| s.recording = false);
        say(&note, 6);
    });
    if spawned.is_err() { with(|s| s.recording = false); }
}

/// What one press of REC does next, from the rules the server holds: `Add(guid)` makes a rule,
/// `Upgrade { add, drop }` makes the series rule and then drops the episode's, `Cancel(id)` ends one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecordStep { Add(String), Upgrade { add: String, drop: String }, Cancel(String) }

/// Pure: an episode records on its own first, a second press makes it the series, a third cancels.
/// A film (`show == episode`) records and cancels. `rules` is `(rule id, guid)`.
pub(crate) fn next_record_step(rules: &[(String, String)], show: &str, episode: &str) -> RecordStep {
    let find = |g: &str| rules.iter().find(|(_, rg)| rg == g).map(|(id, _)| id.clone());
    if let Some(id) = find(show) { return RecordStep::Cancel(id); }
    if show != episode {
        if let Some(id) = find(episode) { return RecordStep::Upgrade { add: show.to_string(), drop: id }; }
    }
    RecordStep::Add(episode.to_string())
}

/// Amber: REC (or INFO) on a guide airing - record it, then its series, then stop. WRITES to the
/// server, off the frame thread; the guide shows the outcome as a passing note and the rules are
/// read again.
pub(crate) fn toggle_record(show: String, episode: String, title: String) {
    if show.is_empty() {
        say("Can't record this one", 5);
        return;
    }
    if with(|s| std::mem::replace(&mut s.recording, true)) { return; }
    let step = with(|s| {
        let rules: Vec<(String, String)> = s.rules.iter().map(|r| (r.id.clone(), r.guid.clone())).collect();
        next_record_step(&rules, &show, if episode.is_empty() { &show } else { &episode })
    });
    let client = plx_plex::plex::client();
    let spawned = std::thread::Builder::new().name("livetv-record".into()).spawn(move || {
        let name = if title.is_empty() { "it".to_string() } else { title };
        let log = |m: &str| plx_base::eventlog::log(&format!("livetv: {m}"));
        let note = match step {
            RecordStep::Cancel(id) => match client.livetv_cancel(&id) {
                Ok(()) => { log(&format!("recording rule {id} cancelled")); format!("Recording of {name} cancelled") }
                Err(why) => { log(&format!("cancel of rule {id} failed - {why}")); format!("Couldn't cancel: {why}") }
            },
            RecordStep::Add(guid) => match client.livetv_record(&guid) {
                Ok(id) => { log(&format!("recording rule {id} created")); if guid == show { format!("Recording {name}") } else { format!("Recording this episode of {name}") } }
                Err(why) => { log(&format!("record failed - {why}")); format!("Couldn't record: {why}") }
            },
            RecordStep::Upgrade { add, drop } => match client.livetv_record(&add) {
                Ok(id) => {
                    log(&format!("series rule {id} created"));
                    // the series covers the episode; the episode's own rule only duplicates it
                    if let Err(why) = client.livetv_cancel(&drop) { log(&format!("episode rule {drop} not dropped - {why}")); }
                    format!("Recording every episode of {name}")
                }
                Err(why) => { log(&format!("series record failed - {why}")); format!("Couldn't record the series: {why}") }
            },
        };
        reload_rules(&client);
        with(|s| s.recording = false);
        say(&note, 6);
    });
    if spawned.is_err() { with(|s| s.recording = false); }
}

#[cfg(test)]
mod record_tests {
    use super::{next_record_step, RecordStep};
    fn r(id: &str, g: &str) -> (String, String) { (id.into(), g.into()) }
    #[test]
    fn the_recordings_list_puts_series_first_and_the_schedule_last() {
        use plx_plex::plex::livetv::{LiveRule, LiveScheduled};
        use plx_screens::livetv::RecKind;
        let rule = |id: &str, kind: i64| LiveRule { id: id.into(), title: format!("t{id}"), kind, ..Default::default() };
        let sched = [LiveScheduled { title: "s".into(), begins: 5, ..Default::default() }];
        let l = super::build_recs(&[rule("1", 4), rule("2", 2), rule("3", 1)], &sched);
        let kinds: Vec<RecKind> = l.iter().map(|x| x.kind).collect();
        assert_eq!(kinds, [RecKind::Series, RecKind::Film, RecKind::Episode, RecKind::Scheduled]);
        assert_eq!(l[3].id, "", "a scheduled recording has no rule id to cancel");
    }
    #[test]
    fn an_episode_steps_to_the_series_and_then_off() {
        assert_eq!(next_record_step(&[], "show", "ep"), RecordStep::Add("ep".into()));
        assert_eq!(next_record_step(&[r("1", "ep")], "show", "ep"), RecordStep::Upgrade { add: "show".into(), drop: "1".into() });
        assert_eq!(next_record_step(&[r("2", "show")], "show", "ep"), RecordStep::Cancel("2".into()));
        // both held (an older state): the series wins and is the one cancelled
        assert_eq!(next_record_step(&[r("1", "ep"), r("2", "show")], "show", "ep"), RecordStep::Cancel("2".into()));
    }
    #[test]
    fn a_film_records_and_cancels() {
        assert_eq!(next_record_step(&[], "film", "film"), RecordStep::Add("film".into()));
        assert_eq!(next_record_step(&[r("3", "film")], "film", "film"), RecordStep::Cancel("3".into()));
    }
}

/// A passing line on the guide for `seconds`.
fn say(note: &str, seconds: i64) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    // the head of a long server answer is plenty
    let note: String = note.chars().take(90).collect();
    with(|s| { s.note = note; s.note_until = now + seconds; });
}

/// Amber: how many re-tunes a failing live stream gets, and over how long, before the player is
/// left to show its error (or leave). A stream that dies at once every time must not loop.
const RETUNE_MAX: usize = 3;
const RETUNE_WINDOW_MS: u64 = 120_000;
const RETUNE_GAP_MS: u64 = 3_000;

/// Pure policy for [`retune`]: may another re-tune start at `now_ms`, given the starts so far
/// (oldest first)? Drops the ones older than the window.
pub(crate) fn retune_allowed(starts: &mut Vec<u64>, now_ms: u64) -> bool {
    starts.retain(|t| now_ms.saturating_sub(*t) < RETUNE_WINDOW_MS);
    if starts.len() >= RETUNE_MAX { return false; }
    if starts.last().is_some_and(|t| now_ms.saturating_sub(*t) < RETUNE_GAP_MS) { return false; }
    starts.push(now_ms);
    true
}

/// Amber: a live stream failed or ended under the player (PMS drops a grab it stops hearing
/// about; the transcoder can also die). Tune the same channel again and let the frame loop play
/// the new session (`take_ready`), instead of leaving the viewer on an error or back at the guide.
/// Returns true while a re-tune is in flight or was just started, so the caller keeps the player
/// where it is; false when the stream is not live, the channel is unknown or the budget is spent.
pub(crate) fn retune(now_ms: u64) -> bool {
    static STARTS: Mutex<Vec<u64>> = Mutex::new(Vec::new());
    let Some((rk, _)) = plx_media::route::timeline_identity() else { return false };
    if !rk.starts_with("/livetv/sessions/") { return false; }
    if with(|s| s.tuning.is_some() || s.ready.is_some()) { return true; }
    let Some(channel) = with(|s| s.playing.as_ref().map(|(c, _)| c.clone())) else { return false };
    let allowed = retune_allowed(&mut STARTS.lock().unwrap_or_else(|e| e.into_inner()), now_ms);
    if !allowed {
        plx_base::eventlog::log("livetv: re-tune budget spent - leaving the failed stream to the player");
        return false;
    }
    plx_base::eventlog::log(&format!("livetv: stream lost - tuning channel {channel} again"));
    tune(channel);
    true
}

#[cfg(test)]
mod retune_tests {
    use super::retune_allowed;
    #[test]
    fn three_in_the_window_then_no_more() {
        let mut v = Vec::new();
        assert!(retune_allowed(&mut v, 10_000));
        assert!(!retune_allowed(&mut v, 11_000), "too soon after the last");
        assert!(retune_allowed(&mut v, 14_000));
        assert!(retune_allowed(&mut v, 18_000));
        assert!(!retune_allowed(&mut v, 22_000), "fourth inside the window");
    }
    #[test]
    fn the_window_forgets() {
        let mut v = Vec::new();
        for t in [0, 4_000, 8_000] { assert!(retune_allowed(&mut v, t)); }
        assert!(retune_allowed(&mut v, 125_000));
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(13)]
}

/// Amber: while a tune is in flight (and until its session is handed to the player), the channel's
/// name and what it is airing, for the tuning screen (`Bridge::draw_chrome`).
pub(crate) fn tuning_label() -> Option<(String, String)> {
    with(|s| {
        let id = s.tuning.clone()?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let channel = s.channels.iter().find(|c| c.id == id).map(|c| c.title.clone()).unwrap_or_default();
        let programme = s.grid.iter().find(|p| p.channel == id && p.begins <= now && now < p.ends)
            .map(|p| if p.subtitle.is_empty() { p.title.clone() } else { format!("{} \u{b7} {}", p.title, p.subtitle) })
            .unwrap_or_default();
        Some((channel, programme))
    })
}
