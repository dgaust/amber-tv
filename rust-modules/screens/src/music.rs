//! Amber: the Music page (`docs/music-design.md`).
//!
//! One page with its own small history: Recommended (the library's shelves), Artists, Albums and Tracks
//! under a row of tabs, and from them an artist's page, an album's page, Now Playing and the queue. The
//! page owns no data: the application hands it a snapshot ([`MusicModel`], `MusicScreen::input`) and the
//! page asks for what it needs with [`MusicOut`].

use crate::registry::{AppFx, ContentLike};
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, Key, LogicalState, Machine,
};
use plx_machine::present::Provenance;
use plx_plex::plex::music::{MusicHub, MusicItem, MusicKind};
use plx_ui::screen::{
    At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec, HitSource,
    Placed, RenderStrategy, Screen, ScreenEvent, Seat, Step,
};
use plx_ui::{theme, Painter, Rect};
use std::collections::HashMap;
use std::ffi::CString;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MusicStatus {
    #[default]
    Loading,
    Ready,
    /// The server has no music library.
    None,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// One list the page reads: a section listing, an artist's albums, an album's tracks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicList {
    pub items: Vec<MusicItem>,
    pub total: usize,
    pub loading: bool,
    pub loaded: bool,
    pub failed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NowPlaying {
    pub item: MusicItem,
    pub pos_ms: i64,
    pub duration_ms: i64,
    pub playing: bool,
    pub buffering: bool,
    /// What the viewer is waiting for, while a track is on its way (empty once it plays).
    pub status: String,
    /// How far that is, 0..1, or negative when not known.
    pub progress: f32,
    /// Moves a few times a second while waiting, for the moving mark.
    pub anim: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MusicModel {
    pub status: MusicStatus,
    /// The server the art paths belong to (raw id).
    pub sid: u16,
    pub section_title: String,
    pub hubs: Vec<MusicHub>,
    pub lists: HashMap<String, MusicList>,
    pub now: Option<NowPlaying>,
    /// The queue in play order and the position the viewer hears.
    pub queue: Vec<MusicItem>,
    pub qpos: usize,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub note: String,
    /// The viewer pressed blue on another page: open on Now Playing.
    pub open_now_playing: bool,
    /// Another page asked for this album (a card on Home).
    pub open_album: Option<MusicItem>,
}

/// What the page asks of the application.
#[derive(Clone, Debug, PartialEq)]
pub enum MusicOut {
    /// Make sure list `key` is loaded (`hubs`, `artists`, `albums`, `tracks`, `artist:<rk>`, `album:<rk>`, ...).
    Load { key: String },
    More { key: String },
    /// Play these tracks from `start`.
    Play { items: Vec<MusicItem>, start: usize, shuffle: bool },
    PlayNext { item: MusicItem },
    AddToQueue { item: MusicItem },
    TogglePause,
    Next,
    Previous,
    Seek { ms: i64 },
    Stop,
    ToggleShuffle,
    CycleRepeat,
    JumpTo { pos: usize },
    /// Start a radio from a track: the tracks sonically near it follow. Needs Plex Pass and sonic analysis.
    Radio { rk: String },
    /// Rate an item, half stars 0..10 (0 clears it). WRITES to the server.
    Rate { rk: String, half_stars: i32 },
}

#[derive(Clone, Debug, PartialEq)]
enum View {
    Home,
    Artists,
    Albums,
    Tracks,
    Playlists,
    Settings,
    Artist(MusicItem),
    Album(MusicItem),
    NowPlaying,
    Queue,
}

const TABS: [&str; 6] = ["Recommended", "Artists", "Albums", "Tracks", "Playlists", "Settings"];
const ELEM: u32 = 1;
const GROUP: GroupId = GroupId(0);
const LEFT: f32 = plx_ui::consts::MARGIN_X;
const RIGHT: f32 = plx_ui::consts::SCR_W - plx_ui::consts::MARGIN_X - plx_ui::widgets::PAGE_SHIFT_CLOSED;
const SCR_H: f32 = plx_ui::consts::SCR_H;
const TOP: f32 = 150.0;
const COLS: usize = 6;
const GRID_GAP: f32 = 26.0;
/// A grid card and its caption.
const CARD_PITCH_Y: f32 = 322.0;

fn card_w() -> f32 {
    ((RIGHT - LEFT - GRID_GAP * (COLS as f32 - 1.0)) / COLS as f32).floor()
}

pub struct MusicScreen {
    entry: EntryId,
    model: MusicModel,
    seen: bool,
    view: View,
    stack: Vec<(View, usize, usize)>,
    /// The cursor in the current list or grid (flat index) and the first row shown.
    cur: usize,
    /// Which of Play / Shuffle the cursor last stood on (Up from the list goes back to it).
    btn: usize,
    first: usize,
    /// Home: shelf and card.
    hub_row: usize,
    hub_col: usize,
    hub_first: usize,
    tabs: bool,
    /// Now Playing: which row (0 progress, 1 transport, 2 rating) and control.
    np_row: usize,
    np_col: usize,
    /// The rating being set, in half stars.
    stars: i32,
    asked: Vec<String>,
    want_play: Option<(String, bool)>,
    /// The viewer opened a list from here; the first time its data arrives the cursor is reset.
    fresh: bool,
    /// The picture is blacked out while the music plays (any key brings it back).
    dark: bool,
}

impl MusicScreen {
    pub const WORD: &'static str = "music";
    pub const SHAPE: &'static str = "MusicScreen{entry:u32,view:u32,cur:u32,tabs:u32}";

    pub fn new(entry: EntryId) -> Self {
        Self {
            entry,
            model: MusicModel::default(),
            seen: false,
            view: View::Home,
            stack: Vec::new(),
            cur: 0,
            btn: 0,
            first: 0,
            hub_row: 0,
            hub_col: 0,
            hub_first: 0,
            tabs: false,
            np_row: 1,
            np_col: 2,
            stars: 0,
            asked: Vec::new(),
            want_play: None,
            fresh: true,
            dark: false,
        }
    }

    pub fn wants_model(&self) -> bool {
        !self.seen
    }

    /// The application's snapshot. `true` when what the page shows changed.
    pub fn input(&mut self, model: MusicModel) -> bool {
        self.seen = true;
        if model == self.model {
            return false;
        }
        let open = model.open_now_playing && model.now.is_some() && self.view != View::NowPlaying;
        let album = model.open_album.clone();
        self.model = model;
        if open {
            self.stack.clear();
            self.view = View::Home;
            self.push(View::NowPlaying);
        } else if let Some(a) = album {
            self.stack.clear();
            self.view = View::Home;
            self.push(View::Album(a));
        }
        let n = self.len();
        self.cur = self.cur.min(n.saturating_sub(1));
        true
    }

    // ---- what the current view shows -------------------------------------------------------------

    fn key(&self) -> Option<String> {
        match &self.view {
            View::Home => Some("hubs".into()),
            View::Artists => Some("artists".into()),
            View::Albums => Some("albums".into()),
            View::Tracks => Some("tracks".into()),
            View::Playlists => Some("playlists".into()),
            View::Artist(a) => Some(format!("artist:{}", a.rk)),
            View::Album(a) if a.kind == MusicKind::Playlist => Some(format!("playlist:{}", a.rk)),
            View::Album(a) => Some(format!("album:{}", a.rk)),
            View::NowPlaying | View::Queue | View::Settings => None,
        }
    }

    fn list(&self) -> &[MusicItem] {
        match self.key().and_then(|k| self.model.lists.get(&k)) {
            Some(l) => &l.items,
            None => &[],
        }
    }

    /// Items the cursor walks: grids and lists. Artist and album pages put two buttons first.
    fn len(&self) -> usize {
        match &self.view {
            View::Home => 0,
            View::Artist(_) | View::Album(_) => 2 + self.list().len(),
            View::NowPlaying => 0,
            View::Settings => 2,
            View::Queue => self.model.queue.len(),
            _ => self.list().len(),
        }
    }

    fn top_level(&self) -> bool {
        matches!(self.view, View::Home | View::Artists | View::Albums | View::Tracks | View::Playlists | View::Settings)
    }

    fn tab_index(&self) -> usize {
        match self.view {
            View::Artists => 1,
            View::Albums => 2,
            View::Tracks => 3,
            View::Playlists => 4,
            View::Settings => 5,
            _ => 0,
        }
    }

    fn goto_tab(&mut self, t: usize) {
        self.view = [View::Home, View::Artists, View::Albums, View::Tracks, View::Playlists, View::Settings][t.min(5)].clone();
        self.cur = 0;
        self.first = 0;
        self.fresh = true;
    }

    fn push(&mut self, v: View) {
        let old = std::mem::replace(&mut self.view, v);
        self.stack.push((old, self.cur, self.first));
        self.cur = 0;
        self.first = 0;
        self.tabs = false;
        self.fresh = true;
    }

    fn pop(&mut self) -> bool {
        match self.stack.pop() {
            Some((v, cur, first)) => {
                self.view = v;
                self.cur = cur;
                self.first = first;
                true
            }
            None => false,
        }
    }

    /// The list the current view needs and has not asked for yet (asked once, then remembered).
    pub fn next_load(&mut self) -> Option<String> {
        let key = self.key()?;
        if self.model.status != MusicStatus::Ready || self.model.lists.contains_key(&key) && key != "hubs" {
            return None;
        }
        if key == "hubs" && !self.model.hubs.is_empty() {
            return None;
        }
        if self.asked.contains(&key) {
            return None;
        }
        self.asked.push(key.clone());
        Some(key)
    }

    /// Ask for the data the current view needs, once.
    fn want<H: ContentLike>(&mut self, fx: &mut Effects<'_, H>) {
        if let Some(key) = self.next_load() {
            fx.push(Fx::App(AppFx::Music(MusicOut::Load { key })));
        }
    }

    fn out<H: ContentLike>(fx: &mut Effects<'_, H>, o: MusicOut) {
        fx.push(Fx::App(AppFx::Music(o)));
    }

    // ---- keys -----------------------------------------------------------------------------------

    fn grid_visible_rows() -> usize {
        (((SCR_H - TOP) / CARD_PITCH_Y).floor() as usize).max(1)
    }

    fn list_visible_rows(row_h: f32) -> usize {
        (((SCR_H - TOP - 40.0) / row_h).floor() as usize).max(1)
    }

    fn keep_in_view(&mut self, per_row: usize, visible: usize) {
        let row = self.cur / per_row.max(1);
        if row < self.first {
            self.first = row;
        } else if row >= self.first + visible {
            self.first = row + 1 - visible;
        }
    }

    fn key_press<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        if self.model.status != MusicStatus::Ready {
            return Handled::No;
        }
        if self.tabs {
            return self.tab_key(key, fx);
        }
        let h = match self.view.clone() {
            View::Home => self.home_key(key, fx),
            View::Artists | View::Albums | View::Playlists => self.grid_key(key, fx),
            View::Tracks => self.track_list_key(key, fx),
            View::Artist(a) => self.artist_key(key, &a, fx),
            View::Album(a) => self.album_key(key, &a, fx),
            View::NowPlaying => self.np_key(key, fx),
            View::Queue => self.queue_key(key, fx),
            View::Settings => self.settings_key(key),
        };
        fx.invalidate(Provenance::Input);
        h
    }

    fn tab_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let t = self.tab_index();
        match key {
            Key::Left if t > 0 => self.goto_tab(t - 1),
            Key::Left => return Handled::No,
            Key::Right if t + 1 < TABS.len() => self.goto_tab(t + 1),
            Key::Down | Key::Ok => self.tabs = false,
            Key::Up => {}
            _ => return Handled::No,
        }
        self.want(fx);
        fx.invalidate(Provenance::Input);
        Handled::Yes
    }

    fn home_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let hubs = &self.model.hubs;
        if hubs.is_empty() {
            if key == Key::Up {
                self.tabs = true;
                return Handled::Yes;
            }
            return Handled::No;
        }
        match key {
            Key::Up if self.hub_row == 0 => {
                self.tabs = true;
            }
            Key::Up => {
                self.hub_row -= 1;
                self.hub_col = self.hub_col.min(hubs[self.hub_row].items.len().saturating_sub(1));
            }
            Key::Down if self.hub_row + 1 < hubs.len() => {
                self.hub_row += 1;
                self.hub_col = self.hub_col.min(hubs[self.hub_row].items.len().saturating_sub(1));
            }
            Key::Down => {}
            Key::Left if self.hub_col == 0 => return Handled::No,
            Key::Left => self.hub_col -= 1,
            Key::Right => {
                if self.hub_col + 1 < hubs[self.hub_row].items.len() {
                    self.hub_col += 1;
                }
            }
            Key::Ok => {
                if let Some(item) = hubs[self.hub_row].items.get(self.hub_col).cloned() {
                    self.open_item(item, fx);
                }
            }
            _ => return Handled::No,
        }
        let visible = 2;
        if self.hub_row < self.hub_first {
            self.hub_first = self.hub_row;
        } else if self.hub_row >= self.hub_first + visible {
            self.hub_first = self.hub_row + 1 - visible;
        }
        Handled::Yes
    }

    /// OK on a card or row: an artist and an album open their pages, a track plays.
    fn open_item<H: ContentLike>(&mut self, item: MusicItem, fx: &mut Effects<'_, H>) {
        match item.kind {
            MusicKind::Artist => {
                self.push(View::Artist(item));
                self.want(fx);
            }
            MusicKind::Album | MusicKind::Playlist => {
                self.push(View::Album(item));
                self.want(fx);
            }
            MusicKind::Track => {
                Self::out(fx, MusicOut::Play { items: vec![item], start: 0, shuffle: false });
                self.push(View::NowPlaying);
            }
            _ => {}
        }
    }

    fn grid_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let n = self.list().len();
        if n == 0 {
            return if key == Key::Up { self.tabs = true; Handled::Yes } else { Handled::No };
        }
        let col = self.cur % COLS;
        match key {
            Key::Left if col == 0 => return Handled::No,
            Key::Left => self.cur -= 1,
            Key::Right if self.cur + 1 < n && col + 1 < COLS => self.cur += 1,
            Key::Right => {}
            Key::Up if self.cur < COLS => self.tabs = true,
            Key::Up => self.cur -= COLS,
            Key::Down if self.cur + COLS < n => self.cur += COLS,
            Key::Down => {
                // the last row: the last card
                if self.cur / COLS < (n - 1) / COLS {
                    self.cur = n - 1;
                }
            }
            Key::Ok => {
                if let Some(item) = self.list().get(self.cur).cloned() {
                    self.open_item(item, fx);
                    return Handled::Yes;
                }
            }
            _ => return Handled::No,
        }
        self.keep_in_view(COLS, Self::grid_visible_rows());
        // near the end of what has loaded: more
        if let Some(k) = self.key() {
            if self.cur + COLS * 2 >= n && self.model.lists.get(&k).is_some_and(|l| l.items.len() < l.total && !l.loading) {
                Self::out(fx, MusicOut::More { key: k });
            }
        }
        Handled::Yes
    }

    fn track_list_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let n = self.list().len();
        if n == 0 {
            return if key == Key::Up { self.tabs = true; Handled::Yes } else { Handled::No };
        }
        match key {
            Key::Up if self.cur == 0 => self.tabs = true,
            Key::Up => self.cur -= 1,
            Key::Down if self.cur + 1 < n => self.cur += 1,
            Key::Down | Key::Right => {}
            Key::Left => return Handled::No,
            Key::Ok => {
                let items = self.list().to_vec();
                Self::out(fx, MusicOut::Play { items, start: self.cur, shuffle: false });
                self.push(View::NowPlaying);
                return Handled::Yes;
            }
            _ => return Handled::No,
        }
        self.keep_in_view(1, Self::list_visible_rows(64.0));
        if let Some(k) = self.key() {
            if self.cur + 20 >= n && self.model.lists.get(&k).is_some_and(|l| l.items.len() < l.total && !l.loading) {
                Self::out(fx, MusicOut::More { key: k });
            }
        }
        Handled::Yes
    }

    fn artist_key<H: ContentLike>(&mut self, key: Key, artist: &MusicItem, fx: &mut Effects<'_, H>) -> Handled {
        if self.buttons_key(key) {
            return Handled::Yes;
        }
        let n = self.len();
        match key {
            Key::Up if self.cur == 0 => return Handled::Yes,
            Key::Up => self.cur -= 1,
            Key::Down if self.cur + 1 < n => self.cur += 1,
            Key::Down | Key::Right => {}
            Key::Left if self.cur == 1 => self.cur = 0,
            Key::Left => return Handled::No,
            Key::Ok => {
                if self.cur < 2 {
                    // play all of the artist's tracks: ask for them, and play when they arrive
                    let k = format!("tracks-of:{}", artist.rk);
                    self.want_play = Some((k.clone(), self.cur == 1));
                    if !self.model.lists.contains_key(&k) {
                        Self::out(fx, MusicOut::Load { key: k });
                    }
                } else if let Some(album) = self.list().get(self.cur - 2).cloned() {
                    self.push(View::Album(album));
                    self.want(fx);
                }
                return Handled::Yes;
            }
            _ => return Handled::No,
        }
        self.keep_in_view(1, Self::list_visible_rows(112.0));
        Handled::Yes
    }

    fn album_key<H: ContentLike>(&mut self, key: Key, album: &MusicItem, fx: &mut Effects<'_, H>) -> Handled {
        if self.buttons_key(key) {
            return Handled::Yes;
        }
        let n = self.len();
        match key {
            Key::Up if self.cur == 0 => return Handled::Yes,
            Key::Up => self.cur -= 1,
            Key::Down if self.cur + 1 < n => self.cur += 1,
            Key::Down | Key::Right => {}
            Key::Left if self.cur == 1 => self.cur = 0,
            Key::Left => return Handled::No,
            Key::Ok => {
                let items = self.list().to_vec();
                match self.cur {
                    0 => Self::out(fx, MusicOut::Play { items, start: 0, shuffle: false }),
                    1 => Self::out(fx, MusicOut::Play { items, start: 0, shuffle: true }),
                    i => Self::out(fx, MusicOut::Play { items, start: i - 2, shuffle: false }),
                }
                let _ = album;
                self.push(View::NowPlaying);
                return Handled::Yes;
            }
            _ => return Handled::No,
        }
        self.keep_in_view(1, Self::list_visible_rows(64.0).saturating_sub(7).max(3));
        Handled::Yes
    }

    fn settings_key(&mut self, key: Key) -> Handled {
        match key {
            Key::Up if self.cur == 0 => self.tabs = true,
            Key::Up => self.cur -= 1,
            Key::Down if self.cur + 1 < 2 => self.cur += 1,
            Key::Down => {}
            Key::Left if self.cur == 0 && false => {}
            Key::Left => return Handled::No,
            Key::Right | Key::Ok => {
                if self.cur == 0 {
                    plx_data::layout::step_music_crossfade();
                } else {
                    plx_data::layout::toggle_music_levelling();
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn draw_settings(&self, p: Painter, active: bool) {
        let xf = plx_data::layout::music_crossfade();
        let rows = [
            ("Crossfade", if xf == 0 { "Off (gapless)".to_string() } else { format!("{xf} seconds") }, "Tracks overlap by this long. Off plays them back to back with no gap."),
            ("Loudness levelling", if plx_data::layout::music_levelling() { "On".to_string() } else { "Off".to_string() }, "Quietens loud tracks to match the library, using the server's loudness analysis."),
        ];
        for (i, (name, value, help)) in rows.iter().enumerate() {
            let y = TOP + i as f32 * 130.0;
            let f = active && !self.tabs && self.cur == i;
            if f {
                p.rrect(Rect::new(LEFT - 12.0, y - 10.0, RIGHT - LEFT + 24.0, 110.0), 12.0, 12.0, theme::ACCENT);
            }
            let (ink, sub) = if f { (theme::ACCENT_INK, theme::ACCENT_INK) } else { (theme::TEXT_PRIMARY, theme::TEXT_SECONDARY) };
            Self::text(p, name, LEFT + 12.0, y + 26.0, theme::size::BODY, ink, 1, 800.0);
            Self::text(p, help, LEFT + 12.0, y + 66.0, theme::size::MICRO, sub, 0, 1100.0);
            Self::text(p, &format!("{value} \u{203a}"), RIGHT - 460.0, y + 26.0, theme::size::BODY, if f { theme::ACCENT_INK } else { theme::RAIL_CURRENT }, 1, 440.0);
        }
        Self::text(p, "Music is decoded on the TV, so surround mixes play as stereo and hi-res files play at 48 kHz.", LEFT, TOP + 300.0, theme::size::MICRO, theme::TEXT_TERTIARY, 0, 1300.0);
    }

    fn queue_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let n = self.model.queue.len();
        match key {
            Key::Up if self.cur > 0 => self.cur -= 1,
            Key::Down if self.cur + 1 < n => self.cur += 1,
            Key::Up | Key::Down => {}
            Key::Ok => {
                Self::out(fx, MusicOut::JumpTo { pos: self.cur });
                self.pop();
                return Handled::Yes;
            }
            _ => return Handled::No,
        }
        self.keep_in_view(1, Self::list_visible_rows(64.0));
        Handled::Yes
    }

    fn np_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let Some(now) = self.model.now.clone() else { return Handled::No };
        match key {
            Key::Up if self.np_row > 0 => {
                self.np_row -= 1;
                if self.np_row == 3 {
                    self.stars = now.item.user_rating.max(0.0) as i32;
                }
            }
            Key::Down if self.np_row < 3 => {
                self.np_row += 1;
                if self.np_row == 3 {
                    self.stars = now.item.user_rating.max(0.0) as i32;
                }
            }
            Key::Up => {}
            Key::Down => {}
            Key::Left | Key::Right => {
                let dir = if key == Key::Left { -1 } else { 1 };
                match self.np_row {
                    0 => Self::out(fx, MusicOut::Seek { ms: (now.pos_ms + dir * 10_000).clamp(0, now.duration_ms.max(0)) }),
                    1 => {
                        let c = self.np_col as i32 + dir as i32;
                        if (0..5).contains(&c) {
                            self.np_col = c as usize;
                        } else if c < 0 {
                            return Handled::No;
                        }
                    }
                    3 => {
                        let s = (self.stars + dir as i32).clamp(0, 10);
                        self.stars = s;
                    }
                    _ => {
                        let c = self.np_col.min(3) as i32 + dir as i32;
                        if c < 0 {
                            return Handled::No;
                        }
                        self.np_col = (c as usize).min(3);
                    }
                }
            }
            Key::Ok => match self.np_row {
                0 => Self::out(fx, MusicOut::TogglePause),
                1 => match self.np_col {
                    0 => Self::out(fx, MusicOut::ToggleShuffle),
                    1 => Self::out(fx, MusicOut::Previous),
                    2 => Self::out(fx, MusicOut::TogglePause),
                    3 => Self::out(fx, MusicOut::Next),
                    _ => Self::out(fx, MusicOut::CycleRepeat),
                },
                3 => Self::out(fx, MusicOut::Rate { rk: now.item.rk.clone(), half_stars: self.stars }),
                _ => match self.np_col {
                    0 => {
                        self.push(View::Queue);
                        self.cur = self.model.qpos;
                    }
                    1 => {
                        Self::out(fx, MusicOut::Radio { rk: now.item.rk.clone() });
                    }
                    2 => self.dark = true,
                    _ => {
                        Self::out(fx, MusicOut::Stop);
                        self.pop();
                    }
                },
            },
            _ => return Handled::No,
        }
        Handled::Yes
    }

    /// Playback keys, wherever the page is.
    fn media_key<H: ContentLike>(&mut self, wcode: u32, fx: &mut Effects<'_, H>) -> bool {
        use plx_ui::consts::*;
        let playing = self.model.now.is_some();
        match wcode {
            w if w == WCODE_PLAYPAUSE || w == WCODE_PLAY || w == WCODE_PAUSE || w == WCODE_PAUSE_ALT => {
                if playing {
                    Self::out(fx, MusicOut::TogglePause);
                    return true;
                }
            }
            w if w == WCODE_STOP || w == WCODE_STOP_KEY || w == WCODE_STOP_CD => {
                if playing {
                    Self::out(fx, MusicOut::Stop);
                    return true;
                }
            }
            w if w == WCODE_FASTFORWARD || w == WCODE_CH_UP_KEY => {
                if playing {
                    Self::out(fx, MusicOut::Next);
                    return true;
                }
            }
            w if w == WCODE_REWIND || w == WCODE_CH_DOWN_KEY => {
                if playing {
                    Self::out(fx, MusicOut::Previous);
                    return true;
                }
            }
            _ => {}
        }
        false
    }

    // ---- drawing --------------------------------------------------------------------------------

    fn text(p: Painter, s: &str, x: f32, cy: f32, sz: i32, ink: [f32; 4], bold: i32, max_w: f32) {
        let shown = plx_gfx::text::elide(s, max_w.max(10.0), sz, bold, false);
        let Ok(c) = CString::new(shown) else { return };
        let y = plx_gfx::text::text_vcenter_y(sz, bold, cy);
        p.text(c.as_ptr(), x, y, sz, ink, 0, bold);
    }

    /// `s` centred in `r`, measured (not by guessing at the glyphs' width).
    fn centred(p: Painter, s: &str, r: Rect, sz: i32, ink: [f32; 4], bold: i32) {
        let Ok(c) = CString::new(s) else { return };
        let w = plx_gfx::text::text_width(c.as_ptr(), sz, bold).min(r.w - 12.0);
        let y = plx_gfx::text::text_vcenter_y(sz, bold, r.y + r.h * 0.5);
        let shown = plx_gfx::text::elide(s, (r.w - 12.0).max(10.0), sz, bold, false);
        let Ok(cs) = CString::new(shown) else { return };
        p.text(cs.as_ptr(), r.x + (r.w - w) * 0.5, y, sz, ink, 0, bold);
    }

    /// A cover, cropped to fill `r`, with rounded corners (a circle for an artist).
    fn art(&self, p: Painter, path: &str, r: Rect, rad: f32) {
        p.rrect(r, rad, rad, theme::SURFACE_PANEL);
        if path.is_empty() {
            return;
        }
        let (tex, iw, ih) = plx_ui::widgets::resolve_tex_wh_on(self.model.sid, path, 400, 400, 0);
        if tex == 0 || iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let (ra, ia) = (r.w / r.h, iw / ih);
        let uv = if ia > ra {
            let u = (1.0 - ra / ia) * 0.5;
            [u, 0.0, 1.0 - u, 1.0]
        } else {
            let v = (1.0 - ia / ra) * 0.5;
            [0.0, v, 1.0, 1.0 - v]
        };
        p.tex_uv(tex, uv, r, rad, [1.0, 1.0, 1.0, 1.0]);
    }

    fn tabs_row(&self, p: Painter, active: bool) {
        let mut x = LEFT;
        let t = self.tab_index();
        for (i, name) in TABS.iter().enumerate() {
            let w = name.len() as f32 * 15.0 + 48.0;
            let focused = active && self.tabs && i == t;
            let r = Rect::new(x, 52.0, w, 46.0);
            if focused {
                p.rrect(r, 23.0, 23.0, theme::ACCENT);
            } else if i == t {
                p.rrect(r, 23.0, 23.0, theme::with_a(theme::TEXT_PRIMARY, 0.14));
            }
            let ink = if focused { theme::ACCENT_INK } else if i == t { theme::TEXT_PRIMARY } else { theme::TEXT_SECONDARY };
            Self::text(p, name, x + 24.0, r.y + 23.0, theme::size::CAPTION, ink, 1, w - 30.0);
            x += w + 10.0;
        }
        if !self.model.section_title.is_empty() {
            Self::text(p, &self.model.section_title, RIGHT - 520.0, 75.0, theme::size::MICRO, theme::TEXT_TERTIARY, 0, 460.0);
        }
    }

    /// A square card with its caption. `focused` frames the cover only: the text stays as it is.
    fn card(&self, p: Painter, x: f32, y: f32, w: f32, item: &MusicItem, focused: bool) {
        let circle = item.kind == MusicKind::Artist;
        let rad = if circle { w * 0.5 } else { 10.0 };
        if focused {
            let pad = 7.0;
            p.rrect(Rect::new(x - pad, y - pad, w + 2.0 * pad, w + 2.0 * pad), rad + pad, rad + pad, theme::ACCENT);
        }
        self.art(p, item.cover(), Rect::new(x, y, w, w), rad);
        let line2 = match item.kind {
            MusicKind::Artist => {
                if item.leaf_count > 0 { format!("{} albums", item.leaf_count) } else { String::new() }
            }
            MusicKind::Playlist => {
                if item.leaf_count > 0 { format!("{} tracks", item.leaf_count) } else { String::new() }
            }
            MusicKind::Album => {
                if item.year > 0 { format!("{} \u{b7} {}", item.parent_title, item.year) } else { item.parent_title.clone() }
            }
            _ => item.grandparent_title.clone(),
        };
        let top = y + w + 14.0;
        Self::text(p, &item.title, x + 4.0, top + 20.0, theme::size::CAPTION, theme::TEXT_PRIMARY, 1, w - 8.0);
        Self::text(p, &line2, x + 4.0, top + 48.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, w - 8.0);
    }

    fn draw_home(&self, p: Painter, active: bool) {
        let w = card_w();
        if self.model.hubs.is_empty() {
            Self::text(p, "Nothing to show yet.", LEFT, TOP + 60.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 800.0);
            return;
        }
        for (hi, hub) in self.model.hubs.iter().enumerate().skip(self.hub_first).take(2) {
            let y0 = TOP + (hi - self.hub_first) as f32 * 410.0;
            Self::text(p, &hub.title, LEFT, y0, theme::size::BODY, theme::TEXT_PRIMARY, 1, 900.0);
            // the row scrolls so the focused card is in view
            let first = if hi == self.hub_row { self.hub_col.saturating_sub(COLS - 1) } else { 0 };
            for (ci, item) in hub.items.iter().enumerate().skip(first).take(COLS) {
                let x = LEFT + (ci - first) as f32 * (w + GRID_GAP);
                self.card(p, x, y0 + 36.0, w, item, active && !self.tabs && hi == self.hub_row && ci == self.hub_col);
            }
        }
    }

    fn draw_grid(&self, p: Painter, active: bool) {
        let w = card_w();
        let items = self.list();
        if items.is_empty() {
            self.draw_empty(p);
            return;
        }
        let rows = Self::grid_visible_rows() + 1;
        for r in self.first..self.first + rows {
            for c in 0..COLS {
                let i = r * COLS + c;
                let Some(item) = items.get(i) else { break };
                let y = TOP + (r - self.first) as f32 * CARD_PITCH_Y;
                if y > SCR_H {
                    break;
                }
                self.card(p, LEFT + c as f32 * (w + GRID_GAP), y, w, item, active && !self.tabs && i == self.cur);
            }
        }
    }

    fn draw_empty(&self, p: Painter) {
        let text = match self.key().and_then(|k| self.model.lists.get(&k)) {
            Some(l) if l.failed => "That could not be loaded.",
            Some(l) if l.loaded => "Nothing here.",
            _ => "Loading\u{2026}",
        };
        Self::text(p, text, LEFT, TOP + 60.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 800.0);
    }

    fn fmt_time(ms: i64) -> String {
        let s = (ms.max(0) / 1000) as i64;
        format!("{}:{:02}", s / 60, s % 60)
    }

    /// A track row: number, title (and artist when asked), rating, time.
    #[allow(clippy::too_many_arguments)]
    fn track_row(&self, p: Painter, x: f32, y: f32, w: f32, i: usize, t: &MusicItem, focused: bool, with_artist: bool) {
        let playing = self.model.now.as_ref().is_some_and(|n| n.item.rk == t.rk);
        if focused {
            p.rrect(Rect::new(x, y, w, 60.0), 8.0, 8.0, theme::ACCENT);
        }
        let (ink, sub) = if focused { (theme::ACCENT_INK, theme::ACCENT_INK) } else { (theme::TEXT_PRIMARY, theme::TEXT_SECONDARY) };
        let mark = if playing { ">".to_string() } else if t.index > 0 { t.index.to_string() } else { (i + 1).to_string() };
        let mark_ink = if playing && !focused { theme::RAIL_CURRENT } else { sub };
        Self::text(p, &mark, x + 16.0, y + 30.0, theme::size::CAPTION, mark_ink, 0, 50.0);
        let title_w = w - 330.0;
        if with_artist {
            Self::text(p, &t.title, x + 80.0, y + 20.0, theme::size::CAPTION, ink, 1, title_w);
            let line = if t.parent_title.is_empty() { t.grandparent_title.clone() } else { format!("{} \u{b7} {}", t.grandparent_title, t.parent_title) };
            Self::text(p, &line, x + 80.0, y + 44.0, theme::size::MICRO, sub, 0, title_w);
        } else {
            Self::text(p, &t.title, x + 80.0, y + 30.0, theme::size::CAPTION, ink, 1, title_w);
        }
        if t.user_rating > 0.0 {
            let stars = format!("{:.1} stars", t.user_rating / 2.0);
            Self::text(p, &stars, x + w - 300.0, y + 30.0, theme::size::MICRO, if focused { theme::ACCENT_INK } else { theme::RAIL_CURRENT }, 0, 150.0);
        }
        Self::text(p, &Self::fmt_time(t.duration_ms), x + w - 120.0, y + 30.0, theme::size::MICRO, sub, 0, 100.0);
    }

    fn draw_tracks(&self, p: Painter, active: bool) {
        let items = self.list();
        if items.is_empty() {
            self.draw_empty(p);
            return;
        }
        let rows = Self::list_visible_rows(64.0);
        for (k, t) in items.iter().enumerate().skip(self.first).take(rows) {
            let y = TOP + (k - self.first) as f32 * 64.0;
            self.track_row(p, LEFT, y, RIGHT - LEFT, k, t, active && !self.tabs && k == self.cur, true);
        }
    }

    fn button(p: Painter, x: f32, y: f32, w: f32, label: &str, focused: bool) {
        let r = Rect::new(x, y, w, 56.0);
        p.rrect(r, 28.0, 28.0, if focused { theme::ACCENT } else { theme::with_a(theme::TEXT_PRIMARY, 0.14) });
        Self::centred(p, label, r, theme::size::CAPTION, if focused { theme::ACCENT_INK } else { theme::TEXT_PRIMARY }, 1);
    }

    /// A round button wearing an icon: Play and Shuffle on an artist or album page.
    fn icon_button(p: Painter, x: f32, y: f32, icon: plx_ui::icons::Icon, focused: bool) {
        let r = Rect::new(x, y, 72.0, 72.0);
        p.rrect(r, 36.0, 36.0, if focused { theme::ACCENT } else { theme::with_a(theme::TEXT_PRIMARY, 0.14) });
        let d = 34.0;
        plx_ui::icons::draw(p, icon, Rect::new(r.x + (r.w - d) * 0.5, r.y + (r.h - d) * 0.5, d, d), if focused { theme::ACCENT_INK } else { theme::TEXT_PRIMARY });
    }

    /// Play and Shuffle side by side (Left / Right moves between them).
    fn play_shuffle(p: Painter, x: f32, y: f32, cur: usize, active: bool) {
        Self::icon_button(p, x, y, plx_ui::icons::Icon::Play, active && cur == 0);
        Self::icon_button(p, x + 92.0, y, plx_ui::icons::Icon::Shuffle, active && cur == 1);
    }

    /// The keys of that pair of buttons, and the way down to the list and back up. True when it took the key.
    fn buttons_key(&mut self, key: Key) -> bool {
        if self.cur < 2 {
            match key {
                Key::Right => self.cur = 1,
                Key::Left if self.cur == 1 => self.cur = 0,
                Key::Down => {
                    if self.len() > 2 {
                        self.btn = self.cur;
                        self.cur = 2;
                    }
                }
                Key::Up | Key::Left => return key == Key::Up,
                _ => return false,
            }
            true
        } else if key == Key::Up && self.cur == 2 {
            self.cur = self.btn.min(1);
            true
        } else {
            false
        }
    }

    fn draw_album(&self, p: Painter, album: &MusicItem, active: bool) {
        let cover = Rect::new(LEFT, TOP - 20.0, 460.0, 460.0);
        self.art(p, album.cover(), cover, 14.0);
        let tracks = self.list();
        let x = LEFT + 520.0;
        Self::text(p, &album.title, x, TOP + 20.0, 56, theme::TEXT_PRIMARY, 1, RIGHT - x);
        Self::text(p, if album.kind == MusicKind::Playlist { "Playlist" } else { &album.parent_title }, x, TOP + 78.0, theme::size::BODY, theme::RAIL_CURRENT, 0, RIGHT - x);
        let mut meta = Vec::new();
        if album.year > 0 {
            meta.push(album.year.to_string());
        }
        if let Some(g) = album.genres.first() {
            meta.push(g.clone());
        }
        if !tracks.is_empty() {
            let total: i64 = tracks.iter().map(|t| t.duration_ms).sum();
            meta.push(format!("{} tracks, {} min", tracks.len(), total / 60_000));
            if let Some(t) = tracks.first().filter(|_| album.kind != MusicKind::Playlist) {
                if !t.codec.is_empty() {
                    meta.push(t.quality_label());
                }
            }
        }
        Self::text(p, &meta.join("  \u{b7}  "), x, TOP + 120.0, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, RIGHT - x);
        Self::play_shuffle(p, x, TOP + 152.0, self.cur, active);
        if tracks.is_empty() {
            Self::text(p, if self.model.lists.contains_key(&format!("album:{}", album.rk)) { "No tracks." } else { "Loading\u{2026}" }, x, TOP + 280.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 600.0);
            return;
        }
        let rows = 7usize;
        let first = self.first.min(tracks.len().saturating_sub(rows));
        let start = if self.cur >= 2 { first } else { 0 };
        for (k, t) in tracks.iter().enumerate().skip(start).take(rows) {
            let y = TOP + 240.0 + (k - start) as f32 * 64.0;
            self.track_row(p, x, y, RIGHT - x, k, t, active && self.cur == k + 2, false);
        }
    }

    fn draw_artist(&self, p: Painter, artist: &MusicItem, active: bool) {
        let portrait = Rect::new(LEFT, TOP - 10.0, 300.0, 300.0);
        self.art(p, artist.cover(), portrait, 150.0);
        let x = LEFT + 360.0;
        Self::text(p, &artist.title, x, TOP + 40.0, 64, theme::TEXT_PRIMARY, 1, RIGHT - x);
        let mut meta = Vec::new();
        if let Some(g) = artist.genres.first() {
            meta.push(g.clone());
        }
        let albums = self.list();
        if !albums.is_empty() {
            meta.push(format!("{} albums", albums.len()));
        }
        Self::text(p, &meta.join("  \u{b7}  "), x, TOP + 100.0, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, RIGHT - x);
        Self::play_shuffle(p, x, TOP + 140.0, self.cur, active);
        if albums.is_empty() {
            Self::text(p, if self.model.lists.contains_key(&format!("artist:{}", artist.rk)) { "No albums." } else { "Loading\u{2026}" }, x, TOP + 260.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 600.0);
            return;
        }
        let rows = 4usize;
        let start = if self.cur >= 2 { (self.cur - 2).saturating_sub(rows - 1).min(albums.len().saturating_sub(rows)) } else { 0 };
        for (k, a) in albums.iter().enumerate().skip(start).take(rows) {
            let y = TOP + 330.0 + (k - start) as f32 * 112.0;
            let focused = active && self.cur == k + 2;
            if focused {
                p.rrect(Rect::new(x - 10.0, y - 6.0, RIGHT - x + 20.0, 104.0), 10.0, 10.0, theme::ACCENT);
            }
            self.art(p, a.cover(), Rect::new(x, y, 92.0, 92.0), 8.0);
            let (ink, sub) = if focused { (theme::ACCENT_INK, theme::ACCENT_INK) } else { (theme::TEXT_PRIMARY, theme::TEXT_SECONDARY) };
            Self::text(p, &a.title, x + 112.0, y + 30.0, theme::size::BODY, ink, 1, RIGHT - x - 140.0);
            let line = if a.leaf_count > 0 { format!("{}  \u{b7}  {} tracks", a.year, a.leaf_count) } else { a.year.to_string() };
            Self::text(p, &line, x + 112.0, y + 66.0, theme::size::CAPTION, sub, 0, RIGHT - x - 140.0);
        }
    }

    fn draw_queue(&self, p: Painter, active: bool) {
        Self::text(p, "Up next", LEFT, TOP - 20.0, 48, theme::TEXT_PRIMARY, 1, 800.0);
        let q = &self.model.queue;
        if q.is_empty() {
            Self::text(p, "The queue is empty.", LEFT, TOP + 60.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 800.0);
            return;
        }
        let rows = Self::list_visible_rows(64.0) - 1;
        for (k, t) in q.iter().enumerate().skip(self.first).take(rows) {
            let y = TOP + 50.0 + (k - self.first) as f32 * 64.0;
            self.track_row(p, LEFT, y, RIGHT - LEFT, k, t, active && k == self.cur, true);
        }
    }

    fn draw_now_playing(&self, p: Painter, active: bool) {
        let Some(now) = self.model.now.as_ref() else {
            Self::text(p, "Nothing is playing.", LEFT, 400.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 800.0);
            return;
        };
        let it = &now.item;
        self.art(p, it.cover(), Rect::new(LEFT + 40.0, 170.0, 640.0, 640.0), 16.0);
        let x = LEFT + 780.0;
        let w = RIGHT - x;
        Self::text(p, &it.title, x, 220.0, 60, theme::TEXT_PRIMARY, 1, w);
        Self::text(p, it.artist(), x, 290.0, 34, theme::RAIL_CURRENT, 0, w);
        Self::text(p, &it.parent_title, x, 336.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, w);
        if !it.codec.is_empty() {
            Self::text(p, &it.quality_label(), x, 378.0, theme::size::MICRO, theme::TEXT_TERTIARY, 0, w);
        }
        // the progress bar, or what is being waited for
        let by = 440.0;
        let focused = active && self.np_row == 0;
        p.rrect(Rect::new(x, by, w, 8.0), 4.0, 4.0, theme::with_a(theme::TEXT_PRIMARY, 0.22));
        if !now.status.is_empty() {
            if now.progress >= 0.0 {
                p.rrect(Rect::new(x, by, (w * now.progress.clamp(0.0, 1.0)).max(8.0), 8.0), 4.0, 4.0, theme::RAIL_CURRENT);
            } else {
                // not known how long: a mark that travels along the bar
                let travel = (now.anim % 16) as f32 / 16.0;
                let seg = 180.0;
                p.rrect(Rect::new(x + (w - seg) * travel, by, seg, 8.0), 4.0, 4.0, theme::RAIL_CURRENT);
            }
            let dots = ".".repeat((now.anim % 4) as usize);
            Self::text(p, &format!("{}{}", now.status, dots), x, by + 40.0, theme::size::CAPTION, theme::RAIL_CURRENT, 1, w);
        } else {
            let frac = if now.duration_ms > 0 { (now.pos_ms as f32 / now.duration_ms as f32).clamp(0.0, 1.0) } else { 0.0 };
            p.rrect(Rect::new(x, by, (w * frac).max(8.0), 8.0), 4.0, 4.0, theme::RAIL_CURRENT);
            if focused {
                p.rrect(Rect::new(x + w * frac - 12.0, by - 8.0, 24.0, 24.0), 12.0, 12.0, theme::ACCENT);
            }
            Self::text(p, &Self::fmt_time(now.pos_ms), x, by + 38.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, 200.0);
            let left = format!("-{}", Self::fmt_time((now.duration_ms - now.pos_ms).max(0)));
            // right-aligned to the end of the bar
            let lw = CString::new(left.as_str()).map_or(0.0, |c| plx_gfx::text::text_width(c.as_ptr(), theme::size::MICRO, 0));
            Self::text(p, &left, x + w - lw, by + 38.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, 200.0);
        }
        // the transport: shuffle, previous, play/pause, next, repeat, all as icons
        use plx_ui::icons::Icon;
        let repeat = self.model.repeat;
        let ons = [self.model.shuffle, false, false, false, repeat != crate::music::Repeat::Off];
        let ty = 508.0;
        // spread across the width of the bar: the first under its start, the last under its end
        let sizes = [80.0f32, 80.0, 104.0, 80.0, 80.0];
        let gap = ((w - sizes.iter().sum::<f32>()) / 4.0).max(16.0);
        let mut cx = x;
        for (i, on) in ons.iter().enumerate() {
            let big = i == 2;
            let d = sizes[i];
            let r = Rect::new(cx, ty + if big { 0.0 } else { 12.0 }, d, d);
            let f = active && self.np_row == 1 && self.np_col == i;
            p.rrect(r, d * 0.5, d * 0.5, if f { theme::ACCENT } else if *on { theme::with_a(theme::RAIL_CURRENT, 0.45) } else { theme::with_a(theme::TEXT_PRIMARY, 0.14) });
            let ink = if f { theme::ACCENT_INK } else { theme::TEXT_PRIMARY };
            let icon = match i {
                0 => Icon::Shuffle,
                1 => Icon::Previous,
                2 => if now.playing { Icon::Pause } else { Icon::Play },
                3 => Icon::Next,
                _ => Icon::Repeat,
            };
            let g = if big { 46.0 } else { 36.0 };
            plx_ui::icons::draw(p, icon, Rect::new(r.x + (d - g) * 0.5, r.y + (d - g) * 0.5, g, g), ink);
            // repeat one track: a small 1 on the icon
            if i == 4 && repeat == crate::music::Repeat::One {
                let badge = Rect::new(r.x + d - 30.0, r.y + d - 30.0, 24.0, 24.0);
                p.rrect(badge, 12.0, 12.0, theme::RAIL_CURRENT);
                Self::centred(p, "1", badge, theme::size::MICRO, theme::ACCENT_INK, 1);
            }
            cx += d + gap;
        }
        // the rating: five stars in half steps, drawn (a font may lack the glyph)
        let ry = 770.0;
        let rf = active && self.np_row == 3;
        let shown = if rf { self.stars } else if it.user_rating > 0.0 { it.user_rating as i32 } else { 0 };
        if rf {
            p.rrect(Rect::new(x - 12.0, ry - 12.0, 400.0, 60.0), 14.0, 14.0, theme::with_a(theme::TEXT_PRIMARY, 0.14));
        }
        for k in 0..5i32 {
            let sx = x + 8.0 + k as f32 * 70.0;
            p.rrect(Rect::new(sx, ry, 36.0, 36.0), 18.0, 18.0, theme::with_a(theme::TEXT_PRIMARY, 0.25));
            if shown >= 2 * (k + 1) {
                p.rrect(Rect::new(sx, ry, 36.0, 36.0), 18.0, 18.0, theme::RAIL_CURRENT);
            } else if shown == 2 * k + 1 {
                p.rrect(Rect::new(sx, ry, 18.0, 36.0), 9.0, 9.0, theme::RAIL_CURRENT);
            }
        }
        Self::text(p, if rf { "Left / Right to rate, OK to save" } else { "Your rating" }, x + 420.0, ry + 18.0, theme::size::MICRO, theme::TEXT_TERTIARY, 0, 360.0);
        // queue and stop
        let (bw, bgap) = (200.0, ((w - 4.0 * 200.0) / 3.0).max(16.0));
        for (i, label) in ["Queue", "Radio", "Screen off", "Stop"].iter().enumerate() {
            let f = active && self.np_row == 2 && self.np_col == i;
            Self::button(p, x + i as f32 * (bw + bgap), 650.0, bw, label, f);
        }
        // what comes next
        if let Some(next) = self.model.queue.get(self.model.qpos + 1) {
            Self::text(p, "NEXT", x, 880.0, theme::size::MICRO, theme::TEXT_TERTIARY, 1, 200.0);
            Self::text(p, &format!("{}  \u{b7}  {}", next.title, next.artist()), x, 912.0, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, w);
        }
    }

    /// The strip along the bottom while music plays and Now Playing is not showing.
    fn draw_strip(&self, p: Painter) {
        let Some(now) = self.model.now.as_ref() else { return };
        let r = Rect::new(LEFT, SCR_H - 112.0, RIGHT - LEFT, 84.0);
        p.rrect(r, 42.0, 42.0, theme::scrim(0.82));
        self.art(p, now.item.cover(), Rect::new(r.x + 14.0, r.y + 12.0, 60.0, 60.0), 8.0);
        Self::text(p, &now.item.title, r.x + 92.0, r.y + 28.0, theme::size::CAPTION, theme::TEXT_PRIMARY, 1, 700.0);
        Self::text(p, now.item.artist(), r.x + 92.0, r.y + 58.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, 700.0);
        let frac = if now.duration_ms > 0 { (now.pos_ms as f32 / now.duration_ms as f32).clamp(0.0, 1.0) } else { 0.0 };
        p.rrect(Rect::new(r.x + 840.0, r.y + 40.0, 640.0, 4.0), 2.0, 2.0, theme::with_a(theme::TEXT_PRIMARY, 0.22));
        p.rrect(Rect::new(r.x + 840.0, r.y + 40.0, (640.0 * frac).max(4.0), 4.0), 2.0, 2.0, theme::RAIL_CURRENT);
        Self::text(p, if !now.status.is_empty() { "Starting" } else if now.playing { "Playing" } else { "Paused" }, r.x + 1500.0, r.y + 42.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, 120.0);
    }

    fn draw_status(&self, p: Painter) {
        let text = match self.model.status {
            MusicStatus::Loading => "Loading your music\u{2026}",
            MusicStatus::None => "There is no music library on this server.",
            MusicStatus::Failed => "The music library could not be loaded.",
            MusicStatus::Ready => return,
        };
        Self::text(p, text, LEFT, 540.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 1200.0);
    }

    fn focus_rect(&self) -> Rect {
        Rect::new(LEFT, TOP, 300.0, 100.0)
    }
}

impl LogicalState for MusicScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.entry.0).u32(self.cur as u32).u32(self.tabs as u32).u32(self.stack.len() as u32).u32(self.hub_row as u32);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("music status={:?} cur={} hubs={}", self.model.status, self.cur, self.model.hubs.len()));
    }
}

impl<H: ContentLike> Machine<H> for MusicScreen {
    type Ev = ScreenEvent<H>;

    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        let holds = cx.focus.current.is_some_and(|k| k.entry == self.entry && k.elem == ELEM);
        if self.dark {
            // the dark screen takes every press (the key that brings the picture back does nothing else)
            if let ScreenEvent::Input(input) = ev {
                if matches!(input.kind, InputKind::Key { .. }) {
                    if matches!(input.kind, InputKind::Key { edge: Edge::Down, .. }) {
                        self.dark = false;
                        fx.invalidate(Provenance::Input);
                    }
                    return Handled::Yes;
                }
            }
            if matches!(ev, ScreenEvent::PressCommit(_) | ScreenEvent::Activate(_)) {
                self.dark = false;
                fx.invalidate(Provenance::Input);
                return Handled::Yes;
            }
        }
        match ev {
            ScreenEvent::Enter(_) | ScreenEvent::Uncover | ScreenEvent::StoreChanged(..) => {
                self.want(fx);
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { .. } => {
                fx.invalidate(Provenance::Input);
                Handled::Yes
            }
            ScreenEvent::Tick(_) => {
                self.want(fx);
                // an artist's "Play": the tracks have arrived
                if let Some((k, shuffle)) = self.want_play.clone() {
                    if let Some(l) = self.model.lists.get(&k).filter(|l| l.loaded) {
                        let items = l.items.clone();
                        self.want_play = None;
                        Self::out(fx, MusicOut::Play { items, start: 0, shuffle });
                        self.push(View::NowPlaying);
                    }
                }
                Handled::No
            }
            ScreenEvent::Activate(e) if *e == ELEM => self.key_press(Key::Ok, fx),
            ScreenEvent::PressCommit(_) if holds => self.key_press(Key::Ok, fx),
            // a held OK on a card: play it next
            ScreenEvent::PressHold(_) if holds => {
                let item = match &self.view {
                    View::Home => self.model.hubs.get(self.hub_row).and_then(|h| h.items.get(self.hub_col)).cloned(),
                    View::Tracks => self.list().get(self.cur).cloned(),
                    View::Album(_) if self.cur >= 2 => self.list().get(self.cur - 2).cloned(),
                    _ => None,
                };
                if let Some(item) = item.filter(|i| i.kind == MusicKind::Track) {
                    Self::out(fx, MusicOut::PlayNext { item });
                }
                Handled::Yes
            }
            // Back steps back through the page's own history
            ScreenEvent::Input(input)
                if holds && matches!(input.kind, InputKind::Key { key: Key::Back, edge: Edge::Down, .. }) =>
            {
                if self.tabs {
                    self.tabs = false;
                    fx.invalidate(Provenance::Input);
                    return Handled::Yes;
                }
                if self.pop() {
                    self.want(fx);
                    fx.invalidate(Provenance::Input);
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            ScreenEvent::Input(input) if holds && matches!(input.kind, InputKind::Key { key: Key::Other, edge: Edge::Down, wcode, .. } if { self.media_key(wcode, fx) }) => {
                fx.invalidate(Provenance::Input);
                Handled::Yes
            }
            // the yellow key blacks the picture out while music plays
            ScreenEvent::Input(input) if holds && matches!(input.kind, InputKind::Key { wcode: 488, edge: Edge::Down, .. }) => {
                if self.model.now.is_some() {
                    self.dark = true;
                    fx.invalidate(Provenance::Input);
                }
                Handled::Yes
            }
            // the blue key opens Now Playing
            ScreenEvent::Input(input) if holds && matches!(input.kind, InputKind::Key { wcode: 489, edge: Edge::Down, .. }) => {
                if self.model.now.is_some() && self.view != View::NowPlaying {
                    self.push(View::NowPlaying);
                    fx.invalidate(Provenance::Input);
                }
                Handled::Yes
            }
            ScreenEvent::Input(input) if holds => match input.kind {
                InputKind::Key { key, edge: Edge::Down | Edge::Repeat, at_edge: false, .. }
                    if matches!(key, Key::Up | Key::Down | Key::Left | Key::Right) => self.key_press(key, fx),
                _ => Handled::No,
            },
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Focusable<H> for MusicScreen {
    fn groups(&self, _: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        out.push(GroupSpec {
            id: GROUP,
            kind: GroupKind::Row { wrap: false },
            seat: Seat::First,
            reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop, EdgeRule::Stop, EdgeRule::Geometric, EdgeRule::Stop],
            extent: Rect::new(LEFT, TOP, RIGHT - LEFT, 100.0),
            len: 1,
            elem: ElemKind::Control,
        });
    }
    fn group_of(&self, elem: &u32, _: &Cx<'_, H>) -> Option<GroupId> {
        (*elem == ELEM).then_some(GROUP)
    }
    fn neighbour(&self, _: FocusKey<u32>, _: Dir, _: &Cx<'_, H>) -> Step<u32> {
        Step::Edge
    }
    fn place(&self, elem: &u32, _: &Cx<'_, H>, _: At) -> Option<Placed> {
        if *elem != ELEM {
            return None;
        }
        let rect = self.focus_rect();
        Some(Placed { rect, rest_rect: rect, clip: Rect::FULL, index: Some(0) })
    }
    fn reconcile(&self, _: FocusKey<u32>, _: &Cx<'_, H>) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem: ELEM }
    }
    fn seat(&self, _: GroupId, _: Placed, _: &Cx<'_, H>) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem: ELEM }
    }
}

impl<H: ContentLike> Screen<H> for MusicScreen {
    fn name(&self) -> &'static str {
        Self::WORD
    }
    fn state(&self) -> &dyn LogicalState {
        self
    }
    fn crumb(&self, _: &Cx<'_, H>) -> Option<std::borrow::Cow<'_, str>> {
        None
    }
    fn prepare(&mut self, _: &mut plx_ui::frame::Budget, _: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let p = f.painter.alpha(f.page_alpha);
        p.rrect(Rect::new(-200.0, 0.0, plx_ui::consts::SCR_W + 400.0, SCR_H), 0.0, 0.0, theme::SURFACE_APP);
        if self.model.status != MusicStatus::Ready {
            self.draw_status(p);
            return;
        }
        let holds = f.focus.current.is_some_and(|k| k.entry == self.entry && k.elem == ELEM);
        if self.top_level() {
            self.tabs_row(p, holds);
        }
        match &self.view {
            View::Home => self.draw_home(p, holds),
            View::Artists | View::Albums | View::Playlists => self.draw_grid(p, holds),
            View::Tracks => self.draw_tracks(p, holds),
            View::Artist(a) => self.draw_artist(p, a, holds),
            View::Album(a) => self.draw_album(p, a, holds),
            View::NowPlaying => self.draw_now_playing(p, holds),
            View::Queue => self.draw_queue(p, holds),
            View::Settings => self.draw_settings(p, holds),
        }
        if !matches!(self.view, View::NowPlaying) {
            self.draw_strip(p);
        }
        if !self.model.note.is_empty() {
            Self::text(p, &self.model.note, LEFT, SCR_H - 150.0, theme::size::CAPTION, theme::TEXT_PRIMARY, 1, 1200.0);
        }
        if self.dark {
            p.rrect(Rect::new(-200.0, -50.0, plx_ui::consts::SCR_W + 400.0, SCR_H + 100.0), 0.0, 0.0, [0.0, 0.0, 0.0, 1.0]);
        }
    }
    fn render(&self) -> RenderStrategy {
        RenderStrategy::Page
    }
    fn focus_source(&self) -> FocusSource {
        FocusSource::Engine
    }
    fn hit_source(&self) -> HitSource {
        HitSource::Engine
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(rk: &str) -> MusicItem {
        MusicItem { rk: rk.into(), kind: MusicKind::Album, title: format!("album {rk}"), ..Default::default() }
    }

    fn screen_with_albums(n: usize) -> MusicScreen {
        let mut s = MusicScreen::new(EntryId(1));
        let mut m = MusicModel { status: MusicStatus::Ready, ..Default::default() };
        m.lists.insert("albums".into(), MusicList { items: (0..n).map(|i| album(&i.to_string())).collect(), total: n, loaded: true, ..Default::default() });
        s.input(m);
        s.goto_tab(2);
        s
    }

    #[test]
    fn times_read_as_minutes_and_seconds() {
        assert_eq!(MusicScreen::fmt_time(0), "0:00");
        assert_eq!(MusicScreen::fmt_time(65_000), "1:05");
        assert_eq!(MusicScreen::fmt_time(-5), "0:00");
    }

    #[test]
    fn the_tabs_map_to_views_and_back() {
        let mut s = MusicScreen::new(EntryId(1));
        for t in 0..TABS.len() {
            s.goto_tab(t);
            assert_eq!(s.tab_index(), t);
            assert!(s.top_level());
        }
    }

    #[test]
    fn a_grid_asks_for_its_own_list_once() {
        let mut s = MusicScreen::new(EntryId(1));
        let mut m = MusicModel { status: MusicStatus::Ready, ..Default::default() };
        s.input(m.clone());
        s.goto_tab(2);
        assert_eq!(s.next_load().as_deref(), Some("albums"));
        assert_eq!(s.next_load(), None, "asked once");
        m.lists.insert("albums".into(), MusicList::default());
        s.input(m);
        assert_eq!(s.next_load(), None);
    }

    #[test]
    fn the_cursor_stays_inside_the_list_when_the_list_shrinks() {
        let mut s = screen_with_albums(20);
        s.cur = 19;
        let mut m = s.model.clone();
        m.lists.insert("albums".into(), MusicList { items: (0..5).map(|i| album(&i.to_string())).collect(), total: 5, loaded: true, ..Default::default() });
        s.input(m);
        assert_eq!(s.cur, 4);
    }

    #[test]
    fn an_artist_page_counts_two_buttons_before_its_albums() {
        let mut s = MusicScreen::new(EntryId(1));
        let artist = MusicItem { rk: "7".into(), kind: MusicKind::Artist, ..Default::default() };
        let mut m = MusicModel { status: MusicStatus::Ready, ..Default::default() };
        m.lists.insert("artist:7".into(), MusicList { items: vec![album("1"), album("2"), album("3")], total: 3, loaded: true, ..Default::default() });
        s.input(m);
        s.push(View::Artist(artist));
        assert_eq!(s.len(), 5);
        assert!(s.pop());
        assert!(!s.pop(), "nothing left to go back to");
    }

    #[test]
    fn play_and_shuffle_are_side_by_side_and_down_reaches_the_list() {
        let mut s = MusicScreen::new(EntryId(1));
        let artist = MusicItem { rk: "7".into(), kind: MusicKind::Artist, ..Default::default() };
        let mut m = MusicModel { status: MusicStatus::Ready, ..Default::default() };
        m.lists.insert("artist:7".into(), MusicList { items: vec![album("1"), album("2")], total: 2, loaded: true, ..Default::default() });
        s.input(m);
        s.push(View::Artist(artist));
        assert!(s.buttons_key(Key::Right));
        assert_eq!(s.cur, 1, "Right goes from Play to Shuffle");
        assert!(s.buttons_key(Key::Left));
        assert_eq!(s.cur, 0);
        assert!(s.buttons_key(Key::Right));
        assert!(s.buttons_key(Key::Down));
        assert_eq!(s.cur, 2, "Down from Shuffle lands on the first album");
        assert!(s.buttons_key(Key::Up));
        assert_eq!(s.cur, 1, "and Up returns to the button it left");
        assert!(s.buttons_key(Key::Left));
        assert!(!s.buttons_key(Key::Left), "Left on Play is the page's (the sidebar)");
    }

    #[test]
    fn the_card_width_fits_six_columns_between_the_margins() {
        let w = card_w();
        assert!(w > 100.0);
        assert!(LEFT + COLS as f32 * w + (COLS as f32 - 1.0) * GRID_GAP <= RIGHT + 0.5);
    }
}
