// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A parsed document with its style engine, for the integration tests.

#![allow(dead_code)]

use snowlark_dom::{Dom, NodeId, ParseOptions};
use snowlark_style::{computed, DeviceConfig, RestyleStats, StyleData, StyleEngine};

pub struct Page {
    pub dom: Dom,
    pub data: StyleData,
    pub engine: StyleEngine,
    pub doc: NodeId,
}

impl Page {
    /// Parses `html`, collects its `style` elements and styles it.
    pub fn new(html: &str) -> Page {
        Page::with_device(html, DeviceConfig::default())
    }

    pub fn with_device(html: &str, config: DeviceConfig) -> Page {
        let mut dom = Dom::new();
        let doc = dom.parse_html_str(html, ParseOptions::default());
        let data = StyleData::new();
        let mut engine = StyleEngine::new(&dom, doc, config);
        engine.update_all_style_blocks(&dom, &data);
        let mut page = Page {
            dom,
            data,
            engine,
            doc,
        };
        page.restyle();
        page.dom.set_reactions_enabled(true);
        page
    }

    pub fn restyle(&mut self) -> RestyleStats {
        let reactions = self.dom.take_reactions();
        self.engine
            .process_reactions(&self.dom, &mut self.data, &reactions);
        self.engine.restyle(&self.dom, &mut self.data)
    }

    /// The first element matching `selector` (shadow trees not included).
    pub fn q(&self, selector: &str) -> NodeId {
        self.q_in(self.doc, selector)
    }

    /// The first element matching `selector` under `root`.
    pub fn q_in(&self, root: NodeId, selector: &str) -> NodeId {
        let list = self.engine.parse_selector(selector).expect("selector");
        *self
            .engine
            .query_selector_all(&self.dom, &self.data, root, &list)
            .first()
            .unwrap_or_else(|| panic!("no element matches {selector}"))
    }

    /// The computed value of `property` on `element`.
    pub fn value_of(&self, element: NodeId, property: &str) -> String {
        let values = self
            .engine
            .computed_style(&self.dom, &self.data, element)
            .expect("element has a computed style");
        computed::computed_property_by_name(&values, property).expect("known property")
    }

    /// The computed value of `property` on the first element matching `selector`.
    pub fn get(&self, selector: &str, property: &str) -> String {
        self.value_of(self.q(selector), property)
    }
}
