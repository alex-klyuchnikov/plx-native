//! Physically owned Browse-by-Folders model and its worker adapter.

use crate::folders::{FolderTarget, FoldersAdapter, FoldersState, FoldersView};
use plx_machine::machine::{Cx, Effects, Handled, Host, Machine};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use super::StoreEv;
use std::ops::Range;

#[derive(Clone, Debug)]
pub enum FoldersCmd {
    /// Open (or reopen) the section root. A store still holding this section's tree keeps its
    /// levels; a failed root retries.
    Open { target: FolderTarget },
    /// Push one level: the id the activated folder row carried, and its title (the crumb's word).
    Drill { parent: i64, title: String },
    /// The navigation pop of a folder page: drop the top level, keeping the parent's rows.
    Pop,
    /// The screen's window: the indices it shows and the focused index. Every landing evicts the
    /// pages outside these (see `PageCache::evict`).
    Window { wanted: Range<usize>, focus: Option<usize> },
    Close,
    Reset,
    /// The library header asking whether this section advertises the folder view. One probe per
    /// (section, client); the answer rides `Stores::folders_offered`.
    Probe { sid: plx_plex::plex::ServerId, section: i64 },
    /// Optimistic view-state edit (`viewstate::publish_view`), including the fan-out's copies.
    ApplyView { at: crate::viewstate::ViewAt, edit: crate::viewstate::ViewEdit },
}

pub struct FoldersStore {
    state: FoldersState,
    adapter: Arc<FoldersAdapter>,
    notice_gen: AtomicU32,
    notice_dirty: AtomicBool,
}

impl Default for FoldersStore {
    fn default() -> Self {
        Self { state: Default::default(), adapter: Arc::new(Default::default()),
            notice_gen: AtomicU32::new(0), notice_dirty: AtomicBool::new(false) }
    }
}

impl FoldersStore {
    /// Join the owner's view-state publish order (`Stores::view_ledger`).
    pub(crate) fn share_ledger(&mut self, ledger: &crate::viewledger::ViewLedger) {
        self.state.ledger = ledger.clone();
    }

    fn bump(&self) {
        self.notice_dirty.store(true, Ordering::Relaxed);
        self.notice_gen.fetch_add(1, Ordering::Relaxed);
    }
    pub fn gen(&self) -> u32 { self.notice_gen.load(Ordering::Relaxed) }
    pub fn take_notice(&self) -> Option<u32> {
        self.notice_dirty.swap(false, Ordering::Relaxed).then(|| self.gen())
    }
    pub fn view(&self) -> FoldersView<'_> { self.state.view() }
    /// The advertisement probe's answer for a section: `None` while unknown or unasked.
    pub fn offered(&self, sid: plx_plex::plex::ServerId, section: i64) -> Option<bool> {
        self.state.offered(sid, section)
    }
    /// The items the top level's page shows cards for (its wanted window).
    pub fn held_keys(&self) -> super::HeldKeys {
        let mut keys = super::HeldKeys::new();
        if let Some(level) = self.state.view().current() { level.held_keys(&mut keys); }
        keys
    }
    pub fn run(&mut self, cmd: FoldersCmd) -> bool {
        if matches!(cmd, FoldersCmd::Reset) { self.adapter = Arc::new(Default::default()); }
        let changed = self.state.run(&self.adapter, cmd);
        if changed { self.bump(); }
        changed
    }
    pub fn pump(&mut self, gate: &plx_machine::landgate::Gate) -> bool {
        let changed = self.state.pump_with_gate(&self.adapter, gate);
        if changed { self.bump(); }
        changed
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn generation_for_test(&self) -> u32 { self.state.generation() }
    #[cfg(any(test, feature = "test-support"))]
    pub fn adapter_for_test(&self) -> Arc<FoldersAdapter> { Arc::clone(&self.adapter) }
    #[cfg(any(test, feature = "test-support"))]
    pub fn install_for_test(&mut self, cells: Vec<crate::folders::FolderCell>, status: crate::folders::FoldersStatus) {
        self.state.install_for_test(cells, status);
        self.bump();
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn edit_for_test(&mut self, edit: impl FnOnce(&mut crate::folders::FoldersLevel)) {
        self.state.edit_for_test(edit);
        self.bump();
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn take_landing_for_test(&mut self) -> bool {
        let changed = self.state.take_landing_for_test(&self.adapter);
        if changed { self.bump(); }
        changed
    }
}

impl<H: Host> Machine<H> for FoldersStore {
    type Ev = StoreEv<FoldersCmd>;
    fn step(&mut self, ev: &Self::Ev, _cx: &Cx<'_, H>, _fx: &mut Effects<'_, H>) -> Handled {
        match ev {
            StoreEv::Cmd(cmd) => { self.run(cmd.clone()); }
            StoreEv::Pump { .. } => { self.pump(&plx_machine::landgate::Gate::default()); }
        }
        Handled::Yes
    }
}
