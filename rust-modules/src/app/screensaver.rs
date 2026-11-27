//! Amber: the screensaver's trigger and its pictures.
//!
//! After the viewer's idle time (`plx_data::layout::screensaver_minutes`, 2 by default, 0 = off)
//! with nothing playing, the screensaver page is pushed over whatever is showing; any key or click
//! closes it. The pictures are the fanart of the films and shows the server holds that are rated M and under
//! (`plx_data::saver::rating_allowed`), read in the background and shuffled, then refreshed hourly.

use plx_machine::machine::{MachineId, NavOp};
use plx_screens::registry::AppArg;
use plx_ui::dispatch::Dispatcher;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Films read from each movie library, at most.
const PER_LIBRARY: i64 = 1000;
/// The most pictures kept.
const MAX_PICTURES: usize = 300;
const REFRESH_S: i64 = 3600;
/// After a read that found nothing, try again after this.
const RETRY_S: i64 = 600;

static LOADING: AtomicBool = AtomicBool::new(false);
static LAST_TRY: AtomicI64 = AtomicI64::new(0);
/// When something last played (a film, a trailer), on the app clock; 0 = never.
static LAST_WATCHING: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn unix_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// A small shuffle (xorshift), seeded from the clock: the order only has to differ run to run.
fn shuffle<T>(items: &mut [T]) {
    let mut x = (unix_s() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..items.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        items.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

/// Read the pictures: every movie library, its films in pages, those rated M and under with fanart.
fn load() {
    let sid = plx_plex::plex::current_server().raw();
    let client = plx_plex::plex::client();
    let Some(sections) = client.sections() else { return };
    let mut found: Vec<plx_data::saver::SaverImage> = Vec::new();
    // films and shows both: a show's fanart is as good as a film's
    for dir in sections.directory.iter().filter(|d| d.kind == "movie" || d.kind == "show") {
        let Ok(key) = dir.key.parse::<i64>() else { continue };
        let filters = vec![("type".to_string(), if dir.kind == "show" { "2" } else { "1" }.to_string())];
        let mut start = 0;
        while start < PER_LIBRARY {
            let q = plx_plex::plex::SectionQuery {
                section_key: key, sort: "", filters: &filters, start, size: 200, include_meta: false,
            };
            let Some(page) = client.section_items_query(&q) else { break };
            let n = page.metadata.len() as i64;
            found.extend(page.metadata.iter()
                .filter(|m| !m.art.is_empty() && plx_data::saver::rating_allowed(&m.content_rating))
                .map(|m| plx_data::saver::SaverImage {
                    sid, art: m.art.clone(), title: m.title.clone(), year: m.year,
                    rating: plx_data::saver::rating_label(&m.content_rating),
                }));
            if n < 200 { break; }
            start += n;
        }
    }
    shuffle(&mut found);
    found.truncate(MAX_PICTURES);
    plx_base::eventlog::log(&format!("screensaver: {} pictures (rated M and under)", found.len()));
    plx_data::saver::set(found);
}

/// The first read waits this long after Amber starts, so it never competes with the start-up.
const START_DELAY_S: i64 = 90;
static FIRST_TICK: AtomicI64 = AtomicI64::new(0);

fn ensure_loaded() {
    let now = unix_s();
    let first = FIRST_TICK.load(Ordering::Relaxed);
    if first == 0 { FIRST_TICK.store(now, Ordering::Relaxed); return; }
    if now - first < START_DELAY_S { return; }
    let have = plx_data::saver::count() > 0;
    let wait = if have { REFRESH_S } else { RETRY_S };
    if LOADING.load(Ordering::Relaxed) || now - LAST_TRY.load(Ordering::Relaxed) < wait { return; }
    LAST_TRY.store(now, Ordering::Relaxed);
    LOADING.store(true, Ordering::Relaxed);
    if std::thread::Builder::new().name("saver-load".into()).spawn(|| { load(); LOADING.store(false, Ordering::Relaxed); }).is_err() {
        LOADING.store(false, Ordering::Relaxed);
    }
}

/// Is `arg` a page the screensaver may cover? A browsing page, never playback or a sign-in.
fn coverable(arg: &AppArg) -> bool {
    matches!(arg, AppArg::Home | AppArg::Library | AppArg::Search | AppArg::Watchlist | AppArg::LiveTv | AppArg::Music
        | AppArg::LibraryCollections { .. } | AppArg::LibraryRecommended { .. } | AppArg::Content(_)
        | AppArg::Settings(_))
}

/// Called every frame with when the viewer last pressed anything.
pub(crate) fn tick(d: &mut Dispatcher<super::bridge::AppHost>, last_input: u32) {
    let minutes = plx_data::layout::screensaver_minutes();
    if minutes == 0 { return; }
    // pictures are read ahead, so the screensaver can start the moment it is due
    ensure_loaded();
    let Some(top) = d.top_arg() else { return };
    // Watching is not idling: while a film plays (the player page) or a trailer shows, the clock
    // restarts, so the picture that follows is not counted from the last key press before it.
    let now = super::clock::now();
    if !coverable(top) || plx_media::player::preview::view().picture {
        LAST_WATCHING.store(now, Ordering::Relaxed);
        return;
    }
    if plx_data::saver::count() == 0 { return; }
    let last_active = if now.wrapping_sub(last_input) < now.wrapping_sub(LAST_WATCHING.load(Ordering::Relaxed)) {
        last_input
    } else {
        LAST_WATCHING.load(Ordering::Relaxed)
    };
    let idle_ms = now.wrapping_sub(last_active);
    // a wrapped clock (negative in effect) reads as huge: not idle
    if idle_ms > u32::MAX / 2 || idle_ms < minutes * 60_000 { return; }
    plx_base::eventlog::log(&format!("screensaver: starting after {minutes} min idle"));
    d.request(MachineId::Nav, NavOp::Push(AppArg::Screensaver));
}
