//! `<runtime dir>/plxnative-musicspike` (on the TV `/tmp/com.dgaust.amber/plxnative-musicspike`; read directly, not
//! through `devtrig`, so it works in a release build, which has no dev triggers): milestone M0 of `docs/music-design.md`, question S1. Does this television's
//! media pipeline accept an AUDIO-ONLY Load (no video declared, no picture fed), and does it then play PCM
//! that the app feeds it? The app's player refuses a file with no video, so nothing else answers this.
//!
//! The trigger's text names the variant: `audio` (default: only `"audio":"PCM"` declared, `audioOnly:true`) or
//! `withvideo` (the same, but H264 is also declared and never fed: the fallback if the first is refused).
//! Run it from Home with nothing playing: the pipeline takes one Load per process, so the spike ends by
//! quitting the app (open Amber again afterwards to play anything).
//!
//! It feeds a two-tone chord as 48 kHz 16-bit stereo PCM for [`RUN_MS`] after playing starts, and logs one
//! `musicspike:` line per step and per two seconds: what the Load returned, whether the object was ready,
//! the feed verdicts (`O` accepted, `B` buffer full, `e` error) and how far ahead of the clock it ran.
//! Measurement only: it reports nothing to Plex and reads no setting.

use crate::app::App;
use plx_platform::tv::sink::{self, UnloadTicket};
use plx_play_feed::shared::FEED_SHARED;
use std::cell::RefCell;
use std::ffi::CString;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

const RATE: u32 = 48_000;
const CHUNK_FRAMES: usize = 960; // 20 ms
const BYTES_PER_FRAME: usize = 4; // 16-bit stereo
const PREROLL_NS: i64 = 600_000_000;
/// How far ahead of the wall clock the feed runs once playing.
const LEAD_NS: i64 = 1_500_000_000;
const RUN_MS: u32 = 20_000;
const START_AFTER_MS: u32 = 8_000;
const ES_AUDIO: i32 = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    AudioOnly,
    WithVideo,
}

enum Phase {
    Unchecked,
    Off,
    /// The Load is running on its own thread; `rv` is `i32::MIN` until it returns.
    Loading { rv: Arc<AtomicI32>, at: u32, epoch: u32, bind: u8 },
    /// Loaded; feeding. `play_at` is `None` until the pre-roll is in and Play has been asked for.
    Feeding(Feed),
    /// Finished; quit once `quit_at` passes.
    Ending { quit_at: u32 },
    Done,
}

struct Feed {
    variant: Variant,
    /// 0 = no output binding, 1 = bind (setSinkType MAIN + setMediaId + LOADED), 2 = bind and start (PLAYING).
    bind: u8,
    bound: bool,
    epoch: u32,
    next_pts_ns: i64,
    frames_fed: u64,
    play_at: Option<u32>,
    last_log: u32,
    accepted: u64,
    full: u64,
    errors: u64,
    first_verdicts: Vec<char>,
    buf: Vec<u8>,
}

thread_local! { static PHASE: RefCell<Phase> = const { RefCell::new(Phase::Unchecked) }; }

fn log(line: &str) {
    plx_base::eventlog::log(&format!("musicspike: {line}"));
}

fn payload(variant: Variant) -> String {
    let codec = match variant {
        Variant::AudioOnly => r#""codec":{"audio":"PCM"}"#,
        Variant::WithVideo => r#""codec":{"video":"H264","audio":"PCM"}"#,
    };
    let mut p = plx_play_feed::PAYLOAD_AV
        .replace(r#""codec":{"video":"H264","audio":"AC3"}"#, codec)
        .replace(
            r#""esInfo":"#,
            r#""pcmInfo":{"bitsPerSample":16,"sampleRate":48,"channelMode":"stereo"},"esInfo":"#,
        );
    if variant == Variant::AudioOnly {
        p = p.replace(
            r#""adaptiveStreaming":{"audioOnly":false,"maxWidth":1920,"maxHeight":1080,"maxFrameRate":30}"#,
            r#""adaptiveStreaming":{"audioOnly":true}"#,
        );
    }
    plx_play_feed::native_executor::with_app_id(&p)
}

/// One 20 ms chunk of a two-tone chord (A4 and E5), quiet, as little-endian S16 stereo.
fn chunk(buf: &mut Vec<u8>, first_frame: u64) {
    buf.clear();
    for i in 0..CHUNK_FRAMES as u64 {
        let t = (first_frame + i) as f32 / RATE as f32;
        let s = 0.12 * ((t * 440.0 * std::f32::consts::TAU).sin() + (t * 659.25 * std::f32::consts::TAU).sin());
        let v = (s * i16::MAX as f32) as i16;
        buf.extend_from_slice(&v.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }
}

/// Called once per loop iteration from `scenarios::each_frame`, on the UI thread.
pub(crate) fn tick(app: &mut App, now: u32) {
    PHASE.with(|cell| {
        let mut phase = cell.borrow_mut();
        match &mut *phase {
            Phase::Off | Phase::Done => {}
            Phase::Unchecked => {
                if now < START_AFTER_MS {
                    return;
                }
                let Some(text) = std::fs::read_to_string(plx_base::paths::in_runtime_dir("plxnative-musicspike")).ok().map(|s| s.trim().to_string()) else {
                    *phase = Phase::Off;
                    return;
                };
                let variant = if text.contains("withvideo") { Variant::WithVideo } else { Variant::AudioOnly };
                let bind = if text.contains("start") { 2 } else if text.contains("bind") { 1 } else { 0 };
                let Some(epoch) = FEED_SHARED.begin_native_session() else {
                    log("could not begin a native session (another Load owns the pipeline)");
                    *phase = Phase::Done;
                    return;
                };
                log(&format!("armed, variant={} bind={bind} epoch={epoch}", if variant == Variant::AudioOnly { "audio" } else { "withvideo" }));
                let json = payload(variant);
                let rv = Arc::new(AtomicI32::new(i32::MIN));
                let rv2 = rv.clone();
                let spawned = std::thread::Builder::new().name("musicspike-load".into()).spawn(move || {
                    let c = match CString::new(json) {
                        Ok(c) => c,
                        Err(_) => {
                            rv2.store(-1, Ordering::SeqCst);
                            return;
                        }
                    };
                    let ok = unsafe { sink::installed().load(c.as_ptr(), epoch) };
                    FEED_SHARED.mark_native_load_returned(epoch, None);
                    rv2.store(ok, Ordering::SeqCst);
                });
                if spawned.is_err() {
                    log("could not start the load thread");
                    *phase = Phase::Done;
                    return;
                }
                VARIANT.with(|v| v.set(Some(variant)));
                *phase = Phase::Loading { rv, at: now, epoch, bind };
            }
            Phase::Loading { rv, at, epoch, bind } => {
                let got = rv.load(Ordering::SeqCst);
                if got == i32::MIN {
                    if now.wrapping_sub(*at) > 20_000 {
                        log("the Load has not returned after 20 s");
                        *phase = Phase::Done;
                    }
                    return;
                }
                let waited = now.wrapping_sub(*at);
                log(&format!("Load returned ok={got} after {waited} ms"));
                if got == 0 {
                    log("RESULT: the pipeline refused this Load");
                    *phase = Phase::Ending { quit_at: now.wrapping_add(1500) };
                    return;
                }
                let variant = VARIANT.with(|v| v.get()).unwrap_or(Variant::AudioOnly);
                *phase = Phase::Feeding(Feed {
                    variant,
                    bind: *bind,
                    bound: false,
                    epoch: *epoch,
                    next_pts_ns: 0,
                    frames_fed: 0,
                    play_at: None,
                    last_log: now,
                    accepted: 0,
                    full: 0,
                    errors: 0,
                    first_verdicts: Vec::new(),
                    buf: Vec::with_capacity(CHUNK_FRAMES * BYTES_PER_FRAME),
                });
            }
            Phase::Feeding(f) => {
                let mt = app.adapters.player.mt();
                let sink = sink::installed();
                let ready = unsafe { sink.ready(mt) };
                let loaded = unsafe { sink.is_load_completed(mt) };
                // fed ahead of where the clock stands (before Play, ahead of the start)
                let clock_ns = f.play_at.map_or(0, |at| i64::from(now.wrapping_sub(at)) * 1_000_000);
                let limit = if f.play_at.is_some() { clock_ns + LEAD_NS } else { PREROLL_NS };
                // the output binding the video path does: once the pipeline has named its media id
                if f.bind > 0 && !f.bound {
                    let id = FEED_SHARED.media_id.lock().unwrap().clone();
                    if let Some(id) = id {
                        unsafe { sink.plane_bind(mt, id.as_ptr()) };
                        log(&format!("plane bound, media id {}", id.to_string_lossy()));
                        if f.bind > 1 {
                            unsafe { sink.plane_start(mt, 0, 0, 1920, 1080) };
                            log("plane started (PLAYING)");
                        }
                        f.bound = true;
                    }
                }
                let mut guard = 0;
                while f.next_pts_ns < limit && guard < 200 && loaded != 0 {
                    guard += 1;
                    chunk(&mut f.buf, f.frames_fed);
                    let r = unsafe { sink.feed(mt, f.buf.as_ptr(), f.buf.len() as u32, f.next_pts_ns, ES_AUDIO) } as u8 as char;
                    if f.first_verdicts.len() < 8 {
                        f.first_verdicts.push(r);
                    }
                    match r {
                        'O' => {
                            f.accepted += 1;
                            f.frames_fed += CHUNK_FRAMES as u64;
                            f.next_pts_ns += (CHUNK_FRAMES as i64 * 1_000_000_000) / RATE as i64;
                        }
                        'B' => {
                            f.full += 1;
                            break;
                        }
                        _ => {
                            f.errors += 1;
                            break;
                        }
                    }
                }
                if f.play_at.is_none() && f.next_pts_ns >= PREROLL_NS {
                    let rv = unsafe { sink.play(mt) };
                    log(&format!("pre-roll fed ({} ms); Play rv={rv}; first verdicts {:?}", f.next_pts_ns / 1_000_000, f.first_verdicts));
                    f.play_at = Some(now);
                }
                if now.wrapping_sub(f.last_log) >= 2000 {
                    f.last_log = now;
                    let _ = f.epoch;
                    let t = f.play_at.map_or(0, |at| now.wrapping_sub(at));
                    log(&format!(
                        "t={t}ms ready={ready} loaded={loaded} fed={}ms ahead={}ms accepted={} full={} errors={}",
                        f.next_pts_ns / 1_000_000,
                        (f.next_pts_ns - clock_ns) / 1_000_000,
                        f.accepted, f.full, f.errors
                    ));
                }
                let finished = f.play_at.is_some_and(|at| now.wrapping_sub(at) >= RUN_MS);
                let stuck = f.play_at.is_none() && now.wrapping_sub(f.last_log) > 10_000 && f.accepted == 0;
                if finished || stuck {
                    let heard = if finished { "ran to the end: ask whether the tone was heard" } else { "never accepted a single sample" };
                    log(&format!(
                        "RESULT ({}): {heard}; accepted={} full={} errors={}",
                        if f.variant == Variant::AudioOnly { "audio-only" } else { "with-video declared" },
                        f.accepted, f.full, f.errors
                    ));
                    let ticket = UnloadTicket::issue(mt);
                    unsafe { sink.unload(&ticket) };
                    *phase = Phase::Ending { quit_at: now.wrapping_add(1500) };
                }
            }
            Phase::Ending { quit_at } => {
                if now.wrapping_sub(*quit_at) < u32::MAX / 2 && now >= *quit_at {
                    log("done; quitting so the next start has a clean pipeline");
                    app.request_quit();
                    *phase = Phase::Done;
                }
            }
        }
    });
}

thread_local! { static VARIANT: std::cell::Cell<Option<Variant>> = const { std::cell::Cell::new(None) }; }
