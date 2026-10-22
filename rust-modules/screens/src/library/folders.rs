//! Owned **Browse by Folders** page: one level of the server's `folder` listing as a poster grid.
//!
//! The server's By Folder listing is path-shaped, not query-shaped: a level is a page of mixed rows
//! (folder rows first, then loose files), and a folder row's press opens the next level. The
//! `folders` store owns the tree and its paging; this screen owns only presentation state and the
//! identity→element registry, exactly as `collection.rs` does for a collection's members.
//!
//! **One stack entry per level.** The header's Folders control pushed the root; a folder card's OK
//! pushes this same screen with the child's `parent` id (the digits the server's own `key` carried —
//! the screen never invents one) and sends `FoldersCmd::Drill` so the store's level stack moves with
//! the nav stack. BACK is the page's own pop (`ContentReq::Back`); leaving a level for good sends
//! `FoldersCmd::Pop`, which is what keeps the store's top level equal to the page on top.
//!
//! The page is a `plx_ui::cards::Stack` of three sections (`Sec`): the head (the back crumb and the
//! level's title, drawn here, never focusable), the grid, and the status read-outs. Folder tiles
//! wear the neutral folder face (`plx_ui::folder_tile`); file tiles are ordinary posters — the
//! caption policy, the resume bar and the watched mark all work on them unchanged.

use plx_data::folders::{FolderCell, FoldersLevel, FoldersStatus};
use plx_data::pms::PmsMovie;
use plx_data::stores::folders::FoldersCmd;
use plx_ui::cards::{self as ui_cards, TileLabel};
use plx_ui::cards::{CardSource, GridSpec, Kind, SectionSpec, Stack, StackEvent, StackPage};
use plx_ui::consts::*;
use plx_ui::label::{Label, VAlign};
use plx_machine::machine::{Canon, Cx, Effects, EntryId, GroupId, Handled, Host, InputEvent,
    InputKind, Key, Leave, LogicalState, Machine, Measure, Tick, Edge};
use plx_machine::present::Provenance;
use plx_ui::screen::{At, DrawFrame, FocusSource, Focusable, HitSource, Link, RenderStrategy,
    Screen, ScreenEvent};
use plx_ui::route_screen::{self, CRUMB_BAND, CRUMB_RISE};
use plx_ui::theme;
use plx_ui::widgets::{Art, PageGround, StatusKind, StatusOverlay};
use plx_ui::{Env, Painter, Rect};

use super::super::registry::{merged_ranges, row_neighbourhood, tile_facts, AppFx, CardKeys,
    ContentArg, ContentLike, ContentReq, FoldersLike, LibraryLike, LibraryReq, PageMemory};

const RETRY_ELEM: u32 = 1;
const FIRST_CARD_ELEM: u32 = 0x1000;
/// Grid slots whose row is not held (never read, or evicted): slot `i` is `HOLE_ELEM_BASE + i`.
/// The same scheme the Collection page uses — see its doc on the reserved range.
const HOLE_ELEM_BASE: u32 = 0x5000_0000;
const _: () = assert!(HOLE_ELEM_BASE >= plx_ui::geom::NAMESPACE_FLOOR);
const _: () = assert!(HOLE_ELEM_BASE as u64 + 0x3fff_ffff < plx_ui::dispatch::STRIP_BASE as u64);
const _: () = assert!(HOLE_ELEM_BASE > super::super::registry::ALERT + 0x100);

fn hole_elem(index: usize) -> u32 { HOLE_ELEM_BASE + index.min(0x3fff_ffff) as u32 }

/// The element key a folder row interns under. A folder has no `ratingKey`; its identity in this
/// section is the `parent` id the server's `key` carried, and the store already parsed exactly that.
fn folder_rk(parent: i64) -> String { format!("folder:{parent}") }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sec {
    /// The back crumb and the level's title, drawn by the page, never a focus stop.
    Head,
    /// The grid of folder and file rows.
    Items,
    /// The quiet read-outs, and Retry for a failed load.
    Status,
}
const HEAD_GROUP: GroupId = GroupId(0);
const GRID_GROUP: GroupId = GroupId(1);
const STATUS_GROUP: GroupId = GroupId(2);

// ── Geometry: the grid is the six-column poster grid at `MARGIN_X`; the head is the title band
// above it, in the same 96 px inset the Collection page reads inside.

const CONTENT_TOP: f32 = MARGIN_X;
const HEAD_TOP: f32 = CONTENT_TOP;
const HEAD_H: f32 = 140.0;
const GRID_TOP: f32 = HEAD_TOP + HEAD_H;
const TITLE_W: f32 = SCR_W - 2.0 * MARGIN_X;
const STATUS_FRAME: Rect = Rect { x: MARGIN_X, y: GRID_TOP, w: SCR_W - 2.0 * MARGIN_X, h: SCR_H - GRID_TOP - MARGIN_X };

/// How much of the head is left at `scroll`: all of it at the top, none once the first row has
/// risen to the content edge (the Collection page's rule).
fn head_alpha(scroll: f32) -> f32 {
    (1.0 - scroll / (GRID_TOP - CONTENT_TOP)).clamp(0.0, 1.0)
}

/// One run of consecutive slots with a projected element: slot `first + n` is `elems[n]`.
#[derive(Default)]
struct Run {
    first: usize,
    elems: Vec<u32>,
}

/// The elements of the slots the page can paint or step to, as of one store revision. Derived, not
/// logical state (the Collection page's `Projection`, without the labels or the restore pins).
#[derive(Default)]
struct Projection {
    total: usize,
    runs: Vec<Run>,
    index: std::collections::HashMap<u32, usize>,
    applied: Vec<std::ops::Range<usize>>,
    revision: Option<u64>,
}

impl Projection {
    fn run_of(&self, index: usize) -> Option<(&Run, usize)> {
        let at = self.runs.partition_point(|run| run.first <= index).checked_sub(1)?;
        let run = &self.runs[at];
        let offset = index - run.first;
        (offset < run.elems.len()).then_some((run, offset))
    }
    fn elem(&self, index: usize) -> Option<u32> { self.run_of(index).map(|(run, offset)| run.elems[offset]) }
    fn cells(&self) -> usize { self.runs.iter().map(|run| run.elems.len()).sum() }
}

/// The grid's content (`CardSource`): the store's level read through the page's projection.
struct Cells<'a> {
    level: &'a FoldersLevel,
    cards: &'a CardKeys,
    proj: &'a Projection,
}

impl<'a> Cells<'a> {
    fn fresh(&self) -> bool { self.proj.total == self.level.shown() }

    fn elem_at(&self, index: usize) -> Option<u32> {
        if index >= self.level.shown() { return None; }
        if let Some(elem) = self.proj.elem(index).filter(|_| self.fresh()) { return Some(elem); }
        Some(match self.level.cell(index) {
            Some(FolderCell::Folder { parent, .. }) => self.cards.elem_for(plx_plex::plex::ServerId::UNSET, &folder_rk(*parent))
                .unwrap_or_else(|| hole_elem(index)),
            Some(FolderCell::Item(m)) => self.cards.elem_for(m.sid, &m.rk).unwrap_or_else(|| hole_elem(index)),
            None => hole_elem(index),
        })
    }

    fn index(&self, elem: u32) -> Option<usize> {
        if elem >= HOLE_ELEM_BASE {
            return Some((elem - HOLE_ELEM_BASE) as usize).filter(|&i| i < self.level.shown());
        }
        if self.fresh() {
            if let Some(&index) = self.proj.index.get(&elem) { return Some(index); }
        }
        let identity = self.cards.get(elem)?;
        if identity.sid == plx_plex::plex::ServerId::UNSET {
            // A folder row: its slot is the level's row whose folder carries this id.
            let parent = identity.rk.strip_prefix("folder:")?.parse::<i64>().ok()?;
            (0..self.level.shown()).find(|&i| matches!(self.level.cell(i),
                Some(FolderCell::Folder { parent: p, .. }) if *p == parent))
        } else {
            (0..self.level.shown()).find(|&i| matches!(self.level.cell(i),
                Some(FolderCell::Item(m)) if m.sid == identity.sid && m.rk == identity.rk))
        }
    }
}

impl<H: Host<Elem = u32>> CardSource<H> for Cells<'_> {
    fn len(&self) -> usize { self.level.shown() }
    fn elem(&self, i: usize) -> u32 { self.elem_at(i).unwrap_or(RETRY_ELEM) }
    fn index_of(&self, e: &u32) -> Option<usize> { self.index(*e) }
    fn art(&self, i: usize) -> Art<'_> {
        match self.level.cell(i) {
            Some(FolderCell::Item(m)) => Art::Poster(Some(tile_facts::of(m))),
            // A folder row carries no artwork: the face is the neutral folder tile, drawn in
            // `overlay` over the tile's ground.
            _ => Art::Poster(None),
        }
    }
    fn label(&self, i: usize) -> TileLabel {
        match self.level.cell(i) {
            Some(FolderCell::Folder { title, .. }) => TileLabel::title(title),
            Some(FolderCell::Item(m)) => {
                crate::registry::captions::label(&crate::registry::captions::policy(),
                    &tile_facts::caption_facts(m), None, false)
            }
            None => TileLabel::default(),
        }
    }
    fn progress(&self, i: usize) -> Option<f32> { self.level.cell(i).and_then(|c| c.item()).and_then(PmsMovie::resume_frac) }
    fn overlay(&self, p: Painter, i: usize, tile: &ui_cards::Tile, _measure: &dyn Measure) {
        if matches!(self.level.cell(i), Some(FolderCell::Folder { .. })) {
            let (w, h) = (tile.rect.w / tile.scale, tile.rect.h / tile.scale);
            let rest = Rect::new(tile.rect.cx() - w * 0.5, tile.rect.cy() - h * 0.5, w, h);
            plx_ui::folder_tile::draw(p, rest, tile.rect, tile.radius);
        }
    }
    fn unread(&self, i: usize) -> bool { self.level.cell(i).is_none() }
    fn more(&self) -> bool { self.level.more() }
}

/// The page's content and the state its sections read.
struct Page {
    entry: EntryId,
    section: u32,
    parent: u64,
    /// The crumb's word: the folder's title, empty at the section root.
    name: String,
    cards: CardKeys,
    proj: Projection,
    want: Vec<std::ops::Range<usize>>,
    sent_window: Option<(std::ops::Range<usize>, Option<usize>)>,
    want_end: usize,
    focus_elem: Option<u32>,
    synced: Option<u64>,
}

pub struct FoldersScreen {
    page: Page,
    stack: Stack<Sec>,
    teardown_popped: bool,
    ground: PageGround,
    ground_seeded: bool,
}

impl LogicalState for FoldersScreen {
    fn write(&self, c: &mut Canon) {
        let page = &self.page;
        c.u32(page.entry.0).u32(page.section).u64(page.parent).str(&page.name)
            .u32(page.cards.next).seq(page.cards.len());
        for key in &page.cards.keys { c.u32(key.sid.raw() as u32).str(&key.rk).u32(key.elem); }
        self.stack.write(c);
    }
    fn probe(&self, out: &mut String) {
        let page = &self.page;
        out.push_str(&format!("folders section={} parent={} name={} cards={} next={}",
            page.section, page.parent, page.name, page.cards.len(), page.cards.next));
    }
}

impl Page {
    fn level<'a, H: FoldersLike>(&self, cx: &Cx<'a, H>) -> Option<&'a FoldersLevel> {
        // The store's top level is this page's level while the nav stack and the store stack move
        // together; a mismatch (a teardown rebuilt the store from a deeper target) still shows the
        // level named by the id the page carries.
        let view = H::folders(cx);
        let level = view.current()?;
        let root = self.parent == 0;
        (root == level.target.parent.is_none()
            && (root || level.target.parent == Some(self.parent as i64))).then_some(level)
    }

    /// The title the page wears: the folder's own name, or the section's at the root.
    fn head_title<'a, H: LibraryLike>(&self, cx: &Cx<'a, H>) -> std::borrow::Cow<'a, str> {
        if !self.name.is_empty() { return std::borrow::Cow::Owned(self.name.clone()); }
        H::directory(cx).current().and_then(|i| H::directory(cx).sections().get(i))
            .map(|s| std::borrow::Cow::Borrowed(s.row.title.as_str()))
            .unwrap_or_else(|| std::borrow::Cow::Borrowed(plx_platform::i18n::msg::library_header_folders()))
    }

    fn project(&mut self, level: &FoldersLevel, revision: Option<u64>) {
        let shown = level.shown();
        let default = [0..4 * ui_cards::GRID_COLS];
        let ranges = merged_ranges(if self.want.is_empty() { default.to_vec() } else { self.want.clone() }, shown);
        if revision.is_some() && self.proj.revision == revision && self.proj.total == shown && self.proj.applied == ranges {
            return;
        }
        let rows: Vec<(usize, &FolderCell)> = ranges.iter().flat_map(|r| r.clone())
            .filter_map(|i| level.cell(i).map(|cell| (i, cell))).collect();
        let rks: Vec<(plx_plex::plex::ServerId, String)> = rows.iter().map(|(_, cell)| match *cell {
            FolderCell::Folder { parent, .. } => (plx_plex::plex::ServerId::UNSET, folder_rk(*parent)),
            FolderCell::Item(m) => (m.sid, m.rk.clone()),
        }).collect();
        let keys = self.cards.intern_all(rks.iter().map(|(sid, rk)| (*sid, rk.as_str())), "folders");
        assert!(self.cards.next <= HOLE_ELEM_BASE, "folders element-key space reached the hole range");
        let before: std::collections::HashSet<u32> = self.proj.index.keys().copied().collect();
        let mut proj = Projection { total: shown, revision, ..Default::default() };
        let mut held = rows.iter().zip(keys).peekable();
        for range in &ranges {
            let mut run = Run { first: range.start, elems: Vec::with_capacity(range.len()) };
            for i in range.clone() {
                match held.next_if(|((at, _), _)| *at == i) {
                    Some(((_, _), key)) => { proj.index.entry(key).or_insert(i); run.elems.push(key); }
                    None => run.elems.push(hole_elem(i)),
                }
            }
            proj.runs.push(run);
        }
        proj.applied = ranges;
        self.proj = proj;
        let now = &self.proj.index;
        self.cards.prune(self.proj.cells(), |elem| now.contains_key(&elem) || before.contains(&elem));
    }

    fn cells<'a>(&'a self, level: &'a FoldersLevel) -> Cells<'a> {
        Cells { level, cards: &self.cards, proj: &self.proj }
    }
}

impl<H: ContentLike + FoldersLike + LibraryLike> StackPage<H> for Page {
    type Key = Sec;
    type Cards<'a> = Cells<'a>;

    fn revision(&self, cx: &Cx<'_, H>) -> u64 {
        let view = H::folders(cx);
        let status = view.current().map_or(0, |l| match l.status() {
            FoldersStatus::Loading => 1,
            FoldersStatus::Ready => 2,
            FoldersStatus::Empty => 3,
            FoldersStatus::Unavailable => 4,
            FoldersStatus::Failed => 5,
        });
        view.revision() << 4 | status << 1
    }

    /// A hole whose page has landed: focus on it moves to the row's own element in the landing's
    /// frame, in the same slot (the Collection page's rule).
    fn supersede(&self, cx: &Cx<'_, H>, want: &u32) -> Option<u32> {
        let slot = want.checked_sub(HOLE_ELEM_BASE)? as usize;
        let level = self.level(cx)?;
        let now = self.cells(level).elem_at(slot)?;
        (now != *want && now < HOLE_ELEM_BASE).then_some(now)
    }

    fn sections(&self, cx: &Cx<'_, H>, out: &mut Vec<SectionSpec<Sec>>) {
        out.push(SectionSpec::new(Sec::Head, Kind::Custom { height: HEAD_H, focusable: false }, HEAD_GROUP).ranked(1));
        out.push(SectionSpec::new(Sec::Items, Kind::Grid { spec: GridSpec::new(GRID_TOP, CONTENT_TOP) }, GRID_GROUP));
        let failed = self.level(cx).is_some_and(|l| l.status() == FoldersStatus::Failed);
        let retry = if failed {
            FoldersScreen::status_overlay(self.level(cx), cx.tick.ms, cx.measure).action_frame_measured(cx.measure)
        } else { None };
        out.push(SectionSpec::new(Sec::Status,
            Kind::Overlay { rect: retry.unwrap_or(STATUS_FRAME), focusable: retry.is_some() }, STATUS_GROUP).ranked(2));
    }

    fn fallback(&self, _cx: &Cx<'_, H>, out: &mut Vec<Sec>) {
        out.extend([Sec::Items, Sec::Status, Sec::Head]);
    }

    fn caption_policy(&self, k: Sec) -> Option<plx_ui::caption_policy::CaptionPolicy> {
        (k == Sec::Items).then(crate::registry::captions::policy)
    }

    fn cards<'a>(&'a self, cx: &'a Cx<'_, H>, k: Sec) -> Option<Cells<'a>> {
        match k {
            Sec::Items => self.level(cx).filter(|l| l.status() == FoldersStatus::Ready).map(|l| self.cells(l)),
            Sec::Head | Sec::Status => None,
        }
    }

    fn elem_of(&self, k: Sec) -> Option<u32> {
        match k { Sec::Status => Some(RETRY_ELEM), Sec::Head | Sec::Items => None }
    }

    fn card_has_menu(&self, cx: &Cx<'_, H>, _k: Sec, elem: &u32) -> bool {
        let Some(level) = self.level(cx) else { return false };
        self.cells(level).index(*elem)
            .and_then(|index| level.cell(index))
            .and_then(|cell| cell.item())
            .is_some_and(crate::registry::item_has_menu)
    }

    fn custom_draw(&self, k: Sec, f: &mut DrawFrame<'_, '_, H>, _r: Rect, scroll: f32) {
        let p = f.painter.alpha(f.page_alpha);
        match k {
            Sec::Head => {
                let alpha = head_alpha(scroll);
                if alpha <= 0.0 { return; }
                let p = p.alpha(alpha);
                if let Some(back_to) = H::back_to(f.cx) {
                    route_screen::draw_back_crumb(p,
                        Rect::new(MARGIN_X, HEAD_TOP - CRUMB_RISE - scroll, TITLE_W, CRUMB_BAND),
                        &back_to, f.measure);
                }
                let title = self.head_title(f.cx);
                let fit = f.measure.fit_line(&title, TITLE_W, theme::size::DISPLAY, true);
                Label::new(fit.as_ptr(), theme::size::DISPLAY, theme::TEXT_PRIMARY).bold()
                    .v(VAlign::CapTop).draw(p, Rect::new(MARGIN_X, HEAD_TOP - scroll, TITLE_W, 0.0));
            }
            Sec::Status => if self.level(f.cx).is_some_and(|l| l.status() != FoldersStatus::Ready) {
                let focused = f.focus.current.is_some_and(|key| key.entry == self.entry && key.elem == RETRY_ELEM);
                FoldersScreen::status_overlay(self.level(f.cx), f.cx.tick.ms, f.measure)
                    .focused(focused).draw_measured(&Env::inert(), p, f.measure);
            },
            Sec::Items => {}
        }
    }
}

plx_ui::focusable_via_view!(FoldersScreen, H: [ContentLike + FoldersLike + LibraryLike], view);

impl FoldersScreen {
    pub const SHAPE: &'static str = "FoldersScreen{entry:u32,section:u32,parent:u64,name:String,next_elem:u32,card_keys:[{sid:u32,rk:String,elem:u32}],stack:Stack{scroll:{pos:f32,vel:f32},target:f32,sections:[Grid{bands,pop}]}}";

    pub fn new(entry: EntryId, section: u32, parent: u64, name: String) -> Self {
        Self {
            page: Page { entry, section, parent, name, cards: CardKeys::new(FIRST_CARD_ELEM),
                proj: Projection::default(), want: Vec::new(), sent_window: None, want_end: 0,
                focus_elem: None, synced: None },
            stack: Stack::new(entry).hold_hint().home_when_unfocused(true),
            teardown_popped: false, ground: PageGround::new(), ground_seeded: false,
        }
    }

    fn view<H: ContentLike + FoldersLike + LibraryLike>(&self) -> impl Focusable<H> + '_ {
        self.stack.view(&self.page)
    }

    fn entry(&self) -> EntryId { self.page.entry }

    fn level<'a, H: FoldersLike>(&self, cx: &Cx<'a, H>) -> Option<&'a FoldersLevel> {
        self.page.level(cx)
    }

    fn request_open<H: ContentLike + FoldersLike + LibraryLike>(&mut self, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        // The server the level belongs to; for the first open of a section, the directory's current
        // library. `UNSET` only when neither answers, which the store treats as "no client yet".
        let sid = self.level(cx).map(|l| l.target.sid)
            .or_else(|| H::directory(cx).current().and_then(|i| H::directory(cx).sections().get(i)).and_then(|s| s.sid))
            .unwrap_or(plx_plex::plex::ServerId::UNSET);
        fx.push(plx_machine::machine::Fx::App(AppFx::Store(
            plx_data::stores::StoreId::Folders,
            plx_data::stores::StoreCmd::Folders(FoldersCmd::Open {
                target: plx_data::folders::FolderTarget {
                    sid, section: self.page.section as i64,
                    parent: (self.page.parent != 0).then(|| self.page.parent as i64),
                    title: self.page.name.clone(),
                },
            }),
        )));
    }

    fn plan<H: ContentLike + FoldersLike + LibraryLike>(&mut self, cx: &Cx<'_, H>) {
        let Some(level) = self.level(cx) else { return };
        let cols = ui_cards::GRID_COLS;
        let mut ranges = match self.stack.grid_window(&self.page, cx, Sec::Items) {
            Some(painted) => vec![painted.start.saturating_sub(cols)..painted.end + cols],
            None => vec![0..4 * cols],
        };
        ranges.push(0..cols);
        let focus = cx.focus.current.filter(|key| key.entry == self.entry()).map(|key| key.elem);
        if let Some(elem) = focus.filter(|&e| e != RETRY_ELEM) {
            if let Some(index) = self.page.cells(level).index(elem) { ranges.push(row_neighbourhood(index, cols)); }
        }
        self.page.want = ranges;
    }

    fn reproject<H: ContentLike + FoldersLike + LibraryLike>(&mut self, cx: &Cx<'_, H>) {
        let Some(level) = self.level(cx) else { return };
        self.plan(cx);
        self.page.project(level, Some(H::folders(cx).revision()));
    }

    /// Tell the store which indices the grid shows and which holds focus, so it keeps those pages
    /// and evicts the rest (the Collection page's rule, without the restore anchor).
    fn send_window<H: ContentLike + FoldersLike + LibraryLike>(&mut self, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) {
        let Some(level) = self.level(cx) else { return };
        let painted = self.stack.grid_window(&self.page, cx, Sec::Items).unwrap_or(0..0);
        let focus = cx.focus.current.filter(|key| key.entry == self.entry())
            .and_then(|key| self.page.cells(level).index(key.elem));
        let anchor = focus.filter(|at| painted.contains(at)).unwrap_or(painted.start);
        let wanted = anchor.saturating_sub(plx_data::folders::PAGE_SIZE)
            ..(anchor + 2 * plx_data::folders::PAGE_SIZE).max(self.page.want_end.min(anchor + 3 * plx_data::folders::PAGE_SIZE));
        let window = (wanted, focus);
        if self.page.sent_window.as_ref() == Some(&window) { return; }
        let (wanted, focus) = window.clone();
        self.page.sent_window = Some(window);
        fx.push(plx_machine::machine::Fx::App(AppFx::Store(
            plx_data::stores::StoreId::Folders,
            plx_data::stores::StoreCmd::Folders(FoldersCmd::Window { wanted, focus }),
        )));
    }

    /// The page's read-out, one per non-Ready status (the shared `StatusOverlay`, the Library's
    /// placement).
    fn status_overlay<'a>(level: Option<&FoldersLevel>, tick: u32,
        _measure: &dyn Measure) -> StatusOverlay<'a> {
        match level.map(|l| l.status()).unwrap_or(FoldersStatus::Loading) {
            // placeholder-exempt: builds the value only; counted where drawn (StatusOverlay::draw_geometry, Working)
            FoldersStatus::Loading => StatusOverlay::new(STATUS_FRAME, plx_platform::i18n::msg::browse_folders_loading_c(), StatusKind::Working).phase(tick),
            FoldersStatus::Empty => StatusOverlay::new(STATUS_FRAME, plx_platform::i18n::msg::browse_folders_empty_c(), StatusKind::Empty),
            FoldersStatus::Unavailable => StatusOverlay::new(Rect::FULL, plx_platform::i18n::msg::browse_folders_empty_c(), StatusKind::Failed),
            FoldersStatus::Failed => StatusOverlay::new(Rect::FULL, plx_platform::i18n::msg::browse_home_failed_c(), StatusKind::Failed)
                .page(plx_ui::icons::Icon::ServerBadgeMinus).action(plx_platform::i18n::msg::browse_action_retry_c()),
            FoldersStatus::Ready => StatusOverlay::new(STATUS_FRAME, c"", StatusKind::Empty),
        }
    }

    fn resync<H: ContentLike + FoldersLike + LibraryLike>(&mut self, cx: &Cx<'_, H>) {
        let Some(level) = self.level(cx) else { return };
        let revision = H::folders(cx).revision();
        if self.page.synced != Some(revision) {
            self.plan(cx);
            self.page.project(level, Some(revision));
            self.page.synced = Some(revision);
        }
    }

    fn tick<H: FoldersLike>(&mut self, t: Tick, cx: &Cx<'_, H>) {
        let Some(level) = self.level(cx) else { return };
        let focus = cx.focus.current.filter(|key| key.entry == self.entry());
        self.page.focus_elem = focus.map(|key| key.elem).filter(|&e| e != RETRY_ELEM);
        let focused = focus.and_then(|key| self.page.cells(level).index(key.elem))
            .and_then(|index| level.cell(index)).and_then(|cell| cell.item());
        let fallback = (0..level.shown()).find_map(|i| level.cell(i)).and_then(|cell| cell.item());
        let target = PageGround::page_target(focused.or(fallback).map(tile_facts::of));
        if self.ground_seeded { self.ground.key_target(target, t.dt()); }
        else { self.ground.jump_target(target); self.ground_seeded = true; }
    }

    pub fn focused_item<'a, H: FoldersLike>(&self,
        focus: Option<plx_machine::machine::FocusKey<u32>>, cx: &Cx<'a, H>) -> Option<&'a PmsMovie> {
        let level = self.level(cx)?;
        let index = focus.filter(|key| key.entry == self.entry())
            .and_then(|key| self.page.cells(level).index(key.elem))?;
        level.cell(index).and_then(|cell| cell.item())
    }
}

impl<H: ContentLike + FoldersLike + LibraryLike> Machine<H> for FoldersScreen {
    type Ev = ScreenEvent<H>;
    fn step(&mut self, ev: &Self::Ev, cx: &Cx<'_, H>, fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            ScreenEvent::Tick(_) => self.resync(cx),
            ScreenEvent::StoreChanged(ord, _) if *ord == plx_data::stores::StoreId::Folders.ord() => {
                if let Some(level) = self.level(cx) {
                    self.plan(cx);
                    self.page.project(level, Some(H::folders(cx).revision()));
                    self.page.synced = Some(H::folders(cx).revision());
                }
            }
            _ => {}
        }
        let card = match self.stack.on(&self.page, ev, cx, fx) {
            Some(StackEvent::Card(_, card)) => Some(card),
            _ => None,
        };
        if !matches!(ev, ScreenEvent::RestoreMemory(_)) { self.reproject(cx); }
        match ev {
            ScreenEvent::Tick(t) => {
                self.tick(*t, cx);
                if let Some(ui_cards::CardEvent::Want(range)) = card {
                    let loaded = self.level(cx).map_or(0, |l| l.shown());
                    self.page.want_end = range.end.max(loaded.saturating_add(plx_data::folders::PAGE_SIZE));
                    self.page.sent_window = None;
                }
                self.send_window(cx, fx);
                Handled::Yes
            }
            ScreenEvent::Enter(_) | ScreenEvent::Uncover => {
                self.request_open(cx, fx);
                fx.invalidate(Provenance::Nav);
                Handled::Yes
            }
            ScreenEvent::FocusMoved { .. } => { fx.invalidate(Provenance::Input); Handled::Yes }
            ScreenEvent::Activate(elem) if *elem == RETRY_ELEM => {
                self.request_open(cx, fx); Handled::Yes
            }
            ScreenEvent::PressCommit(_) => {
                if let Some(ui_cards::CardEvent::Activate(elem)) = card {
                    if elem < HOLE_ELEM_BASE {
                        if let Some(level) = self.level(cx) {
                            if let Some(index) = self.page.cells(level).index(elem) {
                                match level.cell(index) {
                                    Some(FolderCell::Folder { title, parent }) => {
                                        // The store's level stack and the nav stack move together:
                                        // the Drill first, then the push of the child page.
                                        fx.push(plx_machine::machine::Fx::App(AppFx::Store(
                                            plx_data::stores::StoreId::Folders,
                                            plx_data::stores::StoreCmd::Folders(FoldersCmd::Drill {
                                                parent: *parent, title: title.clone(),
                                            }),
                                        )));
                                        fx.push(plx_machine::machine::Fx::App(AppFx::Library(
                                            LibraryReq::OpenFolder {
                                                section: self.page.section,
                                                parent: *parent as u64,
                                                name: title.clone(),
                                            },
                                        )));
                                    }
                                    Some(FolderCell::Item(m)) => {
                                        fx.push(plx_machine::machine::Fx::App(AppFx::Content(ContentReq::Push(
                                            ContentArg::Detail { sid: m.sid, rk: m.rk.clone() }))));
                                    }
                                    None => {}
                                }
                            }
                        }
                    }
                }
                Handled::Yes
            }
            ScreenEvent::PressHold(_) => {
                if matches!(card, Some(ui_cards::CardEvent::Hold(_))) {
                    fx.push(plx_machine::machine::Fx::App(AppFx::Content(ContentReq::ItemMenu)));
                    Handled::Yes
                } else { Handled::No }
            }
            ScreenEvent::Input(InputEvent { kind: InputKind::Key { key: Key::Back, edge: Edge::Down, .. }, .. }) => {
                fx.push(plx_machine::machine::Fx::App(AppFx::Content(ContentReq::Back))); Handled::Yes
            }
            ScreenEvent::WillLeave(Leave::ForGood) | ScreenEvent::Unmount => {
                // The nav pop of THIS level is what pops the store's level stack; a level buried by
                // a deeper push (`Leave::Deeper`) keeps its rows.
                if self.level(cx).is_some() && !self.teardown_popped {
                    self.teardown_popped = true;
                    fx.push(plx_machine::machine::Fx::App(AppFx::Store(plx_data::stores::StoreId::Folders,
                        plx_data::stores::StoreCmd::Folders(FoldersCmd::Pop))));
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

impl<H: ContentLike + FoldersLike + LibraryLike> Screen<H> for FoldersScreen {
    fn focused_card<'a>(&self, cx: &Cx<'a, H>, focus: Option<plx_machine::machine::FocusKey<u32>>, at: Option<At>) -> Option<plx_ui::screen::FocusedCard<'a>> {
        let item = self.focused_item(focus, cx)?;
        let placed = focus.zip(at).and_then(|(key, at)| Focusable::<H>::place(self, &key.elem, cx, at));
        Some(plx_ui::screen::FocusedCard::new(item, placed))
    }
    fn redraw_focused(&self, f: &mut DrawFrame<'_, '_, H>, focus: Option<plx_machine::machine::FocusKey<u32>>) {
        self.stack.view(&self.page).redraw_focused(f, focus);
    }
    fn name(&self) -> &'static str { "folders" }
    fn state(&self) -> &dyn LogicalState { self }
    fn crumb(&self, cx: &Cx<'_, H>) -> Option<std::borrow::Cow<'_, str>> {
        H::back_to(cx).map(|name| std::borrow::Cow::Owned(name.into_owned()))
    }
    fn prepare(&mut self, _budget: &mut plx_ui::frame::Budget, _cx: &Cx<'_, H>) {}
    fn draw(&mut self, f: &mut DrawFrame<'_, '_, H>) {
        let p = f.painter.alpha(f.page_alpha);
        self.ground.draw(p, Rect::FULL);
        self.stack.view(&self.page).paint(f);
    }
    fn render(&self) -> RenderStrategy { RenderStrategy::Page }
    fn focus_source(&self) -> FocusSource { FocusSource::Engine }
    fn hit_source(&self) -> HitSource { HitSource::Engine }
    fn links(&self, _out: &mut Vec<Link>) {}
    fn memory_at(&self, _focus: Option<plx_machine::machine::FocusKey<u32>>) -> PageMemory {
        PageMemory::None
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> { Some(self) }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> { Some(self) }
}
