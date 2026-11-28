//! Amber: music's data and its playback engine (`docs/music-design.md`).
//!
//! - **Data**: the server's music library, loaded off the frame thread into lists the music page reads
//!   as a snapshot each frame (`Bridge::music_feed` -> [`model`]), like the Live TV guide.
//! - **Queue**: what is playing and what follows, with shuffle and repeat.
//! - **Engine**: one native pipeline Load for a whole listening session. Tracks are downloaded
//!   (`Client::music_fetch_part`), decoded to 48 kHz stereo PCM on a worker (`music_decode`) and fed as
//!   one continuous stream, so the next track follows the last with no gap. The pipeline will not output
//!   sound until it has a picture to bind to (M0 spike, `docs/music-design.md` 12), so a black frame is
//!   fed once a second, behind the UI. Everything that touches the sink runs in [`tick`], on the frame
//!   thread.

use plx_plex::plex::music::{MusicHub, MusicItem, MusicKind, MusicSection};
use plx_play_feed::music_decode::{self as decode, Msg};
use plx_play_feed::shared::FEED_SHARED;
use plx_screens::music::{MusicList, MusicModel, MusicStatus, NowPlaying, Repeat};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

const PAGE: usize = 120;
/// Files larger than this are not held in memory.
const MAX_FILE: usize = 260 * 1024 * 1024;
/// The next track is fetched ahead only when it is no bigger than this.
const PREFETCH_MAX: usize = 100 * 1024 * 1024;

#[derive(Default)]
struct Shared {
    status: MusicStatus,
    sections: Vec<MusicSection>,
    section: Option<i64>,
    sid: u16,
    loading: bool,
    loaded_at: i64,
    hubs: Vec<MusicHub>,
    hubs_loading: bool,
    lists: HashMap<String, MusicList>,
    note: String,
    note_until: i64,
    /// Moves when what the page shows may have changed.
    stamp: u64,
    q: Queue,
}

#[derive(Default)]
struct Queue {
    items: Vec<MusicItem>,
    /// The play order: indexes into `items`.
    order: Vec<usize>,
    /// Where in `order` the playing track is.
    pos: usize,
    shuffle: bool,
    repeat: Repeat,
    /// What the viewer asked for, until the engine takes it.
    want_start: bool,
    /// The queue position the viewer hears now.
    shown_pos: usize,
}

static SHARED: Mutex<Option<Shared>> = Mutex::new(None);
/// Moves when the sidebar's question (is there music?) may have a new answer.
static GEN: AtomicU32 = AtomicU32::new(0);
/// Set by the blue key on another page: the music page opens on Now Playing when it next gets its snapshot.
/// An album another page asked to open (by key), until the music page takes it.
static OPEN_ALBUM: Mutex<Option<String>> = Mutex::new(None);
static OPEN_NP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn with<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
    let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Shared::default))
}

fn bump(s: &mut Shared) {
    s.stamp = s.stamp.wrapping_add(1);
}

pub(crate) fn generation() -> u32 {
    GEN.load(Ordering::Relaxed)
}

/// Does the current server have a music library? (`false` until the first load answers.)
pub(crate) fn available() -> bool {
    with(|s| s.status == MusicStatus::Ready)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

// ---- loading ---------------------------------------------------------------------------------------

/// Load the library's sections when due (once, then every ten minutes while the page is open).
pub(crate) fn tick_data(active: bool) {
    let now = now_secs();
    let cur_sid = plx_plex::plex::current_server().raw();
    let due = with(|s| {
        if s.loading {
            return false;
        }
        // another server (or a profile that reaches another): what was loaded belongs to the last one
        if s.loaded_at != 0 && s.sid != cur_sid {
            s.loaded_at = 0;
            s.status = MusicStatus::Loading;
            s.sections.clear();
            s.section = None;
            s.lists.clear();
            s.hubs.clear();
            bump(s);
        }
        // a failed load is tried again soon (the server may simply not have been reachable yet)
        let every = if s.status == MusicStatus::Failed { 60 } else if active { 600 } else { 3600 };
        if s.loaded_at == 0 || now - s.loaded_at >= every {
            s.loading = true;
            true
        } else {
            false
        }
    });
    if !due {
        return;
    }
    let sid = plx_plex::plex::current_server().raw();
    let spawned = std::thread::Builder::new().name("music-load".into()).spawn(move || {
        let client = plx_plex::plex::client();
        let sections = client.music_sections();
        let had = available();
        with(|s| {
            s.loading = false;
            s.loaded_at = now;
            s.sid = sid;
            match sections {
                Some(list) if !list.is_empty() => {
                    s.status = MusicStatus::Ready;
                    if s.section.is_none_or(|k| !list.iter().any(|x| x.key == k)) {
                        s.section = list.first().map(|x| x.key);
                    }
                    s.sections = list;
                }
                Some(_) => {
                    s.status = MusicStatus::None;
                    s.sections.clear();
                }
                None => {
                    if s.sections.is_empty() {
                        s.status = MusicStatus::Failed;
                    }
                }
            }
            bump(s);
        });
        if had != available() {
            GEN.fetch_add(1, Ordering::Relaxed);
        }
        plx_base::eventlog::log(&format!("music: library loaded - {} music section(s)", with(|s| s.sections.len())));
    });
    if spawned.is_err() {
        with(|s| s.loading = false);
    }
}

/// What a list key asks of the server. `artists`, `albums`, `recent` page through the section;
/// `artist:<rk>` / `album:<rk>` / `playlist:<rk>` are one item's children; `popular:<rk>` its top tracks.
fn fetch_list(client: &plx_plex::plex::Client, section: i64, key: &str, start: usize) -> Option<(Vec<MusicItem>, usize)> {
    let page = if key == "artists" {
        client.music_list(section, MusicKind::Artist, start, PAGE, "titleSort")
    } else if key == "albums" {
        client.music_list(section, MusicKind::Album, start, PAGE, "titleSort")
    } else if key == "recent" {
        client.music_list(section, MusicKind::Album, start, PAGE, "addedAt:desc")
    } else if key == "tracks" {
        client.music_list(section, MusicKind::Track, start, PAGE, "titleSort")
    } else if key == "playlists" {
        client.music_playlists()
    } else if let Some(rk) = key.strip_prefix("artist:") {
        client.music_children(rk, false)
    } else if let Some(rk) = key.strip_prefix("album:") {
        client.music_children(rk, false)
    } else if let Some(rk) = key.strip_prefix("playlist:") {
        client.music_children(rk, true)
    } else if let Some(rk) = key.strip_prefix("popular:") {
        client.music_popular(section, rk, 8)
    } else if let Some(rk) = key.strip_prefix("tracks-of:") {
        client.music_artist_tracks(rk)
    } else {
        None
    }?;
    Some((page.items, page.total))
}

/// Make sure list `key` has its first page (or is loading it).
pub(crate) fn request(key: &str) {
    let go = with(|s| {
        let Some(sec) = s.section else { return None };
        let l = s.lists.entry(key.to_owned()).or_default();
        if l.loading || l.loaded {
            return None;
        }
        l.loading = true;
        Some(sec)
    });
    let Some(section) = go else { return };
    let key = key.to_owned();
    let spawned = std::thread::Builder::new().name("music-list".into()).spawn({
        let key = key.clone();
        move || {
            let client = plx_plex::plex::client();
            let got = fetch_list(client, section, &key, 0);
            with(|s| {
                let l = s.lists.entry(key.clone()).or_default();
                l.loading = false;
                match got {
                    Some((items, total)) => {
                        l.loaded = true;
                        l.failed = false;
                        l.total = total.max(items.len());
                        l.items = items;
                    }
                    None => l.failed = true,
                }
                bump(s);
            });
        }
    });
    if spawned.is_err() {
        with(|s| {
            if let Some(l) = s.lists.get_mut(&key) {
                l.loading = false;
                l.failed = true;
            }
        });
    }
}

/// Load the next page of a paged list.
pub(crate) fn more(key: &str) {
    let go = with(|s| {
        let sec = s.section?;
        let l = s.lists.get_mut(key)?;
        if l.loading || !l.loaded || l.items.len() >= l.total {
            return None;
        }
        l.loading = true;
        Some((sec, l.items.len()))
    });
    let Some((section, start)) = go else { return };
    let key = key.to_owned();
    let _ = std::thread::Builder::new().name("music-more".into()).spawn(move || {
        let client = plx_plex::plex::client();
        let got = fetch_list(client, section, &key, start);
        with(|s| {
            if let Some(l) = s.lists.get_mut(&key) {
                l.loading = false;
                if let Some((items, total)) = got {
                    l.total = total.max(l.items.len() + items.len());
                    l.items.extend(items);
                }
            }
            bump(s);
        });
    });
}

/// The library's shelves (Recently Played, Recently Added, ...).
pub(crate) fn request_hubs() {
    let go = with(|s| {
        if s.hubs_loading || !s.hubs.is_empty() {
            return None;
        }
        s.hubs_loading = true;
        s.section
    });
    let Some(section) = go else { return };
    let _ = std::thread::Builder::new().name("music-hubs".into()).spawn(move || {
        let client = plx_plex::plex::client();
        let hubs = client.music_hubs(section);
        with(|s| {
            s.hubs_loading = false;
            if let Some(h) = hubs {
                s.hubs = h;
            }
            bump(s);
        });
    });
}

// ---- the page's snapshot ----------------------------------------------------------------------------

pub(crate) fn model_stamp() -> u64 {
    // the position moves a few times a second while playing; the page asks again every frame it is up
    with(|s| s.stamp) ^ ENGINE.with(|e| e.borrow().stamp()).wrapping_mul(0x9e37_79b9)
}

pub(crate) fn model() -> MusicModel {
    let (heard, starting) = ENGINE.with(|e| {
        let e = e.borrow();
        (e.now_playing(), e.starting())
    });
    let anim = crate::app::clock::now() / 300;
    with(|s| {
        // before the first sound: the track that is about to play, and what it is waiting for
        let now = heard.or_else(|| {
            if starting.is_none() && !s.q.want_start {
                return None;
            }
            let (status, progress) = starting.unwrap_or(("Getting ready".to_owned(), -1.0));
            let item = s.q.items.get(*s.q.order.get(s.q.pos)?)?.clone();
            Some(NowPlaying { duration_ms: item.duration_ms, item, pos_ms: 0, playing: false, buffering: true, status, progress, anim })
        });
        build_model(s, now)
    })
}

fn build_model(s: &mut Shared, now: Option<NowPlaying>) -> MusicModel {
    {
        MusicModel {
        status: s.status,
        sid: s.sid,
        section_title: s.sections.iter().find(|x| Some(x.key) == s.section).map(|x| x.title.clone()).unwrap_or_default(),
        hubs: s.hubs.clone(),
        lists: s.lists.clone(),
        now,
        queue: s.q.order.iter().filter_map(|&i| s.q.items.get(i).cloned()).collect(),
        qpos: s.q.shown_pos,
        shuffle: s.q.shuffle,
        repeat: s.q.repeat,
        note: if now_secs() < s.note_until { s.note.clone() } else { String::new() },
        open_now_playing: OPEN_NP.swap(false, Ordering::AcqRel),
        open_album: OPEN_ALBUM.lock().ok().and_then(|mut o| o.take()).and_then(|rk| {
            s.lists.values().flat_map(|l| l.items.iter()).find(|i| i.kind == MusicKind::Album && i.rk == rk).cloned()
        }),
        }
    }
}

fn note(s: &mut Shared, text: &str) {
    s.note = text.to_owned();
    s.note_until = now_secs() + 4;
    bump(s);
}

// ---- the queue ---------------------------------------------------------------------------------------

/// A deterministic shuffle seed that differs between plays.
fn shuffled(n: usize, keep_first: Option<usize>) -> Vec<usize> {
    let mut v: Vec<usize> = (0..n).collect();
    let mut x = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.subsec_nanos()) as u64) | 1;
    for i in (1..n).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
    if let Some(first) = keep_first {
        if let Some(at) = v.iter().position(|&i| i == first) {
            v.swap(0, at);
        }
    }
    v
}

/// Play `items` from `start` (replacing the queue).
pub(crate) fn play(items: Vec<MusicItem>, start: usize, shuffle: bool) {
    let tracks: Vec<MusicItem> = items.into_iter().filter(|i| i.kind == MusicKind::Track && !i.part_key.is_empty()).collect();
    if tracks.is_empty() {
        with(|s| note(s, "Nothing to play here."));
        return;
    }
    let start = start.min(tracks.len() - 1);
    with(|s| {
        let n = tracks.len();
        s.q.items = tracks;
        s.q.shuffle = shuffle;
        s.q.order = if shuffle { shuffled(n, Some(start)) } else { (0..n).collect() };
        s.q.pos = if shuffle { 0 } else { start };
        s.q.want_start = true;
        bump(s);
    });
}

/// Items that are not tracks (an album, an artist) are expanded by the page before `play`.
pub(crate) fn play_next(item: MusicItem) {
    with(|s| {
        if item.part_key.is_empty() {
            return;
        }
        s.q.items.push(item);
        let at = s.q.items.len() - 1;
        let pos = (s.q.pos + 1).min(s.q.order.len());
        s.q.order.insert(pos, at);
        if s.q.order.len() == 1 {
            s.q.want_start = true;
        }
        note(s, "Playing next");
    });
}

pub(crate) fn add_to_queue(item: MusicItem) {
    with(|s| {
        if item.part_key.is_empty() {
            return;
        }
        s.q.items.push(item);
        let at = s.q.items.len() - 1;
        s.q.order.push(at);
        if s.q.order.len() == 1 {
            s.q.want_start = true;
        }
        note(s, "Added to the queue");
    });
}

pub(crate) fn toggle_shuffle() {
    with(|s| {
        s.q.shuffle = !s.q.shuffle;
        let cur = s.q.order.get(s.q.pos).copied();
        let n = s.q.items.len();
        if s.q.shuffle {
            s.q.order = shuffled(n, cur);
            s.q.pos = 0;
        } else {
            s.q.order = (0..n).collect();
            s.q.pos = cur.unwrap_or(0);
        }
        bump(s);
    });
    ENGINE.with(|e| e.borrow_mut().queue_changed = true);
}

pub(crate) fn cycle_repeat() {
    with(|s| {
        s.q.repeat = match s.q.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
        bump(s);
    });
}

/// Rate a track or album, `half_stars` 0..10, off the frame thread.
pub(crate) fn rate(rk: String, half_stars: i32) {
    with(|s| {
        for l in s.lists.values_mut() {
            for i in l.items.iter_mut().filter(|i| i.rk == rk) {
                i.user_rating = if half_stars > 0 { half_stars as f32 } else { -1.0 };
            }
        }
        for i in s.q.items.iter_mut().filter(|i| i.rk == rk) {
            i.user_rating = if half_stars > 0 { half_stars as f32 } else { -1.0 };
        }
        bump(s);
    });
    let _ = std::thread::Builder::new().name("music-rate".into()).spawn(move || {
        let ok = plx_plex::plex::client().music_rate(&rk, half_stars);
        if !ok {
            with(|s| note(s, "The rating was not saved."));
        }
    });
}

// ---- the engine -------------------------------------------------------------------------------------

/// One black 320x180 H.264 picture (Annex B with its parameter sets): the picture the pipeline needs to
/// bind its output to.
const BLACK_FRAME: &[u8] = include_bytes!("music_black.h264");

const ES_VIDEO: i32 = 1;
const ES_AUDIO: i32 = 2;
const PREROLL_NS: i64 = 700_000_000;
const LEAD_NS: i64 = 2_000_000_000;

struct TrackRun {
    dec: Option<decode::Running>,
    /// Where in the pipeline's timeline this track's audio begins (ns).
    first_pts: i64,
    /// The content time of the first access unit that arrived (ns): a seek lands on the frame before its
    /// target, so the timeline is measured from the unit actually decoded, not from the target.
    base_ns: Option<i64>,
    last_end_pts: i64,
    /// The gain the viewer's loudness levelling gives this track (1.0 = none) and its crossfade, in ms.
    level: f32,
    xf_ms: i64,
}

/// A crossfade in progress: the next track, decoding alongside the end of this one.
struct Fade {
    run: TrackRun,
    pos: usize,
    total: i64,
    done: i64,
}

/// What is on the pipeline's timeline: a track from its first pts.
struct Slot {
    first_pts: i64,
    offset_ms: i64,
    item: MusicItem,
    duration_ms: i64,
    order_pos: usize,
}

enum File {
    None,
    /// A track's file arriving in chunks (the decoder reads it as it grows).
    Stream { rk: String, stream: Arc<decode::Stream> },
}

/// Download `key` into `stream` in ranged chunks, growing from half a megabyte to eight.
fn download(key: String, total: usize, stream: Arc<decode::Stream>) {
    let client = plx_plex::plex::client();
    let mut at = 0usize;
    let mut chunk = 512 * 1024usize;
    loop {
        // nobody is reading it any more (the track was skipped): stop fetching
        if Arc::strong_count(&stream) == 1 {
            return;
        }
        if total > 0 && at >= total {
            break;
        }
        let mut to = at + chunk - 1;
        if total > 0 {
            to = to.min(total - 1);
        }
        match client.music_fetch_range(&key, at, to) {
            Some(bytes) if !bytes.is_empty() => {
                let asked = to - at + 1;
                // a server that ignored the range sent the whole file
                if at == 0 && bytes.len() > asked && (total == 0 || bytes.len() >= total) {
                    stream.set_all(bytes);
                    return;
                }
                at += bytes.len();
                let short = bytes.len() < asked;
                stream.push(&bytes);
                if total == 0 && short {
                    break;
                }
            }
            _ => {
                stream.fail();
                return;
            }
        }
        chunk = (chunk * 2).min(8 * 1024 * 1024);
    }
    stream.finish();
}

enum Boot {
    Off,
    Loading { rv: Arc<AtomicI32>, at: u32 },
    Live,
}

struct Engine {
    boot: Boot,
    epoch: u32,
    bound: bool,
    video_sent: bool,
    played: bool,
    paused: bool,
    clock_ns: i64,
    last_tick: u32,
    next_audio_pts: i64,
    next_video_pts: i64,
    cur: Option<TrackRun>,
    /// The file of the track that is starting: its download shows on screen while the viewer waits.
    cur_stream: Option<Arc<decode::Stream>>,
    fade: Option<Fade>,
    slots: VecDeque<Slot>,
    playing: Option<Slot>,
    pending_au: Option<decode::Au>,
    file: File,
    queue_changed: bool,
    last_report: u32,
    reported_rk: String,
    error_streak: u32,
    buffering: bool,
    // what the viewer asked for since the last tick
    pending_pause: Option<bool>,
    skip: Option<i32>,
    seek_to_ms: Option<i64>,
    stop_asked: bool,
    jump: Option<usize>,
    /// The end of the last track is on the timeline; release once the clock passes it.
    drained: bool,
    /// Feeds the pipeline answered with an error in a row (it is gone: the app was backgrounded, say).
    feed_errors: u32,
    /// Decode this queue position (from this many ms) as soon as its file is here.
    restart_at: Option<(usize, i64)>,
    /// The track that follows the one decoded, once it is time to start it.
    restart_next_pos: Option<usize>,
}

impl Default for Engine {
    fn default() -> Self {
        Engine {
            boot: Boot::Off,
            epoch: 0,
            bound: false,
            video_sent: false,
            played: false,
            paused: false,
            clock_ns: 0,
            last_tick: 0,
            next_audio_pts: 0,
            next_video_pts: 0,
            cur: None,
            cur_stream: None,
            fade: None,
            slots: VecDeque::new(),
            playing: None,
            pending_au: None,
            file: File::None,
            queue_changed: false,
            last_report: 0,
            reported_rk: String::new(),
            error_streak: 0,
            buffering: false,
            pending_pause: None,
            skip: None,
            seek_to_ms: None,
            stop_asked: false,
            jump: None,
            drained: false,
            feed_errors: 0,
            restart_at: None,
            restart_next_pos: None,
        }
    }
}

thread_local! {
    static ENGINE: std::cell::RefCell<Engine> = std::cell::RefCell::new(Engine::default());
}

/// The gain loudness levelling gives a track: the server's track gain (a negative dB figure is a quieter
/// track), never a boost, 1.0 when levelling is off or the server sent none.
fn level_for(item: &MusicItem) -> f32 {
    if !plx_data::layout::music_levelling() {
        return 1.0;
    }
    match item.gain.or(item.album_gain) {
        Some(db) if db < 0.0 => 10f32.powf(db / 20.0).clamp(0.05, 1.0),
        _ => 1.0,
    }
}

/// `pcm` (16-bit little-endian) scaled by `gain`.
fn scale(pcm: &mut [u8], gain: f32) {
    for s in pcm.chunks_exact_mut(2) {
        let v = i16::from_le_bytes([s[0], s[1]]) as f32 * gain;
        s.copy_from_slice(&(v.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
    }
}

/// `a` faded out and `b` faded in, by how far through the crossfade it is (`r` 0..1), summed into `a`.
fn crossfade(a: &mut [u8], b: &[u8], r: f32) {
    let r = r.clamp(0.0, 1.0);
    for (x, y) in a.chunks_exact_mut(2).zip(b.chunks_exact(2)) {
        let v = i16::from_le_bytes([x[0], x[1]]) as f32 * (1.0 - r) + i16::from_le_bytes([y[0], y[1]]) as f32 * r;
        x.copy_from_slice(&(v.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
    }
}

/// The viewer's intents, applied on the next [`tick`].
pub(crate) fn toggle_pause() {
    ENGINE.with(|e| {
        let mut e = e.borrow_mut();
        if e.active() && e.played {
            let want = !e.paused;
            e.pending_pause = Some(want);
        }
    });
}

pub(crate) fn next_track() {
    ENGINE.with(|e| e.borrow_mut().skip = Some(1));
}

pub(crate) fn previous_track() {
    ENGINE.with(|e| e.borrow_mut().skip = Some(-1));
}

pub(crate) fn seek_to(ms: i64) {
    ENGINE.with(|e| e.borrow_mut().seek_to_ms = Some(ms.max(0)));
}

pub(crate) fn stop() {
    ENGINE.with(|e| e.borrow_mut().stop_asked = true);
}

/// Open album `rk` on the Music page (a card on Home asks).
pub(crate) fn request_album(rk: &str) {
    if let Ok(mut o) = OPEN_ALBUM.lock() {
        *o = Some(rk.to_owned());
    }
    with(bump);
}

/// Home's "Recently Added Albums" row: the library's newest albums, once they are here. Called every frame.
pub(crate) fn publish_home_row() {
    let (ready, cards) = with(|s| {
        if s.status != MusicStatus::Ready || s.section.is_none() {
            return (false, Vec::new());
        }
        let sid = plx_plex::plex::current_server();
        let cards = s.lists.get("recent").map(|l| {
            l.items.iter().take(16).filter(|a| !a.cover().is_empty()).map(|a| plx_data::pms::PmsMovie {
                sid,
                rk: format!("{}{}", plx_data::pms::MUSIC_KEY_PREFIX, a.rk),
                title: a.title.clone(),
                summary: a.parent_title.clone(),
                year: a.year,
                thumb: a.cover().to_owned(),
                ..Default::default()
            }).collect::<Vec<_>>()
        });
        (true, cards.unwrap_or_default())
    });
    if ready {
        request("recent");
    }
    plx_data::pms::set_music_row(cards);
}

/// What the pill beside the clock says while music plays, and whether it is playing (not paused).
pub(crate) fn now_label() -> Option<(String, bool)> {
    ENGINE.with(|e| {
        let e = e.borrow();
        let p = e.playing.as_ref()?;
        let artist = p.item.artist();
        let line = if artist.is_empty() { p.item.title.clone() } else { format!("{} \u{b7} {}", p.item.title, artist) };
        Some((line, e.played && !e.paused))
    })
}

pub(crate) fn request_now_playing() {
    OPEN_NP.store(true, Ordering::Release);
    with(bump);
}

/// Is there a music session (playing or paused)? The global media keys ask.
pub(crate) fn has_session() -> bool {
    ENGINE.with(|e| e.borrow().playing.is_some())
}

/// A film or channel is about to start: give the pipeline back first.
pub(crate) fn stop_for_video(mt: &plx_base::task::MainThread) {
    ENGINE.with(|e| {
        let mut e = e.borrow_mut();
        if e.active() {
            e.release(mt, "a video is starting");
        }
    });
}

impl Engine {
    /// While a track is on its way (no slot is playing yet): what the viewer is waiting for, and how far it is.
    fn starting(&self) -> Option<(String, f32)> {
        if self.playing.is_some() || !self.active() {
            return None;
        }
        Some(match &self.boot {
            Boot::Loading { .. } => ("Starting the audio output".to_owned(), -1.0),
            _ => match &self.cur_stream {
                Some(st) if st.is_failed() => ("The download failed".to_owned(), -1.0),
                Some(st) if !st.is_complete() && st.total > 0 => {
                    let got = st.downloaded();
                    (format!("Downloading {} of {} MB", got / 1_000_000, st.total / 1_000_000), got as f32 / st.total as f32)
                }
                Some(_) => ("Getting ready to play".to_owned(), -1.0),
                None => ("Fetching the track".to_owned(), -1.0),
            },
        })
    }

    fn stamp(&self) -> u64 {
        let wait = self.starting().map_or(0u64, |(s, p)| {
            (crate::app::clock::now() / 250) as u64 ^ ((p.max(0.0) * 1000.0) as u64) << 20 ^ (s.len() as u64) << 50
        });
        let pos = self.position_ms().unwrap_or(0) / 250;
        wait ^ (pos as u64)
            ^ ((self.paused as u64) << 40)
            ^ ((self.playing.is_some() as u64) << 41)
            ^ ((self.buffering as u64) << 42)
            ^ ((self.playing.as_ref().map_or(0, |p| p.first_pts as u64)) << 8)
    }

    fn active(&self) -> bool {
        !matches!(self.boot, Boot::Off)
    }

    fn position_ms(&self) -> Option<i64> {
        let p = self.playing.as_ref()?;
        Some(((self.clock_ns - p.first_pts) / 1_000_000).max(0) + p.offset_ms)
    }

    fn now_playing(&self) -> Option<NowPlaying> {
        let p = self.playing.as_ref()?;
        Some(NowPlaying {
            item: p.item.clone(),
            pos_ms: self.position_ms().unwrap_or(0).min(p.duration_ms.max(0)),
            duration_ms: p.duration_ms,
            playing: self.played && !self.paused,
            buffering: self.buffering,
            status: String::new(),
            progress: -1.0,
            anim: 0,
        })
    }

    fn drop_tracks(&mut self) {
        if let Some(mut c) = self.cur.take() {
            if let Some(d) = c.dec.take() {
                d.stop();
            }
        }
        if let Some(mut f) = self.fade.take() {
            if let Some(d) = f.run.dec.take() {
                d.stop();
            }
        }
        self.pending_au = None;
        self.slots.clear();
    }

    /// Give the pipeline back: unload, then the audited release tail. Idempotent.
    fn release(&mut self, mt: &plx_base::task::MainThread, why: &str) {
        use plx_platform::tv::sink::{self, UnloadTicket};
        if !self.active() {
            return;
        }
        plx_base::eventlog::log(&format!("music: releasing the pipeline ({why})"));
        self.drop_tracks();
        self.report("stopped");
        if matches!(self.boot, Boot::Live) {
            let s = sink::installed();
            unsafe { s.unload(&UnloadTicket::issue(mt)) };
            struct Port;
            impl plx_play_feed::native_release::NativeReleasePort for Port {
                fn callback_gate_retire(&self, mt: &plx_base::task::MainThread) -> bool {
                    unsafe { sink::installed().callback_gate_retire(mt) != 0 }
                }
                fn callback_intercepts(&self, mt: &plx_base::task::MainThread) -> u32 {
                    unsafe { sink::installed().callback_intercepts(mt) }
                }
                fn plane_unload(&self, mt: &plx_base::task::MainThread) {
                    unsafe { sink::installed().plane_unload(mt) };
                }
                fn destroy(&self, mt: &plx_base::task::MainThread) -> bool {
                    unsafe { sink::installed().destroy(mt) != 0 }
                }
                fn quarantine(&self, mt: &plx_base::task::MainThread) {
                    unsafe { sink::installed().quarantine(mt) };
                }
            }
            plx_play_feed::native_release::finish_unloaded_object(
                &Port,
                &FEED_SHARED,
                mt,
                self.epoch,
                || self.bound,
                |_| {},
                |_| {},
            );
        }
        let keep = (self.pending_pause, self.skip);
        let _ = keep;
        *self = Engine::default();
        plx_media::player::preview::set_music_holds_output(false);
        with(bump);
    }

    /// Tell the server what the viewer is doing with the track (off the frame thread).
    fn report(&mut self, state: &str) {
        let Some(p) = self.playing.as_ref() else { return };
        let (rk, key, dur) = (p.item.rk.clone(), format!("/library/metadata/{}", p.item.rk), p.duration_ms);
        let time = self.position_ms().unwrap_or(0);
        let state = state.to_owned();
        let _ = std::thread::Builder::new().name("music-timeline".into()).spawn(move || {
            plx_plex::plex::client().music_timeline(&rk, &key, &state, time, dur);
        });
    }

    fn begin_session(&mut self, now: u32) {
        let Some(epoch) = FEED_SHARED.begin_native_session() else {
            with(|s| note(s, "Another video is using the output."));
            return;
        };
        let payload = plx_play_feed::native_executor::with_app_id(
            &plx_play_feed::PAYLOAD_AV
                .replace(r#""codec":{"video":"H264","audio":"AC3"}"#, r#""codec":{"video":"H264","audio":"PCM"}"#)
                .replace(
                    r#""esInfo":"#,
                    r#""pcmInfo":{"bitsPerSample":16,"sampleRate":48,"channelMode":"stereo"},"esInfo":"#,
                ),
        );
        let rv = Arc::new(AtomicI32::new(i32::MIN));
        let rv2 = rv.clone();
        let spawned = std::thread::Builder::new().name("music-load".into()).spawn(move || {
            let Ok(c) = std::ffi::CString::new(payload) else {
                rv2.store(0, Ordering::SeqCst);
                return;
            };
            let ok = unsafe { plx_platform::tv::sink::installed().load(c.as_ptr(), epoch) };
            FEED_SHARED.mark_native_load_returned(epoch, None);
            if ok == 0 {
                FEED_SHARED.publish_native_load_failure(epoch);
            }
            rv2.store(ok, Ordering::SeqCst);
        });
        if spawned.is_err() {
            return;
        }
        *self = Engine { boot: Boot::Loading { rv, at: now }, epoch, last_tick: now, ..Engine::default() };
        plx_media::player::preview::set_music_holds_output(true);
        plx_base::eventlog::log(&format!("music: starting the pipeline, epoch {epoch}"));
    }

    /// The queue's entry `pos` in play order: (index into items, the item).
    fn entry(pos: usize) -> Option<(usize, MusicItem)> {
        with(|s| {
            let i = *s.q.order.get(pos)?;
            Some((i, s.q.items.get(i)?.clone()))
        })
    }

    /// Where the queue goes after `pos`: `auto` is the end of a track (repeat-one stays), else a press.
    fn step_pos(pos: usize, dir: i32, auto: bool) -> Option<usize> {
        with(|s| {
            let n = s.q.order.len();
            if n == 0 {
                return None;
            }
            if auto && s.q.repeat == Repeat::One {
                return Some(pos);
            }
            if dir >= 0 {
                if pos + 1 < n {
                    Some(pos + 1)
                } else if s.q.repeat == Repeat::All {
                    Some(0)
                } else {
                    None
                }
            } else if pos > 0 {
                Some(pos - 1)
            } else if s.q.repeat == Repeat::All {
                Some(n - 1)
            } else {
                Some(0)
            }
        })
    }

    fn start_fetch(&mut self, item: &MusicItem) {
        let stream = Arc::new(decode::Stream::new(item.part_size.max(0) as usize));
        let (s2, key, total) = (stream.clone(), item.part_key.clone(), item.part_size.max(0) as usize);
        let spawned = std::thread::Builder::new().name("music-fetch".into()).spawn(move || download(key, total, s2));
        if spawned.is_ok() {
            self.file = File::Stream { rk: item.rk.clone(), stream };
        } else {
            stream.fail();
            self.file = File::None;
        }
    }

    /// The file for `item`, arriving or here; starts the download if none is under way for it.
    fn file_for(&mut self, item: &MusicItem) -> Option<Arc<decode::Stream>> {
        if !matches!(&self.file, File::Stream { rk, .. } if *rk == item.rk) {
            self.start_fetch(item);
        }
        match std::mem::replace(&mut self.file, File::None) {
            File::Stream { stream, .. } => Some(stream),
            File::None => None,
        }
    }

    /// Begin decoding the track at queue position `pos`, from `offset_ms`. False while its file is coming.
    fn start_track(&mut self, pos: usize, offset_ms: i64) -> bool {
        let Some((_, item)) = Self::entry(pos) else { return false };
        if item.part_size as usize > MAX_FILE {
            // too large to hold in memory: skip it
            plx_base::eventlog::log(&format!("music: \"{}\" is too large to play ({} MB)", item.title, item.part_size / 1_000_000));
            with(|s| note(s, "A track was skipped: its file is too large."));
            self.error_streak += 1;
            self.restart_next_pos = None;
            match Self::step_pos(pos, 1, false) {
                Some(np) if np != pos && self.error_streak < 3 => self.restart_at = Some((np, 0)),
                _ => {
                    self.restart_at = None;
                    self.drained = true;
                }
            }
            return false;
        }
        let Some(data) = self.file_for(&item) else {
            self.buffering = true;
            return false;
        };
        self.buffering = false;
        self.cur_stream = Some(data.clone());
        let dec = decode::spawn(data, offset_ms);
        plx_base::eventlog::log(&format!("music: decoding \"{}\" from {} ms", item.title, offset_ms));
        let first = self.next_audio_pts;
        self.slots.push_back(Slot { first_pts: first, offset_ms, item: item.clone(), duration_ms: item.duration_ms, order_pos: pos });
        self.cur = Some(TrackRun { dec: Some(dec), first_pts: first, base_ns: None, last_end_pts: first, level: level_for(&item), xf_ms: i64::from(plx_data::layout::music_crossfade()) * 1000 });
        with(|s| {
            s.q.pos = pos;
            bump(s);
        });
        true
    }

    /// Flush the pipeline and play the queue from `pos` at `offset_ms`.
    fn restart(&mut self, mt: &plx_base::task::MainThread, pos: usize, offset_ms: i64) {
        let sink = plx_platform::tv::sink::installed();
        self.drop_tracks();
        unsafe {
            sink.pause(mt);
            sink.plane_pause(mt);
            sink.flush(mt);
        }
        self.clock_ns = 0;
        self.next_audio_pts = 0;
        self.next_video_pts = 0;
        self.played = false;
        self.paused = false;
        self.drained = false;
        self.playing = None;
        with(|s| s.q.pos = pos);
        self.restart_at = Some((pos, offset_ms));
    }
}

// ---- the frame tick ---------------------------------------------------------------------------------

/// Called once per loop iteration, on the frame thread, with the frame clock (ms).
pub(crate) fn tick(app: &mut crate::app::App, now: u32) {
    ENGINE.with(|e| e.borrow_mut().step(app.adapters.player.mt(), now));
}

impl Engine {
    fn step(&mut self, mt: &plx_base::task::MainThread, now: u32) {
        use plx_platform::tv::sink;
        let sink = sink::installed();
        let dt_ns = if self.last_tick == 0 { 0 } else { i64::from(now.wrapping_sub(self.last_tick).min(250)) * 1_000_000 };
        self.last_tick = now;

        if self.stop_asked {
            self.stop_asked = false;
            with(|s| s.q.want_start = false);
            self.release(mt, "the viewer stopped it");
            return;
        }

        // a new queue to play (or the shuffle toggled): start, or restart in place
        let want = with(|s| std::mem::take(&mut s.q.want_start));
        if want {
            let pos = with(|s| s.q.pos);
            match self.boot {
                Boot::Off => {
                    self.begin_session(now);
                    self.restart_at = Some((pos, 0));
                    return;
                }
                Boot::Loading { .. } => {
                    self.restart_at = Some((pos, 0));
                }
                Boot::Live => self.restart(mt, pos, 0),
            }
        }
        if self.queue_changed {
            self.queue_changed = false;
            // the order changed under the playing track: carry on from the one that plays now
            if let (Boot::Live, Some(p)) = (&self.boot, self.playing.as_ref()) {
                let rk = p.item.rk.clone();
                let new_pos = with(|s| s.q.order.iter().position(|&i| s.q.items.get(i).is_some_and(|t| t.rk == rk)));
                if let Some(np) = new_pos {
                    with(|s| s.q.pos = np);
                    // what was decoded ahead follows the old order: drop it and decode the new next
                    if let Some(mut c) = self.cur.take() {
                        if let Some(d) = c.dec.take() {
                            d.stop();
                        }
                        self.slots.clear();
                        self.pending_au = None;
                        self.next_audio_pts = self.next_audio_pts.max(c.last_end_pts);
                        self.restart_next_pos = Self::step_pos(np, 1, false).filter(|&n| n != np);
                    }
                }
            }
        }

        match &self.boot {
            Boot::Off => return,
            Boot::Loading { rv, at } => {
                let got = rv.load(Ordering::SeqCst);
                if got == i32::MIN {
                    if now.wrapping_sub(*at) > 20_000 {
                        with(|s| note(s, "The audio output did not start."));
                        self.release(mt, "the Load never returned");
                    }
                    return;
                }
                if got == 0 {
                    with(|s| note(s, "The audio output refused to start."));
                    self.release(mt, "the Load was refused");
                    return;
                }
                plx_base::eventlog::log("music: pipeline loaded");
                self.boot = Boot::Live;
            }
            Boot::Live => {}
        }

        // the output binding the video path does, once the pipeline has named its media id
        if !self.bound {
            let id = FEED_SHARED.media_id.lock().unwrap().clone();
            if let Some(id) = id {
                unsafe { sink.plane_bind(mt, id.as_ptr()) };
                plx_base::eventlog::log("music: output bound");
                self.bound = true;
            }
        }
        if self.bound && !self.video_sent && FEED_SHARED.frames.load(Ordering::Relaxed) >= 2 {
            let info = FEED_SHARED.source_info.lock().unwrap().clone();
            if let Some(bytes) = info {
                let rv = unsafe { sink.plane_send_video_data(mt, bytes.as_ptr() as *const std::os::raw::c_char) };
                if rv != -1 {
                    unsafe { sink.plane_start(mt, 0, 0, 1920, 1080) };
                    plx_base::eventlog::log("music: output started");
                    self.video_sent = true;
                }
            }
        }
        if unsafe { sink.is_load_completed(mt) } == 0 {
            return;
        }

        // pause / resume
        if let Some(pause) = self.pending_pause.take() {
            if pause != self.paused {
                unsafe {
                    if pause {
                        sink.pause(mt);
                        sink.plane_pause(mt);
                    } else {
                        sink.play(mt);
                        sink.plane_resume(mt);
                    }
                }
                self.paused = pause;
                self.report(if pause { "paused" } else { "playing" });
                with(bump);
            }
        }
        // skip and seek
        if let Some(pos) = self.jump.take() {
            self.restart(mt, pos, 0);
        }
        if let Some(dir) = self.skip.take() {
            let pos = with(|s| s.q.pos);
            let shown = self.playing.as_ref().map_or(pos, |p| p.order_pos);
            if dir < 0 && self.position_ms().unwrap_or(0) > 3000 {
                self.restart(mt, shown, 0);
            } else if let Some(np) = Self::step_pos(shown, dir, false) {
                self.restart(mt, np, 0);
            } else {
                self.release(mt, "the queue ended");
                return;
            }
        }
        if let Some(ms) = self.seek_to_ms.take() {
            if let Some(p) = self.playing.as_ref() {
                let (pos, dur) = (p.order_pos, p.duration_ms);
                self.restart(mt, pos, ms.min((dur - 500).max(0)));
            }
        }

        // a restart waits for its file, then decodes
        if let Some((pos, off)) = self.restart_at {
            if self.start_track(pos, off) {
                self.restart_at = None;
            }
        } else if let Some(pos) = self.restart_next_pos {
            if self.start_track(pos, 0) {
                self.restart_next_pos = None;
            }
        }

        // feed what the decoder has made, up to the lead
        let limit = if self.played { self.clock_ns + LEAD_NS } else { PREROLL_NS };
        let mut guard = 0;
        while self.next_audio_pts < limit && guard < 160 {
            guard += 1;
            let (mut au, fresh) = match self.pending_au.take() {
                Some(a) => (a, false),
                None => {
                    let Some(cur) = self.cur.as_mut() else { break };
                    let Some(dec) = cur.dec.as_ref() else { break };
                    match dec.rx.try_recv() {
                        Ok(Msg::Au(a)) => (a, true),
                        Ok(Msg::End) => {
                            self.error_streak = 0;
                            let end = cur.last_end_pts;
                            self.next_audio_pts = self.next_audio_pts.max(end);
                            // a crossfade has the next track part-way in already: it carries on from here
                            if let Some(f) = self.fade.take() {
                                with(|s| s.q.pos = f.pos);
                                self.cur = Some(f.run);
                                continue;
                            }
                            let shown = with(|s| s.q.pos);
                            self.cur = None;
                            match Self::step_pos(shown, 1, true) {
                                Some(np) => {
                                    with(|s| s.q.pos = np);
                                    self.restart_next_pos = Some(np);
                                }
                                None => self.drained = true,
                            }
                            break;
                        }
                        Ok(Msg::Failed(why)) => {
                            plx_base::eventlog::log(&format!("music: decode failed: {why}"));
                            self.error_streak += 1;
                            let shown = with(|s| s.q.pos);
                            self.cur = None;
                            self.fade = None;
                            if self.error_streak >= 3 {
                                with(|s| note(s, "That track could not be played."));
                                self.drained = true;
                            } else {
                                match Self::step_pos(shown, 1, false) {
                                    Some(np) if np != shown => {
                                        with(|s| s.q.pos = np);
                                        self.restart_next_pos = Some(np);
                                    }
                                    _ => self.drained = true,
                                }
                            }
                            break;
                        }
                        Err(_) => break,
                    }
                }
            };
            let Some(cur) = self.cur.as_mut() else { break };
            let base = match cur.base_ns {
                Some(b) => b,
                None => {
                    cur.base_ns = Some(au.pts_ns);
                    // the slot for this track stands for where the decode really began
                    let first = cur.first_pts;
                    if let Some(slot) = self.slots.iter_mut().rev().find(|s| s.first_pts == first) {
                        slot.offset_ms = au.pts_ns / 1_000_000;
                    }
                    au.pts_ns
                }
            };
            let pts = cur.first_pts + (au.pts_ns - base).max(0);
            let dur = (au.pcm.len() / decode::BYTES_PER_FRAME) as i64 * 1_000_000_000 / decode::RATE;
            if fresh {
                if cur.level < 0.999 {
                    scale(&mut au.pcm, cur.level);
                }
                // the end of this track is near: bring the next in under it
                let remaining_ms = {
                    let first = cur.first_pts;
                    let slot_dur = self.slots.iter().rev().find(|s| s.first_pts == first).map_or(0, |s| s.duration_ms);
                    slot_dur - (au.pts_ns - base) / 1_000_000
                };
                if self.fade.is_none() && cur.xf_ms > 0 && remaining_ms > 0 && remaining_ms <= cur.xf_ms && remaining_ms > cur.xf_ms / 2 {
                    let shown = with(|s| s.q.pos);
                    if let Some(np) = Self::step_pos(shown, 1, true).filter(|&n| n != shown) {
                        if let Some((_, item)) = Self::entry(np) {
                            if matches!(&self.file, File::Stream { rk, .. } if *rk == item.rk) {
                                if let File::Stream { stream: data, .. } = std::mem::replace(&mut self.file, File::None) {
                                    let dec = decode::spawn(data, 0);
                                    plx_base::eventlog::log(&format!("music: crossfading into \"{}\"", item.title));
                                    self.slots.push_back(Slot { first_pts: pts, offset_ms: 0, item: item.clone(), duration_ms: item.duration_ms, order_pos: np });
                                    let au_ms = (dur / 1_000_000).max(1);
                                    self.fade = Some(Fade {
                                        run: TrackRun { dec: Some(dec), first_pts: pts, base_ns: None, last_end_pts: pts, level: level_for(&item), xf_ms: i64::from(plx_data::layout::music_crossfade()) * 1000 },
                                        pos: np,
                                        total: (remaining_ms / au_ms).max(1),
                                        done: 0,
                                    });
                                }
                            }
                        }
                    }
                }
                if let Some(f) = self.fade.as_mut() {
                    let got = f.run.dec.as_ref().and_then(|d| d.rx.try_recv().ok());
                    match got {
                        Some(Msg::Au(n)) => {
                            let mut npcm = n.pcm;
                            if f.run.level < 0.999 {
                                scale(&mut npcm, f.run.level);
                            }
                            f.run.base_ns.get_or_insert(n.pts_ns);
                            crossfade(&mut au.pcm, &npcm, (f.done as f32 + 0.5) / f.total as f32);
                            f.done += 1;
                            f.run.last_end_pts = pts + dur;
                        }
                        Some(_) => self.fade = None,
                        None => {}
                    }
                }
            }
            let r = unsafe { sink.feed(mt, au.pcm.as_ptr(), au.pcm.len() as u32, pts, ES_AUDIO) } as u8;
            if r == b'O' {
                self.feed_errors = 0;
                cur.last_end_pts = pts + dur;
                self.next_audio_pts = self.next_audio_pts.max(pts + dur);
            } else {
                if r != b'B' {
                    self.feed_errors += 1;
                }
                // the pipeline's queue is full: this unit goes first next tick
                self.pending_au = Some(au);
                break;
            }
        }
        if self.feed_errors >= 200 {
            with(|s| note(s, "The audio output stopped."));
            self.release(mt, "the pipeline stopped answering");
            return;
        }
        // nothing came of the queue (every track failed or was skipped): give the pipeline back
        if self.drained && !self.played && self.cur.is_none() && self.next_audio_pts == 0 && self.restart_at.is_none() {
            self.release(mt, "nothing could be played");
            return;
        }
        // the next track's file, while this one plays
        if self.cur.is_some() && self.fade.is_none() && matches!(self.file, File::None) && self.restart_next_pos.is_none() {
            let pos = with(|s| s.q.pos);
            if let Some(np) = Self::step_pos(pos, 1, true).filter(|&n| n != pos) {
                if let Some((_, item)) = Self::entry(np) {
                    if (item.part_size as usize) <= PREFETCH_MAX {
                        self.start_fetch(&item);
                    }
                }
            }
        }
        // one black picture a second, a little ahead of the audio
        let mut g = 0;
        while self.next_video_pts <= self.next_audio_pts + 200_000_000 && g < 4 {
            g += 1;
            let r = unsafe { sink.feed(mt, BLACK_FRAME.as_ptr(), BLACK_FRAME.len() as u32, self.next_video_pts, ES_VIDEO) } as u8;
            if r != b'O' {
                break;
            }
            self.next_video_pts += 1_000_000_000;
        }
        // play once the pre-roll is in
        if !self.played && (self.next_audio_pts >= PREROLL_NS || (self.drained && self.next_audio_pts > 0)) {
            unsafe {
                sink.set_volume(mt, 100);
                sink.play(mt);
                sink.plane_resume(mt);
            }
            self.played = true;
            // the output presents about 400 ms after Play (the PCM lead); the on-screen clock follows what is heard
            self.clock_ns = -400_000_000;
            plx_base::eventlog::log("music: playing");
        }
        if self.played && !self.paused {
            self.clock_ns += dt_ns;
        }
        // the track the viewer hears now
        while self.slots.front().is_some_and(|s| s.first_pts <= self.clock_ns) && self.played {
            let next = self.slots.pop_front().unwrap();
            if self.playing.is_some() {
                self.report("stopped");
            }
            with(|s| {
                s.q.shown_pos = next.order_pos;
                bump(s);
            });
            self.playing = Some(next);
            self.last_report = now;
            self.report("playing");
        }
        if self.playing.is_some() && now.wrapping_sub(self.last_report) >= 10_000 && !self.paused {
            self.last_report = now;
            self.report("playing");
        }
        // the end of the queue: everything fed has been heard
        if self.drained && self.played && self.clock_ns >= self.next_audio_pts + 600_000_000 && self.slots.is_empty() {
            self.release(mt, "the queue ended");
        }
        let _ = &self.reported_rk;
    }
}

pub(crate) fn jump_to(pos: usize) {
    ENGINE.with(|e| e.borrow_mut().jump = Some(pos));
}

/// One request from the music page.
pub(crate) fn handle(o: plx_screens::music::MusicOut) {
    use plx_screens::music::MusicOut as O;
    match o {
        O::Load { key } => {
            if key == "hubs" {
                request_hubs();
            } else {
                request(&key);
            }
        }
        O::More { key } => more(&key),
        O::Play { items, start, shuffle } => play(items, start, shuffle),
        O::PlayNext { item } => play_next(item),
        O::AddToQueue { item } => add_to_queue(item),
        O::TogglePause => toggle_pause(),
        O::Next => next_track(),
        O::Previous => previous_track(),
        O::Seek { ms } => seek_to(ms),
        O::Stop => stop(),
        O::ToggleShuffle => toggle_shuffle(),
        O::CycleRepeat => cycle_repeat(),
        O::JumpTo { pos } => jump_to(pos),
        O::Radio { rk } => radio(rk),
        O::Rate { rk, half_stars } => rate(rk, half_stars),
    }
}

/// Start a radio from `rk`: the track itself, then the tracks the server finds sonically near it.
pub(crate) fn radio(rk: String) {
    with(|s| note(s, "Starting a radio\u{2026}"));
    let _ = std::thread::Builder::new().name("music-radio".into()).spawn(move || {
        let client = plx_plex::plex::client();
        let seed = client.music_item(&rk);
        let near = client.music_nearest(&rk, 60);
        match (seed, near) {
            (Some(seed), Some(near)) if !near.items.is_empty() => {
                let mut items = vec![seed];
                items.extend(near.items.into_iter().filter(|i| i.rk != rk));
                play(items, 0, false);
            }
            _ => with(|s| note(s, "This server has no sonic radio for that track.")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }
    fn samples(b: &[u8]) -> Vec<i16> {
        b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
    }

    #[test]
    fn scaling_halves_the_amplitude_and_never_wraps() {
        let mut b = pcm(&[1000, -1000, 32767, -32768]);
        scale(&mut b, 0.5);
        assert_eq!(samples(&b), vec![500, -500, 16383, -16384]);
        let mut loud = pcm(&[30000]);
        scale(&mut loud, 2.0);
        assert_eq!(samples(&loud), vec![32767], "clamped, not wrapped");
    }

    #[test]
    fn a_crossfade_runs_from_the_outgoing_to_the_incoming_track() {
        let (a, b) = (pcm(&[1000, 1000]), pcm(&[-1000, -1000]));
        let mut start = a.clone();
        crossfade(&mut start, &b, 0.0);
        assert_eq!(samples(&start), vec![1000, 1000]);
        let mut end = a.clone();
        crossfade(&mut end, &b, 1.0);
        assert_eq!(samples(&end), vec![-1000, -1000]);
        let mut mid = a;
        crossfade(&mut mid, &b, 0.5);
        assert_eq!(samples(&mid), vec![0, 0]);
    }

    #[test]
    fn a_shuffle_is_a_permutation_that_keeps_the_chosen_track_first() {
        for n in [1usize, 2, 7, 40] {
            let v = shuffled(n, Some(n - 1));
            assert_eq!(v[0], n - 1);
            let mut s = v.clone();
            s.sort_unstable();
            assert_eq!(s, (0..n).collect::<Vec<_>>());
        }
    }

    #[test]
    fn levelling_only_ever_quietens_a_track() {
        let item = |g: Option<f32>| MusicItem { gain: g, ..Default::default() };
        // levelling is off in a fresh layout: every track plays at its own level
        assert_eq!(level_for(&item(Some(-6.0))), 1.0);
        assert!(10f32.powf(-6.0 / 20.0) < 0.51 && 10f32.powf(-6.0 / 20.0) > 0.49);
    }

    #[test]
    fn the_queue_steps_by_the_repeat_mode() {
        let _g = plx_base::testlock::serial();
        with(|s| {
            s.q = Queue::default();
            s.q.order = vec![0, 1, 2];
            s.q.repeat = Repeat::Off;
        });
        assert_eq!(Engine::step_pos(0, 1, true), Some(1));
        assert_eq!(Engine::step_pos(2, 1, true), None, "the end of the queue");
        assert_eq!(Engine::step_pos(0, -1, false), Some(0), "previous at the start stays");
        with(|s| s.q.repeat = Repeat::All);
        assert_eq!(Engine::step_pos(2, 1, true), Some(0), "repeat all wraps");
        assert_eq!(Engine::step_pos(0, -1, false), Some(2));
        with(|s| s.q.repeat = Repeat::One);
        assert_eq!(Engine::step_pos(1, 1, true), Some(1), "repeat one stays on a track that ends");
        assert_eq!(Engine::step_pos(1, 1, false), Some(2), "but a press still skips");
        with(|s| s.q = Queue::default());
    }
}
