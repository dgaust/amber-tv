//! Amber: music on a Sonos speaker. The TV is the remote: the speaker fetches the files from Plex itself and plays
//! its own queue, and this module drives it over the home network (UPnP / SOAP on port 1400, no account) and
//! reads back what it reports.
//!
//! - **Finding speakers**: an SSDP search (a scan of the local /24 for port 1400 when nothing answers), then one
//!   `GetZoneGroupState` call names every room and the speaker that leads its group.
//! - **The queue**: the speaker's queue holds this app's play order, so Next / Previous / gapless work on the
//!   speaker itself. The first tracks go in at once and play starts; the rest follow in the background.
//! - **Reading back**: once a second the speaker is asked which track it is on and where, and the page shows
//!   that. A track that is not one of ours (the Sonos app took over) ends the session here.

use plx_plex::plex::music::MusicItem;
use plx_screens::music::{Repeat, Speaker};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const PORT: u16 = 1400;
/// The server is not at an address a speaker on the home network can use.
const NO_LAN: &str = "the server's address is not on the home network";
/// A Sonos queue holds 1000 tracks.
const MAX_QUEUE: usize = 1000;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Room {
    pub uuid: String,
    pub name: String,
    pub ip: String,
    /// The speaker that leads this room's group (commands for the music go to it).
    pub coord_uuid: String,
    pub coord_ip: String,
}

static ROOMS: Mutex<Vec<Room>> = Mutex::new(Vec::new());
static VOLUMES: Mutex<Option<HashMap<String, i32>>> = Mutex::new(None);
static TARGET: Mutex<Option<Room>> = Mutex::new(None);
static SCANNING: AtomicBool = AtomicBool::new(false);
/// The volume of the playing group when it has more than one room (-1 otherwise).
static GROUP_VOL: AtomicI32 = AtomicI32::new(-1);
/// Rooms this app put into the group (they leave it when the music goes back to the TV).
static JOINED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Moves when what the page shows may have changed.
static GEN: AtomicU32 = AtomicU32::new(0);
static SNAP: Mutex<Snap> = Mutex::new(Snap::new());
static CMD: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
/// Which controller is the current one (a controller that was replaced stops touching the shared state).
static CTL_ID: AtomicU32 = AtomicU32::new(0);
/// Rooms whose group is being changed (a second press waits for the first).
static BUSY: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Volume steps not sent yet (a room's id, or `GROUP`), merged so a held key is one request at a time.
static VOL_PENDING: Mutex<Vec<(String, i32)>> = Mutex::new(Vec::new());
static VOL_WORKER: AtomicBool = AtomicBool::new(false);
const GROUP: &str = "\u{1}group";

fn bump() {
    GEN.fetch_add(1, Ordering::Relaxed);
}

/// What the speaker last said, for the page.
#[derive(Clone, Debug)]
pub(crate) struct Snap {
    pub active: bool,
    /// The queue is being sent over; nothing plays yet.
    pub loading: bool,
    /// The queue position (play order) the speaker is on.
    pub qpos: usize,
    pub pos_ms: i64,
    pub dur_ms: i64,
    pub playing: bool,
    at: Option<Instant>,
}

impl Snap {
    const fn new() -> Snap {
        Snap { active: false, loading: false, qpos: 0, pos_ms: 0, dur_ms: 0, playing: false, at: None }
    }
}

fn snap_with<R>(f: impl FnOnce(&mut Snap) -> R) -> R {
    let mut g = SNAP.lock().unwrap_or_else(|e| e.into_inner());
    let r = f(&mut g);
    bump();
    r
}

// ---- what the page reads -----------------------------------------------------------------------------

pub(crate) fn generation() -> u32 {
    GEN.load(Ordering::Relaxed)
}

/// True while the music is set to play on a speaker.
pub(crate) fn selected() -> bool {
    TARGET.lock().is_ok_and(|t| t.is_some())
}

pub(crate) fn target_id() -> String {
    TARGET.lock().ok().and_then(|t| t.as_ref().map(|r| r.uuid.clone())).unwrap_or_default()
}

pub(crate) fn target_name() -> String {
    TARGET.lock().ok().and_then(|t| t.as_ref().map(|r| r.name.clone())).unwrap_or_default()
}

/// The speaker has a session (loading, playing or paused).
pub(crate) fn active() -> bool {
    SNAP.lock().is_ok_and(|s| s.active)
}

/// The session, with the position moved on to now while it plays.
pub(crate) fn snapshot() -> Option<Snap> {
    let s = SNAP.lock().ok()?.clone();
    if !s.active {
        return None;
    }
    let mut s = s;
    if s.playing && !s.loading {
        if let Some(at) = s.at {
            s.pos_ms += at.elapsed().as_millis() as i64;
        }
    }
    if s.dur_ms > 0 {
        s.pos_ms = s.pos_ms.min(s.dur_ms);
    }
    Some(s)
}

pub(crate) fn scanning() -> bool {
    SCANNING.load(Ordering::Relaxed)
}

pub(crate) fn speakers() -> Vec<Speaker> {
    let busy = BUSY.lock().map(|b| b.clone()).unwrap_or_default();
    let cur = TARGET.lock().ok().and_then(|t| t.as_ref().map(|r| r.coord_uuid.clone()));
    let vols = VOLUMES.lock().ok().and_then(|v| v.clone()).unwrap_or_default();
    ROOMS
        .lock()
        .map(|r| {
            r.iter()
                .map(|r| Speaker { id: r.uuid.clone(), name: r.name.clone(), volume: vols.get(&r.uuid).copied().unwrap_or(-1), current: cur.as_deref() == Some(r.coord_uuid.as_str()), busy: busy.contains(&r.uuid) })
                .collect()
        })
        .unwrap_or_default()
}

// ---- finding speakers --------------------------------------------------------------------------------

/// Clears the scanning flag however the scan ends (a panic included).
struct ScanDone;
impl Drop for ScanDone {
    fn drop(&mut self) {
        SCANNING.store(false, Ordering::Release);
        bump();
    }
}

/// Look for speakers in the background (the page shows them as they arrive).
pub(crate) fn rescan() {
    if SCANNING.swap(true, Ordering::AcqRel) {
        return;
    }
    bump();
    let _ = std::thread::Builder::new().name("sonos-scan".into()).spawn(|| {
        let _done = ScanDone;
        let rooms = discover();
        plx_base::eventlog::log(&format!("sonos: found {} room(s)", rooms.len()));
        let vols: HashMap<String, i32> = std::thread::scope(|sc| {
            let hs: Vec<_> = rooms.iter().map(|r| sc.spawn(move || (r.uuid.clone(), get_volume(&r.ip)))).collect();
            hs.into_iter().filter_map(|h| h.join().ok()).filter_map(|(u, v)| v.map(|v| (u, v))).collect()
        });
        if let Ok(mut v) = VOLUMES.lock() {
            v.get_or_insert_with(HashMap::new).extend(vols);
        }
        // a scan that finds nothing (a Wi-Fi blip) leaves the rooms already known
        if !rooms.is_empty() {
            if let Ok(mut r) = ROOMS.lock() {
                *r = rooms;
            }
        }
        refresh_target();
        refresh_group_volume();
    });
}

/// The target room as the latest topology names it.
fn refresh_target() {
    let rooms = ROOMS.lock().map(|r| r.clone()).unwrap_or_default();
    if let Ok(mut t) = TARGET.lock() {
        if let Some(cur) = t.as_ref() {
            if let Some(n) = rooms.iter().find(|r| r.uuid == cur.uuid) {
                *t = Some(n.clone());
            }
        }
    }
}

fn get_group_volume(ip: &str) -> Option<i32> {
    let b = soap(ip, "GroupRenderingControl", "GetGroupVolume", "<InstanceID>0</InstanceID>").ok()?;
    tag(&b, "CurrentVolume")?.trim().parse().ok()
}

/// The playing group's volume, and each of its rooms' own.
fn refresh_group_volume() {
    let Some(t) = TARGET.lock().ok().and_then(|t| t.clone()) else {
        GROUP_VOL.store(-1, Ordering::Relaxed);
        return;
    };
    let members: Vec<Room> = ROOMS.lock().map(|r| r.iter().filter(|r| r.coord_uuid == t.coord_uuid).cloned().collect()).unwrap_or_default();
    let vols: Vec<(String, Option<i32>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = members.iter().map(|r| sc.spawn(move || (r.uuid.clone(), get_volume(&r.ip)))).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    if let Ok(mut v) = VOLUMES.lock() {
        let m = v.get_or_insert_with(HashMap::new);
        for (u, x) in vols {
            if let Some(x) = x {
                m.insert(u, x);
            }
        }
    }
    GROUP_VOL.store(if members.len() >= 2 { get_group_volume(&t.coord_ip).unwrap_or(-1) } else { -1 }, Ordering::Relaxed);
    bump();
}

/// Read the topology again from the speaker that leads the group (after a room joined or left).
fn refresh_zones(ip: &str) {
    if let Ok(body) = soap(ip, "ZoneGroupTopology", "GetZoneGroupState", "") {
        let rooms = parse_zones(&body);
        if !rooms.is_empty() {
            if let Ok(mut r) = ROOMS.lock() {
                *r = rooms;
            }
            refresh_target();
        }
    }
    refresh_group_volume();
}

pub(crate) fn group_volume_now() -> i32 {
    GROUP_VOL.load(Ordering::Relaxed)
}

/// (the room is in the playing group, how many rooms are, the room leads it).
pub(crate) fn member_state(id: &str) -> (bool, usize, bool) {
    let Some(t) = TARGET.lock().ok().and_then(|t| t.clone()) else { return (false, 0, false) };
    let rooms = ROOMS.lock().map(|r| r.clone()).unwrap_or_default();
    let n = rooms.iter().filter(|r| r.coord_uuid == t.coord_uuid).count();
    let inside = rooms.iter().find(|r| r.uuid == id).is_some_and(|r| r.coord_uuid == t.coord_uuid);
    (inside, n, id == t.coord_uuid)
}

/// Put the room into the playing group, or take it out (in the background).
pub(crate) fn toggle_member(id: &str) {
    let Some(t) = TARGET.lock().ok().and_then(|t| t.clone()) else { return };
    let Some(room) = ROOMS.lock().ok().and_then(|r| r.iter().find(|r| r.uuid == id).cloned()) else { return };
    {
        let mut b = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if b.contains(&room.uuid) {
            return;
        }
        b.push(room.uuid.clone());
    }
    bump();
    let (inside, _, _) = member_state(id);
    let _ = std::thread::Builder::new().name("sonos-group".into()).spawn(move || {
        if inside {
            let r = soap(&room.ip, "AVTransport", "BecomeCoordinatorOfStandaloneGroup", "<InstanceID>0</InstanceID>");
            plx_base::eventlog::log(&format!("sonos: {} leaves the group ({})", room.name, match &r { Ok(_) => "ok".to_owned(), Err(e) => e.clone() }));
            if let Ok(mut j) = JOINED.lock() {
                j.retain(|u| *u != room.uuid);
            }
            restore_tv_input(vec![room.clone()], 600);
        } else {
            let r = soap(&room.ip, "AVTransport", "SetAVTransportURI", &format!("<InstanceID>0</InstanceID><CurrentURI>x-rincon:{}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>", esc(&t.coord_uuid)));
            plx_base::eventlog::log(&format!("sonos: {} joins the group ({})", room.name, match &r { Ok(_) => "ok".to_owned(), Err(e) => e.clone() }));
            if r.is_ok() {
                if let Ok(mut j) = JOINED.lock() {
                    j.push(room.uuid.clone());
                }
            }
        }
        std::thread::sleep(Duration::from_millis(400));
        refresh_zones(&t.coord_ip);
        if let Ok(mut b) = BUSY.lock() {
            b.retain(|u| *u != room.uuid);
        }
        bump();
    });
}

/// Move the whole playing group's volume by `delta`, the rooms keeping their balance.
pub(crate) fn group_volume(delta: i32) {
    let v = GROUP_VOL.load(Ordering::Relaxed);
    if v >= 0 {
        GROUP_VOL.store((v + delta).clamp(0, 100), Ordering::Relaxed);
        bump();
    }
    queue_volume(GROUP, delta);
}

/// Move a room's volume by `delta`.
pub(crate) fn volume(id: &str, delta: i32) {
    if let Ok(mut m) = VOLUMES.lock() {
        if let Some(v) = m.get_or_insert_with(HashMap::new).get_mut(id) {
            *v = (*v + delta).clamp(0, 100);
        }
    }
    bump();
    queue_volume(id, delta);
}

/// Steps pile up while a request is out, and go as one: a held key is a few requests, not a flood, and each one is
/// relative so no read-then-write can lose a step.
fn queue_volume(key: &str, delta: i32) {
    if let Ok(mut p) = VOL_PENDING.lock() {
        match p.iter_mut().find(|(k, _)| k == key) {
            Some(e) => e.1 = (e.1 + delta).clamp(-10, 10),
            None => p.push((key.to_owned(), delta.clamp(-10, 10))),
        }
    }
    if VOL_WORKER.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = std::thread::Builder::new().name("sonos-volume".into()).spawn(|| {
        loop {
            let batch = VOL_PENDING.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
            if batch.is_empty() {
                VOL_WORKER.store(false, Ordering::Release);
                // a step that arrived as the worker was leaving
                let more = VOL_PENDING.lock().is_ok_and(|p| !p.is_empty());
                if more && !VOL_WORKER.swap(true, Ordering::AcqRel) {
                    continue;
                }
                break;
            }
            for (key, d) in batch {
                send_volume(&key, d);
            }
            std::thread::sleep(Duration::from_millis(120));
        }
        refresh_group_volume();
    });
}

fn send_volume(key: &str, d: i32) {
    if d == 0 {
        return;
    }
    if key == GROUP {
        let Some(t) = TARGET.lock().ok().and_then(|t| t.clone()) else { return };
        let _ = soap(&t.coord_ip, "GroupRenderingControl", "SnapshotGroupVolume", "<InstanceID>0</InstanceID>");
        if let Ok(b) = soap(&t.coord_ip, "GroupRenderingControl", "SetRelativeGroupVolume", &format!("<InstanceID>0</InstanceID><Adjustment>{d}</Adjustment>")) {
            if let Some(v) = tag(&b, "NewVolume").and_then(|v| v.trim().parse::<i32>().ok()) {
                GROUP_VOL.store(v, Ordering::Relaxed);
                bump();
            }
        }
        return;
    }
    let Some(room) = ROOMS.lock().ok().and_then(|r| r.iter().find(|r| r.uuid == key).cloned()) else { return };
    if let Ok(b) = soap(&room.ip, "RenderingControl", "SetRelativeVolume", &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><Adjustment>{d}</Adjustment>")) {
        if let Some(v) = tag(&b, "NewVolume").and_then(|v| v.trim().parse::<i32>().ok()) {
            if let Ok(mut m) = VOLUMES.lock() {
                m.get_or_insert_with(HashMap::new).insert(room.uuid.clone(), v);
            }
            bump();
        }
    }
}

/// A room the music is playing in now (a session is loading or playing there).
fn in_use(r: &Room) -> bool {
    active() && TARGET.lock().ok().and_then(|t| t.clone()).is_some_and(|t| t.uuid == r.uuid || t.coord_uuid == r.coord_uuid)
}

/// Switch these rooms back to their TV input (a soundbar left on a Sonos queue is silent for the TV). Rooms with no TV
/// input refuse, which is fine. A room that has music again by then is left alone.
fn restore_tv_input(rooms: Vec<Room>, after_ms: u64) {
    if rooms.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("sonos-tv".into()).spawn(move || {
        std::thread::sleep(Duration::from_millis(after_ms));
        for r in rooms {
            put_back_on_tv(&r);
        }
    });
}

fn put_back_on_tv(r: &Room) {
    if in_use(r) {
        return;
    }
    let uri = format!("x-sonos-htastream:{}:spdif", esc(&r.uuid));
    let set = soap(&r.ip, "AVTransport", "SetAVTransportURI", &format!("<InstanceID>0</InstanceID><CurrentURI>{uri}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"));
    if set.is_ok() {
        let _ = soap(&r.ip, "AVTransport", "Play", "<InstanceID>0</InstanceID><Speed>1</Speed>");
        plx_base::eventlog::log(&format!("sonos: {} is back on its TV input", r.name));
    }
}

/// The music is over on these rooms (the speaker has already been told to stop): its queue is emptied (it held a
/// link with the server's token), rooms this app joined leave the group, and the rooms that played go back to their
/// TV input. Blocking: it runs on the controller's thread.
fn wind_down(playing: Vec<Room>) {
    let Some(lead) = playing.first().cloned() else { return };
    if !in_use(&lead) {
        let _ = soap(&lead.coord_ip, "AVTransport", "RemoveAllTracksFromQueue", "<InstanceID>0</InstanceID>");
    }
    let joined = JOINED.lock().map(|mut j| std::mem::take(&mut *j)).unwrap_or_default();
    for r in playing.iter().filter(|r| joined.contains(&r.uuid)) {
        if !in_use(r) {
            let _ = soap(&r.ip, "AVTransport", "BecomeCoordinatorOfStandaloneGroup", "<InstanceID>0</InstanceID>");
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    for r in &playing {
        put_back_on_tv(r);
    }
    refresh_zones(&lead.coord_ip);
}

fn discover() -> Vec<Room> {
    let mut ips = ssdp();
    if ips.is_empty() {
        ips = scan_subnet();
    }
    plx_base::eventlog::log(&format!("sonos: {} speaker(s) answered", ips.len()));
    for ip in &ips {
        if let Ok(body) = soap(ip, "ZoneGroupTopology", "GetZoneGroupState", "") {
            let rooms = parse_zones(&body);
            if !rooms.is_empty() {
                return rooms;
            }
        }
    }
    Vec::new()
}

fn ssdp() -> Vec<String> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return Vec::new() };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(300)));
    let msg = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:ZonePlayer:1\r\n\r\n";
    let mut ips: Vec<String> = Vec::new();
    let t0 = Instant::now();
    let mut sent = 0;
    let mut buf = [0u8; 2048];
    while t0.elapsed() < Duration::from_millis(2500) {
        if sent < 3 && t0.elapsed() > Duration::from_millis(800 * sent) {
            let _ = sock.send_to(msg.as_bytes(), "239.255.255.250:1900");
            sent += 1;
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            let text = String::from_utf8_lossy(&buf[..n]);
            if text.contains("ZonePlayer") || text.contains("Sonos") {
                let ip = from.ip().to_string();
                if !ips.contains(&ip) {
                    ips.push(ip);
                }
            }
        }
    }
    ips
}

/// This TV's address on the home network (no packet is sent).
fn local_ip() -> Option<std::net::Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

/// Every address of the local /24 that has port 1400 open and says it is a Sonos.
fn scan_subnet() -> Vec<String> {
    let Some(me) = local_ip() else { return Vec::new() };
    let o = me.octets();
    let found: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let hosts: Vec<u8> = (1..=254u8).collect();
    std::thread::scope(|sc| {
        for chunk in hosts.chunks(16) {
            let found = &found;
            sc.spawn(move || {
                for &h in chunk {
                    let ip = format!("{}.{}.{}.{}", o[0], o[1], o[2], h);
                    let Ok(addr) = format!("{ip}:{PORT}").parse::<SocketAddr>() else { continue };
                    if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
                        if let Some((200, body)) = http(&ip, "GET", "/xml/device_description.xml", &[], "", 2000) {
                            if body.contains("Sonos") {
                                found.lock().unwrap_or_else(|e| e.into_inner()).push(ip);
                            }
                        }
                    }
                }
            });
        }
    });
    found.into_inner().unwrap_or_default()
}

// ---- the Plex server, as a speaker can reach it --------------------------------------------------------

/// The Plex server's IPv4 address on the home network, once found (with the server's machine id), and when a look
/// for it last came up empty.
static PLEX_LAN: Mutex<Option<(String, String)>> = Mutex::new(None);
static PLEX_LAN_MISS: Mutex<Option<Instant>> = Mutex::new(None);

/// This TV may reach the server by an IPv6 name, which a speaker cannot use. Then the server's IPv4 address is looked
/// for on the home network (every host of the /24 with Plex's port open that names this server's machine id), once,
/// and remembered. `None`: the client's own address is fine, or the server was not found.
fn plex_home_address(client: &plx_plex::plex::Client) -> Option<String> {
    if client.music_lan_has_address() {
        return None;
    }
    let mid = client.machine_id().to_owned();
    if mid.is_empty() {
        return None;
    }
    if let Some((m, ip)) = PLEX_LAN.lock().ok().and_then(|g| g.clone()) {
        if m == mid {
            return Some(ip);
        }
    }
    if PLEX_LAN_MISS.lock().ok().and_then(|g| *g).is_some_and(|at| at.elapsed() < Duration::from_secs(60)) {
        return None;
    }
    let found = find_plex(&mid, client.port().clamp(1, 65535) as u16);
    match &found {
        Some(_) => plx_base::eventlog::log("sonos: found the Plex server on the home network (speakers cannot use its IPv6 address)"),
        None => plx_base::eventlog::log("sonos: the Plex server was not found at an IPv4 address on the home network"),
    }
    if let Ok(mut g) = PLEX_LAN.lock() {
        *g = found.clone().map(|ip| (mid, ip));
    }
    if let Ok(mut m) = PLEX_LAN_MISS.lock() {
        *m = found.is_none().then(Instant::now);
    }
    found
}

fn find_plex(mid: &str, port: u16) -> Option<String> {
    let me = local_ip()?;
    let o = me.octets();
    let found: Mutex<Option<String>> = Mutex::new(None);
    let want = format!("machineIdentifier=\"{mid}\"");
    let hosts: Vec<u8> = (1..=254u8).collect();
    std::thread::scope(|sc| {
        for chunk in hosts.chunks(4) {
            let (found, want) = (&found, &want);
            sc.spawn(move || {
                for &h in chunk {
                    let ip = format!("{}.{}.{}.{}", o[0], o[1], o[2], h);
                    let Ok(addr) = format!("{ip}:{port}").parse::<SocketAddr>() else { continue };
                    if TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
                        if let Some((200, body)) = http_at(&ip, port, "GET", "/identity", &[], "", 2000) {
                            if body.contains(want.as_str()) {
                                *found.lock().unwrap_or_else(|e| e.into_inner()) = Some(ip);
                            }
                        }
                    }
                }
            });
        }
    });
    found.into_inner().unwrap_or_default()
}

// ---- the wire ----------------------------------------------------------------------------------------

/// One HTTP exchange with the speaker at `ip`: (status, body).
fn http(ip: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str, timeout_ms: u64) -> Option<(u16, String)> {
    http_at(ip, PORT, method, path, headers, body, timeout_ms)
}

fn http_at(ip: &str, port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str, timeout_ms: u64) -> Option<(u16, String)> {
    let addr: SocketAddr = format!("{ip}:{port}").parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(timeout_ms)));
    let _ = s.set_write_timeout(Some(Duration::from_millis(timeout_ms)));
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {ip}:{port}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).ok()?;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                // a speaker's replies are small: a host that never stops is not one
                if out.len() > 4 * 1024 * 1024 {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    let at = out.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&out[..at]).into_owned();
    let rest = &out[at + 4..];
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    let chunked = head.to_ascii_lowercase().contains("transfer-encoding: chunked");
    let body = if chunked { unchunk(rest) } else { rest.to_vec() };
    Some((status, String::from_utf8_lossy(&body).into_owned()))
}

/// A chunked body put back together, on bytes (a chunk's size counts bytes, and may end inside a character).
fn unchunk(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = b;
    while let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = String::from_utf8_lossy(&rest[..eol]);
        let n = usize::from_str_radix(size.split(';').next().unwrap_or("0").trim(), 16).unwrap_or(0);
        let after = &rest[eol + 2..];
        if n == 0 || after.len() < n {
            break;
        }
        out.extend_from_slice(&after[..n]);
        rest = &after[n..];
        while rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    out
}

fn control_path(service: &str) -> &'static str {
    match service {
        "RenderingControl" => "/MediaRenderer/RenderingControl/Control",
        "ZoneGroupTopology" => "/ZoneGroupTopology/Control",
        "GroupRenderingControl" => "/MediaRenderer/GroupRenderingControl/Control",
        _ => "/MediaRenderer/AVTransport/Control",
    }
}

/// One SOAP action; the reply's body, or why it failed.
fn soap(ip: &str, service: &str, action: &str, inner: &str) -> Result<String, String> {
    let urn = format!("urn:schemas-upnp-org:service:{service}:1");
    let body = format!(
        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{urn}\">{inner}</u:{action}></s:Body></s:Envelope>"
    );
    let action_hdr = format!("\"{urn}#{action}\"");
    match http(ip, "POST", control_path(service), &[("Content-Type", "text/xml; charset=\"utf-8\""), ("SOAPACTION", action_hdr.as_str())], &body, 4000) {
        Some((200, b)) => Ok(b),
        Some((code, b)) => Err(format!("{action}: {code} {}", tag(&b, "errorCode").unwrap_or_default())),
        None => Err(format!("{action}: no answer")),
    }
}

fn get_volume(ip: &str) -> Option<i32> {
    let b = soap(ip, "RenderingControl", "GetVolume", "<InstanceID>0</InstanceID><Channel>Master</Channel>").ok()?;
    tag(&b, "CurrentVolume")?.trim().parse().ok()
}

// ---- small XML helpers -------------------------------------------------------------------------------

fn unesc(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn esc(s: &str) -> String {
    // XML has no place for most control characters: one in a title would make the speaker refuse the track
    let s: String = s.chars().filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r')).collect();
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The text inside the first `<name>..</name>`, unescaped.
fn tag(s: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let a = s.find(&open)? + open.len();
    let b = s[a..].find(&format!("</{name}>"))? + a;
    Some(unesc(&s[a..b]))
}

/// The value of `name="..."` inside one tag's text.
fn attr(t: &str, name: &str) -> Option<String> {
    let k = format!(" {name}=\"");
    let a = t.find(&k)? + k.len();
    let b = t[a..].find('"')? + a;
    Some(unesc(&t[a..b]))
}

fn ip_of(location: &str) -> String {
    location.trim_start_matches("http://").split(':').next().unwrap_or("").to_owned()
}

/// The rooms of a `GetZoneGroupState` reply, each with the speaker that leads its group.
fn parse_zones(body: &str) -> Vec<Room> {
    let xml = tag(body, "ZoneGroupState").unwrap_or_default();
    let xml = if xml.contains("<ZoneGroup") { xml } else { unesc(&xml) };
    let mut rooms = Vec::new();
    for g in xml.split("<ZoneGroup ").skip(1) {
        let head_end = g.find('>').unwrap_or(0);
        let coord = attr(&format!(" {}", &g[..head_end]), "Coordinator").unwrap_or_default();
        let group = g.split("</ZoneGroup>").next().unwrap_or(g);
        let members: Vec<(String, String, String, bool)> = group
            .split("<ZoneGroupMember ")
            .skip(1)
            .filter_map(|m| {
                let t = format!(" {}", &m[..m.find('>')?]);
                Some((attr(&t, "UUID")?, attr(&t, "ZoneName")?, ip_of(&attr(&t, "Location")?), attr(&t, "Invisible").as_deref() == Some("1") || attr(&t, "IsZoneBridge").as_deref() == Some("1")))
            })
            .collect();
        let coord_ip = members.iter().find(|m| m.0 == coord).map(|m| m.2.clone()).unwrap_or_default();
        for (uuid, name, ip, invisible) in &members {
            if *invisible || ip.is_empty() || coord_ip.is_empty() {
                continue;
            }
            rooms.push(Room { uuid: uuid.clone(), name: name.clone(), ip: ip.clone(), coord_uuid: coord.clone(), coord_ip: coord_ip.clone() });
        }
    }
    rooms.sort_by(|a, b| a.name.cmp(&b.name));
    rooms.dedup_by(|a, b| a.uuid == b.uuid);
    rooms
}

/// `H:MM:SS` (or `H:MM:SS.mmm`) as milliseconds.
fn hms(s: &str) -> i64 {
    let mut parts = s.trim().split(':');
    let (h, m, sec) = (parts.next(), parts.next(), parts.next());
    let (Some(h), Some(m), Some(sec)) = (h, m, sec) else { return 0 };
    let h: i64 = h.parse().unwrap_or(0);
    let m: i64 = m.parse().unwrap_or(0);
    let sec: f64 = sec.parse().unwrap_or(0.0);
    ((h * 3600 + m * 60) as f64 * 1000.0 + sec * 1000.0) as i64
}

fn fmt_hms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn content_type(i: &MusicItem) -> &'static str {
    match i.codec.as_str() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "aac" | "alac" | "m4a" => "audio/mp4",
        "wav" | "pcm" => "audio/wav",
        "ogg" | "vorbis" | "opus" => "audio/ogg",
        _ => "audio/mpeg",
    }
}

/// The track's metadata as the speaker's queue wants it (what the Sonos app shows).
fn didl(i: &MusicItem, url: &str, art: Option<&str>) -> String {
    let art = art.map(|a| format!("<upnp:albumArtURI>{}</upnp:albumArtURI>", esc(a))).unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\"><item id=\"-1\" parentID=\"-1\" restricted=\"true\"><res protocolInfo=\"http-get:*:{}:*\" duration=\"{}\">{}</res>{art}<dc:title>{}</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><dc:creator>{}</dc:creator><upnp:album>{}</upnp:album></item></DIDL-Lite>",
        content_type(i),
        fmt_hms(i.duration_ms),
        esc(url),
        esc(&i.title),
        esc(i.artist()),
        esc(&i.parent_title),
    )
}

// ---- the controller ----------------------------------------------------------------------------------

#[derive(Debug)]
enum Cmd {
    /// Send the queue and play from this position, this far in.
    Load { pos: usize, offset_ms: i64 },
    Toggle,
    Next,
    Prev,
    Seek(i64),
    Jump(usize),
    Stop,
    /// The play order changed under the track that plays: send it again from the same track.
    Reload,
    /// A track went in right after the one that plays, at this queue position.
    Inserted(usize),
    Repeat,
    /// The controller ends; `stop`: stop the speaker too, `wind`: give its rooms back to the TV.
    Quit { stop: bool, wind: bool },
}

fn send(c: Cmd) {
    if let Some(tx) = CMD.lock().ok().and_then(|g| g.clone()) {
        let _ = tx.send(c);
    }
}

/// Make `id` (empty: this TV) where the music plays. Starting the controller, or ending it. False when there is no
/// such room.
pub(crate) fn choose(id: &str) -> bool {
    let stop = active();
    let playing: Vec<Room> = match TARGET.lock().ok().and_then(|t| t.clone()) {
        Some(t) => ROOMS.lock().map(|r| r.iter().filter(|r| r.coord_uuid == t.coord_uuid).cloned().collect()).unwrap_or_default(),
        None => Vec::new(),
    };
    if id.is_empty() {
        // the old controller stops the speaker, gives the rooms back to the TV, and ends; nothing it does after this
        // touches what the page shows
        CTL_ID.fetch_add(1, Ordering::AcqRel);
        send(Cmd::Quit { stop, wind: true });
        snap_with(|s| *s = Snap::new());
        if let Ok(mut t) = TARGET.lock() {
            *t = None;
        }
        if let Ok(mut c) = CMD.lock() {
            *c = None;
        }
        bump();
        return true;
    }
    let Some(room) = ROOMS.lock().ok().and_then(|r| r.iter().find(|r| r.uuid == id).cloned()) else { return false };
    let same = TARGET.lock().ok().and_then(|t| t.as_ref().map(|t| t.uuid == room.uuid)).unwrap_or(false);
    if same {
        return true;
    }
    // moving to a room of the group that played: the group stays as it is
    let wind = !playing.iter().any(|r| r.uuid == room.uuid);
    let ctl = CTL_ID.fetch_add(1, Ordering::AcqRel) + 1;
    send(Cmd::Quit { stop, wind });
    snap_with(|s| *s = Snap::new());
    let (tx, rx) = channel();
    if let Ok(mut c) = CMD.lock() {
        *c = Some(tx);
    }
    if let Ok(mut t) = TARGET.lock() {
        *t = Some(room.clone());
    }
    let _ = std::thread::Builder::new().name("sonos-ctl".into()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(|| Ctl::new(room, ctl).run(rx))).is_err() {
            plx_base::eventlog::log("sonos: the controller failed");
            if CTL_ID.load(Ordering::Acquire) == ctl {
                if let Ok(mut c) = CMD.lock() {
                    *c = None;
                }
                if let Ok(mut t) = TARGET.lock() {
                    *t = None;
                }
                snap_with(|s| *s = Snap::new());
                super::music::sonos_ended();
            }
        }
    });
    bump();
    true
}

pub(crate) fn load(pos: usize, offset_ms: i64) {
    send(Cmd::Load { pos, offset_ms });
}
pub(crate) fn toggle() {
    send(Cmd::Toggle);
}
pub(crate) fn next() {
    send(Cmd::Next);
}
pub(crate) fn previous() {
    send(Cmd::Prev);
}
pub(crate) fn seek(ms: i64) {
    send(Cmd::Seek(ms));
}
pub(crate) fn jump(pos: usize) {
    send(Cmd::Jump(pos));
}
pub(crate) fn stop() {
    send(Cmd::Stop);
}
pub(crate) fn reload() {
    send(Cmd::Reload);
}
pub(crate) fn inserted(pos: usize) {
    send(Cmd::Inserted(pos));
}
pub(crate) fn repeat_changed() {
    send(Cmd::Repeat);
}

struct Ctl {
    id: u32,
    room: Room,
    /// The queue position (play order) of the speaker's first track.
    base: usize,
    /// How many tracks of the order, from `base`, are in the speaker's queue.
    enq: usize,
    last_qpos: usize,
    last_poll: Instant,
    last_report: Instant,
    /// Consecutive polls with no answer, and with the speaker stopped.
    silent: u32,
    stopped: u32,
    /// Hold off adding tracks until then (a failed add).
    append_after: Instant,
    /// When play was last asked for, whether it was asked again, and the state last seen (for the log).
    started: Instant,
    replayed: bool,
    last_state: String,
    /// The speaker has played something this session, and how many polls in a row showed a track that is not ours.
    played: bool,
    foreign: u32,
}

impl Ctl {
    fn new(room: Room, id: u32) -> Ctl {
        let now = Instant::now();
        Ctl { id, room, base: 0, enq: 0, last_qpos: usize::MAX, last_poll: now, last_report: now, silent: 0, stopped: 0, append_after: now, started: now, replayed: false, last_state: String::new(), played: false, foreign: 0 }
    }

    /// This is still the controller of the session (a replaced one stops touching what the page shows).
    fn mine(&self) -> bool {
        CTL_ID.load(Ordering::Acquire) == self.id
    }

    fn snap<R>(&self, f: impl FnOnce(&mut Snap) -> R) -> Option<R> {
        self.mine().then(|| snap_with(f))
    }

    /// A change of track was asked for: the speaker may be a while fetching it.
    fn expect_change(&mut self) {
        self.started = Instant::now();
        self.replayed = true;
        self.foreign = 0;
    }

    fn av(&self, action: &str, inner: &str) -> Result<String, String> {
        soap(&self.room.coord_ip, "AVTransport", action, inner)
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        plx_base::eventlog::log(&format!("sonos: controlling {} ({})", self.room.name, self.room.coord_ip));
        loop {
            let more = snap_with_peek(|s| s.active && !s.loading) && self.base + self.enq < super::music::sonos_len() && self.enq < MAX_QUEUE;
            let wait = if more { self.append_after.saturating_duration_since(Instant::now()).clamp(Duration::from_millis(1), Duration::from_millis(200)) } else { Duration::from_millis(200) };
            match rx.recv_timeout(wait) {
                Ok(Cmd::Quit { stop, wind }) => {
                    if stop {
                        let _ = self.av("Stop", "<InstanceID>0</InstanceID>");
                    }
                    if wind {
                        self.wind_down();
                    }
                    break;
                }
                Ok(c) => self.command(c),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if snap_with_peek(|s| s.active && !s.loading) {
                if self.last_poll.elapsed() >= Duration::from_millis(1000) {
                    self.last_poll = Instant::now();
                    self.poll();
                }
                if more && Instant::now() >= self.append_after {
                    self.append_one();
                }
            }
        }
        plx_base::eventlog::log("sonos: controller ended");
    }

    fn say(&self, text: &str) {
        super::music::say(text);
    }

    /// The rooms of this controller's group go back to the TV.
    fn wind_down(&self) {
        let rooms: Vec<Room> = ROOMS.lock().map(|r| r.iter().filter(|r| r.coord_uuid == self.room.coord_uuid).cloned().collect()).unwrap_or_default();
        wind_down(if rooms.is_empty() { vec![self.room.clone()] } else { rooms });
    }

    /// The session is over (the queue ended, the speaker was taken over, or the viewer stopped it). `wind`: the rooms
    /// go back to the TV (not when someone else has the speaker now).
    fn end_session(&mut self, stop_speaker: bool, wind: bool) {
        if !self.mine() {
            return;
        }
        if stop_speaker {
            let _ = self.av("Stop", "<InstanceID>0</InstanceID>");
        }
        self.snap(|s| *s = Snap::new());
        super::music::sonos_ended();
        if wind {
            self.wind_down();
        }
    }

    fn command(&mut self, c: Cmd) {
        match c {
            Cmd::Load { pos, offset_ms } => self.load(pos, offset_ms),
            Cmd::Toggle => {
                let playing = snap_with_peek(|s| s.playing);
                let r = self.av(if playing { "Pause" } else { "Play" }, "<InstanceID>0</InstanceID><Speed>1</Speed>");
                if r.is_ok() {
                    self.snap(|s| {
                        s.pos_ms = snapshot_pos(s);
                        s.at = Some(Instant::now());
                        s.playing = !playing;
                    });
                }
            }
            Cmd::Next => {
                self.expect_change();
                let _ = self.av("Next", "<InstanceID>0</InstanceID>");
                self.last_poll = Instant::now() - Duration::from_millis(800);
            }
            Cmd::Prev => {
                let (rel, at) = snapshot().map_or((0, 0), |s| (s.pos_ms, s.qpos));
                if rel > 3000 || (at <= self.base && at == 0) {
                    let _ = self.av("Seek", "<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>0:00:00</Target>");
                } else if at <= self.base {
                    // the speaker's queue starts here: send it again from the track before
                    self.expect_change();
                    self.load(at - 1, 0);
                    return;
                } else {
                    self.expect_change();
                    let _ = self.av("Previous", "<InstanceID>0</InstanceID>");
                }
                self.last_poll = Instant::now() - Duration::from_millis(800);
            }
            Cmd::Seek(ms) => {
                if self.av("Seek", &format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{}</Target>", fmt_hms(ms))).is_ok() {
                    self.snap(|s| {
                        s.pos_ms = ms;
                        s.at = Some(Instant::now());
                    });
                }
            }
            Cmd::Jump(pos) => {
                if pos >= self.base && pos - self.base < self.enq {
                    let n = pos - self.base + 1;
                    self.expect_change();
                    let _ = self.av("Seek", &format!("<InstanceID>0</InstanceID><Unit>TRACK_NR</Unit><Target>{n}</Target>"));
                    let _ = self.av("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>");
                    self.last_poll = Instant::now() - Duration::from_millis(800);
                } else {
                    self.load(pos, 0);
                }
            }
            Cmd::Stop => self.end_session(true, true),
            Cmd::Reload => {
                if let Some(s) = snapshot() {
                    let pos = super::music::sonos_pos();
                    self.load(pos, s.pos_ms);
                }
            }
            Cmd::Inserted(pos) => {
                if pos >= self.base && pos - self.base < self.enq {
                    if let Some(item) = super::music::sonos_item(pos) {
                        if self.add(&item, pos - self.base + 1).is_ok() {
                            self.enq += 1;
                        }
                    }
                }
            }
            Cmd::Repeat => self.set_mode(),
            Cmd::Quit { .. } => {}
        }
    }

    fn set_mode(&self) {
        let mode = match super::music::sonos_repeat() {
            Repeat::Off => "NORMAL",
            Repeat::All => "REPEAT_ALL",
            Repeat::One => "REPEAT_ONE",
        };
        let _ = self.av("SetPlayMode", &format!("<InstanceID>0</InstanceID><NewPlayMode>{mode}</NewPlayMode>"));
    }

    /// Add `item` to the speaker's queue at track number `at` (0 = the end).
    fn add(&self, item: &MusicItem, at: usize) -> Result<(), String> {
        let client = plx_plex::plex::client();
        let via = plex_home_address(client);
        let Some(url) = client.music_lan_url_via(&item.part_key, via.as_deref()) else { return Err(NO_LAN.into()) };
        let art = (!item.cover().is_empty()).then(|| client.music_lan_url_via(item.cover(), via.as_deref())).flatten();
        let meta = didl(item, &url, art.as_deref());
        self.av(
            "AddURIToQueue",
            &format!(
                "<InstanceID>0</InstanceID><EnqueuedURI>{}</EnqueuedURI><EnqueuedURIMetaData>{}</EnqueuedURIMetaData><DesiredFirstTrackNumberEnqueued>{at}</DesiredFirstTrackNumberEnqueued><EnqueueAsNext>0</EnqueueAsNext>",
                esc(&url),
                esc(&meta)
            ),
        )
        .map(|_| ())
    }

    fn append_one(&mut self) {
        let pos = self.base + self.enq;
        let Some(item) = super::music::sonos_item(pos) else { return };
        match self.add(&item, 0) {
            Ok(()) => self.enq += 1,
            Err(e) => {
                plx_base::eventlog::log(&format!("sonos: adding a track failed ({e})"));
                self.append_after = Instant::now() + Duration::from_secs(5);
            }
        }
    }

    /// Replace the speaker's queue with the play order and play from `pos`, `offset_ms` in.
    fn load(&mut self, pos: usize, offset_ms: i64) {
        let len = super::music::sonos_len();
        if len == 0 {
            return;
        }
        let pos = pos.min(len - 1);
        self.snap(|s| {
            *s = Snap::new();
            s.active = true;
            s.loading = true;
            s.qpos = pos;
        });
        // a few tracks behind the one that starts (for Previous); further back the queue is sent again
        self.base = pos.saturating_sub(5);
        self.enq = 0;
        self.last_qpos = usize::MAX;
        self.stopped = 0;
        self.silent = 0;
        plx_base::eventlog::log(&format!("sonos: sending the queue from track {} to {}", pos + 1, self.room.name));
        if let Err(e) = self.start(pos, offset_ms) {
            if !self.mine() {
                return;
            }
            plx_base::eventlog::log(&format!("sonos: could not start ({e})"));
            if e == NO_LAN {
                self.say(&format!("{} cannot reach Plex from the home network.", self.room.name));
            } else {
                self.say(&format!("{} did not start playing.", self.room.name));
            }
            self.end_session(false, true);
        }
    }

    fn start(&mut self, pos: usize, offset_ms: i64) -> Result<(), String> {
        let _ = self.av("Stop", "<InstanceID>0</InstanceID>");
        self.av("RemoveAllTracksFromQueue", "<InstanceID>0</InstanceID>")?;
        for p in self.base..=pos {
            if !self.mine() {
                return Err("replaced".into());
            }
            let item = super::music::sonos_item(p).ok_or("the queue changed")?;
            self.add(&item, 0)?;
            self.enq += 1;
        }
        self.av("SetAVTransportURI", &format!("<InstanceID>0</InstanceID><CurrentURI>x-rincon-queue:{}#0</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>", self.room.coord_uuid))?;
        self.set_mode();
        self.av("Seek", &format!("<InstanceID>0</InstanceID><Unit>TRACK_NR</Unit><Target>{}</Target>", pos - self.base + 1))?;
        if offset_ms > 1000 {
            let _ = self.av("Seek", &format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{}</Target>", fmt_hms(offset_ms)));
        }
        self.av("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")?;
        self.started = Instant::now();
        self.replayed = false;
        self.played = false;
        self.foreign = 0;
        self.snap(|s| {
            s.loading = false;
            s.playing = true;
            s.pos_ms = offset_ms.max(0);
            s.at = Some(Instant::now());
        });
        self.last_poll = Instant::now() - Duration::from_millis(600);
        Ok(())
    }

    /// Ask the speaker where it is.
    fn poll(&mut self) {
        let pos = self.av("GetPositionInfo", "<InstanceID>0</InstanceID>");
        let info = self.av("GetTransportInfo", "<InstanceID>0</InstanceID>");
        let (Ok(pos), Ok(info)) = (pos, info) else {
            self.silent += 1;
            if self.silent >= 6 {
                self.say(&format!("{} stopped answering.", self.room.name));
                self.end_session(false, false);
            }
            return;
        };
        self.silent = 0;
        let state = tag(&info, "CurrentTransportState").unwrap_or_default();
        let track: usize = tag(&pos, "Track").and_then(|t| t.trim().parse().ok()).unwrap_or(0);
        let uri = tag(&pos, "TrackURI").unwrap_or_default();
        let dur = hms(&tag(&pos, "TrackDuration").unwrap_or_default());
        let rel = hms(&tag(&pos, "RelTime").unwrap_or_default());
        if state != self.last_state {
            plx_base::eventlog::log(&format!("sonos: the speaker is {state} (track {track}, {} ms in)", rel));
            self.last_state = state.clone();
        }
        if state == "STOPPED" || track == 0 {
            // the speaker takes a while to start a track it is fetching: stopped is normal at first (a nudge after a
            // few seconds), and only a long stop means it is over
            let since = self.started.elapsed();
            if since < Duration::from_secs(20) {
                if !self.replayed && since > Duration::from_secs(4) {
                    self.replayed = true;
                    plx_base::eventlog::log("sonos: still stopped, asking it to play again");
                    let _ = self.av("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>");
                }
                return;
            }
            self.stopped += 1;
            if self.stopped >= 3 {
                plx_base::eventlog::log("sonos: the speaker stopped, the session ends");
                if !self.played {
                    self.say(&format!("{} could not play this. Can it reach Plex?", self.room.name));
                }
                self.end_session(false, true);
            }
            return;
        }
        self.stopped = 0;
        if state == "PLAYING" {
            self.played = true;
        }
        let qpos = self.base + track - 1;
        let ours = super::music::sonos_item(qpos).is_some_and(|it| {
            let id = it.part_key.split('/').nth(3).unwrap_or("");
            !id.is_empty() && uri.contains(&format!("/library/parts/{id}/"))
        });
        if !ours {
            // between two tracks the speaker may name none for a moment: only a second look in a row ends the session
            if state == "TRANSITIONING" {
                return;
            }
            self.foreign += 1;
            if self.foreign >= 2 {
                plx_base::eventlog::log("sonos: the speaker is playing something else");
                self.say(&format!("{} was taken over.", self.room.name));
                self.end_session(false, false);
            }
            return;
        }
        self.foreign = 0;
        let playing = state == "PLAYING" || state == "TRANSITIONING";
        self.snap(|s| {
            s.qpos = qpos;
            s.pos_ms = rel;
            s.dur_ms = dur;
            s.playing = playing;
            s.at = Some(Instant::now());
        });
        if qpos != self.last_qpos {
            self.last_qpos = qpos;
            super::music::sonos_shown(qpos);
            self.report("playing", rel, dur);
        } else if self.last_report.elapsed() >= Duration::from_secs(10) && playing {
            self.report("playing", rel, dur);
        }
    }

    /// Tell the server what plays (it counts the play and shows it on its dashboard).
    fn report(&mut self, state: &str, time: i64, dur: i64) {
        self.last_report = Instant::now();
        let Some(item) = super::music::sonos_item(self.last_qpos) else { return };
        let state = state.to_owned();
        let _ = std::thread::Builder::new().name("music-timeline".into()).spawn(move || {
            plx_plex::plex::client().music_timeline(&item.rk, &format!("/library/metadata/{}", item.rk), &state, time, dur.max(item.duration_ms));
        });
    }
}

fn snap_with_peek<R>(f: impl FnOnce(&Snap) -> R) -> R {
    f(&SNAP.lock().unwrap_or_else(|e| e.into_inner()))
}

fn snapshot_pos(s: &Snap) -> i64 {
    match (s.playing, s.at) {
        (true, Some(at)) => s.pos_ms + at.elapsed().as_millis() as i64,
        _ => s.pos_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZONES: &str = r#"<s:Envelope><s:Body><u:GetZoneGroupStateResponse><ZoneGroupState>&lt;ZoneGroups&gt;&lt;ZoneGroup Coordinator=&quot;RINCON_A01400&quot; ID=&quot;RINCON_A01400:1&quot;&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_A01400&quot; Location=&quot;http://10.0.0.62:1400/xml/device_description.xml&quot; ZoneName=&quot;Living Room&quot;&gt;&lt;Satellite UUID=&quot;RINCON_S01400&quot; Location=&quot;http://10.0.0.70:1400/xml/device_description.xml&quot; Invisible=&quot;1&quot;/&gt;&lt;/ZoneGroupMember&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_B01400&quot; Location=&quot;http://10.0.0.63:1400/xml/device_description.xml&quot; ZoneName=&quot;Office&quot;/&gt;&lt;/ZoneGroup&gt;&lt;ZoneGroup Coordinator=&quot;RINCON_C01400&quot; ID=&quot;RINCON_C01400:2&quot;&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_C01400&quot; Location=&quot;http://10.0.0.61:1400/xml/device_description.xml&quot; ZoneName=&quot;Media Room&quot;/&gt;&lt;/ZoneGroup&gt;&lt;/ZoneGroups&gt;</ZoneGroupState></u:GetZoneGroupStateResponse></s:Body></s:Envelope>"#;

    #[test]
    fn the_rooms_come_with_the_speaker_that_leads_their_group() {
        let r = parse_zones(ZONES);
        let names: Vec<&str> = r.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["Living Room", "Media Room", "Office"]);
        let office = r.iter().find(|r| r.name == "Office").unwrap();
        assert_eq!(office.ip, "10.0.0.63");
        assert_eq!(office.coord_ip, "10.0.0.62", "Office plays in the Living Room's group");
        assert_eq!(office.coord_uuid, "RINCON_A01400");
        let media = r.iter().find(|r| r.name == "Media Room").unwrap();
        assert_eq!(media.coord_ip, "10.0.0.61");
    }

    #[test]
    fn times_go_both_ways() {
        assert_eq!(hms("0:03:45"), 225_000);
        assert_eq!(hms("1:00:01.5"), 3_601_500);
        assert_eq!(hms("NOT_IMPLEMENTED"), 0);
        assert_eq!(fmt_hms(225_000), "0:03:45");
        assert_eq!(fmt_hms(3_661_000), "1:01:01");
    }

    #[test]
    fn metadata_is_escaped_for_the_queue() {
        let it = MusicItem { title: "Rock & Roll <3".into(), codec: "flac".into(), duration_ms: 61_000, ..Default::default() };
        let d = didl(&it, "http://10.0.0.5:32400/library/parts/1/2/file.flac?X-Plex-Token=a&b=c", None);
        assert!(d.contains("Rock &amp; Roll &lt;3"));
        assert!(d.contains("X-Plex-Token=a&amp;b=c"));
        assert!(d.contains("audio/flac") && d.contains("0:01:01"));
        assert!(tag(&format!("<x>{}</x>", esc(&d)), "x").unwrap() == d, "escaping twice round-trips");
    }

    #[test]
    fn a_chunked_reply_is_put_back_together() {
        assert_eq!(unchunk(b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"), b"hello world");
    }

    #[test]
    fn a_chunk_that_ends_inside_a_character_does_not_panic() {
        // "Küche" is six bytes, the second chunk starting in the middle of the ü
        let body = "Küche".as_bytes();
        let mut wire = Vec::new();
        wire.extend_from_slice(b"2\r\n");
        wire.extend_from_slice(&body[..2]);
        wire.extend_from_slice(b"\r\n4\r\n");
        wire.extend_from_slice(&body[2..]);
        wire.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(String::from_utf8_lossy(&unchunk(&wire)), "Küche");
    }

    #[test]
    fn control_characters_never_reach_the_speaker() {
        assert_eq!(esc("a\u{1}b\u{8}c\td & e"), "abc\td &amp; e");
    }

    #[test]
    fn a_bridge_is_not_a_room() {
        let z = r#"<ZoneGroupState>&lt;ZoneGroups&gt;&lt;ZoneGroup Coordinator=&quot;RINCON_A&quot; ID=&quot;x&quot;&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_A&quot; Location=&quot;http://10.0.0.62:1400/xml/device_description.xml&quot; ZoneName=&quot;Living Room&quot;/&gt;&lt;/ZoneGroup&gt;&lt;ZoneGroup Coordinator=&quot;RINCON_B&quot; ID=&quot;y&quot;&gt;&lt;ZoneGroupMember UUID=&quot;RINCON_B&quot; Location=&quot;http://10.0.0.64:1400/xml/device_description.xml&quot; ZoneName=&quot;BRIDGE&quot; IsZoneBridge=&quot;1&quot;/&gt;&lt;/ZoneGroup&gt;&lt;/ZoneGroups&gt;</ZoneGroupState>"#;
        let names: Vec<String> = parse_zones(z).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["Living Room"]);
    }
}
