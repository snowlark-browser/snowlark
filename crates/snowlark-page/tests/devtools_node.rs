// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What the developer tools' element tree menu does to a node: the text of the Copy rows (selectors,
//! JS paths and XPaths that resolve back to the same node, shadow trees included), deleting,
//! duplicating, hiding, scrolling, focusing and storing as a global, all through the DOM's own
//! mutation paths (a MutationObserver sees them).

use std::time::Duration;

use snowlark_dom::{Dom, NodeData, NodeId};
use snowlark_net::{NetConfig, NetThread};
use snowlark_page::devtools_node::CopyKind;
use snowlark_page::{Page, PageOptions, RenderSetup};

fn rendered(net: &NetThread, html: &str) -> Page {
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

/// The element with this id.
fn by_id(page: &Page, id: &str) -> NodeId {
    let dom = page.engine().dom();
    let doc = page.engine().document().unwrap();
    dom.descendants(doc)
        .find(|&d| dom.is_element(d) && dom.get_attribute(d, "id") == Some(id))
        .unwrap()
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

const PAGE: &str = r#"<!doctype html><body>
<div id=a><p>one</p><p>two</p><span class=s>x</span></div>
<div id=a2 class="a b"><p>three</p><!--c--><p>four</p> text <i>t</i> more</div>
<div id="1x:y"><b></b><b></b></div>
<ul><li>1</li><li>2</li><li id=dup>3</li></ul>
<div id=dup>second dup</div>
<svg id=sv><g><path id=pa></path><path></path></g></svg>
<div id=host1></div>
<script>
  const s1 = document.getElementById('host1').attachShadow({mode: 'open'});
  s1.innerHTML = '<div><span>a</span><span>b</span></div>' +
    '<div id=in><span>c</span><span id=dup>d</span></div><p>x</p>';
  const s2 = s1.querySelector('#in').firstElementChild.attachShadow({mode: 'open'});
  s2.innerHTML = '<div><b>n</b><b>m</b></div><div><b>o</b></div>';
  // Every element, in the document and in the shadow trees, gets a number.
  window.all = [];
  (function walk(root) {
    for (const el of root.querySelectorAll('*')) {
      el.setAttribute('data-n', String(all.length));
      all.push(el);
      if (el.shadowRoot) walk(el.shadowRoot);
    }
  })(document);
  window.unique = (k, sel) => {
    const m = all[k].getRootNode().querySelectorAll(sel);
    return m.length === 1 && m[0] === all[k];
  };
</script>"#;

/// Every element of the document and of its shadow trees, with its number.
fn numbered(page: &Page) -> Vec<(usize, NodeId)> {
    let dom = page.engine().dom();
    let doc = page.engine().document().unwrap();
    let mut out = Vec::new();
    collect(&dom, doc, &mut out);
    out.sort();
    out
}

fn collect(dom: &Dom, root: NodeId, out: &mut Vec<(usize, NodeId)>) {
    for d in dom.descendants(root).filter(|&d| dom.is_element(d)) {
        if let Some(n) = dom.get_attribute(d, "data-n").and_then(|v| v.parse().ok()) {
            out.push((n, d));
        }
        if let Some(s) = dom.open_shadow_root(d) {
            collect(dom, s, out);
        }
    }
}

#[test]
fn selectors_and_js_paths_resolve_back_to_the_node_through_shadow_trees() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, PAGE);
    let nodes = numbered(&page);
    // (the page itself, the three hosts' trees and the svg)
    assert!(nodes.len() > 30, "{}", nodes.len());
    let mut in_shadow = 0;
    for (k, node) in nodes {
        let sel = page
            .engine()
            .devtools_copy(node, CopyKind::Selector)
            .unwrap_or_else(|| panic!("a selector for #{k}"));
        let js = format!("unique({k}, {sel:?})");
        assert_eq!(eval(&mut page, &js), "true", "selector {sel} of #{k}");
        let path = page
            .engine()
            .devtools_copy(node, CopyKind::JsPath)
            .unwrap_or_else(|| panic!("a JS path for #{k}"));
        let js = format!("({path}) === all[{k}]");
        assert_eq!(eval(&mut page, &js), "true", "JS path {path} of #{k}");
        if path.contains(".shadowRoot.") {
            in_shadow += 1;
        }
    }
    assert!(in_shadow >= 10, "{in_shadow} nodes in shadow trees");
    // The shapes: an id that is the only one is the whole selector; a repeated id is not used.
    let dom_node = |page: &mut Page, js: &str| -> NodeId {
        let k: usize = eval(page, &format!("({js}).getAttribute('data-n')"))
            .parse()
            .unwrap();
        numbered(page).into_iter().find(|(n, _)| *n == k).unwrap().1
    };
    let a = dom_node(&mut page, "document.getElementById('a')");
    assert_eq!(
        page.engine().devtools_copy(a, CopyKind::Selector).unwrap(),
        "#a"
    );
    let ids = dom_node(&mut page, "document.getElementById('1x:y')");
    assert_eq!(
        page.engine()
            .devtools_copy(ids, CopyKind::Selector)
            .unwrap(),
        "#\\31 x\\:y"
    );
    let li = dom_node(&mut page, "document.querySelector('li[id]')");
    let sel = page.engine().devtools_copy(li, CopyKind::Selector).unwrap();
    assert!(sel.starts_with("html > body > ul > li"), "{sel}");
    let deep = dom_node(
        &mut page,
        "document.getElementById('host1').shadowRoot.querySelector('#in').firstElementChild.shadowRoot.querySelectorAll('b')[2]",
    );
    assert_eq!(
        page.engine().devtools_copy(deep, CopyKind::JsPath).unwrap(),
        "document.querySelector(\"#host1\").shadowRoot.querySelector(\"#in > span:nth-child(1)\").shadowRoot.querySelector(\"div:nth-child(2) > b\")"
    );
}

/// Resolves the XPaths the menu writes (XPath 1.0 over the DOM: absolute and `//*[@id=...]` paths
/// of name tests with positions, `text()`, `comment()` and `*[name()=...]`), as `document.evaluate`
/// would from `root`.
fn resolve_xpath(dom: &Dom, root: NodeId, xpath: &str) -> Vec<NodeId> {
    let (mut context, rest) = match xpath.strip_prefix("//") {
        Some(r) => {
            // //*[@id="x"]/more
            let (pred, rest) = r.split_once('/').map_or((r, ""), |(p, rest)| (p, rest));
            let id = pred
                .strip_prefix("*[@id=")
                .and_then(|p| p.strip_suffix(']'))
                .map(|p| p.trim_matches(['"', '\'']).to_owned())
                .expect("an id step");
            let found: Vec<NodeId> = dom
                .descendants(root)
                .filter(|&d| dom.is_element(d) && dom.get_attribute(d, "id") == Some(id.as_str()))
                .collect();
            (found, rest.to_owned())
        }
        None => (vec![root], xpath.trim_start_matches('/').to_owned()),
    };
    for step in rest.split('/').filter(|s| !s.is_empty()) {
        let (test, pos) = match step.rsplit_once('[') {
            Some((t, p)) if !p.starts_with("name()") && p.ends_with(']') => {
                (t, p[..p.len() - 1].parse::<usize>().ok())
            }
            _ => (step, None),
        };
        // (`*[name()='svg'][2]` is split above at the last `[`; a bare `*[name()='x']` is a test)
        let (test, pos) = if test.starts_with("*[name()") && !test.ends_with(']') {
            (step, None)
        } else {
            (test, pos)
        };
        let mut next = Vec::new();
        for c in &context {
            let matching: Vec<NodeId> = dom
                .children(*c)
                .filter(|&k| match dom.data(k) {
                    NodeData::Element(_) => {
                        if let Some(q) = test.strip_prefix("*[name()=") {
                            let q = q.strip_suffix(']').unwrap().trim_matches(['"', '\'']);
                            dom.qualified_name(k) == q
                        } else {
                            dom.namespace(k) == &snowlark_dom::ns!(html)
                                && dom.local_name(k).to_string().eq_ignore_ascii_case(test)
                        }
                    }
                    NodeData::Text(_) | NodeData::CDataSection(_) => test == "text()",
                    NodeData::Comment(_) => test == "comment()",
                    _ => false,
                })
                .collect();
            match pos {
                Some(p) => next.extend(matching.get(p - 1).copied()),
                None => next.extend(matching),
            }
        }
        context = next;
    }
    context
}

#[test]
fn xpaths_resolve_back_to_the_node_within_its_tree() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, PAGE);
    let mut checked_text = 0;
    for (k, node) in numbered(&page) {
        for full in [false, true] {
            let kind = if full {
                CopyKind::FullXPath
            } else {
                CopyKind::XPath
            };
            let xp = page
                .engine()
                .devtools_copy(node, kind)
                .unwrap_or_else(|| panic!("an XPath for #{k}"));
            let dom = page.engine().dom();
            let root = dom.root(node);
            let found = resolve_xpath(&dom, root, &xp);
            assert_eq!(found, [node], "{xp} (full: {full}) of #{k}");
            if full {
                assert!(xp.starts_with('/') && !xp.starts_with("//"), "{xp}");
            }
        }
    }
    // Text and comment nodes have XPaths too.
    let nodes: Vec<NodeId> = {
        let dom = page.engine().dom();
        let doc = page.engine().document().unwrap();
        dom.descendants(doc)
            .filter(|&d| {
                matches!(dom.data(d), NodeData::Text(t) if !t.trim().is_empty())
                    || matches!(dom.data(d), NodeData::Comment(_))
            })
            .collect()
    };
    for node in nodes {
        let xp = page
            .engine()
            .devtools_copy(node, CopyKind::FullXPath)
            .unwrap();
        let dom = page.engine().dom();
        let root = dom.root(node);
        assert_eq!(resolve_xpath(&dom, root, &xp), [node], "{xp}");
        checked_text += 1;
    }
    assert!(checked_text >= 8);
    // The shapes of Chrome's: the id when it is the only one, positions only among same-named
    // siblings.
    let n: usize = eval(
        &mut page,
        "document.querySelectorAll('#a2 > p')[1].getAttribute('data-n')",
    )
    .parse()
    .unwrap();
    let p = numbered(&page)
        .into_iter()
        .find(|(k, _)| *k == n)
        .unwrap()
        .1;
    assert_eq!(
        page.engine().devtools_copy(p, CopyKind::XPath).unwrap(),
        "//*[@id=\"a2\"]/p[2]"
    );
    assert_eq!(
        page.engine().devtools_copy(p, CopyKind::FullXPath).unwrap(),
        "/html/body/div[2]/p[2]"
    );
}

#[test]
fn outer_html_is_the_markup_of_the_node() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, PAGE);
    let n: usize = eval(
        &mut page,
        "document.getElementById('a').getAttribute('data-n')",
    )
    .parse()
    .unwrap();
    let a = numbered(&page)
        .into_iter()
        .find(|(k, _)| *k == n)
        .unwrap()
        .1;
    let html = page.engine().devtools_copy(a, CopyKind::OuterHtml).unwrap();
    assert_eq!(
        html,
        eval(&mut page, "document.getElementById('a').outerHTML")
    );
    assert!(html.starts_with("<div id=\"a\""), "{html}");
}

#[test]
fn deleting_and_duplicating_go_through_the_dom_so_observers_see_them() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><div id=box><p id=one>1</p><p id=two class=k>2<b>x</b></p></div>\
         <script>window.records = [];\
         new MutationObserver(r => { for (const m of r) records.push(m.type + ':' + m.addedNodes.length + ':' + m.removedNodes.length); })\
           .observe(document.getElementById('box'), {childList: true, attributes: true, subtree: true});</script>",
    );
    let find = by_id;
    let two = find(&page, "two");
    let copy = page
        .with(|e, cx| e.devtools_duplicate(cx, two))
        .expect("a copy");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(
            &mut page,
            "[...document.getElementById('box').children].map(c => c.outerHTML).join('|')"
        ),
        "<p id=\"one\">1</p>|<p id=\"two\" class=\"k\">2<b>x</b></p>|<p id=\"two\" class=\"k\">2<b>x</b></p>"
    );
    assert_ne!(copy, two);
    assert_eq!(eval(&mut page, "records.join()"), "childList:1:0");
    // Delete the original; the copy stays.
    assert!(page.with(|e, cx| e.devtools_delete(cx, two)));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(&mut page, "document.getElementById('box').children.length"),
        "2"
    );
    assert_eq!(
        eval(&mut page, "records.join()"),
        "childList:1:0,childList:0:1"
    );
    // Nothing to delete twice, and a root has no duplicate.
    assert!(!page.with(|e, cx| e.devtools_delete(cx, two)));
    let root = page.engine().document().unwrap();
    assert!(page.with(|e, cx| e.devtools_duplicate(cx, root)).is_none());
}

#[test]
fn hiding_toggles_one_declaration_and_leaves_the_rest_of_the_style() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><p id=p style='color: red; background: url(\"a;b.png\")'>text</p>\
         <script>window.records = [];\
         new MutationObserver(r => { for (const m of r) records.push(m.attributeName); })\
           .observe(document.body, {attributes: true, subtree: true});</script>",
    );
    let p = by_id(&page, "p");
    assert!(!page.engine().devtools_is_hidden(p));
    assert!(page.with(|e, cx| e.devtools_toggle_hidden(cx, p)));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert!(page.engine().devtools_is_hidden(p));
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('p')).visibility"
        ),
        "hidden"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('p').getAttribute('style')"
        ),
        "color: red; background: url(\"a;b.png\"); visibility: hidden !important;"
    );
    assert!(!page.with(|e, cx| e.devtools_toggle_hidden(cx, p)));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('p')).visibility"
        ),
        "visible"
    );
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('p').getAttribute('style')"
        ),
        "color: red; background: url(\"a;b.png\");"
    );
    assert_eq!(eval(&mut page, "records.join()"), "style,style");
}

#[test]
fn store_as_global_names_temp_variables_in_the_page() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><p id=p>hi</p><i id=i></i><script>window.temp2 = 'taken';</script>",
    );
    let find = by_id;
    let (p, i) = (find(&page, "p"), find(&page, "i"));
    let (name, text) = page.with(|e, cx| e.devtools_store_global(cx, p)).unwrap();
    assert_eq!(name, "temp1");
    assert_eq!(text, "<p id=\"p\">hi</p>");
    // temp2 is in use: the next is temp3.
    let (name, _) = page.with(|e, cx| e.devtools_store_global(cx, i)).unwrap();
    assert_eq!(name, "temp3");
    assert_eq!(
        eval(
            &mut page,
            "temp1 === document.getElementById('p') && temp3 === document.getElementById('i')"
        ),
        "true"
    );
    assert_eq!(eval(&mut page, "temp2"), "taken");
}

#[test]
fn scrolling_and_focusing_use_the_page_algorithms() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'><div style='height:3000px'></div>\
         <input id=in style='display:block'><div id=plain>x</div>\
         <script>window.focused = []; document.getElementById('in').addEventListener('focus', () => focused.push('in'));</script>",
    );
    let find = by_id;
    let (input, plain) = (find(&page, "in"), find(&page, "plain"));
    assert_eq!(eval(&mut page, "window.scrollY"), "0");
    page.with(|e, cx| e.devtools_scroll_into_view(cx, input));
    assert_eq!(eval(&mut page, "window.scrollY > 2000"), "true");
    page.with(|e, cx| e.devtools_focus(cx, input));
    assert_eq!(eval(&mut page, "document.activeElement.id"), "in");
    assert_eq!(eval(&mut page, "focused.join()"), "in");
    // Not focusable: nothing happens, as for `focus()`.
    page.with(|e, cx| e.devtools_focus(cx, plain));
    assert_eq!(eval(&mut page, "document.activeElement.id"), "in");
}

#[test]
fn expandable_descendants_are_the_elements_with_children() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let page = rendered(
        &net,
        "<!doctype html><body><div id=a><p><b>x</b></p><i></i><u><s>y</s></u></div>",
    );
    let a = by_id(&page, "a");
    let names: Vec<String> = page
        .engine()
        .devtools_expandable_descendants(a)
        .into_iter()
        .map(|n| page.engine().dom().local_name(n).to_string())
        .collect();
    assert_eq!(names, ["p", "b", "u", "s"]);
}

#[test]
fn attribute_edits_set_rename_and_remove_through_set_attribute() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><p id=p class=\"a b\" title=t>x</p>\
         <script>window.records = [];\
         new MutationObserver(r => { for (const m of r) records.push(m.attributeName + ':' + m.oldValue); })\
           .observe(document.getElementById('p'), {attributes: true, attributeOldValue: true});</script>",
    );
    let p = by_id(&page, "p");
    assert_eq!(
        page.engine().devtools_attribute_text(p, "class").unwrap(),
        "class=\"a b\""
    );
    let attrs =
        "[...document.getElementById('p').attributes].map(a => a.name + '=' + a.value).join('|')";
    // Edit a value in place: the attribute keeps its position.
    assert!(page.with(|e, cx| e.devtools_set_attributes(
        cx,
        p,
        Some("class"),
        "class=\"c &amp; d\""
    )));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(eval(&mut page, attrs), "id=p|class=c & d|title=t");
    // Rename: the old one goes, the new ones are added (bare and single-quoted values too).
    assert!(page.with(|e, cx| e.devtools_set_attributes(
        cx,
        p,
        Some("title"),
        "data-x='1' hidden data-y=2"
    )));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(&mut page, attrs),
        "id=p|class=c & d|data-x=1|hidden=|data-y=2"
    );
    // Editing to nothing removes the attribute; an edit that changes nothing reports none.
    assert!(page.with(|e, cx| e.devtools_set_attributes(cx, p, Some("data-y"), "")));
    assert!(!page.with(|e, cx| e.devtools_set_attributes(cx, p, None, "id=p")));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('p').hasAttribute('data-y')"
        ),
        "false"
    );
    // The page's observer saw each change as the mutation it is.
    assert_eq!(
        eval(&mut page, "records.join()"),
        "class:a b,title:t,data-x:null,hidden:null,data-y:null,data-y:2"
    );
    assert_eq!(
        page.engine().devtools_attribute_text(p, "data-x").unwrap(),
        "data-x=\"1\""
    );
}

#[test]
fn set_outer_html_replaces_the_element_and_text_edits_set_the_data() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><div id=box><i>before</i><p id=p>old <b>x</b></p><u>after</u></div>\
         <script>window.records = [];\
         new MutationObserver(r => { for (const m of r) records.push(m.type + ':' + m.addedNodes.length + ':' + m.removedNodes.length); })\
           .observe(document.getElementById('box'), {childList: true, characterData: true, subtree: true});</script>",
    );
    let p = by_id(&page, "p");
    assert_eq!(
        page.engine().devtools_html_text(p).unwrap(),
        "<p id=\"p\">old <b>x</b></p>"
    );
    let first = page
        .with(|e, cx| {
            e.devtools_set_outer_html(cx, p, "<section id=s>\n  new\n</section><em>two</em>")
        })
        .expect("replaced")
        .expect("an element");
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(&mut page, "document.getElementById('box').innerHTML"),
        "<i>before</i><section id=\"s\">\n  new\n</section><em>two</em><u>after</u>"
    );
    assert_eq!(page.engine().dom().local_name(first).to_string(), "section");
    assert_eq!(eval(&mut page, "records.join()"), "childList:2:1");
    // The document element can't be replaced.
    let html = {
        let dom = page.engine().dom();
        let doc = page.engine().document().unwrap();
        dom.document_element(doc).unwrap()
    };
    assert!(page
        .with(|e, cx| e.devtools_set_outer_html(cx, html, "<p>"))
        .is_none());
    // Text: a text node's data through nodeValue.
    let text = {
        let dom = page.engine().dom();
        let i = by_id(&page, "box");
        dom.descendants(i)
            .find(|&d| matches!(dom.data(d), NodeData::Text(t) if &**t == "before"))
            .unwrap()
    };
    assert_eq!(page.engine().devtools_text_value(text).unwrap(), "before");
    assert!(page.with(|e, cx| e.devtools_set_text(cx, text, "after all")));
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('box').firstChild.firstChild.data"
        ),
        "after all"
    );
    assert_eq!(eval(&mut page, "records.length"), "2");
}

#[test]
fn renaming_an_attribute_keeps_its_place_among_the_others() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<!doctype html><body><p id=p a=1 b=2 c=3 d=4>x</p>");
    let p = by_id(&page, "p");
    let attrs =
        "[...document.getElementById('p').attributes].map(a => a.name + '=' + a.value).join('|')";
    // b becomes x, in b's place; c and d stay behind it.
    assert!(page.with(|e, cx| e.devtools_set_attributes(cx, p, Some("b"), "x=9")));
    assert_eq!(eval(&mut page, attrs), "id=p|a=1|x=9|c=3|d=4");
    // One attribute becomes two, still in its place.
    assert!(page.with(|e, cx| e.devtools_set_attributes(cx, p, Some("x"), "y=1 z=2")));
    assert_eq!(eval(&mut page, attrs), "id=p|a=1|y=1|z=2|c=3|d=4");
    // An edit of the value alone changes nothing else.
    assert!(page.with(|e, cx| e.devtools_set_attributes(cx, p, Some("a"), "a=5")));
    assert_eq!(eval(&mut page, attrs), "id=p|a=5|y=1|z=2|c=3|d=4");
    // Taking it out keeps the others' order.
    assert!(page.with(|e, cx| e.devtools_set_attributes(cx, p, Some("y"), "")));
    assert_eq!(eval(&mut page, attrs), "id=p|a=5|z=2|c=3|d=4");
}

#[test]
fn paste_appends_markup_to_the_element() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body><div id=box><i>one</i></div><p id=leaf></p>",
    );
    let (box_, leaf) = (by_id(&page, "box"), by_id(&page, "leaf"));
    let first = page
        .with(|e, cx| e.devtools_paste(cx, box_, "text<b>two</b><u>three</u>"))
        .expect("an element");
    assert_eq!(page.engine().dom().local_name(first).to_string(), "b");
    assert_eq!(
        eval(&mut page, "document.getElementById('box').innerHTML"),
        "<i>one</i>text<b>two</b><u>three</u>"
    );
    // Into an element with no children, and a copy of an element (Cut then Paste).
    let html = page.engine().devtools_html_text(box_).unwrap();
    assert!(page.with(|e, cx| e.devtools_delete(cx, box_)));
    assert!(page
        .with(|e, cx| e.devtools_paste(cx, leaf, &html))
        .is_some());
    assert_eq!(
        eval(&mut page, "document.getElementById('leaf').innerHTML"),
        "<div id=\"box\"><i>one</i>text<b>two</b><u>three</u></div>"
    );
}

const FORCED_PAGE: &str = "<!doctype html><body style='margin:0'>\
    <style>\
      #a { color: rgb(0, 0, 1); background-color: rgb(1, 1, 1); }\
      #a:hover { color: rgb(1, 0, 0); }\
      #a:active { color: rgb(2, 0, 0); }\
      #a:focus { color: rgb(3, 0, 0); }\
      #w:hover { background-color: rgb(9, 0, 0); }\
      #w:focus-within { margin-left: 7px; }\
      #a:focus-visible { background-color: rgb(5, 0, 0); }\
      #l:visited { color: rgb(4, 0, 0); }\
      #t:target { color: rgb(6, 0, 0); }\
    </style>\
    <div id=w><button id=a>b</button></div><a id=l href='http://x.test/never'>l</a><i id=t>t</i>";

#[test]
fn forced_states_reach_style_and_selectors_and_survive_the_pages_own_state_changes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, FORCED_PAGE);
    let (a, w, l, t) = (
        by_id(&page, "a"),
        by_id(&page, "w"),
        by_id(&page, "l"),
        by_id(&page, "t"),
    );
    let color = "getComputedStyle(document.getElementById('a')).color";
    let wbg = "getComputedStyle(document.getElementById('w')).backgroundColor";
    assert_eq!(eval(&mut page, color), "rgb(0, 0, 1)");
    // :hover on the button, and so on its ancestor; selector matching sees it too.
    page.engine().devtools_force_state(a, 1, true);
    assert_eq!(eval(&mut page, color), "rgb(1, 0, 0)");
    assert_eq!(eval(&mut page, wbg), "rgb(9, 0, 0)");
    assert_eq!(
        eval(&mut page, "document.getElementById('a').matches(':hover') + ',' + document.getElementById('w').matches(':hover')"),
        "true,true"
    );
    assert_eq!(page.engine().devtools_forced_kinds(a), 0b10);
    // The later kind wins by order in the sheet (:focus after :hover): both on.
    page.engine().devtools_force_state(a, 2, true);
    assert_eq!(eval(&mut page, color), "rgb(3, 0, 0)");
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('w')).marginLeft"
        ),
        "7px"
    );
    // The page's own focus changes don't undo it: focus and blur the button.
    eval(
        &mut page,
        "document.getElementById('a').focus(); document.getElementById('a').blur()",
    );
    assert_eq!(eval(&mut page, color), "rgb(3, 0, 0)");
    assert_eq!(
        eval(&mut page, "document.getElementById('a').matches(':focus')"),
        "true"
    );
    // Off: back to what the page had.
    page.engine().devtools_force_state(a, 2, false);
    page.engine().devtools_force_state(a, 1, false);
    assert_eq!(eval(&mut page, color), "rgb(0, 0, 1)");
    assert_eq!(eval(&mut page, wbg), "rgba(0, 0, 0, 0)");
    assert_eq!(page.engine().devtools_forced_kinds(a), 0);
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('a').matches(':hover, :focus')"
        ),
        "false"
    );
    // :active, :focus-visible, :focus-within, :visited and :target.
    page.engine().devtools_force_state(a, 0, true);
    assert_eq!(eval(&mut page, color), "rgb(2, 0, 0)");
    page.engine().devtools_force_state(a, 0, false);
    page.engine().devtools_force_state(a, 5, true);
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('a')).backgroundColor"
        ),
        "rgb(5, 0, 0)"
    );
    page.engine().devtools_force_state(a, 5, false);
    page.engine().devtools_force_state(a, 4, true);
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('w')).marginLeft"
        ),
        "7px"
    );
    page.engine().devtools_force_state(a, 4, false);
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('w')).marginLeft"
        ),
        "0px"
    );
    page.engine().devtools_force_state(l, 3, true);
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('l').matches(':visited')"
        ),
        "true"
    );
    page.engine().devtools_force_state(l, 3, false);
    assert_eq!(
        eval(
            &mut page,
            "document.getElementById('l').matches(':visited')"
        ),
        "false"
    );
    page.engine().devtools_force_state(t, 6, true);
    assert_eq!(
        eval(
            &mut page,
            "getComputedStyle(document.getElementById('t')).color"
        ),
        "rgb(6, 0, 0)"
    );
    page.engine().devtools_clear_forced();
    assert_eq!(page.engine().devtools_forced_kinds(t), 0);
    let _ = w;
}
