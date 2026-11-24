//! Amber: Live TV's data - the server's DVR, its channels and the guide grid - and tuning, all off
//! the frame thread. The guide page reads a snapshot of it each frame (`Bridge::livetv_feed`); a
//! tune that lands is played by the frame loop (`take_ready`).

use plx_plex::plex::livetv::{LiveChannel, LiveDvr, LiveProgramme};
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
    });
    (now / 60).hash(&mut h);
    plx_plex::plex::current_server().raw().hash(&mut h);
    h.finish()
}

/// What the guide page draws: every channel with its airings from an hour ago on.
pub(crate) fn model(now: i64) -> LiveTvModel {
    with(|s| {
        let rows = s.channels.iter().map(|c| LiveRow {
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
            }).collect(),
        }).filter(|r| !r.slots.is_empty()).collect();
        LiveTvModel { status: s.status, rows, tuning: s.tuning.clone(), tune_failed: s.tune_failed,
            sid: plx_plex::plex::current_server().raw() }
    })
}

/// Tune `channel` and, when the server answers with a live session, hand it to the frame loop.
pub(crate) fn tune(channel: String) {
    let Some(dvr) = with(|s| {
        if s.tuning.is_some() { return None; }
        s.tuning = Some(channel.clone());
        s.tune_failed = false;
        s.dvr.clone()
    }) else { return };
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
    let stamp = now / 30 + i64::from(generation()) * 1_000_000_007;
    if LAST.swap(stamp, Ordering::Relaxed) == stamp { return; }
    let sid = plx_plex::plex::current_server();
    let items = with(|s| {
        s.channels.iter().filter_map(|c| {
            let p = s.grid.iter().find(|p| p.channel == c.id && p.begins <= now && now < p.ends)?;
            Some(plx_data::pms::PmsMovie {
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
            })
        }).collect::<Vec<_>>()
    });
    plx_data::pms::set_live_row(items);
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
