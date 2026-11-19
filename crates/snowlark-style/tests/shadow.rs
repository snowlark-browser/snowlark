// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shadow DOM scoping (CSS Scoping 1): shadow-tree sheets, `:host`, `::slotted`, `::part`, inheritance
//! through the flat tree.

mod common;

use common::Page;
use snowlark_dom::{ShadowRootInit, ShadowRootMode};

const DOC: &str = "<!DOCTYPE html><style>
   p { color: rgb(1, 1, 1); }
   #host { color: rgb(7, 7, 7); }
   #host::part(label) { margin-top: 9px; }
 </style>
 <div id=host class=big>
   <template shadowrootmode=open>
     <style>
       :host { width: 10px; }
       :host(.big) { height: 20px; }
       p { color: rgb(2, 2, 2); }
       ::slotted(span) { padding-top: 4px; }
       ::slotted(.named) { padding-left: 5px; }
       slot[name=n] { color: rgb(8, 8, 8); }
     </style>
     <p id=inner part=label>inside</p>
     <slot></slot>
     <slot name=n></slot>
   </template>
   <span id=light>light</span>
   <em id=named class=named slot=n>named</em>
   <b id=unslotted slot=missing>x</b>
 </div>
 <p id=outer>outside</p>";

#[test]
fn shadow_sheets_are_scoped() {
    let p = Page::new(DOC);
    let host = p.q("#host");
    let shadow = p.dom.shadow_root(host).expect("declarative shadow root");
    let inner = p.q_in(shadow, "#inner");
    // The shadow tree's `p` rule applies inside only; the document's `p` rule does not reach in.
    assert_eq!(p.value_of(inner, "color"), "rgb(2, 2, 2)");
    assert_eq!(p.get("#outer", "color"), "rgb(1, 1, 1)");
}

#[test]
fn host_selectors() {
    let p = Page::new(DOC);
    assert_eq!(p.get("#host", "width"), "10px");
    assert_eq!(p.get("#host", "height"), "20px");
}

#[test]
fn slotted_and_flat_tree_inheritance() {
    let p = Page::new(DOC);
    assert_eq!(p.get("#light", "padding-top"), "4px");
    assert_eq!(p.get("#named", "padding-left"), "5px");
    // Slotted elements inherit from their slot: the named slot is rgb(8, 8, 8).
    assert_eq!(p.get("#named", "color"), "rgb(8, 8, 8)");
    // The default slot inherits from the host.
    assert_eq!(p.get("#light", "color"), "rgb(7, 7, 7)");
    // An element assigned to no slot is not in the flat tree: no stored style, resolved on demand.
    let unslotted = p.q("#unslotted");
    assert!(p.data.styles(unslotted).is_none());
}

#[test]
fn part_pseudo_element() {
    let p = Page::new(DOC);
    let host = p.q("#host");
    let shadow = p.dom.shadow_root(host).unwrap();
    let inner = p.q_in(shadow, "#inner");
    assert_eq!(p.value_of(inner, "margin-top"), "9px");
}

#[test]
fn script_attached_shadow_root_and_slot_changes() {
    let mut p =
        Page::new("<!DOCTYPE html><div id=h><span id=a slot=one>a</span><span id=b>b</span></div>");
    let host = p.q("#h");
    let shadow = p
        .dom
        .attach_shadow(host, ShadowRootInit::new(ShadowRootMode::Open))
        .unwrap();
    p.engine.shadow_root_attached(&p.dom, &mut p.data, host);
    p.restyle();
    // No slots yet: the host's children are out of the flat tree.
    assert!(p.data.styles(p.q("#a")).is_none());
    let doc = p.doc;
    let style = p.dom.create_element(doc, "style", None).unwrap();
    p.dom.set_text_content(
        style,
        "slot { color: rgb(5, 5, 5); } ::slotted(*) { width: 1px; }",
    );
    p.dom.append_child(shadow, style).unwrap();
    let slot = p.dom.create_element(doc, "slot", None).unwrap();
    p.dom.set_attribute(slot, "name", "one").unwrap();
    p.dom.append_child(shadow, slot).unwrap();
    // The page layer reports the insertions (reactions) and processes the style element.
    p.engine.update_style_block(&p.dom, &p.data, style);
    p.restyle();
    assert_eq!(p.get("#a", "width"), "1px");
    assert_eq!(p.get("#a", "color"), "rgb(5, 5, 5)");
    assert!(p.data.styles(p.q("#b")).is_none());
    // Renaming the slot reassigns: #a leaves the flat tree.
    p.dom.set_attribute(slot, "name", "two").unwrap();
    p.restyle();
    assert!(p.data.styles(p.q("#a")).is_none());
    p.dom.set_attribute(p.q("#b"), "slot", "two").unwrap();
    p.restyle();
    assert_eq!(p.get("#b", "width"), "1px");
}

#[test]
fn adopted_sheets_in_a_shadow_root() {
    let mut p = Page::new(
        "<!DOCTYPE html><div id=h><template shadowrootmode=open><p id=i>x</p></template></div>",
    );
    let host = p.q("#h");
    let shadow = p.dom.shadow_root(host).unwrap();
    let base = url::Url::parse("about:blank").unwrap();
    let sheet = p.engine.parse_sheet(
        &p.data,
        "p { width: 4px; } :host { height: 5px; }",
        &base,
        "",
    );
    p.engine
        .set_adopted_sheets(&p.dom, &p.data, shadow, vec![sheet]);
    p.restyle();
    let inner = p.q_in(shadow, "#i");
    assert_eq!(p.value_of(inner, "width"), "4px");
    assert_eq!(p.get("#h", "height"), "5px");
    p.engine
        .set_adopted_sheets(&p.dom, &p.data, shadow, Vec::new());
    p.restyle();
    assert_eq!(p.value_of(inner, "width"), "auto");
}

/// A sheet's rules changed in place (`replaceSync`, `insertRule`) apply in every shadow root
/// that adopted it: the cascade data cached for its old rules is not used again, for the shadow
/// root that changed nor for another that shares the sheet.
#[test]
fn adopted_sheets_changed_in_place_restyle_their_shadow_roots() {
    let mut p = Page::new(
        "<!DOCTYPE html><div id=h1><template shadowrootmode=open><p id=i>x</p></template></div>\
         <div id=h2><template shadowrootmode=open><p id=i>y</p></template></div>",
    );
    let s1 = p.dom.shadow_root(p.q("#h1")).unwrap();
    let s2 = p.dom.shadow_root(p.q("#h2")).unwrap();
    let base = url::Url::parse("about:blank").unwrap();
    let sheet = p.engine.parse_sheet(&p.data, "", &base, "");
    p.engine
        .set_adopted_sheets(&p.dom, &p.data, s1, vec![sheet.clone()]);
    p.engine
        .set_adopted_sheets(&p.dom, &p.data, s2, vec![sheet.clone()]);
    p.restyle();
    let (i1, i2) = (p.q_in(s1, "#i"), p.q_in(s2, "#i"));
    assert_eq!(p.value_of(i1, "width"), "auto");
    // (as the page hands each adopting shadow root its list again after a change)
    sheet.replace_rules("p { width: 4px; }");
    for s in [s1, s2] {
        p.engine
            .set_adopted_sheets(&p.dom, &p.data, s, vec![sheet.clone()]);
    }
    p.restyle();
    assert_eq!(p.value_of(i1, "width"), "4px");
    assert_eq!(p.value_of(i2, "width"), "4px");
    sheet.replace_rules("p { width: 6px; }");
    for s in [s1, s2] {
        p.engine
            .set_adopted_sheets(&p.dom, &p.data, s, vec![sheet.clone()]);
    }
    p.restyle();
    assert_eq!(p.value_of(i1, "width"), "6px");
    assert_eq!(p.value_of(i2, "width"), "6px");
}

/// CSS Shadow Parts 1 §4.2: `exportparts` forwards an inner part to the outer tree, under its own
/// name or a new one (`inner: outer`), through any number of hosts; a part not exported is not
/// reachable from outside its host's tree.
#[test]
fn exportparts_forwards_parts_to_outer_trees() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>
           #outer::part(a) { padding-top: 1px; }
           #outer::part(renamed) { padding-left: 2px; }
           #outer::part(hidden) { padding-right: 3px; }
           #outer::part(deep) { padding-bottom: 4px; }
         </style>
         <div id=outer><template shadowrootmode=open>
           <div id=mid exportparts='a, b : renamed, , bad name, deep'><template shadowrootmode=open>
             <div id=mid2 exportparts=deep><template shadowrootmode=open>
               <span id=d part=deep>d</span>
             </template></div>
             <span id=a part=a>a</span><span id=b part=b>b</span><span id=h part=hidden>h</span>
           </template></div>
         </template></div>",
    );
    let outer = p.q("#outer");
    let s1 = p.dom.shadow_root(outer).unwrap();
    let mid = p.q_in(s1, "#mid");
    let s2 = p.dom.shadow_root(mid).unwrap();
    let a = p.q_in(s2, "#a");
    let b = p.q_in(s2, "#b");
    let h = p.q_in(s2, "#h");
    let mid2 = p.q_in(s2, "#mid2");
    let s3 = p.dom.shadow_root(mid2).unwrap();
    let d = p.q_in(s3, "#d");
    assert_eq!(p.value_of(a, "padding-top"), "1px");
    assert_eq!(p.value_of(b, "padding-left"), "2px");
    assert_eq!(p.value_of(h, "padding-right"), "0px");
    assert_eq!(p.value_of(d, "padding-bottom"), "4px");
    // Changing what the host exports restyles the parts it forwards.
    p.dom.set_attribute(mid, "exportparts", "b").unwrap();
    p.restyle();
    assert_eq!(p.value_of(a, "padding-top"), "0px");
    assert_eq!(p.value_of(b, "padding-left"), "0px");
}

/// CSS Scoping 1 §3.2.2 / CSS Pseudo 4: `::slotted()` may be followed by `::before`, `::after`,
/// `::marker` and `::placeholder`.
#[test]
fn slotted_pseudo_elements() {
    let p = Page::new(
        "<!DOCTYPE html><div id=h><template shadowrootmode=open>
           <style>::slotted(span)::before { content: 'x'; color: rgb(3, 4, 5); }</style><slot></slot>
         </template><span id=s>s</span></div>",
    );
    let s = p.q("#s");
    let before = p
        .engine
        .pseudo_style(&p.dom, &p.data, s, &snowlark_style::PseudoElement::Before)
        .expect("a ::before style");
    assert_eq!(
        snowlark_style::computed::computed_property_by_name(&before, "color").unwrap(),
        "rgb(3, 4, 5)"
    );
}

#[test]
fn implicit_scope_roots() {
    // CSS Cascade 6 §2.1.1: `@scope` without a `<scope-start>` scopes to the owner's parent element,
    // or to the shadow host for a `style` that is a child of the shadow root.
    let p = Page::new(
        "<!DOCTYPE html><div id=a><style>@scope { :scope { padding-top: 1px; } p { padding-left: 2px; } }</style>
           <p id=in>in</p></div><p id=out>out</p>
         <div id=h><template shadowrootmode=open>
           <style>@scope { :scope { padding-top: 3px; } p { padding-left: 4px; } }</style><p id=sp>s</p>
         </template></div>",
    );
    assert_eq!(p.get("#a", "padding-top"), "1px");
    assert_eq!(p.get("#in", "padding-left"), "2px");
    assert_eq!(p.get("#out", "padding-left"), "0px");
    assert_eq!(p.get("#h", "padding-top"), "3px");
    let shadow = p
        .dom
        .shadow_root(p.q("#h"))
        .expect("declarative shadow root");
    let sp = p.q_in(shadow, "#sp");
    assert_eq!(p.value_of(sp, "padding-left"), "4px");
}

/// Removing a node of a shadow tree away from its slots leaves the slot assignment, and the flat
/// tree around the slots, as it was: the host's subtree is not restyled for it.
#[test]
fn a_removal_away_from_the_slots_does_not_restyle_the_host() {
    let mut p = Page::new(
        "<!DOCTYPE html><div id=h><template shadowrootmode=open><div><b id=gone>x</b><i>y</i>\
         </div><p><slot></slot></p></template><span>1</span><span>2</span><span>3</span>\
         <span>4</span><span>5</span><span>6</span></div>",
    );
    let shadow = p.dom.shadow_root(p.q("#h")).unwrap();
    let gone = p.q_in(shadow, "#gone");
    let parent = p.dom.parent(gone).unwrap();
    p.dom.remove_child(parent, gone).unwrap();
    let stats = p.restyle();
    assert!(stats.restyled <= 2, "{stats:?}");
    // (the light children are still slotted)
    let span = p.q("span");
    assert_eq!(
        p.data
            .slotted_nodes(p.q_in(shadow, "slot"))
            .map(|n| n.len()),
        Some(6)
    );
    assert!(p.data.styles(span).is_some());
}

/// A slot that loses its assigned nodes shows its fallback content again: the host restyles.
#[test]
fn a_slot_left_without_assigned_nodes_shows_its_fallback() {
    let mut p = Page::new(
        "<!DOCTYPE html><style>#fb { color: rgb(3, 3, 3); }</style><div id=h><template \
         shadowrootmode=open><slot><em id=fb>fallback</em></slot></template><span id=s>1</span>\
         </div>",
    );
    let shadow = p.dom.shadow_root(p.q("#h")).unwrap();
    let fb = p.q_in(shadow, "#fb");
    assert!(p.data.styles(fb).is_none());
    let (host, s) = (p.q("#h"), p.q("#s"));
    p.dom.remove_child(host, s).unwrap();
    p.restyle();
    assert!(p.data.styles(fb).is_some());
}

/// Two shadow hosts whose shadow trees each have a `style` element with `css` (the second with
/// `media`), and their shadow roots.
fn two_hosts(css: &str, media: &str) -> (Page, [snowlark_dom::NodeId; 2]) {
    let p = Page::new(&format!(
        "<!DOCTYPE html><div id=h1><template shadowrootmode=open><style>{css}</style><p id=i>x</p>\
         </template></div><div id=h2><template shadowrootmode=open><style media='{media}'>{css}\
         </style><p id=i>y</p></template></div>"
    ));
    let roots = [p.q("#h1"), p.q("#h2")].map(|h| p.dom.shadow_root(h).unwrap());
    (p, roots)
}

/// `style` elements with the same text are each a sheet of their own (HTML "update a style block"
/// creates one per element), with their own media, over contents parsed once.
#[test]
fn style_elements_with_the_same_text_share_parsed_contents() {
    use style::stylesheets::StylesheetInDocument;
    let (p, [s1, s2]) = two_hosts("p { width: 3px; }", "print");
    let a = p.engine.tree_sheets(s1)[0].1.clone();
    let b = p.engine.tree_sheets(s2)[0].1.clone();
    assert_ne!(a, b);
    let guard = a.stylo().shared_lock.read();
    assert!(std::ptr::eq(
        a.stylo().contents(&guard),
        b.stylo().contents(&guard)
    ));
    drop(guard);
    assert_eq!(a.media_text(), "");
    assert_eq!(b.media_text(), "print");
    assert_eq!(p.value_of(p.q_in(s1, "#i"), "width"), "3px");
    assert_eq!(p.value_of(p.q_in(s2, "#i"), "width"), "auto");
}

/// A rule inserted through one sheet's `cssRules` is that sheet's alone, though its `style`
/// element's text is the other's.
#[test]
fn a_cssom_change_to_a_sheet_sharing_contents_is_its_own() {
    let (mut p, [s1, s2]) = two_hosts("p { width: 3px; }", "");
    let a = p.engine.tree_sheets(s1)[0].1.clone();
    let b = p.engine.tree_sheets(s2)[0].1.clone();
    a.rules().insert("p { height: 7px; }", 1).unwrap();
    p.engine.sheets_changed();
    p.restyle();
    assert_eq!(a.rules().len(), 2);
    assert_eq!(b.rules().len(), 1);
    assert_eq!(p.value_of(p.q_in(s1, "#i"), "height"), "7px");
    assert_eq!(p.value_of(p.q_in(s2, "#i"), "height"), "auto");
    assert_eq!(p.value_of(p.q_in(s2, "#i"), "width"), "3px");
}

/// `@scope` without a `<scope-start>` is scoped to its own `style` element's shadow host in each
/// shadow tree, though the trees' `style` text is the same.
#[test]
fn implicit_scope_roots_of_style_elements_with_the_same_text() {
    let (p, [s1, s2]) = two_hosts("@scope { p { width: 3px; } }", "");
    assert_eq!(p.value_of(p.q_in(s1, "#i"), "width"), "3px");
    assert_eq!(p.value_of(p.q_in(s2, "#i"), "width"), "3px");
}
