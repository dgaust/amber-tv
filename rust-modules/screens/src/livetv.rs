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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveRow {
    pub id: String,
    pub number: String,
    pub title: String,
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
}

/// What the guide asks of the application.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveTvOut {
    Tune { channel: String },
}

const ELEM: u32 = 1;
/// Group 0: the group a page's fresh entry seats focus in, so opening the guide lands in it.
const GROUP: GroupId = GroupId(0);

const LEFT: f32 = plx_ui::consts::MARGIN_X;
const RIGHT: f32 = plx_ui::consts::SCR_W - plx_ui::consts::MARGIN_X - plx_ui::widgets::PAGE_SHIFT_CLOSED;
const RULER_Y: f32 = 388.0;
const RULER_H: f32 = 34.0;
const GRID_Y: f32 = RULER_Y + RULER_H + 10.0;
const ROW_H: f32 = 84.0;
const ROW_GAP: f32 = 8.0;
const CHANNEL_W: f32 = 150.0;
const CELL_GAP: f32 = 6.0;
const X0: f32 = LEFT + CHANNEL_W + CELL_GAP;
/// The guide shows two hours across.
const WINDOW_S: i64 = 2 * 3600;
const HALF_HOUR: i64 = 1800;

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
}

impl LiveTvScreen {
    pub const WORD: &'static str = "livetv";
    pub const SHAPE: &'static str = "LiveTvScreen{entry:u32,row:u32,slot:u32}";

    pub fn new(entry: EntryId) -> Self {
        Self { entry, model: LiveTvModel::default(), now: 0, row: 0, slot: 0, first_row: 0, window: 0 }
    }

    /// The application's snapshot and clock. `true` when what the page shows changed.
    pub fn input(&mut self, model: LiveTvModel, now: i64) -> bool {
        let minute_moved = now / 60 != self.now / 60;
        self.now = now;
        if model == self.model {
            return minute_moved;
        }
        let first = self.model.rows.is_empty() && !model.rows.is_empty();
        self.model = model;
        if first {
            self.row = 0;
            self.window = (now / HALF_HOUR) * HALF_HOUR;
            self.slot = self.slot_at(0, now);
        }
        self.row = self.row.min(self.model.rows.len().saturating_sub(1));
        let n = self.model.rows.get(self.row).map_or(0, |r| r.slots.len());
        self.slot = self.slot.min(n.saturating_sub(1));
        true
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

    fn key_press<H: ContentLike>(&mut self, key: Key, fx: &mut Effects<'_, H>) -> Handled {
        if self.model.rows.is_empty() {
            return Handled::No;
        }
        let anchor = self.focused().map_or(self.now, |(_, s)| s.begins.max(self.window).max(self.floor_now()));
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
                let earliest = self.slot == 0 || self.model.rows[self.row].slots[self.slot - 1].ends <= self.floor_now();
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

    fn clock(&self, t: i64) -> String {
        let hm = plx_platform::i18n::local_clock_time(t).unwrap_or([0, 0]);
        // CLDR puts a NARROW no-break space before AM/PM, which the UI faces do not carry
        plx_platform::i18n::current().time_of_day(hm).unwrap_or_default().replace('\u{202f}', " ")
    }

    fn draw_details(&self, p: Painter, measure: &dyn plx_machine::machine::Measure) {
        let Some((row, s)) = self.focused() else { return };
        let w = 1080.0;
        Self::text(p, &s.title, LEFT, 104.0, theme::size::TITLE, theme::TEXT_PRIMARY, 1, w);
        let mut y = 152.0;
        if !s.subtitle.is_empty() {
            Self::text(p, &s.subtitle, LEFT, y, theme::size::BODY, theme::TEXT_PRIMARY, 1, w);
            y += 40.0;
        }
        let mut meta = s.meta.clone();
        let channel = format!("{} {}", row.number, row.title);
        let timing = if s.begins <= self.now && self.now < s.ends {
            format!("{} min left", ((s.ends - self.now) + 59) / 60)
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
            p.rrect(Rect::new(LEFT, y, CHANNEL_W, ROW_H), 6.0, 6.0, theme::with_a(theme::TEXT_PRIMARY, 0.05));
            Self::text(p, &row.number, LEFT + 14.0, y + 26.0, theme::size::CAPTION, theme::RAIL_CURRENT, 1, CHANNEL_W - 20.0);
            Self::text(p, &row.title, LEFT + 14.0, y + 58.0, theme::size::MICRO, theme::TEXT_SECONDARY, 0, CHANNEL_W - 24.0);
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
                let tw = w - 24.0;
                // a sliver too narrow to read carries no text at all
                if tw > 56.0 {
                    Self::text(p, &s.title, x + 14.0, y + 28.0, theme::size::CAPTION, ink, if focused { 1 } else { 0 }, tw);
                    let line = if focused && s.begins <= self.now && self.now < s.ends {
                        format!("{} min left", ((s.ends - self.now) + 59) / 60)
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
        c.u32(self.entry.0).u32(self.row as u32).u32(self.slot as u32);
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
            // OK on a control commits as a press (or, by pointer, an activation)
            ScreenEvent::Activate(e) if *e == ELEM => self.key_press(Key::Ok, fx),
            ScreenEvent::PressCommit(_) if holds => self.key_press(Key::Ok, fx),
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
        self.draw_details(p, f.measure);
        self.draw_ruler(p);
        let active = f.focus.current.is_some_and(|k| k.entry == self.entry && k.elem == ELEM);
        self.draw_grid(p, active);
        if self.model.tuning.is_some() {
            Self::text(p, "Tuning\u{2026}", RIGHT - 220.0, 170.0, theme::size::BODY, theme::RAIL_CURRENT, 1, 220.0);
        } else if self.model.tune_failed {
            Self::text(p, "Could not tune that channel", RIGHT - 420.0, 170.0, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, 420.0);
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
