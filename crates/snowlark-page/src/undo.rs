// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Undo and redo of user edits (Ctrl+Z, Ctrl+Y and Ctrl+Shift+Z; Input Events Level 2
//! `historyUndo` and `historyRedo`).
//!
//! Each user edit of an editing host is one step: the DOM mutations it made, as mutation records
//! with their old values (the host is observed while the edit runs, see [`Engine::undo_begin`]), and
//! the selection before and after. Undoing a step applies the records' inverses newest first, which
//! puts back the very nodes the edit removed (the stack keeps them alive); the mutations that
//! makes are recorded in turn and become the redo step, whose inverses redo the edit. A text
//! control's step is its value and selection before and after. Typing, and deleting in one
//! direction, coalesce into the step before when they continue where it ended, as typing makes one
//! undo step until the caret moves. Ctrl+Z undoes the newest step when it belongs to the focused
//! editing host or text control.

use snowlark_bindings::rt::Cx;
use snowlark_dom::{Dom, MutationKind, MutationRecord, NodeId};

use crate::engine::ranges::Bp;
use crate::engine::Engine;

/// How many steps are kept.
const DEPTH: usize = 1000;

/// What a step changed.
pub(crate) enum Content {
    /// An editing host's mutations, oldest first.
    Dom(Vec<MutationRecord>),
    /// A text control's value.
    Value { before: String, after: String },
}

/// A selection before or after a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Sel {
    /// The document selection's anchor and focus.
    Doc(Option<(Bp, Bp)>),
    /// A text control's selection (UTF-16).
    Control(u32, u32),
}

/// One undoable user edit.
pub(crate) struct Step {
    /// The editing host or text control.
    pub target: NodeId,
    /// The kind of edit that coalesces with the next of its kind (typing, deleting backward or
    /// forward), if any.
    pub merge: Option<&'static str>,
    pub content: Content,
    pub before: Sel,
    pub after: Sel,
}

/// A user edit being recorded.
struct Recording {
    records: Vec<MutationRecord>,
    /// The first record of the DOM's queue that belongs to the edit.
    mark: usize,
    /// The host this recording marked as observed (unmarked at the end).
    marked: Option<NodeId>,
}

#[derive(Default)]
pub struct UndoState {
    undo: Vec<Step>,
    redo: Vec<Step>,
    recording: Option<Recording>,
}

impl UndoState {
    /// The nodes the steps keep alive (for the wrapper GC): the targets and every node a record
    /// names, of steps whose target is still in a document.
    pub(crate) fn gc_nodes(&self, dom: &Dom, out: &mut Vec<NodeId>) {
        for s in self.undo.iter().chain(&self.redo) {
            if !(dom.is_alive(s.target) && dom.is_connected(s.target)) {
                continue;
            }
            out.push(s.target);
            if let Content::Dom(records) = &s.content {
                for r in records {
                    out.push(r.target);
                    out.extend(&r.added_nodes);
                    out.extend(&r.removed_nodes);
                    out.extend(r.previous_sibling);
                    out.extend(r.next_sibling);
                }
            }
        }
    }
}

/// The coalescing kind of an `inputType`.
pub(crate) fn merge_kind(input_type: &str) -> Option<&'static str> {
    match input_type {
        "insertText" => Some("typing"),
        "deleteContentBackward" => Some("backward"),
        "deleteContentForward" => Some("forward"),
        _ => None,
    }
}

impl Engine {
    /// Starts recording the mutations of a user edit of `host`. Returns false when one is being
    /// recorded already (the outer recording keeps them).
    pub(crate) fn undo_begin(&self, host: NodeId) -> bool {
        if self.undo.borrow().recording.is_some() {
            return false;
        }
        let (mark, marked) = {
            let mut dom = self.dom_mut();
            let marked = (!dom.is_observed(host)).then(|| {
                dom.set_observed(host, true);
                host
            });
            (dom.mutation_records().len(), marked)
        };
        self.undo.borrow_mut().recording = Some(Recording {
            records: Vec::new(),
            mark,
            marked,
        });
        true
    }

    /// The mutation records handed to observers: a recording keeps those of its edit.
    pub(crate) fn undo_saw_records(&self, records: &[MutationRecord]) {
        let mut st = self.undo.borrow_mut();
        if let Some(r) = st.recording.as_mut() {
            r.records.extend(records.iter().skip(r.mark).cloned());
            r.mark = 0;
        }
    }

    /// Ends the recording `undo_begin` started, returning the edit's records.
    pub(crate) fn undo_end(&self) -> Vec<MutationRecord> {
        let Some(mut r) = self.undo.borrow_mut().recording.take() else {
            return Vec::new();
        };
        let mut dom = self.dom_mut();
        r.records
            .extend(dom.mutation_records().iter().skip(r.mark).cloned());
        if let Some(host) = r.marked {
            if dom.is_alive(host) {
                dom.set_observed(host, false);
            }
        }
        r.records
    }

    /// Adds a step for a user edit: coalesced into the newest when it continues it, and the redo
    /// steps go.
    pub(crate) fn undo_push(&self, step: Step) {
        let mut st = self.undo.borrow_mut();
        st.redo.clear();
        if let Some(top) = st.undo.last_mut() {
            if step.merge.is_some()
                && top.merge == step.merge
                && top.target == step.target
                && top.after == step.before
            {
                match (&mut top.content, step.content) {
                    (Content::Dom(a), Content::Dom(b)) => {
                        a.extend(b);
                        top.after = step.after;
                        return;
                    }
                    (Content::Value { after, .. }, Content::Value { after: new, .. }) => {
                        *after = new;
                        top.after = step.after;
                        return;
                    }
                    (_, content) => {
                        let step = Step { content, ..step };
                        st.undo.push(step);
                        return;
                    }
                }
            }
        }
        st.undo.push(step);
        if st.undo.len() > DEPTH {
            st.undo.remove(0);
        }
    }

    /// Whether the newest undo (`redo` false) or redo step belongs to `target`.
    pub(crate) fn can_undo(&self, target: NodeId, redo: bool) -> bool {
        let st = self.undo.borrow();
        let list = if redo { &st.redo } else { &st.undo };
        list.last().is_some_and(|s| s.target == target)
    }

    /// Undoes (or redoes) the newest step of `target`. Returns whether there was one.
    pub(crate) fn undo_apply(&self, cx: &mut Cx, target: NodeId, redo: bool) -> bool {
        let step = {
            let mut st = self.undo.borrow_mut();
            let list = if redo { &mut st.redo } else { &mut st.undo };
            if !list.last().is_some_and(|s| s.target == target) {
                return false;
            }
            list.pop().unwrap()
        };
        let content = match step.content {
            Content::Dom(records) => {
                let recording = self.undo_begin(target);
                self.undo_inverse(cx, &records);
                Content::Dom(if recording {
                    self.undo_end()
                } else {
                    Vec::new()
                })
            }
            Content::Value { before, after } => {
                self.set_edited_text(target, before.clone());
                self.html.borrow_mut().edited_since_focus.insert(target);
                self.control_edited(target);
                Content::Value {
                    before: after,
                    after: before,
                }
            }
        };
        match step.before {
            Sel::Doc(sel) => {
                let doc = self.dom().node_document(target);
                let ok = sel.is_none_or(|(a, f)| {
                    let dom = self.dom();
                    dom.is_alive(a.0) && dom.is_alive(f.0) && dom.is_connected(a.0)
                });
                if ok {
                    self.set_doc_selection(doc, sel);
                }
            }
            Sel::Control(s, e) => {
                self.set_selection(target, s, e, None);
                self.control_edited(target);
            }
        }
        let inverse = Step {
            target,
            merge: None,
            content,
            before: step.after,
            after: step.before,
        };
        let mut st = self.undo.borrow_mut();
        if redo {
            st.undo.push(inverse);
        } else {
            st.redo.push(inverse);
        }
        true
    }

    /// Applies the inverses of `records`, newest first.
    fn undo_inverse(&self, cx: &mut Cx, records: &[MutationRecord]) {
        for r in records.iter().rev() {
            if !self.dom().is_alive(r.target) {
                continue;
            }
            match r.kind {
                MutationKind::CharacterData => {
                    let mut dom = self.dom_mut();
                    let len = dom.length(r.target);
                    let _ =
                        dom.replace_data(r.target, 0, len, r.old_value.as_deref().unwrap_or(""));
                }
                MutationKind::Attributes => {
                    let Some(name) = r.attribute_name.as_ref() else {
                        continue;
                    };
                    let ns = r.attribute_namespace.as_ref().map(|n| n.as_ref());
                    let mut dom = self.dom_mut();
                    match &r.old_value {
                        Some(v) => {
                            let _ = dom.set_attribute_ns(r.target, ns, name, v);
                        }
                        None => dom.remove_attribute_ns(r.target, ns, name),
                    }
                }
                MutationKind::ChildList => {
                    let mut dom = self.dom_mut();
                    for &a in &r.added_nodes {
                        if dom.is_alive(a) && dom.parent(a) == Some(r.target) {
                            dom.remove_node(a);
                        }
                    }
                    let child_of_target =
                        |dom: &Dom, n: NodeId| dom.is_alive(n) && dom.parent(n) == Some(r.target);
                    // (back before the old next sibling; a node that was the last child is
                    // appended, after children the page added since)
                    let before = match (r.next_sibling, r.previous_sibling) {
                        (Some(n), _) if child_of_target(&dom, n) => Some(n),
                        (None, _) => None,
                        (_, Some(p)) if child_of_target(&dom, p) => dom.next_sibling(p),
                        (_, None) => dom.first_child(r.target),
                        _ => None,
                    };
                    for &n in &r.removed_nodes {
                        if dom.is_alive(n) && dom.parent(n).is_none() {
                            let _ = dom.insert_before(r.target, n, before);
                        }
                    }
                }
            }
            self.after_mutation(cx);
        }
    }
}
