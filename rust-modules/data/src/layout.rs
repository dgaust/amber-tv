//! Amber: the viewer's own arrangement of the sidebar and of Home's rows - their order and which
//! are hidden - kept in one small JSON file beside the app's other persistent files, so it
//! survives a reboot and a reinstall. Identities, not positions: a sidebar entry is `search`,
//! `home`, `watchlist` or `lib:<section key>`; a Home row is `<hubIdentifier>|<key>`. An entry the
//! file does not name keeps its default place after the named ones.
//!
//! One cached copy for the process, read once and written through on every change, so the sidebar
//! (application layer) and Home's merge (this crate) can never overwrite each other's half.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    pub sidebar_order: Vec<String>,
    pub sidebar_hidden: Vec<String>,
    pub home_order: Vec<String>,
    pub home_hidden: Vec<String>,
    /// Live TV channels the viewer has tuned, most recent first (guide channel ids).
    pub recent_channels: Vec<String>,
    /// Amber: the viewer's order of the Live TV guide's channels (guide channel ids); a channel it does
    /// not name follows the named ones in the server's order.
    pub channel_order: Vec<String>,
    /// Minutes idle before the screensaver; 0 is off, absent is [`SCREENSAVER_DEFAULT_MINUTES`].
    pub screensaver_minutes: Option<u32>,
    /// Amber music: seconds of crossfade between tracks (0 is gapless), and loudness levelling (0 off, 1 on).
    pub music_crossfade: u8,
    pub music_levelling: u8,
}

static CACHE: Mutex<Option<Layout>> = Mutex::new(None);
static GEN: AtomicU32 = AtomicU32::new(1);
static HOME_ROWS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn path() -> PathBuf {
    let dev = std::path::Path::new("/media/developer");
    if dev.is_dir() {
        return dev.join(format!("{}-layout.json", plx_base::paths::app_id()));
    }
    plx_base::paths::runtime_dir().join("amber-layout.json")
}

fn load() -> Layout {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(l: &Layout) {
    let Ok(bytes) = serde_json::to_vec_pretty(l) else { return };
    let target = path();
    let tmp = target.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &target);
    }
}

/// The current arrangement.
pub fn get() -> Layout {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    c.get_or_insert_with(load).clone()
}

/// Change the arrangement and keep it.
pub fn update(f: impl FnOnce(&mut Layout)) {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let l = c.get_or_insert_with(load);
    f(l);
    save(l);
    GEN.fetch_add(1, Ordering::Relaxed);
}

/// What the Music crossfade setting steps through, in seconds (0 is none: tracks follow each other gapless).
pub const MUSIC_CROSSFADE_CHOICES: [u8; 6] = [0, 2, 4, 6, 8, 12];

pub fn music_crossfade() -> u8 {
    get().music_crossfade
}

pub fn step_music_crossfade() -> u8 {
    let now = music_crossfade();
    let at = MUSIC_CROSSFADE_CHOICES.iter().position(|m| *m == now).unwrap_or(0);
    let next = MUSIC_CROSSFADE_CHOICES[(at + 1) % MUSIC_CROSSFADE_CHOICES.len()];
    update(|l| l.music_crossfade = next);
    next
}

pub fn music_levelling() -> bool {
    get().music_levelling != 0
}

pub fn toggle_music_levelling() -> bool {
    let on = !music_levelling();
    update(|l| l.music_levelling = u8::from(on));
    on
}

/// Idle minutes before the screensaver, unless the viewer chose another.
pub const SCREENSAVER_DEFAULT_MINUTES: u32 = 2;
/// What the Settings row steps through (0 is off).
pub const SCREENSAVER_CHOICES: [u32; 6] = [0, 1, 2, 5, 10, 30];

/// The screensaver's idle time in minutes; 0 means it is off.
pub fn screensaver_minutes() -> u32 {
    get().screensaver_minutes.unwrap_or(SCREENSAVER_DEFAULT_MINUTES)
}

/// The next choice after the current one, wrapping.
pub fn step_screensaver_minutes() -> u32 {
    let now = screensaver_minutes();
    let at = SCREENSAVER_CHOICES.iter().position(|m| *m == now).unwrap_or(0);
    let next = SCREENSAVER_CHOICES[(at + 1) % SCREENSAVER_CHOICES.len()];
    update(|l| l.screensaver_minutes = Some(next));
    next
}

/// "Off", "1 minute", "5 minutes".
pub fn screensaver_label(minutes: u32) -> String {
    match minutes {
        0 => "Off".to_string(),
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

/// Most channels the "Recent Channels" row remembers.
pub const RECENT_CHANNELS: usize = 12;

/// The viewer tuned `id`: it goes to the front of the recent channels.
pub fn note_recent_channel(id: &str) {
    if id.is_empty() { return; }
    update(|l| {
        if l.recent_channels.first().is_some_and(|c| c == id) { return; }
        l.recent_channels.retain(|c| c != id);
        l.recent_channels.insert(0, id.to_string());
        l.recent_channels.truncate(RECENT_CHANNELS);
    });
}

/// Moves on every change: what a derived list re-reads on.
pub fn generation() -> u32 {
    GEN.load(Ordering::Relaxed)
}

/// A Home row's identity.
pub fn home_row_id(hub_id: &str, key: &str) -> String {
    format!("{hub_id}|{key}")
}

/// Home's rows as the last merge arranged them (hidden ones left out): what a move steps through.
pub fn home_rows() -> Vec<String> {
    HOME_ROWS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(crate) fn set_home_rows(rows: Vec<String>) {
    *HOME_ROWS.lock().unwrap_or_else(|e| e.into_inner()) = rows;
}

impl Layout {
    /// `ids` in the viewer's order: the ones the layout names first, in its order, then the rest
    /// in their given order.
    pub fn arrange<'a>(order: &[String], ids: &'a [String]) -> Vec<&'a String> {
        let mut out: Vec<&String> = ids.iter().collect();
        out.sort_by_key(|id| order.iter().position(|o| o == *id).unwrap_or(usize::MAX));
        out
    }

    /// Move `id` one place up (`-1`) or down (`+1`) within `visible`, the current arrangement,
    /// and keep the result as the whole order.
    pub fn step(order: &mut Vec<String>, visible: &[String], id: &str, dir: i32) {
        let mut all: Vec<String> = visible.to_vec();
        let Some(i) = all.iter().position(|x| x == id) else { return };
        let j = i as i32 + dir;
        if j < 0 || j as usize >= all.len() { return; }
        all.swap(i, j as usize);
        *order = all;
    }

    pub fn toggle(hidden: &mut Vec<String>, id: &str) {
        if let Some(i) = hidden.iter().position(|x| x == id) { hidden.remove(i); } else { hidden.push(id.to_owned()); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arrange_puts_named_first_and_keeps_the_rest_in_order() {
        let ids: Vec<String> = ["search", "home", "lib:1", "lib:2", "watchlist"].iter().map(|s| s.to_string()).collect();
        let order = vec!["lib:2".to_string(), "home".to_string()];
        let got: Vec<&str> = Layout::arrange(&order, &ids).into_iter().map(|s| s.as_str()).collect();
        assert_eq!(got, ["lib:2", "home", "search", "lib:1", "watchlist"]);
    }
    #[test]
    fn step_moves_within_the_arrangement() {
        let visible: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let mut order = Vec::new();
        Layout::step(&mut order, &visible, "c", -1);
        assert_eq!(order, ["a", "c", "b"]);
    }
}
