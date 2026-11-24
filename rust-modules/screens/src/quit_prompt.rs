//! Amber: "Quit Amber?" - asked when BACK is pressed at the top of Home. Quit ends the app (it is not sent to
//! the background); Stay, or BACK again, returns to Home. The Home button on the remote still leaves
//! Amber running, as it always did.

use crate::registry::ContentLike;
use plx_machine::machine::{
    Canon, Cx, Edge, Effects, EntryId, FocusKey, Fx, GroupId, Handled, InputKind, Key, LogicalState, Machine, NavOp,
};
use plx_machine::present::Provenance;
use plx_ui::screen::{
    At, AxisMask, Dir, DrawFrame, EdgeRule, ElemKind, FocusSource, Focusable, GroupKind, GroupSpec, HitSource,
    Placed, RenderStrategy, Screen, ScreenEvent, Seat, Step,
};
use plx_ui::{theme, Painter, Rect};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};

const ELEM: u32 = 1;
const GROUP: GroupId = GroupId(0);

static QUIT: AtomicBool = AtomicBool::new(false);

/// Has the viewer chosen Quit? Read (and cleared) by the main loop, which ends the app.
pub fn take_quit() -> bool {
    QUIT.swap(false, Ordering::Relaxed)
}

const BTN_W: f32 = 300.0;
const BTN_H: f32 = 84.0;
const BTN_GAP: f32 = 40.0;

pub struct QuitPromptScreen {
    entry: EntryId,
    /// 0 = Quit, 1 = Stay.
    choice: u8,
    done: bool,
}

impl QuitPromptScreen {
    pub const WORD: &'static str = "quitprompt";
    pub const SHAPE: &'static str = "QuitPromptScreen{entry:u32,choice:u32}";

    pub fn new(entry: EntryId) -> Self {
        Self { entry, choice: 0, done: false }
    }

    fn button(&self, i: u8) -> Rect {
        let (w, h) = (plx_ui::consts::SCR_W, plx_ui::consts::SCR_H);
        let total = 2.0 * BTN_W + BTN_GAP;
        Rect::new((w - total) * 0.5 + f32::from(i) * (BTN_W + BTN_GAP), h * 0.5 + 20.0, BTN_W, BTN_H)
    }

    fn finish<H: ContentLike>(&mut self, quit: bool, fx: &mut Effects<'_, H>) {
        if self.done { return; }
        self.done = true;
        if quit { QUIT.store(true, Ordering::Relaxed); }
        fx.push(Fx::Nav(NavOp::Pop));
    }

    fn text(p: Painter, s: &str, cx: f32, cy: f32, sz: i32, ink: [f32; 4], bold: i32, measure: Option<&dyn plx_machine::machine::Measure>) {
        let Ok(c) = CString::new(s) else { return };
        let w = measure.map_or(0.0, |m| m.width(&c, sz, bold != 0));
        let y = plx_gfx::text::text_vcenter_y(sz, bold, cy);
        p.text(c.as_ptr(), cx - w * 0.5, y, sz, ink, 0, bold);
    }
}

impl LogicalState for QuitPromptScreen {
    fn write(&self, c: &mut Canon) {
        c.u32(self.entry.0).u32(u32::from(self.choice));
    }
    fn probe(&self, out: &mut String) {
        out.push_str(&format!("quitprompt choice={}", self.choice));
    }
}

impl<H: ContentLike> Machine<H> for QuitPromptScreen {
    type Ev = ScreenEvent<H>;

    fn step(&mut self, ev: &Self::Ev, _cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            ScreenEvent::Enter(_) => {
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::Activate(_) | ScreenEvent::PressCommit(_) => {
                let quit = self.choice == 0;
                self.finish(quit, fx);
                Handled::Yes
            }
            ScreenEvent::PressHold(_) => Handled::Yes,
            ScreenEvent::Input(input) => match input.kind {
                InputKind::Key { key, edge: Edge::Down, .. } => {
                    match key {
                        Key::Left => self.choice = 0,
                        Key::Right => self.choice = 1,
                        Key::Ok => { let quit = self.choice == 0; self.finish(quit, fx); }
                        Key::Back => self.finish(false, fx),
                        _ => {}
                    }
                    fx.invalidate(Provenance::Input);
                    Handled::Yes
                }
                InputKind::Click { x, y, .. } => {
                    for i in 0..2u8 {
                        let r = self.button(i);
                        if x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h {
                            self.finish(i == 0, fx);
                        }
                    }
                    Handled::Yes
                }
                InputKind::Pointer { x, y, .. } => {
                    for i in 0..2u8 {
                        let r = self.button(i);
                        if x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h && self.choice != i {
                            self.choice = i;
                            fx.invalidate(Provenance::Input);
                        }
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

impl<H: ContentLike> Focusable<H> for QuitPromptScreen {
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

impl<H: ContentLike> Screen<H> for QuitPromptScreen {
    fn name(&self) -> &'static str {
        Self::WORD
    }
    /// the whole screen is this page's: the sidebar and clock are gone
    fn hides_chrome(&self) -> f32 {
        1.0
    }
    fn claims_screen(&self) -> bool {
        true
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
        p.rrect(Rect::new(-200.0, 0.0, w + 400.0, h), 0.0, 0.0, [0.02, 0.02, 0.03, 1.0]);
        Self::text(p, "Quit Amber?", w * 0.5, h * 0.5 - 70.0, theme::size::TITLE, theme::TEXT_PRIMARY, 1, Some(f.measure));
        Self::text(p, "Home on the remote leaves it running in the background.", w * 0.5, h * 0.5 - 14.0, theme::size::CAPTION, theme::TEXT_SECONDARY, 0, Some(f.measure));
        for (i, label) in [(0u8, "Quit"), (1u8, "Stay")] {
            let r = self.button(i);
            let on = self.choice == i;
            let (fill, ink) = if on { ([1.0, 1.0, 1.0, 1.0], [0.04, 0.04, 0.06, 1.0]) } else { ([1.0, 1.0, 1.0, 0.10], theme::TEXT_PRIMARY) };
            p.rrect(r, 14.0, 0.0, fill);
            Self::text(p, label, r.x + r.w * 0.5, r.y + r.h * 0.5, theme::size::BODY, ink, i32::from(on), Some(f.measure));
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
