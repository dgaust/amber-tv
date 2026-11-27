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
const ES_VIDEO: i32 = 1;
/// One black 320x180 H.264 picture (Annex B, with its parameter sets), fed once a second by the `dummyvideo`
/// option so the pipeline has a picture to bind the output to, as it has in a film.
const BLACK_FRAME: &[u8] = &[0, 0, 0, 1, 103, 66, 192, 11, 220, 20, 25, 249, 240, 17, 0, 0, 3, 0, 1, 0, 0, 3, 0, 2, 15, 20, 43, 128, 0, 0, 0, 1, 104, 206, 15, 44, 128, 0, 0, 1, 6, 5, 255, 255, 76, 220, 69, 233, 189, 230, 217, 72, 183, 150, 44, 216, 32, 217, 35, 238, 239, 120, 50, 54, 52, 32, 45, 32, 99, 111, 114, 101, 32, 49, 54, 53, 32, 45, 32, 72, 46, 50, 54, 52, 47, 77, 80, 69, 71, 45, 52, 32, 65, 86, 67, 32, 99, 111, 100, 101, 99, 32, 45, 32, 67, 111, 112, 121, 108, 101, 102, 116, 32, 50, 48, 48, 51, 45, 50, 48, 50, 53, 32, 45, 32, 104, 116, 116, 112, 58, 47, 47, 119, 119, 119, 46, 118, 105, 100, 101, 111, 108, 97, 110, 46, 111, 114, 103, 47, 120, 50, 54, 52, 46, 104, 116, 109, 108, 32, 45, 32, 111, 112, 116, 105, 111, 110, 115, 58, 32, 99, 97, 98, 97, 99, 61, 48, 32, 114, 101, 102, 61, 49, 32, 100, 101, 98, 108, 111, 99, 107, 61, 49, 58, 48, 58, 48, 32, 97, 110, 97, 108, 121, 115, 101, 61, 48, 120, 49, 58, 48, 120, 49, 49, 49, 32, 109, 101, 61, 104, 101, 120, 32, 115, 117, 98, 109, 101, 61, 55, 32, 112, 115, 121, 61, 49, 32, 112, 115, 121, 95, 114, 100, 61, 49, 46, 48, 48, 58, 48, 46, 48, 48, 32, 109, 105, 120, 101, 100, 95, 114, 101, 102, 61, 48, 32, 109, 101, 95, 114, 97, 110, 103, 101, 61, 49, 54, 32, 99, 104, 114, 111, 109, 97, 95, 109, 101, 61, 49, 32, 116, 114, 101, 108, 108, 105, 115, 61, 49, 32, 56, 120, 56, 100, 99, 116, 61, 48, 32, 99, 113, 109, 61, 48, 32, 100, 101, 97, 100, 122, 111, 110, 101, 61, 50, 49, 44, 49, 49, 32, 102, 97, 115, 116, 95, 112, 115, 107, 105, 112, 61, 49, 32, 99, 104, 114, 111, 109, 97, 95, 113, 112, 95, 111, 102, 102, 115, 101, 116, 61, 45, 50, 32, 116, 104, 114, 101, 97, 100, 115, 61, 54, 32, 108, 111, 111, 107, 97, 104, 101, 97, 100, 95, 116, 104, 114, 101, 97, 100, 115, 61, 49, 32, 115, 108, 105, 99, 101, 100, 95, 116, 104, 114, 101, 97, 100, 115, 61, 48, 32, 110, 114, 61, 48, 32, 100, 101, 99, 105, 109, 97, 116, 101, 61, 49, 32, 105, 110, 116, 101, 114, 108, 97, 99, 101, 100, 61, 48, 32, 98, 108, 117, 114, 97, 121, 95, 99, 111, 109, 112, 97, 116, 61, 48, 32, 99, 111, 110, 115, 116, 114, 97, 105, 110, 101, 100, 95, 105, 110, 116, 114, 97, 61, 48, 32, 98, 102, 114, 97, 109, 101, 115, 61, 48, 32, 119, 101, 105, 103, 104, 116, 112, 61, 48, 32, 107, 101, 121, 105, 110, 116, 61, 49, 32, 107, 101, 121, 105, 110, 116, 95, 109, 105, 110, 61, 49, 32, 115, 99, 101, 110, 101, 99, 117, 116, 61, 52, 48, 32, 105, 110, 116, 114, 97, 95, 114, 101, 102, 114, 101, 115, 104, 61, 48, 32, 114, 99, 61, 99, 114, 102, 32, 109, 98, 116, 114, 101, 101, 61, 48, 32, 99, 114, 102, 61, 50, 51, 46, 48, 32, 113, 99, 111, 109, 112, 61, 48, 46, 54, 48, 32, 113, 112, 109, 105, 110, 61, 48, 32, 113, 112, 109, 97, 120, 61, 54, 57, 32, 113, 112, 115, 116, 101, 112, 61, 52, 32, 105, 112, 95, 114, 97, 116, 105, 111, 61, 49, 46, 52, 48, 32, 97, 113, 61, 49, 58, 49, 46, 48, 48, 0, 128, 0, 0, 1, 101, 136, 132, 4, 188, 152, 160, 0, 56, 163, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 39, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 93, 117, 215, 128];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    AudioOnly,
    WithVideo,
}

enum Phase {
    Unchecked,
    Off,
    /// The Load is running on its own thread; `rv` is `i32::MIN` until it returns.
    Loading { rv: Arc<AtomicI32>, at: u32, epoch: u32, bind: u8, dummy: bool },
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
    video_sent: bool,
    dummy_video: bool,
    next_video_pts_ns: i64,
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
                let variant = if text.contains("withvideo") || text.contains("dummyvideo") { Variant::WithVideo } else { Variant::AudioOnly };
                let dummy = text.contains("dummyvideo");
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
                *phase = Phase::Loading { rv, at: now, epoch, bind, dummy };
            }
            Phase::Loading { rv, at, epoch, bind, dummy } => {
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
                    video_sent: false,
                    dummy_video: *dummy,
                    next_video_pts_ns: 0,
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
                        f.bound = true;
                    }
                }
                // what the video path then does, once a couple of pictures have gone through: hand the
                // plane the pipeline's own description of the stream, then start it (setState PLAYING)
                if f.bound && f.bind > 1 && !f.video_sent && FEED_SHARED.frames.load(Ordering::Relaxed) >= 2 {
                    let info = FEED_SHARED.source_info.lock().unwrap().clone();
                    if let Some(bytes) = info {
                        let rv = unsafe { sink.plane_send_video_data(mt, bytes.as_ptr() as *const std::os::raw::c_char) };
                        log(&format!("setMediaVideoData rv={rv}"));
                        if rv != -1 {
                            unsafe { sink.plane_start(mt, 0, 0, 1920, 1080) };
                            log("plane started (PLAYING)");
                            f.video_sent = true;
                        }
                    }
                }
                // one black picture a second, a little ahead of the audio
                if f.dummy_video {
                    let mut g = 0;
                    while f.next_video_pts_ns <= f.next_pts_ns && g < 4 && loaded != 0 {
                        g += 1;
                        let r = unsafe { sink.feed(mt, BLACK_FRAME.as_ptr(), BLACK_FRAME.len() as u32, f.next_video_pts_ns, ES_VIDEO) } as u8 as char;
                        if r != 'O' {
                            if f.first_verdicts.len() < 12 { f.first_verdicts.push(r); }
                            break;
                        }
                        f.next_video_pts_ns += 1_000_000_000;
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
                    let vol = unsafe { sink.set_volume(mt, 100) };
                    let rv = unsafe { sink.play(mt) };
                    log(&format!("set_volume(100) rv={vol}"));
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
