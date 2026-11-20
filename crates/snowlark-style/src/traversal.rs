// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The restyle traversal: stylo's `DomTraversal` with a pre-order step that runs `recalc_style_at` on each
//! element the restyle hints and dirty bits lead to, and counts what it did.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::OnceLock;

use servo_arc::Arc;
use style::context::{SharedStyleContext, StyleContext};
use style::dom::TNode;
use style::properties::ComputedValues;

use style::traversal::{recalc_style_at, DomTraversal};

use crate::handle::{StyleElement, StyleNode};

/// What one restyle did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestyleStats {
    /// Elements the traversal visited.
    pub traversed: u32,
    /// Elements whose style was (re)computed.
    pub restyled: u32,
    /// Elements that ran selector matching (the rest only recascaded or were shared).
    pub matched: u32,
    /// Styles shared through the style sharing cache.
    pub shared: u32,
    /// Whether some of the traversal's elements were styled on stylo's thread pool.
    pub parallel: bool,
}

/// The `SNOWLARK_TEST_PANIC=restyle` testing switch: a panic inside the traversal (on one of stylo's
/// pool threads when it runs in parallel), the first time an element is styled in the process.
fn test_panic() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    *ON.get_or_init(|| std::env::var("SNOWLARK_TEST_PANIC").is_ok_and(|v| v.trim() == "restyle"))
        && !FIRED.swap(true, Ordering::Relaxed)
}

pub(crate) struct Recalc<'a> {
    pub shared: SharedStyleContext<'a>,
    traversed: AtomicU32,
    restyled: AtomicU32,
    matched: AtomicU32,
    styles_shared: AtomicU32,
    on_pool: AtomicBool,
}

impl<'a> Recalc<'a> {
    pub fn new(shared: SharedStyleContext<'a>) -> Self {
        Recalc {
            shared,
            traversed: AtomicU32::new(0),
            restyled: AtomicU32::new(0),
            matched: AtomicU32::new(0),
            styles_shared: AtomicU32::new(0),
            on_pool: AtomicBool::new(false),
        }
    }

    pub fn stats(&self) -> RestyleStats {
        RestyleStats {
            traversed: self.traversed.load(Ordering::Relaxed),
            restyled: self.restyled.load(Ordering::Relaxed),
            matched: self.matched.load(Ordering::Relaxed),
            shared: self.styles_shared.load(Ordering::Relaxed),
            parallel: self.on_pool.load(Ordering::Relaxed),
        }
    }
}

impl<'a, 'dom> DomTraversal<StyleElement<'dom>> for Recalc<'a> {
    fn process_preorder<F>(
        &self,
        context: &mut StyleContext<StyleElement<'dom>>,
        node: StyleNode<'dom>,
        note_child: F,
    ) where
        F: FnMut(StyleNode<'dom>),
    {
        let Some(element) = node.as_element() else {
            return;
        };
        if test_panic() {
            panic!("SNOWLARK_TEST_PANIC=restyle: a panic in the restyle traversal");
        }
        let before = context.thread_local.statistics.clone();
        let (primary_changed, change) = {
            let mut data = element.ensure_data_mut();
            let old = data.styles.primary.clone();
            let old_pseudos = data.styles.pseudos.clone();
            recalc_style_at(self, context, element, &mut data, note_child);
            // stylo's own damage is for Servo's layout; ours is classified below. Left set, it would
            // make every later traversal visit the element again.
            data.damage = Default::default();
            let new = &data.styles.primary;
            let same =
                |a: Option<&Arc<ComputedValues>>, b: Option<&Arc<ComputedValues>>| match (a, b) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    (None, None) => true,
                    _ => false,
                };
            let primary_changed = !same(old.as_ref(), new.as_ref());
            let pseudos_changed = old_pseudos
                .as_array()
                .iter()
                .zip(data.styles.pseudos.as_array())
                .any(|(a, b)| !same(a.as_ref(), b.as_ref()));
            let change = (primary_changed || pseudos_changed).then(|| {
                crate::damage::classify_element(
                    old.as_ref(),
                    new.as_ref(),
                    old_pseudos.as_array(),
                    data.styles.pseudos.as_array(),
                )
            });
            (primary_changed, change)
        };
        element.clear_dirty_descendants();

        let after = &context.thread_local.statistics;
        self.traversed.fetch_add(1, Ordering::Relaxed);
        if rayon::current_thread_index().is_some() && !self.on_pool.load(Ordering::Relaxed) {
            self.on_pool.store(true, Ordering::Relaxed);
        }
        self.matched.fetch_add(
            after.elements_matched - before.elements_matched,
            Ordering::Relaxed,
        );
        self.styles_shared.fetch_add(
            after.styles_shared - before.styles_shared,
            Ordering::Relaxed,
        );
        if primary_changed {
            self.restyled.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(change) = change {
            element.note_restyled(change);
        }
    }

    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(
        &self,
        _context: &mut StyleContext<StyleElement<'dom>>,
        _node: StyleNode<'dom>,
    ) {
        unreachable!("the style traversal is pre-order only")
    }

    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.shared
    }
}
