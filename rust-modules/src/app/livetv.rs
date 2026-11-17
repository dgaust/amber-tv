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
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
/// Moves when the sidebar's question - is there Live TV? - may have a new answer.
static GEN: AtomicU32 = AtomicU32::new(0);

fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Shared::default))
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

/// What the guide page draws: every channel with its airings from an hour ago on.
pub(crate) fn model(now: i64) -> LiveTvModel {
    with(|s| {
        let rows = s.channels.iter().map(|c| LiveRow {
            id: c.id.clone(),
            number: c.number.clone(),
            title: c.title.clone(),
            slots: s.grid.iter().filter(|p| p.channel == c.id && p.ends > now - 3600).map(|p| LiveSlot {
                begins: p.begins,
                ends: p.ends,
                title: p.title.clone(),
                subtitle: p.subtitle.clone(),
                meta: meta_of(p),
                summary: p.summary.clone(),
            }).collect(),
        }).filter(|r| !r.slots.is_empty()).collect();
        LiveTvModel { status: s.status, rows, tuning: s.tuning.clone(), tune_failed: s.tune_failed }
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
                Some(m) => s.ready = Some(m),
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
