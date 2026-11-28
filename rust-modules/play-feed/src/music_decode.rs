//! Amber music: one audio file, decoded on a worker thread into the 48 kHz 16-bit stereo PCM the
//! television's mixer takes (`docs/music-design.md` 3.2).
//!
//! The file is the track's own bytes, already downloaded (`Client::music_fetch_part`), read by the
//! bundled FFmpeg through a memory AVIO; the decode is `plx_media_codecs::speed_decoder::SpeedDecoder`
//! at 1x, which decodes whatever codec the bundled FFmpeg has, downmixes to stereo and resamples to
//! 48 kHz. The access units go out through a bounded channel: the worker blocks when the engine has
//! all it wants, so a long track costs a few seconds of PCM in memory, not the whole of it.

use plx_media_codecs::ffi::*;
use plx_media_codecs::SpeedDecoder;
use std::os::raw::{c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// 48 kHz, 16-bit, stereo.
pub const BYTES_PER_FRAME: usize = 4;
pub const RATE: i64 = 48_000;
/// How many access units may wait for the feeder (about 20 ms each): a few seconds.
const QUEUE_AUS: usize = 400;
const AVSEEK_SIZE_FLAG: c_int = 0x10000;
const AVERROR_EOF_CODE: c_int = -541_478_725;

/// A track file that is still arriving: the bytes so far, and how many there will be. The decoder reads it
/// as it grows (a read past the end waits for more), so decoding starts with the first chunk, not the last.
pub struct Stream {
    buf: Mutex<Vec<u8>>,
    cv: Condvar,
    /// The file's size when the server said (0 = unknown until it is all here).
    pub total: usize,
    /// 0 arriving, 1 complete, 2 failed.
    state: AtomicU8,
    /// The download goes no further than this many bytes (a track held ready fetches only its start).
    cap: AtomicUsize,
}

impl Stream {
    pub fn new(total: usize) -> Stream {
        Stream { buf: Mutex::new(Vec::with_capacity(total.min(300 << 20))), cv: Condvar::new(), total, state: AtomicU8::new(0), cap: AtomicUsize::new(usize::MAX) }
    }
    pub fn push(&self, bytes: &[u8]) {
        if let Ok(mut b) = self.buf.lock() {
            b.extend_from_slice(bytes);
        }
        self.cv.notify_all();
    }
    /// The server sent the whole file where a part was asked for: replace what there is.
    pub fn set_all(&self, bytes: Vec<u8>) {
        if let Ok(mut b) = self.buf.lock() {
            *b = bytes;
        }
        self.finish();
    }
    pub fn finish(&self) {
        self.state.store(1, Ordering::SeqCst);
        self.cv.notify_all();
    }
    pub fn fail(&self) {
        self.state.store(2, Ordering::SeqCst);
        self.cv.notify_all();
    }
    pub fn set_cap(&self, bytes: usize) {
        self.cap.store(bytes, Ordering::SeqCst);
    }
    pub fn cap(&self) -> usize {
        self.cap.load(Ordering::SeqCst)
    }
    pub fn downloaded(&self) -> usize {
        self.buf.lock().map_or(0, |b| b.len())
    }
    pub fn is_complete(&self) -> bool {
        self.state.load(Ordering::SeqCst) == 1
    }
    pub fn is_failed(&self) -> bool {
        self.state.load(Ordering::SeqCst) == 2
    }
}

/// One piece of decoded audio and where in the track it starts.
pub struct Au {
    pub pts_ns: i64,
    pub pcm: Vec<u8>,
}

pub enum Msg {
    Au(Au),
    /// The track's end: every access unit has been sent.
    End,
    /// The file could not be opened or decoded; the reason is for the log.
    Failed(String),
}

/// What opening the file told us, before any audio.
#[derive(Clone, Debug, Default)]
pub struct Probed {
    pub duration_ms: i64,
    pub codec: String,
}

pub struct Running {
    pub rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    probed: Arc<std::sync::Mutex<Option<Probed>>>,
}

impl Running {
    pub fn probed(&self) -> Option<Probed> {
        self.probed.lock().ok().and_then(|p| p.clone())
    }
    /// Ask the worker to stop and wait for it. A worker blocked on a full queue wakes within 50 ms.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // drain so a blocked send can finish
        while self.rx.try_recv().is_ok() {}
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Start decoding `file` from `start_ms` on its own thread.
pub fn spawn(file: Arc<Stream>, start_ms: i64) -> Running {
    let (tx, rx) = sync_channel::<Msg>(QUEUE_AUS);
    let stop = Arc::new(AtomicBool::new(false));
    let probed = Arc::new(std::sync::Mutex::new(None));
    let (stop2, probed2) = (stop.clone(), probed.clone());
    let handle = std::thread::Builder::new()
        .name("music-decode".into())
        .spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&file, start_ms, &tx, &stop2, &probed2)));
            match r {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = tx.send(Msg::Failed(e));
                }
                Err(_) => {
                    let _ = tx.send(Msg::Failed("the decoder panicked".into()));
                }
            }
        })
        .ok();
    Running { rx, stop, handle, probed }
}

/// The file the AVIO reads: the growing stream and a cursor.
struct MemIo {
    stream: Arc<Stream>,
    stop: Arc<AtomicBool>,
    pos: usize,
}

/// `AVERROR(EIO)`.
const AVERROR_IO: c_int = -5;

extern "C" fn mem_read(op: *mut c_void, dst: *mut u8, n: c_int) -> c_int {
    let io = unsafe { &mut *(op as *mut MemIo) };
    let Ok(mut b) = io.stream.buf.lock() else { return AVERROR_IO };
    loop {
        if io.pos < b.len() {
            let take = (b.len() - io.pos).min(n.max(0) as usize);
            unsafe { std::ptr::copy_nonoverlapping(b.as_ptr().add(io.pos), dst, take) };
            io.pos += take;
            return take as c_int;
        }
        if io.stream.is_complete() {
            return AVERROR_EOF_CODE;
        }
        if io.stream.is_failed() {
            return AVERROR_IO;
        }
        if io.stop.load(Ordering::SeqCst) {
            return AVERROR_EOF_CODE;
        }
        b = match io.stream.cv.wait_timeout(b, Duration::from_millis(100)) {
            Ok((g, _)) => g,
            Err(_) => return AVERROR_IO,
        };
    }
}

/// The whole size, when it is known or the file has all arrived (waits for the rest otherwise).
fn total_size(io: &MemIo) -> Option<usize> {
    if io.stream.total > 0 {
        return Some(io.stream.total);
    }
    let mut b = io.stream.buf.lock().ok()?;
    loop {
        if io.stream.is_complete() {
            return Some(b.len());
        }
        if io.stream.is_failed() || io.stop.load(Ordering::SeqCst) {
            return None;
        }
        b = io.stream.cv.wait_timeout(b, Duration::from_millis(100)).ok()?.0;
    }
}

extern "C" fn mem_seek(op: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let io = unsafe { &mut *(op as *mut MemIo) };
    if whence & AVSEEK_SIZE_FLAG != 0 {
        return total_size(io).map_or(-1, |t| t as i64);
    }
    let base = match whence & 3 {
        0 => 0i64,
        1 => io.pos as i64,
        2 => match total_size(io) {
            Some(t) => t as i64,
            None => return -1,
        },
        _ => return -1,
    };
    let to = base + offset;
    if to < 0 {
        return -1;
    }
    // a place not yet downloaded is fine: a read there waits for it
    if io.stream.total > 0 && to as usize > io.stream.total {
        return -1;
    }
    io.pos = to as usize;
    to
}

/// Send a message, giving up when asked to stop. False = stopped (or the receiver is gone).
fn send(tx: &SyncSender<Msg>, stop: &AtomicBool, mut m: Msg) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        match tx.try_send(m) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(back)) => {
                m = back;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn run(
    file: &Arc<Stream>,
    start_ms: i64,
    tx: &SyncSender<Msg>,
    stop: &Arc<AtomicBool>,
    probed: &std::sync::Mutex<Option<Probed>>,
) -> Result<(), String> {
    if !abi_ok() {
        return Err("the bundled FFmpeg is not loaded".into());
    }
    ensure_registered();
    unsafe {
        let mut io = Box::new(MemIo { stream: file.clone(), stop: stop.clone(), pos: 0 });
        let buf = av_malloc(32768) as *mut u8;
        if buf.is_null() {
            return Err("av_malloc failed".into());
        }
        let avio = avio_alloc_context(
            buf,
            32768,
            0,
            &mut *io as *mut MemIo as *mut c_void,
            Some(mem_read),
            None,
            Some(mem_seek),
        );
        if avio.is_null() {
            free_ptr(buf as *mut c_void);
            return Err("avio_alloc_context failed".into());
        }
        let mut fmt = avformat_alloc_context();
        if fmt.is_null() {
            free_avio(avio);
            return Err("avformat_alloc_context failed".into());
        }
        (*fmt).pb = avio;
        if avformat_open_input(&mut fmt, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut()) < 0 || fmt.is_null() {
            free_avio(avio);
            return Err("avformat_open_input failed".into());
        }
        let outcome = decode_all(fmt, start_ms, tx, stop, probed);
        avformat_close_input(&mut fmt);
        free_avio(avio);
        drop(io);
        outcome
    }
}

unsafe fn decode_all(
    fmt: *mut AVFormatContext,
    start_ms: i64,
    tx: &SyncSender<Msg>,
    stop: &AtomicBool,
    probed: &std::sync::Mutex<Option<Probed>>,
) -> Result<(), String> {
    if avformat_find_stream_info(fmt, std::ptr::null_mut()) < 0 {
        return Err("find_stream_info failed".into());
    }
    let ai = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, -1, std::ptr::null_mut(), 0);
    if ai < 0 {
        return Err("the file has no audio stream".into());
    }
    let st = *(*fmt).streams.add(ai as usize);
    let par = stream_codecpar(st);
    let tb = stream_time_base(st);
    let name = std::ffi::CStr::from_ptr(avcodec_get_name((*par).codec_id)).to_string_lossy().into_owned();
    let dur = fmt_duration(fmt);
    if let Ok(mut p) = probed.lock() {
        *p = Some(Probed { duration_ms: if dur > 0 { dur / 1000 } else { 0 }, codec: name.clone() });
    }
    let Some(mut dec) = SpeedDecoder::open(par, 1000) else {
        return Err(format!("no decoder for {name} in the bundled FFmpeg"));
    };
    if start_ms > 0 && tb.den > 0 && tb.num > 0 {
        let ts = start_ms * i64::from(tb.den) / (1000 * i64::from(tb.num));
        // AVSEEK_FLAG_BACKWARD: the nearest frame before it
        let _ = av_seek_frame(fmt, ai, ts, 1);
    }
    let pkt = av_packet_alloc();
    if pkt.is_null() {
        return Err("av_packet_alloc failed".into());
    }
    let mut failed = false;
    while av_read_frame(fmt, pkt) >= 0 {
        if stop.load(Ordering::SeqCst) {
            av_packet_unref(pkt);
            break;
        }
        if (*pkt).stream_index == ai {
            let pts = if (*pkt).pts == i64::MIN { (*pkt).dts } else { (*pkt).pts };
            let pts_ns = if pts == i64::MIN || tb.den == 0 {
                0
            } else {
                (i128::from(pts) * i128::from(tb.num) * 1_000_000_000 / i128::from(tb.den)) as i64
            };
            for (at, pcm) in dec.decode(pkt, pts_ns) {
                if !send(tx, stop, Msg::Au(Au { pts_ns: at, pcm })) {
                    failed = true;
                    break;
                }
            }
        }
        av_packet_unref(pkt);
        if failed {
            break;
        }
    }
    let mut p = pkt;
    av_packet_free(&mut p);
    if !failed && !stop.load(Ordering::SeqCst) {
        send(tx, stop, Msg::End);
    }
    Ok(())
}

/// Wait up to `ms` for the next message (the engine's tick uses `try_recv`; tests and tools wait).
pub fn recv(rx: &Receiver<Msg>, ms: u64) -> Option<Msg> {
    match rx.recv_timeout(Duration::from_millis(ms)) {
        Ok(m) => Some(m),
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io_over(bytes: Vec<u8>, total: usize) -> MemIo {
        let stream = Arc::new(Stream::new(total));
        stream.push(&bytes);
        stream.finish();
        MemIo { stream, stop: Arc::new(AtomicBool::new(false)), pos: 0 }
    }

    #[test]
    fn the_stream_io_reads_seeks_and_reports_its_size() {
        let mut io = io_over((0u8..100).collect(), 100);
        let op = &mut io as *mut MemIo as *mut c_void;
        let mut out = [0u8; 16];
        assert_eq!(mem_read(op, out.as_mut_ptr(), 16), 16);
        assert_eq!(out[15], 15);
        assert_eq!(mem_seek(op, 0, AVSEEK_SIZE_FLAG), 100);
        assert_eq!(mem_seek(op, -10, 2), 90);
        assert_eq!(mem_read(op, out.as_mut_ptr(), 16), 10, "only what is left");
        assert_eq!(mem_read(op, out.as_mut_ptr(), 16), AVERROR_EOF_CODE);
        assert_eq!(mem_seek(op, 500, 0), -1, "past the end is refused");
        assert_eq!(mem_seek(op, 20, 0), 20);
    }

    #[test]
    fn a_read_waits_for_bytes_that_are_still_arriving() {
        let stream = Arc::new(Stream::new(8));
        stream.push(&[1, 2, 3, 4]);
        let mut io = MemIo { stream: stream.clone(), stop: Arc::new(AtomicBool::new(false)), pos: 4 };
        let s2 = stream.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            s2.push(&[5, 6, 7, 8]);
            s2.finish();
        });
        let op = &mut io as *mut MemIo as *mut c_void;
        let mut out = [0u8; 8];
        assert_eq!(mem_read(op, out.as_mut_ptr(), 8), 4, "blocked until the rest came");
        assert_eq!(&out[..4], &[5, 6, 7, 8]);
        assert_eq!(mem_read(op, out.as_mut_ptr(), 8), AVERROR_EOF_CODE);
        t.join().unwrap();
    }

    #[test]
    fn a_failed_download_ends_the_read_with_an_error() {
        let stream = Arc::new(Stream::new(8));
        stream.fail();
        let mut io = MemIo { stream, stop: Arc::new(AtomicBool::new(false)), pos: 0 };
        let mut out = [0u8; 4];
        assert_eq!(mem_read(&mut io as *mut MemIo as *mut c_void, out.as_mut_ptr(), 4), AVERROR_IO);
    }
}
