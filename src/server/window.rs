//! A window is a tab containing a tree of panes.

use std::collections::HashMap;

use crate::layout::{Dir, Layout, PaneId, Rect};
use crate::server::pane::Pane;

pub struct Window {
    pub id: u64,
    pub title: String,
    pub title_locked: bool,
    pub layout: Layout,
    pub panes: HashMap<PaneId, Pane>,
    pub active: PaneId,
    pub previous_active: Option<PaneId>,
    pub zoom: bool,
}

impl Window {
    pub fn new(id: u64, pane: Pane) -> Self {
        let pane_id = pane.id;
        let title = pane.title.clone();
        let mut panes = HashMap::new();
        panes.insert(pane_id, pane);
        Self {
            id,
            title,
            title_locked: false,
            layout: Layout::new(pane_id),
            panes,
            active: pane_id,
            previous_active: None,
            zoom: false,
        }
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(&self.active)
    }

    pub fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        self.panes.get_mut(&self.active)
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        self.panes.get_mut(&id)
    }

    pub fn display_title(&self) -> String {
        if self.title_locked {
            return self.title.clone();
        }
        if let Some(pane) = self.active_pane() {
            if !pane.title.is_empty() {
                return pane.title.clone();
            }
        }
        if self.title.is_empty() {
            "shell".to_string()
        } else {
            self.title.clone()
        }
    }

    pub fn set_active(&mut self, pane: PaneId) -> bool {
        if !self.panes.contains_key(&pane) || self.active == pane {
            return false;
        }
        self.previous_active = Some(self.active);
        self.active = pane;
        true
    }

    pub fn focus_next(&mut self, forward: bool) {
        let next = self.layout.next_pane(self.active, forward);
        self.set_active(next);
    }

    pub fn focus_previous(&mut self) {
        if let Some(prev) = self.previous_active {
            if self.panes.contains_key(&prev) {
                let current = self.active;
                self.active = prev;
                self.previous_active = Some(current);
            }
        }
    }

    pub fn split(&mut self, new_pane: Pane, dir: Dir) -> Result<(), String> {
        let target = self.active;
        let id = new_pane.id;
        if self.layout.split(target, id, dir).is_none() {
            return Err("active pane is no longer in the layout".into());
        }
        self.panes.insert(id, new_pane);
        self.previous_active = Some(target);
        self.active = id;
        self.zoom = false;
        Ok(())
    }

    /// Remove a pane without terminating its process (the caller owns process
    /// shutdown to keep this cheap in the event loop).
    pub fn remove_pane(&mut self, pane: PaneId) -> Option<Pane> {
        if !self.panes.contains_key(&pane) {
            return None;
        }
        self.layout.close(pane);
        let removed = self.panes.remove(&pane);
        if let Some(prev) = self.previous_active {
            if prev == pane {
                self.previous_active = None;
            }
        }
        if self.active == pane {
            let next = self
                .previous_active
                .filter(|id| self.panes.contains_key(id))
                .or_else(|| self.layout.leaves().first().copied());
            if let Some(next) = next {
                self.active = next;
            }
        }
        if self.layout.leaf_count() <= 1 {
            self.zoom = false;
        }
        removed
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Resize every pane according to the solved layout for `area`.
    pub fn apply_layout(&mut self, area: Rect) {
        if self.panes.is_empty() || area.is_empty() {
            return;
        }
        if self.zoom {
            if let Some(pane) = self.panes.get_mut(&self.active) {
                pane.resize(area.w.max(1), area.h.max(1));
            }
            return;
        }
        let framed = self.layout.leaf_count() > 1;
        let solved = self.layout.solve(area, framed);
        for placement in solved.panes {
            if let Some(pane) = self.panes.get_mut(&placement.pane) {
                pane.resize(placement.content.w.max(1), placement.content.h.max(1));
            }
        }
    }
}
