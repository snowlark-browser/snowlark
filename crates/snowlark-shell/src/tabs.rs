// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The tab strip's model: tabs in order, the active one, where new tabs go and which tab becomes
//! active when one closes (as Chrome does it), and the tab rectangles.

/// Tabs in strip order with one active (when there are any).
#[derive(Debug)]
pub struct TabList<T> {
    tabs: Vec<T>,
    active: usize,
    /// Tabs opened in the background from the active tab since it became active: the next one goes
    /// after them, so links open in reading order.
    children: usize,
}

impl<T> Default for TabList<T> {
    fn default() -> Self {
        TabList {
            tabs: Vec::new(),
            active: 0,
            children: 0,
        }
    }
}

/// Where a new tab goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// At the end, active (Ctrl+T, the new tab button).
    End,
    /// After the active tab and the ones already opened from it, not active (a middle-clicked link).
    Background,
    /// After the active tab, active (Ctrl+Shift+click).
    Foreground,
}

impl<T> TabList<T> {
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&T> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut T> {
        self.tabs.get_mut(self.active)
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        self.tabs.get(i)
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        self.tabs.get_mut(i)
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.tabs.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, T> {
        self.tabs.iter_mut()
    }

    pub fn position(&self, f: impl Fn(&T) -> bool) -> Option<usize> {
        self.tabs.iter().position(f)
    }

    /// Adds a tab; returns its index.
    pub fn add(&mut self, tab: T, placement: Placement) -> usize {
        if self.tabs.is_empty() {
            self.tabs.push(tab);
            self.active = 0;
            self.children = 0;
            return 0;
        }
        match placement {
            Placement::End => {
                self.tabs.push(tab);
                self.select(self.tabs.len() - 1);
            }
            Placement::Background => {
                let at = (self.active + 1 + self.children).min(self.tabs.len());
                self.tabs.insert(at, tab);
                self.children += 1;
                return at;
            }
            Placement::Foreground => {
                let at = self.active + 1;
                self.tabs.insert(at, tab);
                self.select(at);
            }
        }
        self.active
    }

    /// Makes tab `i` active.
    pub fn select(&mut self, i: usize) {
        if i < self.tabs.len() && i != self.active {
            self.active = i;
            self.children = 0;
        }
    }

    /// The next tab, wrapping (Ctrl+Tab); `delta` −1 for the previous one.
    pub fn select_relative(&mut self, delta: isize) {
        let n = self.tabs.len() as isize;
        if n > 0 {
            self.select((self.active as isize + delta).rem_euclid(n) as usize);
        }
    }

    /// Ctrl+1..8 select that tab, Ctrl+9 the last one.
    pub fn select_number(&mut self, n: u32) {
        if self.tabs.is_empty() || n == 0 {
            return;
        }
        if n >= 9 {
            self.select(self.tabs.len() - 1);
        } else if (n as usize) <= self.tabs.len() {
            self.select(n as usize - 1);
        }
    }

    /// Moves tab `from` to index `to` (a drag in the strip): the active tab stays active, and the
    /// tabs opened from it no longer go next to it.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let n = self.tabs.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = if self.active == from {
            to
        } else if from < self.active && to >= self.active {
            self.active - 1
        } else if from > self.active && to <= self.active {
            self.active + 1
        } else {
            self.active
        };
        self.children = 0;
    }

    /// Removes tab `i`. Closing the active tab activates the one to its right, else the one to
    /// its left.
    pub fn remove(&mut self, i: usize) -> Option<T> {
        if i >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(i);
        if self.tabs.is_empty() {
            self.active = 0;
        } else if i < self.active {
            self.active -= 1;
        } else if i == self.active {
            self.active = i.min(self.tabs.len() - 1);
            self.children = 0;
        } else if i <= self.active + self.children {
            self.children = self.children.saturating_sub(1);
        }
        Some(tab)
    }
}

/// The tab strip's tab widths: tabs share the room up to `max` each, never narrower than `min`.
pub fn tab_width(count: usize, room: f32, min: f32, max: f32) -> f32 {
    if count == 0 {
        return max;
    }
    (room / count as f32).clamp(min, max).floor()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(t: &TabList<&'static str>) -> Vec<&'static str> {
        t.iter().copied().collect()
    }

    #[test]
    fn new_tabs_and_link_tabs() {
        let mut t = TabList::default();
        t.add("a", Placement::End);
        t.add("b", Placement::End);
        assert_eq!(t.active(), Some(&"b"));
        t.select(0);
        t.add("a1", Placement::Background);
        t.add("a2", Placement::Background);
        assert_eq!(names(&t), ["a", "a1", "a2", "b"]);
        assert_eq!(t.active(), Some(&"a"));
        t.add("a0", Placement::Foreground);
        assert_eq!(names(&t), ["a", "a0", "a1", "a2", "b"]);
        assert_eq!(t.active(), Some(&"a0"));
        // A new active tab starts its own run.
        t.add("x", Placement::Background);
        assert_eq!(names(&t), ["a", "a0", "x", "a1", "a2", "b"]);
    }

    #[test]
    fn closing_picks_the_right_neighbour() {
        let mut t = TabList::default();
        for n in ["a", "b", "c", "d"] {
            t.add(n, Placement::End);
        }
        t.select(1);
        t.remove(1);
        assert_eq!(t.active(), Some(&"c"));
        t.select(2);
        t.remove(2);
        assert_eq!(t.active(), Some(&"c"));
        t.remove(0);
        assert_eq!(names(&t), ["c"]);
        assert_eq!(t.active(), Some(&"c"));
        t.remove(0);
        assert!(t.is_empty());
        assert!(t.active().is_none());
        assert!(t.remove(0).is_none());
    }

    #[test]
    fn closing_before_the_active_tab_keeps_it_active() {
        let mut t = TabList::default();
        for n in ["a", "b", "c"] {
            t.add(n, Placement::End);
        }
        t.remove(0);
        assert_eq!(t.active(), Some(&"c"));
        assert_eq!(t.active_index(), 1);
    }

    #[test]
    fn a_moved_tab_keeps_the_active_one_active() {
        let mut t = TabList::default();
        for n in ["a", "b", "c", "d"] {
            t.add(n, Placement::End);
        }
        t.select(1);
        t.add("x", Placement::Background);
        assert_eq!(names(&t), ["a", "b", "x", "c", "d"]);
        // The active tab moved.
        t.move_tab(1, 3);
        assert_eq!(names(&t), ["a", "x", "c", "b", "d"]);
        assert_eq!(t.active(), Some(&"b"));
        // Another tab moved past it, both ways.
        t.move_tab(0, 4);
        assert_eq!(names(&t), ["x", "c", "b", "d", "a"]);
        assert_eq!(t.active(), Some(&"b"));
        t.move_tab(4, 0);
        assert_eq!(t.active(), Some(&"b"));
        assert_eq!(t.active_index(), 3);
        // Tabs opened from it now go after it, not after the earlier ones.
        t.add("y", Placement::Background);
        assert_eq!(names(&t), ["a", "x", "c", "b", "y", "d"]);
        // Out of range: nothing.
        t.move_tab(9, 0);
        assert_eq!(t.len(), 6);
    }

    #[test]
    fn keyboard_selection() {
        let mut t = TabList::default();
        for n in ["a", "b", "c"] {
            t.add(n, Placement::End);
        }
        t.select_relative(1);
        assert_eq!(t.active(), Some(&"a"));
        t.select_relative(-1);
        assert_eq!(t.active(), Some(&"c"));
        t.select_number(2);
        assert_eq!(t.active(), Some(&"b"));
        t.select_number(7);
        assert_eq!(t.active(), Some(&"b"));
        t.select_number(9);
        assert_eq!(t.active(), Some(&"c"));
    }

    #[test]
    fn widths() {
        assert_eq!(tab_width(1, 1000.0, 48.0, 240.0), 240.0);
        assert_eq!(tab_width(10, 1000.0, 48.0, 240.0), 100.0);
        assert_eq!(tab_width(100, 1000.0, 48.0, 240.0), 48.0);
    }
}
