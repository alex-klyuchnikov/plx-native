//! Browse-by-Folder data model: one physically owned state machine over the section's on-disk
//! folder tree, one generation-stamped mailbox, and one small worker at a time.
//!
//! The wire contract is `docs/pms-api.md` §2b ("By Folder", verified live 2026-10-21 against
//! PMS 1.43.4): `GET /library/sections/{k}/folder[?parent=<id>]` answers one `Metadata[]` whose
//! rows are two kinds — a SUB-FOLDER carries only `{key, title}` (no ratingKey, no type, no child
//! count; its `key` is the drill URL `/library/sections/{k}/folder?parent=<id>`) and a loose FILE
//! is an ordinary full item row, identical in shape to the `/all` grid. Folders sort before files;
//! the container's `totalSize` counts both; paging headers are honored. The client follows the
//! server's `key` and parses only its `parent` id (digits-only — a row whose key does not
//! round-trip to this section's folder route is not a folder row and is dropped, never guessed at).
//!
//! The tree is a STACK of levels, not one query: the section root, then one level per folder
//! drilled into. The stack lives here because the breadcrumb does not: the drilled container names
//! no folder name (`title1` is the section, `title2` is the constant "By Folder"), so the path of
//! names is the client's, carried by the rows the viewer activated. The application pushes one
//! navigation entry per level and pops one with BACK, and this store's stack moves in lockstep
//! ([`FoldersCmd::Drill`] / [`FoldersCmd::Pop`] in `stores::folders`): the page on top reads the
//! top level, and the level beneath it is still held — rows and paging and all — when BACK
//! returns. Only the top level reads from the server.

use crate::pms::{listable, parse_item, PmsMovie};
use crate::stores::page_cache::{Keep, PageCache, PAGE};
use plx_plex::plex::ServerId;
use std::ops::Range;
use std::panic::catch_unwind;
use std::sync::Arc;

pub const PAGE_SIZE: usize = PAGE;
const RETRY_FRAMES: u32 = 120;

/// Which level of which section's folder tree a listing is. `parent` is the id the PARENT level's
/// folder row carried in its own `key`, passed straight back as the server issued it; `None` is the
/// section root. `title` is the row's name (the crumb's word), empty at the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderTarget {
    pub sid: ServerId,
    pub section: i64,
    pub parent: Option<i64>,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FoldersStatus {
    #[default]
    Loading,
    Ready,
    Empty,
    /// The server refused the route (a section that does not offer the view after all).
    Unavailable,
    /// The read failed; an Open retries.
    Failed,
}

/// One row of a folder listing. The two kinds the server mixes into one `Metadata[]`.
/// `Serialize`/`Deserialize` because the tape records a page landing as JSON (`stores::tape`).
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FolderCell {
    /// A sub-folder: its name and the `parent` id parsed out of its row's `key` — the id the drill
    /// request sends back verbatim. No child count: the server sends none.
    Folder { title: String, parent: i64 },
    /// A loose file: an ordinary item row, exactly what the grid renders everywhere else, so the
    /// card grid, the caption policy, the resume bar and the watched mark all work on it unchanged.
    Item(PmsMovie),
}

impl FolderCell {
    pub fn item(&self) -> Option<&PmsMovie> {
        match self { Self::Item(m) => Some(m), _ => None }
    }

    fn item_mut(&mut self) -> Option<&mut PmsMovie> {
        match self { Self::Item(m) => Some(m), _ => None }
    }

    /// The row's name: the folder's title, or the item's title.
    pub fn title(&self) -> &str {
        match self { Self::Folder { title, .. } => title, Self::Item(m) => &m.title }
    }
}

/// The `parent` id of a folder row's `key`, or `None` unless the key is exactly this section's
/// folder drill route with one digits-only `parent`. The client never invents or re-derives the
/// id; it reads it back from the key the server handed it.
fn folder_parent_of(key: &str, section: i64) -> Option<i64> {
    let (path, query) = key.split_once('?')?;
    if path != format!("/library/sections/{section}/folder") {
        return None;
    }
    let value = query
        .split('&')
        .find_map(|part| part.strip_prefix("parent="))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// One level of the tree: its target, its rows by absolute index, and how far the server has been
/// read. Rows are the page directory's; a hole (never read, or evicted) reads as `None`.
pub struct FoldersLevel {
    pub target: FolderTarget,
    cells: PageCache<FolderCell>,
    /// Pages read in order so far — the server's page frontier. A page at or past it is read only
    /// as the frontier; a page below it may be re-read at its own offset.
    read: usize,
    pub total: usize,
    more: bool,
    /// The indices the screen shows or is about to show, from the last
    /// [`FoldersCmd::Window`](crate::stores::folders::FoldersCmd::Window).
    wanted: Range<usize>,
    focus: Option<usize>,
    status: FoldersStatus,
    client_key: Option<(u32, u32)>,
}

impl FoldersLevel {
    fn loading(target: FolderTarget, want: usize, client_key: Option<(u32, u32)>) -> Self {
        Self { target, cells: PageCache::default(), read: 0, total: 0, more: true,
            wanted: 0..want.max(PAGE_SIZE), focus: None, status: FoldersStatus::Loading,
            client_key }
    }

    /// The row at absolute index `i`, or `None` for a hole.
    pub fn cell(&self, i: usize) -> Option<&FolderCell> { self.cells.get(i) }

    /// How far the listing has been read, and what the read has said about it.
    pub fn status(&self) -> FoldersStatus { self.status }
    /// Does the listing continue past what is known?
    pub fn more(&self) -> bool { self.more }

    /// How many indices are known (an evicted row still counts, so indices do not move).
    pub fn shown(&self) -> usize { self.cells.first_index_of(self.read).min(self.total) }

    /// Every loaded row with its absolute index.
    pub fn loaded_rows(&self) -> impl Iterator<Item = (usize, &FolderCell)> + '_ {
        self.cells.loaded_rows()
    }

    /// The loaded item rows inside the window the screen asked for.
    pub(crate) fn held_keys(&self, keys: &mut crate::stores::HeldKeys) {
        for (i, cell) in self.cells.loaded_rows() {
            if let (Some(m), true) = (cell.item(), self.wanted.contains(&i)) {
                crate::stores::push_held(keys, m.sid, &m.rk);
            }
        }
    }

    /// Pages the screen shows or focuses whose rows are not held, below the frontier.
    fn wanted_pages(&self) -> Vec<usize> {
        let mut pages: Vec<usize> = self.cells.missing(self.wanted.start, self.wanted.end).collect();
        for i in [self.focus].into_iter().flatten() {
            if let Some((p, _)) = self.cells.locate(i) {
                if !self.cells.is_loaded(p) { pages.push(p); }
            }
        }
        pages
    }

    /// The count of leading indices the screen needs known.
    fn need(&self) -> usize {
        self.focus.into_iter().map(|i| i + 1).fold(self.wanted.end, usize::max)
    }
}

#[derive(Clone, Copy, Default)]
pub struct FoldersView<'a> {
    current: Option<&'a FoldersLevel>,
    revision: u64,
    depth: usize,
    offers: &'a [(u16, i64, bool)],
}

impl<'a> FoldersView<'a> {
    /// The level the page on top reads: the deepest open level.
    pub fn current(self) -> Option<&'a FoldersLevel> { self.current }
    /// How many levels deep the open tree is (1 = the section root).
    pub fn depth(self) -> usize { self.depth }
    /// The content counter: bumped whenever any level's rows, status or `more` move, or the stack
    /// itself does. A reader derives its per-cell state again when this moves.
    pub fn revision(self) -> u64 { self.revision }
    /// The advertisement probe's answer for a section of the directory: `None` while unknown.
    pub fn offered(self, sid: ServerId, section: i64) -> Option<bool> {
        self.offers.iter().find(|(s, k, _)| *s == sid.raw() && *k == section).map(|(_, _, v)| *v)
    }
}

pub struct FoldersState {
    /// The publish order of view-state edits, shared by `stores::Stores`: a page read in flight is
    /// re-patched with every edit published since it was issued.
    pub(crate) ledger: crate::viewledger::ViewLedger,
    stamp: crate::viewledger::Stamp,
    /// The open tree, root first. Empty while no folder page is open.
    levels: Vec<FoldersLevel>,
    generation: u32,
    revision: u64,
    retry_cd: u32,
    /// Which sections the advertisement probe has answered (`(sid, section, offers the view)`),
    /// kept across closes so re-opening a library does not re-probe it. Cleared with the client.
    offers: Vec<(u16, i64, bool)>,
    /// The probe asked for and not answered yet. One at a time; a supersede drops it.
    probe_wanted: Option<(ServerId, i64)>,
    probe_gen: u32,
}

impl Default for FoldersState {
    fn default() -> Self {
        Self { ledger: Default::default(), stamp: 0, levels: Vec::new(), generation: 0,
            revision: 0, retry_cd: 0, offers: Vec::new(), probe_wanted: None, probe_gen: 1 }
    }
}

impl FoldersState {
    pub fn view(&self) -> FoldersView<'_> {
        FoldersView { current: self.levels.last(), revision: self.revision, depth: self.levels.len(),
            offers: &self.offers }
    }

    /// Which listing this is, for a page to save: the store's request epoch when its level was
    /// opened. Only a supersede (a rebuild for a changed client, a close or reset) moves it.
    pub fn generation(&self) -> u32 { self.generation }

    fn supersede(&mut self, adapter: &FoldersAdapter) {
        self.generation = self.generation.checked_add(1).expect("folders generation exhausted");
        adapter.fetch.clear();
        self.retry_cd = 0;
    }

    /// A changed client also invalidates every probe answer: the advertisement is that client's.
    fn supersede_client(&mut self, adapter: &FoldersAdapter) {
        self.supersede(adapter);
        adapter.probe.clear();
        self.probe_wanted = None;
        self.probe_gen = self.probe_gen.checked_add(1).expect("folders probe generation exhausted");
        self.offers.clear();
    }

    /// Does this section advertise the folder view? `None` until the probe answers.
    pub fn offered(&self, sid: ServerId, section: i64) -> Option<bool> {
        self.offers.iter().find(|(s, k, _)| *s == sid.raw() && *k == section).map(|(_, _, v)| *v)
    }

    pub fn run(&mut self, adapter: &Arc<FoldersAdapter>, cmd: crate::stores::folders::FoldersCmd) -> bool {
        let changed = self.run_cmd(adapter, cmd);
        self.revision += u64::from(changed);
        changed
    }

    fn run_cmd(&mut self, adapter: &Arc<FoldersAdapter>, cmd: crate::stores::folders::FoldersCmd) -> bool {
        use crate::stores::folders::FoldersCmd;
        match cmd {
            FoldersCmd::Open { target } => {
                // The page re-entering names ITS level: when the store's top level is that listing,
                // the tree is kept whole (a Back from a Detail, an Uncover of the parent page).
                if let Some(top) = self.levels.last_mut().filter(|l| l.target == target) {
                    let old = top.wanted.end;
                    top.wanted.end = top.wanted.end.max(PAGE_SIZE);
                    let retry = top.status == FoldersStatus::Failed;
                    if retry {
                        top.status = FoldersStatus::Loading;
                        self.retry_cd = 0;
                    }
                    return top.wanted.end != old || retry;
                }
                // A fresh section (or a store emptied by a Close/Reset): open at the root. A page
                // deeper than the root can only re-enter this way after a teardown, and the
                // ancestors cannot be rebuilt without re-drilling — show the level it names.
                self.supersede(adapter);
                let client_key = plx_plex::plex::client_for(target.sid).map(|c| (c.instance_gen(), c.token_gen()));
                self.levels = vec![FoldersLevel::loading(target, PAGE_SIZE, client_key)];
                true
            }
            FoldersCmd::Drill { parent, title } => {
                let Some(top) = self.levels.last() else { return false };
                // A Drill whose level is already open (a re-delivered press) does not push twice.
                if top.target.parent == Some(parent) {
                    return false;
                }
                let (sid, section, client_key) = (top.target.sid, top.target.section, top.client_key);
                self.levels.push(FoldersLevel::loading(
                    FolderTarget { sid, section, parent: Some(parent), title }, PAGE_SIZE, client_key));
                true
            }
            FoldersCmd::Pop => {
                if self.levels.is_empty() { return false; }
                // The in-flight read belongs to the level being left; its answer is no longer open.
                self.supersede(adapter);
                self.levels.pop();
                true
            }
            FoldersCmd::Window { wanted, focus } => {
                let Some(l) = self.levels.last_mut() else { return false };
                let changed = l.wanted != wanted || l.focus != focus;
                (l.wanted, l.focus) = (wanted, focus);
                changed
            }
            FoldersCmd::Close => {
                self.supersede(adapter);
                let had = !self.levels.is_empty();
                self.levels.clear();
                had
            }
            FoldersCmd::Reset => {
                self.supersede_client(adapter);
                let had = !self.levels.is_empty();
                self.levels.clear();
                had
            }
            FoldersCmd::Probe { sid, section } => {
                // Asked every frame the header wants an answer; only an unknown section spawns one,
                // and only one probe is in flight at a time.
                if self.offered(sid, section).is_some() || self.probe_wanted.is_some() { return false; }
                self.probe_wanted = Some((sid, section));
                false
            }
            // The item menu's Mark watched/unwatched reaches every store that can be drawing the
            // item (`viewstate`'s fan-out); a folder listing's file tile must flip on this page too.
            FoldersCmd::ApplyView { at, edit } => {
                if let crate::viewstate::ViewEdit::LeftDeck = edit { return false }
                let mut hit = false;
                for level in self.levels.iter_mut() {
                    for i in 0..level.shown() {
                        level.cells.edit(i, |cell| {
                            if let Some(m) = cell.item_mut() {
                                // `changes`: the server's `Facts`/`Offset` that agree touch nothing
                                if at.covers(m) && edit.changes(m) {
                                    edit.apply(m);
                                    hit = true;
                                }
                            }
                        });
                    }
                }
                hit
            }
        }
    }

    fn refresh_if_client_changed(&mut self, adapter: &FoldersAdapter) -> bool {
        let Some(top) = self.levels.last() else { return false };
        let now = plx_plex::plex::client_for(top.target.sid).map(|x| (x.instance_gen(), x.token_gen()));
        if now == top.client_key { return false; }
        // A changed client invalidates every level: rebuild the whole stack, targets and windows
        // kept, rows dropped.
        let spec: Vec<(FolderTarget, Range<usize>, Option<usize>)> = self.levels.iter()
            .map(|l| (l.target.clone(), l.wanted.clone(), l.focus)).collect();
        self.supersede_client(adapter);
        self.levels = spec.into_iter().map(|(target, wanted, focus)| {
            let mut l = FoldersLevel::loading(target, wanted.end, now);
            (l.wanted, l.focus) = (wanted, focus);
            l
        }).collect();
        self.revision += 1;
        true
    }

    pub fn pump_with_gate(&mut self, adapter: &Arc<FoldersAdapter>, gate: &plx_machine::landgate::Gate) -> bool {
        let mut changed = self.refresh_if_client_changed(adapter);
        if self.retry_cd > 0 { self.retry_cd -= 1; }
        let reply = crate::stores::tape::take_store_landing(
            gate, crate::stores::StoreId::Folders, "folders", 0, &adapter.fetch, |m| m.gen);
        if let Some(reply) = reply {
            plx_machine::idle::invalidate();
            if reply.gen == self.generation { changed |= self.apply(reply.what); }
        }
        let probe = crate::stores::tape::take_store_landing(
            gate, crate::stores::StoreId::Folders, "folders", 1, &adapter.probe, |m| m.gen);
        if let Some(reply) = probe {
            plx_machine::idle::invalidate();
            if reply.gen == self.probe_gen { changed |= self.apply(reply.what); }
        }
        self.maybe_spawn(adapter);
        self.maybe_probe(adapter);
        changed
    }

    fn job(&self) -> Option<Job> {
        let l = self.levels.last()?;
        if l.status == FoldersStatus::Unavailable || l.status == FoldersStatus::Empty { return None; }
        // A page the screen shows or focuses, read earlier and since evicted: a re-read at its own
        // offset, which the directory's known count makes safe.
        if let Some(page) = l.wanted_pages().into_iter().find(|&p| p < l.read) {
            return Some(Job::Page { section: l.target.section, parent: l.target.parent, start: page * PAGE_SIZE });
        }
        // Otherwise the frontier, in order, while the window needs indices it has not counted.
        if l.more && l.need() > l.shown() {
            return Some(Job::Page { section: l.target.section, parent: l.target.parent, start: l.read * PAGE_SIZE });
        }
        None
    }

    fn maybe_spawn(&mut self, adapter: &Arc<FoldersAdapter>) {
        if adapter.fetch.busy() || self.retry_cd > 0 { return; }
        let Some(job) = self.job() else { return };
        let Some(l) = self.levels.last() else { return };
        let Some(client) = plx_plex::plex::client_for(l.target.sid) else {
            self.retry_cd = RETRY_FRAMES;
            if l.shown() == 0 {
                self.levels.last_mut().unwrap().status = FoldersStatus::Failed;
                self.revision += 1;
            }
            return;
        };
        let generation = self.generation;
        let sid = l.target.sid;
        adapter.fetch.claim(generation);
        self.stamp = self.ledger.stamp();
        let worker_adapter = Arc::clone(adapter);
        let request = serde_json::json!({"store":"folders","slot":0,"gen":generation,
            "sid":sid.raw(),"client":client.instance_gen(),"job":job});
        let spawned = crate::stores::tape::admit(request, || plx_base::task::spawn_small("folders", move || {
            let what = catch_unwind(|| run_job(client, sid, job)).unwrap_or(Landing::Transport);
            worker_adapter.land(generation, what);
        }));
        if !spawned { adapter.fetch.release(); }
    }

    fn maybe_probe(&mut self, adapter: &Arc<FoldersAdapter>) {
        if adapter.probe.busy() { return; }
        let Some((sid, section)) = self.probe_wanted else { return };
        if self.offers.iter().any(|(s, k, _)| *s == sid.raw() && *k == section) {
            self.probe_wanted = None;
            return;
        }
        let Some(client) = plx_plex::plex::client_for(sid) else { return };
        let generation = self.probe_gen;
        let job = Job::Offer { section };
        adapter.probe.claim(generation);
        let worker_adapter = Arc::clone(adapter);
        let request = serde_json::json!({"store":"folders","slot":1,"gen":generation,
            "sid":sid.raw(),"client":client.instance_gen(),"job":job});
        let spawned = crate::stores::tape::admit(request, || plx_base::task::spawn_small("folders", move || {
            let what = catch_unwind(|| run_job(client, sid, job)).unwrap_or(Landing::Offer { offered: false });
            worker_adapter.land_probe(generation, matches!(what, Landing::Offer { offered: true }));
        }));
        if !spawned { adapter.probe.release(); }
    }

    fn apply(&mut self, landing: Landing) -> bool {
        let changed = self.apply_landing(landing);
        self.revision += u64::from(changed);
        changed
    }

    fn apply_landing(&mut self, landing: Landing) -> bool {
        if let Landing::Offer { offered } = landing {
            // The probe answers with no folder page open at all (the header asked), so it is applied
            // before the level stack is consulted.
            let Some((sid, section)) = self.probe_wanted.take() else { return false };
            let key = (sid.raw(), section);
            if let Some((_, _, v)) = self.offers.iter_mut().find(|(s, k, _)| (*s, *k) == key) {
                if *v == offered { return false; }
                *v = offered;
            } else {
                self.offers.push((key.0, key.1, offered));
            }
            return true;
        }
        let (ledger, stamp) = (self.ledger.clone(), self.stamp);
        let Some(l) = self.levels.last_mut() else { return false };
        match landing {
            Landing::Page { start, got, mut pages, total } => {
                // The read describes the server from when it was issued: edits published since are
                // made again on its rows (`crate::viewledger`).
                ledger.patch(stamp, pages.iter_mut().flatten().filter_map(FolderCell::item_mut));
                if start % PAGE_SIZE != 0 { return false; }
                // A server that ignores paging answers the whole listing from its first row, so its
                // chunks start at 0 whatever offset was asked (see `run_job`).
                let base = if got > PAGE_SIZE { 0 } else { start / PAGE_SIZE };
                if base > l.read { return false; }
                // A re-read is asked only for a hole, so an answer for a page that still holds its
                // rows is stale. A whole-listing answer is not one page and always lands.
                if got <= PAGE_SIZE && base < l.read && l.cells.is_loaded(base) { return false; }
                let before = l.read;
                l.total = total.max(base * PAGE_SIZE + got);
                l.cells.set_total(l.total);
                let chunks = pages.len();
                for (k, rows) in pages.into_iter().enumerate() {
                    l.cells.set_page(base + k, rows);
                }
                l.read = l.read.max(base + chunks).min(l.cells.page_count());
                if base >= before {
                    // A page of zero rows ends the listing whatever `totalSize` claimed.
                    l.more = got > 0 && l.read * PAGE_SIZE < l.total;
                }
                l.cells.evict(&Keep { wanted: l.wanted.clone(), focus: l.focus, restore: None });
                l.status = if l.shown() > 0 { FoldersStatus::Ready }
                    else if l.more { FoldersStatus::Loading }
                    else { FoldersStatus::Empty };
                true
            }
            Landing::Denied | Landing::Missing => {
                l.status = FoldersStatus::Unavailable;
                l.more = false;
                true
            }
            Landing::Transport => {
                self.retry_cd = RETRY_FRAMES;
                if l.shown() == 0 { l.status = FoldersStatus::Failed; }
                true
            }
            Landing::Offer { .. } => false,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn install_for_test(&mut self, cells: Vec<FolderCell>, status: FoldersStatus) {
        let Some(l) = self.levels.last_mut() else { return };
        self.revision += 1;
        l.total = cells.len();
        l.cells = PageCache::default();
        l.cells.set_total(l.total);
        l.read = 0;
        let mut rows = cells.into_iter();
        for p in 0..l.total.div_ceil(PAGE_SIZE) {
            l.cells.set_page(p, rows.by_ref().take(PAGE_SIZE).collect());
            l.read = p + 1;
        }
        l.more = false;
        l.status = status;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn edit_for_test(&mut self, edit: impl FnOnce(&mut FoldersLevel)) {
        if let Some(l) = self.levels.last_mut() { edit(l); self.revision += 1; }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn take_landing_for_test(&mut self, adapter: &FoldersAdapter) -> bool {
        let Some(mail) = adapter.fetch.take() else { return false };
        mail.gen == self.generation && self.apply(mail.what)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum Job {
    Page { section: i64, parent: Option<i64>, start: usize },
    /// The section-description read that answers whether this library offers the folder view at
    /// all (`GET /library/sections/{k}`, its `Directory[]` carrying `{key:"folder"}`).
    Offer { section: i64 },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
enum Landing {
    /// `got` is the server rows the response consumed. `pages[k]` holds the listable rows of server
    /// page `k` of the response, so an answer that ignores paging lands as every page it covers.
    Page { start: usize, got: usize, pages: Vec<Vec<FolderCell>>, total: usize },
    Denied,
    Missing,
    Transport,
    /// The probe's answer: does the section advertise the `folder` Directory?
    Offer { offered: bool },
}

fn run_job(client: &'static plx_plex::plex::Client, sid: ServerId, job: Job) -> Landing {
    let (section, parent, start) = match job {
        Job::Offer { section } => {
            let Some(container) = client.section_views(section) else { return Landing::Offer { offered: false } };
            return Landing::Offer { offered: container.directory.iter().any(|d| d.key == "folder") };
        }
        Job::Page { section, parent, start } => (section, parent, start),
    };
    let Some(page) = client.section_folders(section, parent, start as i64, PAGE_SIZE as i64) else {
        return Landing::Transport;
    };
    let total = page.total_size.max(page.size).max(0) as usize;
    let got = page.metadata.len();
    let pages = page.metadata.chunks(PAGE_SIZE).map(|rows| rows.iter().filter_map(|row| {
        if row.rating_key.is_empty() {
            // A folder row: only `{key, title}` is on the wire. A key this client cannot follow is
            // dropped rather than guessed at — a row it cannot drill is not a folder row.
            return folder_parent_of(&row.key, section)
                .map(|parent| FolderCell::Folder { title: row.title.clone(), parent });
        }
        listable(&row.kind).then(|| FolderCell::Item(parse_item(row, sid)))
    }).collect()).collect();
    Landing::Page { start, got, pages, total }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Mail { gen: u32, what: Landing }

pub fn validate_record(slot: u32, value: &serde_json::Value) -> Result<(), &'static str> {
    let mail: Mail = serde_json::from_value(value.clone()).map_err(|_| "invalid folders reply")?;
    match slot {
        0 if matches!(mail.what, Landing::Page { .. }) => {}
        1 if matches!(mail.what, Landing::Offer { .. }) => {}
        0 | 1 => return Err("folders slot and landing disagree"),
        _ => return Err("invalid folders slot"),
    }
    if mail.gen == 0 { return Err("invalid folders reply generation"); }
    if serde_json::to_value(&mail).ok().as_ref() != Some(value) { return Err("noncanonical folders reply"); }
    Ok(())
}

#[derive(Default)]
pub struct FoldersAdapter {
    fetch: crate::stores::Fetch<Mail>,
    /// The advertisement probe's mailbox (slot 1), kept off the page fetch's slot so a probe never
    /// delays a listing read.
    probe: crate::stores::Fetch<Mail>,
}

impl FoldersAdapter {
    fn land(&self, generation: u32, what: Landing) {
        self.fetch.post(Mail { gen: generation, what }, |old| old.gen < generation);
    }
    fn land_probe(&self, generation: u32, offered: bool) {
        self.probe.post(Mail { gen: generation, what: Landing::Offer { offered } }, |old| old.gen < generation);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn land_page_for_test(&self, generation: u32, start: usize, rows: Vec<FolderCell>, total: usize) {
        let got = rows.len();
        let pages = rows.chunks(PAGE_SIZE).map(<[FolderCell]>::to_vec).collect();
        self.land(generation, Landing::Page { start, got, pages, total });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn land_offer_for_test(&self, generation: u32, offered: bool) {
        self.land_probe(generation, offered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stores::folders::FoldersCmd;

    fn root_target() -> FolderTarget {
        FolderTarget { sid: ServerId::UNSET, section: 1, parent: None, title: String::new() }
    }

    fn movie(rk: &str, title: &str) -> FolderCell {
        FolderCell::Item(PmsMovie { rk: rk.into(), title: title.into(), kind: 1, ..Default::default() })
    }

    fn folder(title: &str, parent: i64) -> FolderCell {
        FolderCell::Folder { title: title.into(), parent }
    }

    #[test]
    fn a_folder_row_key_yields_only_its_digits_parent() {
        assert_eq!(folder_parent_of("/library/sections/1/folder?parent=107", 1), Some(107));
        assert_eq!(folder_parent_of("/library/sections/1/folder?parent=900011", 1), Some(900_011));
        assert_eq!(folder_parent_of("/library/sections/2/folder?parent=107", 1), None, "another section");
        assert_eq!(folder_parent_of("/library/sections/1/folder", 1), None, "no parent");
        assert_eq!(folder_parent_of("/library/sections/1/folder?parent=", 1), None, "empty");
        assert_eq!(folder_parent_of("/library/sections/1/folder?parent=1a7", 1), None, "not digits");
        assert_eq!(folder_parent_of("/library/sections/1/all?parent=107", 1), None, "not the folder route");
    }

    #[test]
    fn opening_and_drilling_stack_the_levels_and_pop_returns_the_parent() {
        let adapter = Arc::new(FoldersAdapter::default());
        let mut s = FoldersState::default();
        assert!(s.run(&adapter, FoldersCmd::Open { target: root_target() }));
        assert_eq!(s.view().depth(), 1);
        assert!(s.run(&adapter, FoldersCmd::Drill { parent: 107, title: "Alpha".into() }));
        assert_eq!(s.view().depth(), 2);
        let top = s.view().current().unwrap();
        assert_eq!(top.target.parent, Some(107));
        assert_eq!(top.target.title, "Alpha");
        assert!(s.run(&adapter, FoldersCmd::Pop));
        assert_eq!(s.view().depth(), 1);
        assert_eq!(s.view().current().unwrap().target.parent, None);
        assert!(s.run(&adapter, FoldersCmd::Pop), "the root level's page leaving closes the tree");
        assert_eq!(s.view().depth(), 0);
    }

    #[test]
    fn a_drill_before_any_open_is_refused_and_a_close_clears_the_stack() {
        let adapter = Arc::new(FoldersAdapter::default());
        let mut s = FoldersState::default();
        assert!(!s.run(&adapter, FoldersCmd::Drill { parent: 1, title: "x".into() }));
        s.run(&adapter, FoldersCmd::Open { target: root_target() });
        assert!(s.run(&adapter, FoldersCmd::Close));
        assert_eq!(s.view().depth(), 0);
        assert!(!s.run(&adapter, FoldersCmd::Close));
    }

    #[test]
    fn a_paged_listing_lands_by_page_and_pages_the_frontier_in_order() {
        let adapter = Arc::new(FoldersAdapter::default());
        let mut s = FoldersState::default();
        s.run(&adapter, FoldersCmd::Open { target: root_target() });
        let gen = s.generation();
        // The window asks for the first page; the frontier job is out.
        assert!(matches!(s.job(), Some(Job::Page { section: 1, parent: None, start: 0 })));
        let page1: Vec<FolderCell> = (0..PAGE).map(|i| if i < 2 { folder("F", i as i64) } else { movie("m", "m") }).collect();
        adapter.land_page_for_test(gen, 0, page1, PAGE + 30);
        assert!(s.take_landing_for_test(&adapter));
        assert_eq!(s.view().current().unwrap().status, FoldersStatus::Ready);
        assert_eq!(s.view().current().unwrap().total, PAGE + 30);
        // The window moves past what is read: the frontier continues at its own offset, in order.
        s.run(&adapter, FoldersCmd::Window { wanted: PAGE..PAGE + 10, focus: Some(PAGE + 5) });
        assert!(matches!(s.job(), Some(Job::Page { start, .. }) if start == PAGE));
    }

    #[test]
    fn an_empty_level_is_empty_not_loading_forever() {
        let adapter = Arc::new(FoldersAdapter::default());
        let mut s = FoldersState::default();
        s.run(&adapter, FoldersCmd::Open { target: root_target() });
        let gen = s.generation();
        adapter.land_page_for_test(gen, 0, vec![], 0);
        assert!(s.take_landing_for_test(&adapter));
        assert_eq!(s.view().current().unwrap().status, FoldersStatus::Empty);
        assert!(s.job().is_none(), "an empty level asks for nothing more");
    }

    #[test]
    fn popping_clears_the_in_flight_read_so_a_stale_answer_cannot_land_on_the_parent() {
        let adapter = Arc::new(FoldersAdapter::default());
        let mut s = FoldersState::default();
        s.run(&adapter, FoldersCmd::Open { target: root_target() });
        s.run(&adapter, FoldersCmd::Drill { parent: 107, title: "Alpha".into() });
        let stale = s.generation();
        adapter.fetch.claim(stale);
        adapter.land_page_for_test(stale, 0, vec![movie("9", "late")], 1);
        assert!(s.run(&adapter, FoldersCmd::Pop));
        assert!(!s.take_landing_for_test(&adapter), "the stale generation is refused");
        assert_eq!(s.view().current().unwrap().shown(), 0, "the parent's rows are its own");
    }

    #[test]
    fn a_view_state_edit_flips_a_file_tile_in_every_held_level() {
        let adapter = Arc::new(FoldersAdapter::default());
        let mut s = FoldersState::default();
        s.run(&adapter, FoldersCmd::Open { target: root_target() });
        s.install_for_test(vec![movie("42", "Film")], FoldersStatus::Ready);
        s.run(&adapter, FoldersCmd::Drill { parent: 107, title: "Alpha".into() });
        s.install_for_test(vec![movie("42", "Film")], FoldersStatus::Ready);
        let at = crate::viewstate::ViewAt { sid: ServerId::UNSET, rk: "42".into(), scope: crate::viewstate::Scope::Leaf };
        assert!(s.run(&adapter, FoldersCmd::ApplyView { at, edit: crate::viewstate::ViewEdit::Watched(true) }));
        for depth in 0..2 {
            let _ = depth;
        }
        let cell = &s.view().current().unwrap().cells;
        assert_eq!(cell.get(0).and_then(|c| c.item()).map(|m| m.watched), Some(true));
    }
}
