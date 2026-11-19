// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live collections (DOM §4.2.10) stay live while their members are found lazily: an indexed read
//! finds members only up to its index, and the next read goes on from there until the tree changes.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page_with(html: &str) -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(html, &url::Url::parse("http://example.test/").unwrap());
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

#[test]
fn a_partly_read_collection_sees_insertions_and_removals() {
    let mut page = page_with("<!doctype html><div id=r><p id=a></p><p id=b></p><p id=c></p></div>");
    let r = eval(
        &mut page,
        r#"
        const r = document.getElementById('r');
        const ps = r.getElementsByTagName('p');
        const out = [ps[0].id];
        // A node inserted ahead of where the walk stopped, and one behind it.
        const x = document.createElement('p'); x.id = 'x';
        r.insertBefore(x, r.firstChild);
        out.push(ps[0].id, ps[1].id);
        const y = document.createElement('p'); y.id = 'y';
        r.appendChild(y);
        out.push(ps[4].id, ps.length);
        r.removeChild(document.getElementById('b'));
        out.push(ps[2].id, ps.length, ps[4] === undefined);
        out.join()
        "#,
    );
    assert_eq!(r, "a,x,a,y,5,c,4,true");
}

#[test]
fn attribute_changes_keep_tag_collections_and_update_class_collections() {
    let mut page = page_with(
        "<!doctype html><ul><li class=k>1</li><li>2</li><li class=k>3</li><li>4</li></ul>",
    );
    let r = eval(
        &mut page,
        r#"
        const lis = document.getElementsByTagName('li');
        const ks = document.getElementsByClassName('k');
        const out = [];
        // Read one item, change attributes and text, read on.
        out.push(lis[0].textContent, ks[0].textContent);
        lis[1].className = 'k';
        lis[2].removeAttribute('class');
        lis[3].firstChild.data = 'four';
        lis[0].setAttribute('style', 'color: red');
        out.push(lis[1].textContent, lis[3].textContent, lis.length);
        out.push(ks.length, ks[0].textContent, ks[1].textContent, ks[2] === undefined);
        out.join()
        "#,
    );
    assert_eq!(r, "1,1,2,four,4,2,1,2,true");
}

#[test]
fn child_collections_walk_lazily_and_stay_live() {
    let mut page = page_with("<!doctype html><div id=d>t<b>1</b>u<i>2</i><b>3</b></div>");
    let r = eval(
        &mut page,
        r#"
        const d = document.getElementById('d');
        const kids = d.children, nodes = d.childNodes;
        const out = [kids[0].localName, nodes[1].localName];
        d.firstChild.data = 'changed';
        out.push(kids[1].localName, nodes[3].localName, kids.length, nodes.length);
        d.insertBefore(document.createElement('s'), d.children[1]);
        out.push(kids[1].localName, kids[2].localName, kids.length);
        // Reading in order, then out of order, then past the end.
        out.push(Array.from(kids, k => k.localName).join(''), kids[3].localName, kids[9] === undefined);
        out.join()
        "#,
    );
    assert_eq!(r, "b,b,i,i,3,5,s,i,4,bsib,b,true");
}

#[test]
fn named_and_link_collections_follow_attribute_changes_mid_walk() {
    let mut page = page_with(
        "<!doctype html><a href=1 id=a1></a><a id=a2></a><a href=3 id=a3></a><input name=n id=i1><input id=i2>",
    );
    let r = eval(
        &mut page,
        r#"
        const links = document.links, named = document.getElementsByName('n');
        const out = [links[0].id, named[0].id];
        document.getElementById('a2').setAttribute('href', '2');
        document.getElementById('i2').setAttribute('name', 'n');
        out.push(links[1].id, links.length, named[1].id, named.length);
        out.join()
        "#,
    );
    assert_eq!(r, "a1,i1,a2,3,i2,2");
}
