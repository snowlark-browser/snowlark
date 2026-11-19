// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Caret movement over rendered content: what `Selection.modify("move", ..., "character")` and the
//! arrow keys step over. A caret position is one the user can see, so the walk goes by rendered
//! characters (white space collapsed by CSS Text §4.1.1 does not count), crosses a block or `br`
//! boundary as one step of its own (to the next line's start, the previous line's end), and steps
//! over an atomic inline (an image, a form control) or a non-editable element inside an editing host
//! as one unit.

use snowlark_dom::{local_name, NodeId};

use crate::engine::ranges::Bp;
use crate::engine::Engine;

/// What a walk through rendered content meets, in tree order (or reverse tree order).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stop {
    Text(NodeId),
    Br(NodeId),
    /// An element the caret goes before or after but not into.
    Atomic(NodeId),
    /// Into a block-level element (at its start going forward, its end going backward).
    Enter,
    /// Out of a block-level element.
    Exit,
}

/// A walk's state: the node it is about to enter (`true`) or leave (`false`).
#[derive(Clone, Copy)]
struct Walk {
    at: Option<(NodeId, bool)>,
    root: NodeId,
    forward: bool,
    /// Non-editable elements are atomic (the root is editable).
    editable: bool,
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

impl Engine {
    fn walk_from(&self, bp: Bp, root: NodeId, forward: bool) -> Walk {
        let editable = self.is_editable(root);
        let mut w = Walk {
            at: None,
            root,
            forward,
            editable,
        };
        let (n, o) = bp;
        let dom = self.dom();
        w.at = if dom.is_character_data(n) {
            drop(dom);
            self.after_leaving(&w, n)
        } else {
            let child = if forward {
                dom.child_at(n, o as usize)
            } else {
                o.checked_sub(1).and_then(|i| dom.child_at(n, i as usize))
            };
            match child {
                Some(c) => Some((c, true)),
                None if n == root => None,
                None => Some((n, false)),
            }
        };
        w
    }

    /// The state after leaving `node`.
    fn after_leaving(&self, w: &Walk, node: NodeId) -> Option<(NodeId, bool)> {
        if node == w.root {
            return None;
        }
        let dom = self.dom();
        let sibling = if w.forward {
            dom.next_sibling(node)
        } else {
            dom.previous_sibling(node)
        };
        match sibling {
            Some(s) => Some((s, true)),
            None => dom.parent(node).map(|p| (p, false)),
        }
    }

    fn is_block_level(&self, el: NodeId) -> bool {
        let d = self.style_of_element(el, "display");
        !(d.is_empty() || d.starts_with("inline") || d == "contents" || d == "none")
    }

    fn walk_next(&self, w: &mut Walk) -> Option<Stop> {
        loop {
            let (node, entering) = w.at?;
            let (is_text, is_element) = {
                let dom = self.dom();
                (dom.is_text(node), dom.is_element(node))
            };
            if !entering {
                let block = node != w.root && is_element && self.is_block_level(node);
                w.at = self.after_leaving(w, node);
                if block {
                    return Some(Stop::Exit);
                }
                continue;
            }
            if is_text {
                w.at = self.after_leaving(w, node);
                return Some(Stop::Text(node));
            }
            if !is_element || self.style_of_element(node, "display") == "none" {
                w.at = self.after_leaving(w, node);
                continue;
            }
            if self.dom().is_html_element_named(node, &local_name!("br")) {
                w.at = self.after_leaving(w, node);
                return Some(Stop::Br(node));
            }
            if self.is_atomic_for_caret(node) || (w.editable && !self.is_editable(node)) {
                w.at = self.after_leaving(w, node);
                return Some(Stop::Atomic(node));
            }
            let child = {
                let dom = self.dom();
                if w.forward {
                    dom.first_child(node)
                } else {
                    dom.last_child(node)
                }
            };
            w.at = Some(child.map_or((node, false), |c| (c, true)));
            if self.is_block_level(node) {
                return Some(Stop::Enter);
            }
        }
    }

    /// A text node's characters with their UTF-16 offsets.
    fn chars_of(&self, t: NodeId) -> Vec<(u32, char)> {
        let dom = self.dom();
        let mut at = 0u32;
        dom.character_data(t)
            .unwrap_or("")
            .chars()
            .map(|c| {
                let o = at;
                at += c.len_utf16() as u32;
                (o, c)
            })
            .collect()
    }

    /// The first character met from character `i` of text `t` (exclusive) on its line, going
    /// `forward` or back: `Some(true)` for white space, `Some(false)` for other content (an atomic
    /// inline counts), `None` at the line's edge. With `skip_ws`, white space is passed over.
    fn line_neighbour(
        &self,
        t: NodeId,
        chars: &[(u32, char)],
        i: usize,
        forward: bool,
        skip_ws: bool,
        root: NodeId,
    ) -> Option<bool> {
        fn first(mut cs: impl Iterator<Item = char>, skip_ws: bool) -> Option<bool> {
            cs.find(|&c| !(skip_ws && is_ws(c))).map(is_ws)
        }
        let pick = |cs: &mut dyn Iterator<Item = char>| first(cs, skip_ws);
        let here = if forward {
            pick(&mut chars[i + 1..].iter().map(|&(_, c)| c))
        } else {
            pick(&mut chars[..i].iter().rev().map(|&(_, c)| c))
        };
        if here.is_some() {
            return here;
        }
        let mut w = self.walk_from((t, 0), root, forward);
        while let Some(stop) = self.walk_next(&mut w) {
            match stop {
                Stop::Text(x) => {
                    let cs = self.chars_of(x);
                    let found = if forward {
                        pick(&mut cs.iter().map(|&(_, c)| c))
                    } else {
                        pick(&mut cs.iter().rev().map(|&(_, c)| c))
                    };
                    if found.is_some() {
                        return found;
                    }
                }
                Stop::Atomic(_) => return Some(false),
                Stop::Br(_) | Stop::Enter | Stop::Exit => return None,
            }
        }
        None
    }

    /// Whether character `i` of text `t` is rendered: not white space that CSS Text §4.1.1 collapses
    /// (a collapsible space after another or at a line's start, or at its end).
    fn char_rendered(&self, t: NodeId, chars: &[(u32, char)], i: usize, root: NodeId) -> bool {
        let c = chars[i].1;
        if !is_ws(c) {
            return true;
        }
        let ws = self
            .dom()
            .parent_element(t)
            .map(|p| self.style_of_element(p, "white-space"))
            .unwrap_or_default();
        let keep_spaces = matches!(ws.as_str(), "pre" | "pre-wrap" | "break-spaces");
        if keep_spaces || (c == '\n' && ws == "pre-line") {
            return true;
        }
        if self.line_neighbour(t, chars, i, false, false, root) != Some(false) {
            return false;
        }
        self.line_neighbour(t, chars, i, true, true, root).is_some()
    }

    /// Whether the line has rendered content between `bp` and its edge in the given direction.
    fn line_content_from(&self, bp: Bp, root: NodeId, forward: bool) -> bool {
        let (n, o) = bp;
        if self.dom().is_text(n) {
            let cs = self.chars_of(n);
            let found = cs
                .iter()
                .enumerate()
                .filter(|(_, &(off, _))| if forward { off >= o } else { off < o })
                .any(|(i, _)| self.char_rendered(n, &cs, i, root));
            if found {
                return true;
            }
        }
        let mut w = self.walk_from(bp, root, forward);
        while let Some(stop) = self.walk_next(&mut w) {
            match stop {
                Stop::Text(x) => {
                    let cs = self.chars_of(x);
                    if (0..cs.len()).any(|i| self.char_rendered(x, &cs, i, root)) {
                        return true;
                    }
                }
                Stop::Atomic(_) => return true,
                Stop::Br(_) | Stop::Enter | Stop::Exit => return false,
            }
        }
        false
    }

    /// The caret position one rendered character (or one line boundary, or one atomic inline) from
    /// `bp`, within `root`; `None` at the root's edge.
    pub(crate) fn step_character(&self, bp: Bp, root: NodeId, forward: bool) -> Option<Bp> {
        self.step_rendered(bp, root, forward).map(|(to, _)| to)
    }

    /// The caret position one word from `bp` (`Selection.modify(..., "word")`, Ctrl+arrows), within
    /// `root`, by `caret::word_move`'s units over the rendered characters: forward to the end of the
    /// next word or run of punctuation and over the spaces after it, backward to the start of the
    /// one before. A line boundary is a unit of its own, as is an atomic inline.
    pub(crate) fn step_word(&self, bp: Bp, root: NodeId, forward: bool) -> Option<Bp> {
        use crate::caret::{word_unit, WordUnit};
        let step = |at: Bp| {
            self.step_rendered(at, root, forward)
                .map(|(to, c)| (to, c.map_or(WordUnit::Boundary, word_unit)))
        };
        let (mut to, mut unit) = step(bp)?;
        if unit == WordUnit::Boundary {
            return Some(to);
        }
        while unit == WordUnit::Space {
            match step(to) {
                Some((t, u)) if u != WordUnit::Boundary => (to, unit) = (t, u),
                _ => return Some(to),
            }
        }
        // The unit itself.
        let first = unit;
        let mut at = to;
        let mut next = step(at);
        while let Some((t, u)) = next {
            if u != first {
                break;
            }
            at = t;
            next = step(at);
        }
        // Forward, the spaces after it.
        while let (true, Some((t, WordUnit::Space))) = (forward, next) {
            at = t;
            next = step(at);
        }
        Some(at)
    }

    /// The end of the white space after `bp` in its text node when none of it is rendered (CSS
    /// Text §4.1.1 collapses it away), else `bp`.
    pub(crate) fn collapsed_space_after(&self, bp: Bp) -> Bp {
        let (n, o) = bp;
        if !self.dom().is_text(n) {
            return bp;
        }
        let cs = self.chars_of(n);
        let Some(root) = self
            .editing_host(n)
            .or_else(|| self.dom().parent_element(n))
        else {
            return bp;
        };
        let rest: Vec<usize> = (0..cs.len()).filter(|&i| cs[i].0 >= o).collect();
        let all_hidden = !rest.is_empty()
            && rest
                .iter()
                .all(|&i| is_ws(cs[i].1) && !self.char_rendered(n, &cs, i, root));
        if all_hidden {
            let last = cs[cs.len() - 1];
            (n, last.0 + last.1.len_utf16() as u32)
        } else {
            bp
        }
    }

    /// `step_character`, with the character stepped over (`None` for a line boundary or an atomic
    /// inline). A newline that white space collapsing turns into a space is a space.
    fn step_rendered(&self, bp: Bp, root: NodeId, forward: bool) -> Option<(Bp, Option<char>)> {
        let as_rendered = |t: NodeId, c: char| {
            if c != '\n' {
                return Some(c);
            }
            let ws = self
                .dom()
                .parent_element(t)
                .map(|p| self.style_of_element(p, "white-space"))
                .unwrap_or_default();
            let kept = matches!(
                ws.as_str(),
                "pre" | "pre-wrap" | "pre-line" | "break-spaces"
            );
            if kept {
                None
            } else {
                Some(' ')
            }
        };
        let (n, o) = bp;
        if self.dom().is_text(n) {
            let cs = self.chars_of(n);
            let hit = if forward {
                (0..cs.len())
                    .filter(|&i| cs[i].0 >= o)
                    .find(|&i| self.char_rendered(n, &cs, i, root))
            } else {
                (0..cs.len())
                    .rev()
                    .filter(|&i| cs[i].0 < o)
                    .find(|&i| self.char_rendered(n, &cs, i, root))
            };
            if let Some(i) = hit {
                let c = as_rendered(n, cs[i].1);
                return Some(if forward {
                    ((n, cs[i].0 + cs[i].1.len_utf16() as u32), c)
                } else {
                    ((n, cs[i].0), c)
                });
            }
        }
        let mut crossed = false;
        // (going back: where the line the walk crossed into starts, if nothing is on it)
        let mut line_start: Option<Bp> = None;
        let mut content_behind: Option<bool> = None;
        let mut w = self.walk_from(bp, root, forward);
        while let Some(stop) = self.walk_next(&mut w) {
            match stop {
                Stop::Exit => crossed = true,
                Stop::Enter => {
                    // (a block entered from the start of a line is on that line)
                    let behind = *content_behind
                        .get_or_insert_with(|| self.line_content_from(bp, root, !forward));
                    if behind {
                        crossed = true;
                    }
                }
                Stop::Br(b) => {
                    let (p, i) = {
                        let dom = self.dom();
                        (dom.parent(b)?, dom.index(b) as u32)
                    };
                    if crossed {
                        // An empty line: its only position is before its `br`.
                        return Some(((p, i), None));
                    }
                    crossed = true;
                    line_start = Some((p, if forward { i + 1 } else { i }));
                }
                Stop::Text(t) => {
                    let cs = self.chars_of(t);
                    let hit = if forward {
                        (0..cs.len()).find(|&i| self.char_rendered(t, &cs, i, root))
                    } else {
                        (0..cs.len())
                            .rev()
                            .find(|&i| self.char_rendered(t, &cs, i, root))
                    };
                    if let Some(i) = hit {
                        let (start, end) = (cs[i].0, cs[i].0 + cs[i].1.len_utf16() as u32);
                        let c = if crossed {
                            None
                        } else {
                            as_rendered(t, cs[i].1)
                        };
                        return Some(match (forward, crossed) {
                            (true, true) | (false, false) => ((t, start), c),
                            (true, false) | (false, true) => ((t, end), c),
                        });
                    }
                }
                Stop::Atomic(a) => {
                    let (p, i) = {
                        let dom = self.dom();
                        (dom.parent(a)?, dom.index(a) as u32)
                    };
                    return Some(match (forward, crossed) {
                        (true, true) | (false, false) => ((p, i), None),
                        (true, false) | (false, true) => ((p, i + 1), None),
                    });
                }
            }
        }
        if forward {
            None
        } else {
            line_start.map(|b| (b, None))
        }
    }
}
