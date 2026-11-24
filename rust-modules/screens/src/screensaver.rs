//! Amber: the screensaver - a slow slideshow of fanart over black, shown when the viewer has been
//! idle (`app::screensaver` opens it). Any key or click closes it and does nothing else.
//!
//! The pictures are `plx_data::saver`'s (films rated M and under). Each stays on for [`SLIDE_S`]
//! seconds, drifting in slowly, and the next fades in over [`FADE_S`]. The title sits in a corner
//! that moves with each picture, so no screen element holds one place on a panel.

use crate::registry::ContentLike;
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, LogicalState, Machine, NavOp,
};
use plx_machine::present::Provenance;
use plx_ui::screen::{
    At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec, HitSource,
    Placed, RenderStrategy, Screen, ScreenEvent, Seat, Step,
};
use plx_ui::{theme, Painter, Rect};
use std::ffi::CString;

const ELEM: u32 = 1;
const GROUP: GroupId = GroupId(0);
/// Seconds a picture is on.
const SLIDE_S: f32 = 20.0;
/// Seconds the next one takes to fade in over it.
const FADE_S: f32 = 3.0;
/// The slow push-in over a picture's time: 1.0 up to this scale.
const DRIFT: f32 = 1.08;
/// A pointer has to move this far from where it first was to count as the viewer.
const POINTER_WAKE: f32 = 60.0;

pub struct ScreensaverScreen {
    entry: EntryId,
    images: Vec<plx_data::saver::SaverImage>,
    gen: u32,
    /// The picture on, the one it replaced (fading out), seconds into the current one.
    idx: usize,
    prev: Option<usize>,
    elapsed: f32,
    pointer: Option<(f32, f32)>,
    /// How many pictures have been shown: moves the caption's corner.
    shown: u32,
}

impl ScreensaverScreen {
    pub const WORD: &'static str = "screensaver";
    pub const SHAPE: &'static str = "ScreensaverScreen{entry:u32,idx:u32}";

    pub fn new(entry: EntryId) -> Self {
        Self { entry, images: Vec::new(), gen: u32::MAX, idx: 0, prev: None, elapsed: 0.0, pointer: None, shown: 0 }
    }

    fn refresh(&mut self) {
        let gen = plx_data::saver::generation();
        if gen != self.gen {
            self.gen = gen;
            self.images = plx_data::saver::images();
            self.idx = if self.images.is_empty() { 0 } else { self.idx % self.images.len() };
        }
    }

    /// Advance the clock; `true` when a new picture began.
    fn advance(&mut self, dt: f32) -> bool {
        self.elapsed += dt;
        if self.images.len() > 1 && self.elapsed >= SLIDE_S {
            self.prev = Some(self.idx);
            self.idx = (self.idx + 1) % self.images.len();
            self.elapsed = 0.0;
            self.shown += 1;
            return true;
        }
        false
    }

    /// How far the current picture has faded in, 0..1 (1 when nothing is under it).
    fn fade(&self) -> f32 {
        if self.prev.is_none() { 1.0 } else { (self.elapsed / FADE_S).clamp(0.0, 1.0) }
    }

    fn draw_picture(&self, p: Painter, i: usize, alpha: f32, age: f32) {
        let Some(img) = self.images.get(i) else { return };
        if img.art.is_empty() || alpha <= 0.0 { return; }
        let (tex, iw, ih) = plx_ui::widgets::resolve_tex_wh_on(img.sid, &img.art, 1920, 1080, 0);
        if tex == 0 || iw <= 0.0 || ih <= 0.0 { return; }
        let (w, h) = (plx_ui::consts::SCR_W, plx_ui::consts::SCR_H);
        // cover the screen: crop the picture to its shape
        let (ra, ia) = (w / h, iw / ih);
        let uv = if ia > ra {
            let u = (1.0 - ra / ia) * 0.5;
            [u, 0.0, 1.0 - u, 1.0]
        } else {
            let v = (1.0 - ia / ra) * 0.5;
            [0.0, v, 1.0, 1.0 - v]
        };
        // the push-in: grows about the middle, slowly
        let k = 1.0 + (DRIFT - 1.0) * (age / (SLIDE_S + FADE_S)).clamp(0.0, 1.0);
        let r = Rect::new((w - w * k) * 0.5, (h - h * k) * 0.5, w * k, h * k);
        p.tex_uv(tex, uv, r, 0.0, [1.0, 1.0, 1.0, alpha]);
    }

    fn text(p: Painter, s: &str, x: f32, cy: f32, sz: i32, ink: [f32; 4], bold: i32, max_w: f32) {
        let shown = plx_gfx::text::elide(s, max_w.max(10.0), sz, bold, false);
        let Ok(c) = CString::new(shown) else { return };
        let y = plx_gfx::text::text_vcenter_y(sz, bold, cy);
        p.text(c.as_ptr(), x, y, sz, ink, 0, bold);
    }
}

impl LogicalState for ScreensaverScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.entry.0).u32(self.idx as u32);
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("screensaver pictures={} at={}", self.images.len(), self.idx));
    }
}

impl<H: ContentLike> Machine<H> for ScreensaverScreen {
    type Ev = ScreenEvent<H>;

    fn step(&mut self, ev: &Self::Ev, _cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            ScreenEvent::Enter(_) => {
                self.refresh();
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            // the slideshow moves every frame
            ScreenEvent::Tick(t) => {
                self.refresh();
                self.advance(t.dt());
                fx.invalidate(Provenance::Input);
                Handled::No
            }
            // any press, or a click, is the viewer: close, and do nothing else with it
            ScreenEvent::Activate(_) | ScreenEvent::PressCommit(_) | ScreenEvent::PressHold(_) => {
                fx.push(Fx::Nav(NavOp::Pop));
                Handled::Yes
            }
            ScreenEvent::Input(input) => match input.kind {
                InputKind::Key { edge: Edge::Down, .. } | InputKind::Click { .. } => {
                    fx.push(Fx::Nav(NavOp::Pop));
                    Handled::Yes
                }
                InputKind::Pointer { x, y, .. } => {
                    let (x0, y0) = *self.pointer.get_or_insert((x, y));
                    if (x - x0).abs() + (y - y0).abs() > POINTER_WAKE {
                        fx.push(Fx::Nav(NavOp::Pop));
                    }
                    Handled::Yes
                }
                // everything else (key-up, repeats, the wheel) is swallowed: it is not for the page below
                _ => Handled::Yes,
            },
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike> Focusable<H> for ScreensaverScreen {
    fn groups(&self, _: &Cx<'_, H>, out: &mut Vec<GroupSpec>) {
        out.push(GroupSpec {
            id: GROUP,
            kind: GroupKind::Row { wrap: false },
            seat: Seat::First,
            reachable: AxisMask::BOTH,
            edge: [EdgeRule::Stop, EdgeRule::Stop, EdgeRule::Stop, EdgeRule::Stop],
            extent: Rect::new(0.0, 0.0, plx_ui::consts::SCR_W, plx_ui::consts::SCR_H),
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
        let rect = Rect::new(0.0, 0.0, plx_ui::consts::SCR_W, plx_ui::consts::SCR_H);
        Some(Placed { rect, rest_rect: rect, clip: Rect::FULL, index: Some(0) })
    }
    fn reconcile(&self, _: FocusKey<u32>, _: &Cx<'_, H>) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem: ELEM }
    }
    fn seat(&self, _: GroupId, _: Placed, _: &Cx<'_, H>) -> FocusKey<u32> {
        FocusKey { entry: self.entry, elem: ELEM }
    }
}

impl<H: ContentLike> Screen<H> for ScreensaverScreen {
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
        let (w, h) = (plx_ui::consts::SCR_W, plx_ui::consts::SCR_H);
        p.rrect(Rect::new(-200.0, 0.0, w + 400.0, h), 0.0, 0.0, [0.0, 0.0, 0.0, 1.0]);
        let fade = self.fade();
        if let Some(prev) = self.prev {
            // the picture going out holds full until the new one has covered it
            self.draw_picture(p, prev, 1.0, SLIDE_S + self.elapsed);
        }
        self.draw_picture(p, self.idx, fade, self.elapsed);
        if let Some(img) = self.images.get(self.idx) {
            // a soft shade under the caption, and the caption: its corner moves with each picture
            let corner = self.shown % 4;
            let left = corner % 2 == 0;
            let top = corner >= 2;
            let band_h = 170.0;
            let band = if top { Rect::new(0.0, 0.0, w, band_h) } else { Rect::new(0.0, h - band_h, w, band_h) };
            p.rrect(band, 0.0, 0.0, theme::scrim(0.35 * fade));
            let mut title = if img.year > 0 { format!("{} ({})", img.title, img.year) } else { img.title.clone() };
            if !img.rating.is_empty() { title.push_str(&format!("   \u{b7}   {}", img.rating)); }
            let y = if top { 90.0 } else { h - 90.0 };
            let (x, room) = (plx_ui::consts::MARGIN_X, w - 2.0 * plx_ui::consts::MARGIN_X);
            let ink = [1.0, 1.0, 1.0, 0.85 * fade];
            if left {
                Self::text(p, &title, x, y, theme::size::BODY, ink, 0, room);
            } else {
                let tw = CString::new(title.clone()).map_or(0.0, |c| f.measure.width(&c, theme::size::BODY, false)).min(room);
                Self::text(p, &title, w - plx_ui::consts::MARGIN_X - tw, y, theme::size::BODY, ink, 0, room);
            }
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
