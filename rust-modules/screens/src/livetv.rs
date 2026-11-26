//! Amber: the Live TV guide - the official app's Guide tab.
//!
//! A details panel for the focused programme at the top; under it a time ruler in half hours and
//! one row per channel (number and name on the left, its programmes along the time axis). The
//! focused programme is a white plate; what has already aired of a programme is dimmed. Up/Down
//! move between channels keeping the time, Left/Right between programmes; Left from the earliest
//! one goes to the sidebar. OK on a programme that is on now tunes its channel and plays it.
//!
//! The page owns no data: the application hands it a snapshot (`LiveTvScreen::input`) - the
//! channels with their programmes and the wall clock - and the page asks for a tune with
//! [`LiveTvOut::Tune`].

use crate::registry::{AppFx, ContentLike};
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, Key, LogicalState, Machine,
};
use plx_machine::present::Provenance;
use plx_ui::screen::{
    At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec, HitSource,
    Placed, RenderStrategy, Screen, ScreenEvent, Seat, Step,
};
use plx_ui::page_header::{HeaderItem, HeaderSwap, PageHeader, PageHeaderSpec};
use plx_ui::widgets::{CtlPop, TabStrip};
use plx_ui::{theme, Painter, Rect};
use std::ffi::CString;

/// One airing, as the page draws it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveSlot {
    pub begins: i64,
    pub ends: i64,
    pub title: String,
    pub subtitle: String,
    /// "S4 E3 · 22m · M" - composed by the application.
    pub meta: String,
    pub summary: String,
    /// Landscape art for the details panel, else a poster; a server path.
    pub art: String,
    pub poster: String,
    /// What a recording rule is made for (a show's or film's guid); empty when unknown.
    pub record_guid: String,
    /// The airing's own guid (an episode's); a film's equals `record_guid`.
    pub episode_guid: String,
}

/// Amber: what one line of the Recordings view is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecKind {
    /// A recording the server has lined up.
    #[default]
    Scheduled,
    Series,
    Episode,
    Film,
}

/// Amber: one line of the Recordings view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveRec {
    /// The rule's id (empty for a scheduled recording).
    pub id: String,
    pub kind: RecKind,
    pub title: String,
    pub detail: String,
    /// Unix seconds a scheduled recording begins; 0 for a rule.
    pub begins: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveRow {
    pub id: String,
    pub number: String,
    pub title: String,
    /// The channel's logo, a server path (may be empty).
    pub logo: String,
    pub slots: Vec<LiveSlot>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LiveStatus {
    #[default]
    Loading,
    Ready,
    /// The server has no DVR.
    None,
    Failed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveTvModel {
    pub status: LiveStatus,
    pub rows: Vec<LiveRow>,
    /// A tune in flight: the channel's id.
    pub tuning: Option<String>,
    /// The last tune failed.
    pub tune_failed: bool,
    /// The server the art paths belong to (raw id).
    pub sid: u16,
    /// Amber: the guids a recording rule exists for.
    pub rules: Vec<String>,
    /// Amber: a short, passing status ("Recording Plebs", why a record failed); empty when none.
    pub note: String,
    /// Amber: the Recordings view's lines - the rules, then what is scheduled.
    pub recs: Vec<LiveRec>,
}

/// What the guide asks of the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveTvOut {
    Tune { channel: String },
    /// Amber: record the focused airing's show or film, or cancel its rule (the application knows
    /// which). WRITES to the server.
    /// `show` is what a series rule is made for, `episode` the airing's own (equal for a film).
    Record { show: String, episode: String, title: String },
    /// Amber: end a recording rule from the Recordings view. WRITES to the server.
    CancelRule { id: String },
    /// Amber: move a channel one place up (`-1`) or down (`1`) in the viewer's own order.
    MoveChannel { id: String, dir: i32 },
}

const ELEM: u32 = 1;
/// The recording mark.
const REC_RED: [f32; 4] = [0.93, 0.27, 0.27, 1.0];
/// Group 0: the group a page's fresh entry seats focus in, so opening the guide lands in it.
const GROUP: GroupId = GroupId(0);

const LEFT: f32 = plx_ui::consts::MARGIN_X;
const RIGHT: f32 = plx_ui::consts::SCR_W - plx_ui::consts::MARGIN_X - plx_ui::widgets::PAGE_SHIFT_CLOSED;
/// Amber: the tab row (Guide | Recordings) stands where a library page's header row does; the
/// page's own content starts this far lower than it did without it.
const HEAD_SHIFT: f32 = 128.0;
const RULER_Y: f32 = 388.0 + HEAD_SHIFT;
const RULER_H: f32 = 34.0;
const GRID_Y: f32 = RULER_Y + RULER_H + 10.0;
const ROW_H: f32 = 84.0;
const ROW_GAP: f32 = 8.0;
const CHANNEL_W: f32 = 260.0;
const CELL_GAP: f32 = 6.0;
const X0: f32 = LEFT + CHANNEL_W + CELL_GAP;
/// The guide shows two hours across.
const WINDOW_S: i64 = 2 * 3600;
const HALF_HOUR: i64 = 1800;

/// Amber: the remote's colour keys, as the TV reports them: evdev 398 to 401 (red, green, yellow, blue) and
/// the web keycodes 404 to 406. Not 403: that is the channel rocker's down.
fn is_colour_key(wcode: u32) -> bool {
    matches!(wcode, 398..=401 | 404..=406)
}

fn rows_visible() -> usize {
    (((plx_ui::consts::SCR_H - plx_ui::consts::MARGIN_Y - GRID_Y) / (ROW_H + ROW_GAP)).floor() as usize).max(1)
}

pub struct LiveTvScreen {
    entry: EntryId,
    model: LiveTvModel,
    /// Unix seconds, from the application's clock.
    now: i64,
    row: usize,
    slot: usize,
    first_row: usize,
    window: i64,
    /// Amber: the Recordings view is showing, its selected line and its first visible one.
    recs: bool,
    rec_sel: usize,
    rec_first: usize,
    /// Focus is on the tab row.
    tabs: bool,
    /// Amber: the focused channel has been picked up (a colour key) and Up/Down carry it.
    moving: bool,
    hcaps: TabStrip,
    hpop: CtlPop<1>,
}

impl LiveTvScreen {
    pub const WORD: &'static str = "livetv";
    pub const SHAPE: &'static str = "LiveTvScreen{entry:u32,row:u32,slot:u32,recs:u32,sel:u32,tabs:u32}";

    pub fn new(entry: EntryId) -> Self {
        Self { entry, model: LiveTvModel::default(), now: 0, row: 0, slot: 0, first_row: 0, window: 0, recs: false, rec_sel: 0, rec_first: 0, tabs: false, moving: false, hcaps: TabStrip::new(), hpop: CtlPop::new() }
    }

    /// Amber: a page that has had no snapshot yet; the feed rebuilds one for it whatever its stamp.
    pub fn wants_model(&self) -> bool {
        self.now == 0
    }

    /// The application's snapshot and clock. `true` when what the page shows changed.
    pub fn input(&mut self, model: LiveTvModel, now: i64) -> bool {
        let minute_moved = now / 60 != self.now / 60;
        self.now = now;
        // what focus is on, by identity: a refresh may add or drop airings before it
        let held = self.focused().map(|(row, s)| (row.id.clone(), s.begins));
        let changed = model != self.model;
        if changed {
            let first = self.model.rows.is_empty() && !model.rows.is_empty();
            self.model = model;
            if first {
                self.row = 0;
                self.window = (now / HALF_HOUR) * HALF_HOUR;
                self.slot = self.slot_at(0, now);
            } else if let Some((id, begins)) = held {
                if let Some(r) = self.model.rows.iter().position(|r| r.id == id) {
                    self.row = r;
                    self.slot = self.model.rows[r].slots.iter().position(|s| s.begins == begins)
                        .unwrap_or_else(|| self.slot_at(r, begins));
                }
            }
            self.rec_sel = self.rec_sel.min(self.model.recs.len().saturating_sub(1));
            self.row = self.row.min(self.model.rows.len().saturating_sub(1));
            let n = self.model.rows.get(self.row).map_or(0, |r| r.slots.len());
            self.slot = self.slot.min(n.saturating_sub(1));
        }
        if self.moving { self.follow(); }
        // a programme that finishes while focused hands focus on to what follows it
        let live = self.first_live(self.row);
        let moved_on = self.slot < live;
        if moved_on {
            self.slot = live;
            self.follow();
        }
        changed || minute_moved || moved_on
    }

    fn focused(&self) -> Option<(&LiveRow, &LiveSlot)> {
        let row = self.model.rows.get(self.row)?;
        Some((row, row.slots.get(self.slot)?))
    }

    /// The programme of row `r` on at `t`, else the first after it, else the last.
    fn slot_at(&self, r: usize, t: i64) -> usize {
        let Some(row) = self.model.rows.get(r) else { return 0 };
        row.slots.iter().position(|s| s.begins <= t && t < s.ends)
            .or_else(|| row.slots.iter().position(|s| s.begins >= t))
            .unwrap_or(row.slots.len().saturating_sub(1))
            .max(self.first_live(r))
    }

    /// The first airing of row `r` that has not finished: nothing before it can take focus
    /// (the guide shows what has aired, dimmed, but offers only what is on now or later).
    fn first_live(&self, r: usize) -> usize {
        let Some(row) = self.model.rows.get(r) else { return 0 };
        row.slots.iter().position(|s| s.ends > self.now).unwrap_or(row.slots.len().saturating_sub(1))
    }

    fn floor_now(&self) -> i64 {
        (self.now / HALF_HOUR) * HALF_HOUR
    }

    /// Keep the focused programme inside the two hours shown.
    fn follow(&mut self) {
        let Some((_, s)) = self.focused() else { return };
        let (begins, ends) = (s.begins, s.ends);
        let start = begins.max(self.floor_now());
        if start < self.window {
            self.window = (start / HALF_HOUR) * HALF_HOUR;
        } else if start >= self.window + WINDOW_S - HALF_HOUR {
            self.window = (start / HALF_HOUR) * HALF_HOUR - HALF_HOUR;
        }
        self.window = self.window.max(self.floor_now());
        let _ = ends;
        let visible = rows_visible();
        if self.row < self.first_row {
            self.first_row = self.row;
        } else if self.row >= self.first_row + visible {
            self.first_row = self.row + 1 - visible;
        }
    }

    /// Amber: keys on the tab row. Left and Right move between the tabs and show each as they land
    /// (two tabs: a view each); Down goes back into the page.
    fn tab_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        match key {
            Key::Left if self.recs => self.recs = false,
            Key::Left => return Handled::No,
            Key::Right if !self.recs => {
                self.recs = true;
                self.rec_sel = 0;
                self.rec_first = 0;
            }
            Key::Right | Key::Up | Key::Ok => return Handled::Yes,
            Key::Down => self.tabs = false,
            _ => return Handled::No,
        }
        fx.invalidate(Provenance::Input);
        Handled::Yes
    }

    /// The tab row, laid out the way the library pages' header row is.
    fn header(&self, measure: &dyn plx_machine::machine::Measure) -> (PageHeader, plx_ui::page_header::HeaderLayout) {
        let tab = |label: &str, on: bool| HeaderItem { label: label.to_owned(), selected_plate: on, focusable: true, ..Default::default() };
        let spec = PageHeaderSpec { left: vec![tab("Guide", !self.recs), tab("Recordings", self.recs)], ..Default::default() };
        let page = PageHeader::new(spec, &[], measure);
        let layout = page.layout(plx_ui::consts::SCR_W, measure);
        (page, layout)
    }

    /// Amber: keys in the Recordings view. Up from its first line returns to the guide.
    fn rec_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        let n = self.model.recs.len();
        match key {
            Key::Up if self.rec_sel > 0 => self.rec_sel -= 1,
            Key::Up => self.tabs = true,
            Key::Down if self.rec_sel + 1 < n => self.rec_sel += 1,
            Key::Down | Key::Right | Key::Ok => return Handled::Yes,
            Key::Left => return Handled::No,
            _ => return Handled::No,
        }
        let visible = rows_visible();
        if self.rec_sel < self.rec_first { self.rec_first = self.rec_sel; }
        else if self.rec_sel >= self.rec_first + visible { self.rec_first = self.rec_sel + 1 - visible; }
        fx.invalidate(Provenance::Input);
        Handled::Yes
    }

    /// Amber: keys while a channel is being moved. Up/Down carry it (the application keeps the order and
    /// the page follows it by identity), OK puts it down.
    fn move_key<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        match key {
            Key::Up | Key::Down => {
                let dir = if key == Key::Up { -1 } else { 1 };
                let to = self.row as i32 + dir;
                if to >= 0 && (to as usize) < self.model.rows.len() {
                    if let Some(row) = self.model.rows.get(self.row) {
                        fx.push(Fx::App(AppFx::LiveTv(LiveTvOut::MoveChannel { id: row.id.clone(), dir })));
                    }
                }
            }
            Key::Ok => self.moving = false,
            _ => {}
        }
        fx.invalidate(Provenance::Input);
        Handled::Yes
    }

    fn key_press<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        if self.moving { return self.move_key(key, fx); }
        if self.tabs { return self.tab_key(key, fx); }
        if self.recs { return self.rec_key(key, fx); }
        // Up from the top channel goes to the tab row
        if key == Key::Up && self.row == 0 && self.model.status == LiveStatus::Ready {
            self.tabs = true;
            fx.invalidate(Provenance::Input);
            return Handled::Yes;
        }
        if self.model.rows.is_empty() {
            return Handled::No;
        }
        let anchor = self.focused().map_or(self.now, |(_, s)| s.begins.max(self.window).max(self.now));
        match key {
            Key::Up if self.row > 0 => {
                self.row -= 1;
                self.slot = self.slot_at(self.row, anchor);
            }
            Key::Down if self.row + 1 < self.model.rows.len() => {
                self.row += 1;
                self.slot = self.slot_at(self.row, anchor);
            }
            Key::Up | Key::Down => return Handled::Yes,
            Key::Right => {
                let n = self.model.rows[self.row].slots.len();
                if self.slot + 1 < n { self.slot += 1; } else { return Handled::Yes; }
            }
            Key::Left => {
                // the earliest programme still on the guide: Left goes on to the sidebar
                let earliest = self.slot <= self.first_live(self.row);
                if earliest { return Handled::No; }
                self.slot -= 1;
            }
            Key::Ok => {
                if let Some((row, s)) = self.focused() {
                    if s.begins <= self.now && self.now < s.ends && self.model.tuning.is_none() {
                        fx.push(Fx::App(AppFx::LiveTv(LiveTvOut::Tune { channel: row.id.clone() })));
                    }
                }
                return Handled::Yes;
            }
            _ => return Handled::No,
        }
        self.follow();
        fx.invalidate(Provenance::Input);
        Handled::Yes
    }

    fn x_of(&self, t: i64) -> f32 {
        let per_s = (RIGHT - X0) / WINDOW_S as f32;
        X0 + (t - self.window) as f32 * per_s
    }

    fn row_y(&self, r: usize) -> f32 {
        GRID_Y + (r as f32 - self.first_row as f32) * (ROW_H + ROW_GAP)
    }

    fn focus_rect(&self) -> Rect {
        if self.recs {
            let y = GRID_Y + (self.rec_sel.saturating_sub(self.rec_first)) as f32 * (ROW_H + ROW_GAP);
            return Rect::new(X0, y, RIGHT - X0, ROW_H);
        }
        let Some((_, s)) = self.focused() else { return Rect::new(X0, GRID_Y, 10.0, ROW_H) };
        let x = self.x_of(s.begins).max(X0);
        let r = self.x_of(s.ends).min(RIGHT);
        Rect::new(x, self.row_y(self.row), (r - x - CELL_GAP).max(10.0), ROW_H)
    }

    fn text(p: Painter, s: &str, x: f32, cy: f32, sz: i32, ink: [f32; 4], bold: i32, max_w: f32) {
        let shown = plx_gfx::text::elide(s, max_w.max(10.0), sz, bold, false);
        let Ok(c) = CString::new(shown) else { return };
        let y = plx_gfx::text::text_vcenter_y(sz, bold, cy);
        p.text(c.as_ptr(), x, y, sz, ink, 0, bold);
    }

    /// `s` over at most two lines `max_w` wide, broken between words; the second line is elided
    /// when the rest does not fit. A name that fits on one line leaves the second empty.
    fn two_lines(s: &str, sz: i32, max_w: f32) -> [String; 2] {
        let fits = |t: &str| CString::new(t).is_ok_and(|c| plx_gfx::text::text_width(c.as_ptr(), sz, 0) <= max_w);
        if fits(s) {
            return [s.to_owned(), String::new()];
        }
        let words: Vec<&str> = s.split_whitespace().collect();
        let mut n = 1;
        while n < words.len() && fits(&words[..=n].join(" ")) {
            n += 1;
        }
        [words[..n.min(words.len())].join(" "), words[n.min(words.len())..].join(" ")]
    }

    fn clock(&self, t: i64) -> String {
        let hm = plx_platform::i18n::local_clock_time(t).unwrap_or([0, 0]);
        // CLDR puts a NARROW no-break space before AM/PM, which the UI faces do not carry
        plx_platform::i18n::current().time_of_day(hm).unwrap_or_default().replace('\u{202f}', " ")
    }

    /// The details panel's artwork card: right of the text, under the clock.
    fn art_rect() -> Rect {
        let (w, h) = (416.0, 234.0);
        Rect::new(RIGHT - w, RULER_Y - 20.0 - h, w, h)
    }

    fn draw_art(&self, p: Painter) {
        let Some((_, s)) = self.focused() else { return };
        let path = if !s.art.is_empty() { &s.art } else { &s.poster };
        if path.is_empty() { return; }
        let (tex, iw, ih) = plx_ui::widgets::resolve_tex_wh_on(self.model.sid, path, 960, 960, 0);
        let slot = Self::art_rect();
        // the card takes the picture's shape: a guide's art is often a poster, and cropping a poster
        // to a landscape card cut it to its middle third. Right-aligned, the slot's height.
        let shape = if tex != 0 && iw > 0.0 && ih > 0.0 { (iw / ih).clamp(0.5, 16.0 / 9.0) } else { 2.0 / 3.0 };
        let w = slot.h * shape;
        let r = Rect::new(slot.x + slot.w - w, slot.y, w, slot.h);
        p.rrect(r, 16.0, 16.0, theme::SURFACE_PANEL);
        if tex == 0 || iw <= 0.0 || ih <= 0.0 { return; }
        // cover the card: crop the image to the card's shape
        let (ra, ia) = (r.w / r.h, iw / ih);
        let uv = if ia > ra {
            let u = (1.0 - ra / ia) * 0.5;
            [u, 0.0, 1.0 - u, 1.0]
        } else {
            let v = (1.0 - ia / ra) * 0.5;
            [0.0, v, 1.0, 1.0 - v]
        };
        p.tex_uv(tex, uv, r, 16.0, [1.0, 1.0, 1.0, 1.0]);
    }

    fn draw_details(&self, p: Painter, measure: &dyn plx_machine::machine::Measure) {
        let Some((row, s)) = self.focused() else { return };
        // the text stops short of the artwork card
        let w = (Self::art_rect().x - 48.0 - LEFT).min(1080.0);
        Self::text(p, &s.title, LEFT, 104.0 + HEAD_SHIFT, theme::size::TITLE, theme::TEXT_PRIMARY, 1, w);
        let mut y = 152.0 + HEAD_SHIFT;
        if !s.subtitle.is_empty() {
            Self::text(p, &s.subtitle, LEFT, y, theme::size::BODY, theme::TEXT_PRIMARY, 1, w);
            y += 40.0;
        }
        let mut meta = s.meta.clone();
        let channel = format!("{} {}", row.number, row.title);
        let timing = if s.begins <= self.now && self.now < s.ends {
            format!("{}m left", ((s.ends - self.now) + 59) / 60)
        } else {
            format!("{} - {}", self.clock(s.begins), self.clock(s.ends))
        };
        for part in [timing, channel] {
            if !meta.is_empty() { meta.push_str("  \u{b7}  "); }
            meta.push_str(&part);
        }
        Self::text(p, &meta, LEFT, y, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, w);
        y += 42.0;
        // the synopsis, wrapped to three lines
        let mut line = String::new();
        let mut lines = 0;
        let width = |t: &str| CString::new(t).map_or(0.0, |c| measure.width(&c, theme::size::LABEL, false));
        for word in s.summary.split_whitespace() {
            let trial = if line.is_empty() { word.to_owned() } else { format!("{line} {word}") };
            if width(&trial) > w && !line.is_empty() {
                lines += 1;
                let last = lines == 3;
                Self::text(p, &if last { format!("{line} {word}") } else { line.clone() }, LEFT, y, theme::size::LABEL, theme::TEXT_READING, 0, w);
                y += 34.0;
                line = word.to_owned();
                if last { line.clear(); break; }
            } else {
                line = trial;
            }
        }
        if !line.is_empty() && lines < 3 {
            Self::text(p, &line, LEFT, y, theme::size::LABEL, theme::TEXT_READING, 0, w);
        }
        // Amber: recording - its state, a passing note, and the key that changes it
        if !s.record_guid.is_empty() {
            let series = self.model.rules.iter().any(|g| *g == s.record_guid);
            let single = s.episode_guid != s.record_guid && self.model.rules.iter().any(|g| *g == s.episode_guid);
            let recording = series || single;
            let film = s.episode_guid.is_empty() || s.episode_guid == s.record_guid;
            let hint = if !self.model.note.is_empty() {
                self.model.note.clone()
            } else if series {
                "Recording the series \u{b7} REC to cancel".to_string()
            } else if single {
                "Recording this episode \u{b7} REC again for the series".to_string()
            } else if film {
                "Press REC to record this".to_string()
            } else {
                "Press REC to record this episode".to_string()
            };
            let ink = if recording { REC_RED } else { theme::TEXT_SECONDARY };
            Self::text(p, &hint, LEFT, 352.0 + HEAD_SHIFT, theme::size::CAPTION, ink, if recording { 1 } else { 0 }, w);
        }
    }

    /// "in 3 h", "in 2 days": when a scheduled recording begins, from the clock the page was given.
    fn when(&self, t: i64) -> String {
        let d = t - self.now;
        if d <= 0 { "Recording now".to_string() }
        else if d < 3600 { format!("{} \u{b7} in {} min", self.clock(t), (d + 59) / 60) }
        else if d < 48 * 3600 { format!("{} \u{b7} in {} h", self.clock(t), (d + 1800) / 3600) }
        else { format!("{} \u{b7} in {} days", self.clock(t), d / 86400) }
    }

    /// Amber: the Recordings view - the server's rules, then what it has lined up to record.
    fn draw_recs(&self, p: Painter, active: bool) {
        let recs = &self.model.recs;
        let rules = recs.iter().filter(|r| r.kind != RecKind::Scheduled).count();
        let count = format!("{} rule{} \u{b7} {} scheduled", rules, if rules == 1 { "" } else { "s" }, recs.len() - rules);
        Self::text(p, &count, LEFT, 104.0 + HEAD_SHIFT, theme::size::TITLE, theme::TEXT_PRIMARY, 1, 1000.0);
        let hint = if !self.model.note.is_empty() {
            self.model.note.clone()
        } else {
            match recs.get(self.rec_sel) {
                None => "Nothing is set to record yet. Press REC on a programme in the guide.".to_string(),
                Some(r) if r.kind == RecKind::Scheduled => "Lined up by a rule".to_string(),
                Some(_) => "Press REC to cancel this rule".to_string(),
            }
        };
        Self::text(p, &hint, LEFT, 352.0 + HEAD_SHIFT, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, 1200.0);
        let band = theme::with_a(theme::TEXT_PRIMARY, 0.06);
        p.rrect(Rect::new(LEFT, RULER_Y, CHANNEL_W, RULER_H), 4.0, 4.0, band);
        Self::text(p, "TYPE", LEFT + 16.0, RULER_Y + RULER_H * 0.5, theme::size::MICRO, theme::TEXT_SECONDARY, 1, CHANNEL_W);
        p.rrect(Rect::new(X0, RULER_Y, RIGHT - X0, RULER_H), 4.0, 4.0, band);
        Self::text(p, "RECORDING", X0 + 14.0, RULER_Y + RULER_H * 0.5, theme::size::MICRO, theme::TEXT_SECONDARY, 1, 400.0);
        let idle = theme::with_a(theme::TEXT_PRIMARY, 0.08);
        for i in self.rec_first..(self.rec_first + rows_visible()).min(recs.len()) {
            let r = &recs[i];
            let y = GRID_Y + (i - self.rec_first) as f32 * (ROW_H + ROW_GAP);
            p.rrect(Rect::new(LEFT, y, CHANNEL_W, ROW_H), 6.0, 6.0, theme::with_a(theme::TEXT_PRIMARY, 0.05));
            let (label, dots) = match r.kind {
                RecKind::Series => ("SERIES", 3),
                RecKind::Episode => ("EPISODE", 1),
                RecKind::Film => ("FILM", 1),
                RecKind::Scheduled => ("SCHEDULED", 0),
            };
            let mut x = LEFT + 16.0;
            for (k, a) in [1.0f32, 0.6, 0.3].iter().take(dots).enumerate() {
                let c = [REC_RED[0], REC_RED[1], REC_RED[2], REC_RED[3] * a];
                p.rrect(Rect::new(x + 15.0 * k as f32, y + ROW_H * 0.5 - 6.0, 12.0, 12.0), 6.0, 6.0, c);
            }
            if dots > 0 { x += 15.0 * dots as f32 + 8.0; }
            Self::text(p, label, x, y + ROW_H * 0.5, theme::size::CAPTION, theme::TEXT_SECONDARY, 1, LEFT + CHANNEL_W - 10.0 - x);
            let focused = active && i == self.rec_sel;
            let cell = Rect::new(X0, y, RIGHT - X0, ROW_H);
            p.rrect(cell, 6.0, 6.0, if focused { theme::ACCENT } else { idle });
            let (ink, sub) = if focused { (theme::ACCENT_INK, theme::ACCENT_INK) } else { (theme::TEXT_PRIMARY, theme::TEXT_SECONDARY) };
            Self::text(p, &r.title, X0 + 14.0, y + 28.0, theme::size::CAPTION, ink, if focused { 1 } else { 0 }, cell.w - 28.0);
            let line = match r.kind {
                RecKind::Scheduled => if r.detail.is_empty() { self.when(r.begins) } else { format!("{} \u{b7} {}", self.when(r.begins), r.detail) },
                RecKind::Series => "Every new episode".to_string(),
                _ => r.detail.clone(),
            };
            Self::text(p, &line, X0 + 14.0, y + 58.0, theme::size::MICRO, sub, 0, cell.w - 28.0);
        }
    }

    fn draw_ruler(&self, p: Painter) {
        let band = theme::with_a(theme::TEXT_PRIMARY, 0.06);
        p.rrect(Rect::new(LEFT, RULER_Y, CHANNEL_W, RULER_H), 4.0, 4.0, band);
        Self::text(p, "TODAY", LEFT + 16.0, RULER_Y + RULER_H * 0.5, theme::size::MICRO, theme::TEXT_SECONDARY, 1, CHANNEL_W);
        let mut t = self.window;
        while t < self.window + WINDOW_S {
            let x = self.x_of(t);
            let w = self.x_of(t + HALF_HOUR) - x - CELL_GAP;
            p.rrect(Rect::new(x, RULER_Y, w.min(RIGHT - x), RULER_H), 4.0, 4.0, band);
            Self::text(p, &self.clock(t), x + 14.0, RULER_Y + RULER_H * 0.5, theme::size::MICRO, theme::TEXT_SECONDARY, 0, w - 20.0);
            t += HALF_HOUR;
        }
        // where "now" is
        let xn = self.x_of(self.now);
        if xn > X0 && xn < RIGHT {
            p.rrect(Rect::new(xn - 1.0, RULER_Y, 2.0, plx_ui::consts::SCR_H - RULER_Y - plx_ui::consts::MARGIN_Y), 1.0, 1.0,
                theme::RAIL_CURRENT);
        }
    }

    /// `active`: focus is in the guide. Once Left has handed it to the sidebar the guide shows no
    /// focused plate, so the page never looks as if it still held focus.
    fn draw_grid(&self, p: Painter, active: bool) {
        let visible = rows_visible();
        let idle = theme::with_a(theme::TEXT_PRIMARY, 0.08);
        let aired = theme::scrim(0.35);
        for r in self.first_row..(self.first_row + visible).min(self.model.rows.len()) {
            let row = &self.model.rows[r];
            let y = self.row_y(r);
            let carried = self.moving && r == self.row;
            p.rrect(Rect::new(LEFT, y, CHANNEL_W, ROW_H), 6.0, 6.0, if carried { theme::with_a(theme::RAIL_CURRENT, 0.45) } else { theme::with_a(theme::TEXT_PRIMARY, 0.05) });
            // the logo, fitted into a box on the left; the number (and the name, without a logo) beside it
            let logo = Rect::new(LEFT + 10.0, y + 12.0, 80.0, ROW_H - 24.0);
            let drew = !row.logo.is_empty() && {
                let (tex, iw, ih) = plx_ui::widgets::resolve_tex_wh_on(self.model.sid, &row.logo, 160, 120, 1);
                if tex != 0 && iw > 0.0 && ih > 0.0 {
                    let k = (logo.w / iw).min(logo.h / ih);
                    let (w, h) = (iw * k, ih * k);
                    p.tex(tex, Rect::new(logo.x + (logo.w - w) * 0.5, logo.y + (logo.h - h) * 0.5, w, h), 0.0, [1.0, 1.0, 1.0, 1.0]);
                    true
                } else { false }
            };
            let tx = if drew { logo.x + logo.w + 12.0 } else { LEFT + 14.0 };
            let tw = LEFT + CHANNEL_W - 10.0 - tx;
            // the number, then the name over up to two lines (it was cut after five letters)
            let [first, second] = Self::two_lines(&row.title, theme::size::MICRO, tw);
            let name_y = if second.is_empty() { y + 56.0 } else { y + 46.0 };
            Self::text(p, &row.number, tx, if second.is_empty() { y + 26.0 } else { y + 20.0 }, theme::size::CAPTION, theme::RAIL_CURRENT, 1, tw);
            Self::text(p, &first, tx, name_y, theme::size::MICRO, theme::TEXT_SECONDARY, 0, tw);
            if !second.is_empty() {
                Self::text(p, &second, tx, name_y + 22.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, tw);
            }
            for (i, s) in row.slots.iter().enumerate() {
                if s.ends <= self.window || s.begins >= self.window + WINDOW_S { continue; }
                let x = self.x_of(s.begins).max(X0);
                let right = self.x_of(s.ends).min(RIGHT);
                let w = right - x - CELL_GAP;
                if w < 4.0 { continue; }
                let cell = Rect::new(x, y, w, ROW_H);
                let focused = active && r == self.row && i == self.slot;
                if focused {
                    p.rrect(cell, 6.0, 6.0, theme::ACCENT);
                } else {
                    p.rrect(cell, 6.0, 6.0, idle);
                    // what has already aired of it
                    let xn = self.x_of(self.now).min(right);
                    if xn > x {
                        p.rrect(Rect::new(x, y, (xn - x).min(w), ROW_H), 6.0, 6.0, aired);
                    }
                }
                let (ink, sub) = if focused { (theme::ACCENT_INK, theme::ACCENT_INK) } else { (theme::TEXT_PRIMARY, theme::TEXT_SECONDARY) };
                if w > 40.0 && !s.record_guid.is_empty() {
                    let series = self.model.rules.iter().any(|g| *g == s.record_guid);
                    let single = !series && !s.episode_guid.is_empty() && self.model.rules.iter().any(|g| *g == s.episode_guid);
                    if series && w > 80.0 {
                        // the series: three dots, each fainter than the last
                        for (i, a) in [1.0f32, 0.6, 0.3].iter().enumerate() {
                            let c = [REC_RED[0], REC_RED[1], REC_RED[2], REC_RED[3] * a];
                            p.rrect(Rect::new(x + w - 22.0 - 15.0 * i as f32, y + 10.0, 12.0, 12.0), 6.0, 6.0, c);
                        }
                    } else if series || single {
                        p.rrect(Rect::new(x + w - 22.0, y + 10.0, 12.0, 12.0), 6.0, 6.0, REC_RED);
                    }
                }
                let tw = w - 24.0;
                // a sliver too narrow to read carries no text at all; a narrow slot carries its
                // title alone, centred, rather than a title and a squashed time range
                let tuning_here = focused && (self.model.tuning.as_deref() == Some(row.id.as_str()) || self.model.tune_failed);
                if tw > 56.0 && tw < 150.0 && !tuning_here {
                    Self::text(p, &s.title, x + 14.0, y + ROW_H * 0.5, theme::size::CAPTION, ink, if focused { 1 } else { 0 }, tw);
                } else if tw > 56.0 {
                    Self::text(p, &s.title, x + 14.0, y + 28.0, theme::size::CAPTION, ink, if focused { 1 } else { 0 }, tw);
                    // the tune's state is told quietly, on the focused airing's own second line
                    let line = if focused && self.model.tuning.as_deref() == Some(row.id.as_str()) {
                        "Tuning\u{2026}".to_string()
                    } else if focused && self.model.tune_failed {
                        "Couldn't tune".to_string()
                    } else if focused && s.begins <= self.now && self.now < s.ends {
                        format!("{}m left", ((s.ends - self.now) + 59) / 60)
                    } else {
                        format!("{} - {}", self.clock(s.begins), self.clock(s.ends))
                    };
                    Self::text(p, &line, x + 14.0, y + 58.0, theme::size::MICRO, sub, 0, tw);
                }
            }
        }
    }

    fn draw_status(&self, p: Painter) {
        let text = match self.model.status {
            LiveStatus::Loading => "Loading the guide\u{2026}",
            LiveStatus::None => "There is no Live TV on this server.",
            LiveStatus::Failed => "The guide could not be loaded.",
            LiveStatus::Ready if self.model.rows.is_empty() => "No channels.",
            LiveStatus::Ready => return,
        };
        Self::text(p, text, LEFT, 540.0, theme::size::BODY, theme::TEXT_SECONDARY, 0, 1200.0);
    }
}

impl LogicalState for LiveTvScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.entry.0).u32(self.row as u32).u32(self.slot as u32).u32(self.recs as u32).u32(self.rec_sel as u32).u32(self.tabs as u32);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("livetv rows={} row={} slot={}", self.model.rows.len(), self.row, self.slot));
    }
}

impl<H: ContentLike> Machine<H> for LiveTvScreen {
    type Ev = ScreenEvent<H>;

    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        // The guide answers keys only while it HOLDS focus: once Left has handed focus to the
        // sidebar, Up/Down/Right/OK are the sidebar's, never the grid's underneath it.
        let holds = cx.focus.current.is_some_and(|k| k.entry == self.entry && k.elem == ELEM);
        match ev {
            ScreenEvent::Enter(_) | ScreenEvent::Uncover | ScreenEvent::StoreChanged(..) => {
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { .. } => {
                fx.invalidate(Provenance::Input);
                Handled::Yes
            }
            // the tab row's motion: its capsules travel and its focused tab pops
            ScreenEvent::Tick(t) => {
                let (page, layout) = self.header(cx.measure);
                let focused = if self.tabs && holds { i32::from(self.recs) } else { -1 };
                PageHeader::step(&layout, page.selected(&layout), focused, &mut self.hcaps, &mut self.hpop, t.dt());
                Handled::No
            }
            // OK on a control commits as a press (or, by pointer, an activation)
            ScreenEvent::Activate(e) if *e == ELEM => self.key_press(Key::Ok, fx),
            ScreenEvent::PressCommit(_) if holds => self.key_press(Key::Ok, fx),
            // INFO, or a held OK: record the focused airing's show (or cancel its rule)
            ScreenEvent::PressHold(_) if holds && (self.tabs || self.moving) => Handled::Yes,
            // Back puts a carried channel down rather than leaving the page
            ScreenEvent::Input(input) if holds && self.moving && matches!(input.kind, InputKind::Key { key: Key::Back, edge: Edge::Down, .. }) => {
                self.moving = false;
                fx.invalidate(Provenance::Input);
                Handled::Yes
            }
            ScreenEvent::PressHold(_) if holds && self.recs => {
                // in the Recordings view the same keys end the selected rule
                plx_ui::dispatch::take_rec_hold();
                if let Some(r) = self.model.recs.get(self.rec_sel).filter(|r| r.kind != RecKind::Scheduled) {
                    fx.push(Fx::App(AppFx::LiveTv(LiveTvOut::CancelRule { id: r.id.clone() })));
                }
                Handled::Yes
            }
            ScreenEvent::PressHold(_) if holds => {
                if let Some((_, s)) = self.focused() {
                    // REC, INFO or a held OK: the application steps episode -> series -> off
                    plx_ui::dispatch::take_rec_hold();
                    let episode = if s.episode_guid.is_empty() { s.record_guid.clone() } else { s.episode_guid.clone() };
                    fx.push(Fx::App(AppFx::LiveTv(LiveTvOut::Record { show: s.record_guid.clone(), episode, title: s.title.clone() })));
                }
                Handled::Yes
            }
            // a colour key (red, green, yellow, blue) on the grid picks the focused channel up; again, it is dropped
            ScreenEvent::Input(input) if holds && !self.tabs && !self.recs && matches!(input.kind,
                InputKind::Key { wcode, edge: Edge::Down, .. } if is_colour_key(wcode)) => {
                if !self.model.rows.is_empty() {
                    self.moving = !self.moving;
                    fx.invalidate(Provenance::Input);
                }
                Handled::Yes
            }
            // (a key the guide has no use for is logged once, to learn what the remote sends)
            ScreenEvent::Input(input) if holds && matches!(input.kind, InputKind::Key { key: Key::Other, wcode, edge: Edge::Down, .. }
                if { plx_base::eventlog::log(&format!("livetv: unused key wcode={wcode}")); false }) => Handled::No,
            ScreenEvent::Input(input) if holds => match input.kind {
                InputKind::Key { key, edge: Edge::Down | Edge::Repeat, at_edge: false, .. }
                    if matches!(key, Key::Up | Key::Down | Key::Left | Key::Right) => self.key_press(key, fx),
                _ => Handled::No,
            },
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Focusable<H> for LiveTvScreen {
    fn groups(&self, _: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        out.push(GroupSpec {
            id: GROUP,
            kind: GroupKind::Row { wrap: false },
            seat: Seat::First,
            reachable: AxisMask::BOTH,
            // Left from the guide's first column crosses to the sidebar
            edge: [EdgeRule::Stop, EdgeRule::Stop, EdgeRule::Geometric, EdgeRule::Stop],
            extent: Rect::new(X0, GRID_Y, RIGHT - X0, ROW_H),
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
        if *elem != ELEM { return None; }
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

impl<H: ContentLike> Screen<H> for LiveTvScreen {
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
        p.rrect(Rect::new(-200.0, 0.0, plx_ui::consts::SCR_W + 400.0, plx_ui::consts::SCR_H), 0.0, 0.0, theme::SURFACE_APP);
        if self.model.status != LiveStatus::Ready || self.model.rows.is_empty() {
            self.draw_status(p);
            return;
        }
        let holds = f.focus.current.is_some_and(|k| k.entry == self.entry && k.elem == ELEM);
        {
            let (page, layout) = self.header(f.measure);
            page.draw_with(p, &layout, &self.hcaps, self.hpop.scale_with(0, f.press), None, &HeaderSwap::new());
        }
        if self.recs {
            self.draw_recs(p, holds && !self.tabs);
            return;
        }
        self.draw_art(p);
        self.draw_details(p, f.measure);
        self.draw_ruler(p);
        self.draw_grid(p, holds && !self.tabs);
        if self.moving {
            Self::text(p, "Moving this channel: Up / Down to place it, OK to drop it", LEFT, RULER_Y - 22.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, 1200.0);
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
