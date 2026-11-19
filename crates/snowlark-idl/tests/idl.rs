// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The vendored Web IDL parses and links with no diagnostics, and the checked-in bindings are what the
//! generator produces from it (the `snowlark-idl check` of CI, as a test).

use snowlark_idl::ast::{InterfaceKind, Member};
use snowlark_idl::model::{load_dir, Model};
use snowlark_idl::tool;

fn model() -> Model {
    let (model, errors, count) = load_dir(&tool::workspace_root().join("idl")).unwrap();
    assert_eq!(
        count, 347,
        "webref 3.86 has 346 files, and device-memory.idl"
    );
    assert!(errors.is_empty(), "parse errors: {errors:#?}");
    model
}

#[test]
fn every_file_parses_and_links() {
    let m = model();
    assert!(
        m.diagnostics.is_empty(),
        "link diagnostics: {:#?}",
        m.diagnostics
    );
    assert!(m.interfaces.len() > 1000);
}

#[test]
fn links_the_dom() {
    let m = model();
    let chain: Vec<&str> = m
        .chain("HTMLDivElement")
        .iter()
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(
        chain,
        [
            "HTMLDivElement",
            "HTMLElement",
            "Element",
            "Node",
            "EventTarget"
        ]
    );
    let el = &m.interfaces["Element"];
    assert!(el.includes.iter().any(|x| x == "ParentNode"));
    // Partial interfaces from other specs (cssom-view's getBoundingClientRect) are merged.
    assert!(el
        .members
        .iter()
        .any(|im| matches!(&im.member, Member::Operation(o) if o.name.as_deref() == Some("getBoundingClientRect"))));
    assert_eq!(
        m.interfaces["EventListener"].kind,
        InterfaceKind::CallbackInterface
    );
    // [LegacyWindowAlias] and spec-prose names resolve.
    assert_eq!(m.aliases["SVGPoint"], "DOMPoint");
    assert!(m.typedefs.contains_key("CSSOMString"));
}

#[test]
fn checked_in_bindings_are_up_to_date() {
    let root = tool::workspace_root();
    let files = tool::generate(&root).expect("generate");
    let stale = tool::stale(&files, &tool::generated_dir(&root));
    assert!(stale.is_empty(), "run `snowlark-idl gen`: {stale:#?}");
}
