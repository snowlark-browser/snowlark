// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The document selection and editing: the Selection API, selecting text with the mouse and the
//! keyboard, copying it, and editing in `contenteditable` hosts and `designMode` documents with
//! `beforeinput`/`input` and `execCommand`.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Item, Point};

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

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

fn key(vk: u16, text: Option<char>, mods: Modifiers) -> KeyInput {
    KeyInput {
        vk,
        scan: 0,
        text,
        mods,
        repeat: false,
        paste: None,
        paste_html: None,
    }
}

fn press(page: &mut Page, vk: u16, mods: Modifiers) -> snowlark_page::keyboard::KeyOutcome {
    let k = key(vk, None, mods);
    let out = page.key_down(&k);
    page.key_up(&k);
    out
}

fn type_text(page: &mut Page, s: &str) {
    for c in s.chars() {
        let k = key(c.to_ascii_uppercase() as u16, Some(c), Modifiers::default());
        page.key_down(&k);
        page.key_char(c, &k);
        page.key_up(&k);
    }
}

fn click(page: &mut Page, x: f32, y: f32, clicks: u32, mods: Modifiers) {
    page.pointer_pressed(Point::new(x, y), 0, mods, clicks);
    page.pointer_released(Point::new(x, y), 0, mods, clicks);
}

const CTRL: Modifiers = Modifiers {
    ctrl: true,
    shift: false,
    alt: false,
    meta: false,
};
const SHIFT: Modifiers = Modifiers {
    ctrl: false,
    shift: true,
    alt: false,
    meta: false,
};

/// Live ranges, the selection's among them, follow character data changes, `splitText` and
/// `normalize()` (DOM §4.10 "replace data" steps 8-11, §4.11 "split a Text node" steps 7.2-7.5, §4.4
/// `normalize()` steps 6.1-6.4).
#[test]
fn live_ranges_follow_text_changes() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<p id=p>hello world</p>");
    let out = eval(
        &mut page,
        r#"
        const t = p.firstChild, r = new Range(), out = [];
        r.setStart(t, 6); r.setEnd(t, 11);
        t.insertData(0, "oh, ");
        out.push(r.startOffset + "," + r.endOffset);
        t.deleteData(9, 3);
        out.push(r.startOffset + "," + r.endOffset);
        const n = t.splitText(5);
        out.push((r.startContainer === n) + "," + r.startOffset + "," + r.endOffset);
        const r2 = new Range(); r2.setStart(p, 1);
        t.splitText(2);
        out.push(r2.startOffset);
        p.normalize();
        out.push([r.startContainer === t, r.startOffset, r.endOffset,
                  r2.startContainer === t, r2.startOffset].join());
        getSelection().collapse(t, 3);
        t.insertData(0, "ab");
        out.push(getSelection().anchorOffset);
        out.join("|")
        "#,
    );
    assert_eq!(out, "10,15|9,12|true,4,7|2|true,9,12,true,5|5");
}

/// An editing host that gains focus gets a caret at its first caret position unless the selection
/// is already in it: past collapsed white space and empty inlines, before a `br`, at the start of
/// the block when a non-editable element or a form control comes first.
#[test]
fn focusing_an_editing_host_puts_the_caret_in_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<div id=e contenteditable><p> </p><p><span></span> abc</p></div>\
         <div id=f contenteditable><p>x</p><p>y</p></div>\
         <div id=g contenteditable><div><span></span><span contenteditable=false>z</span></div></div>\
         <div id=h contenteditable><p><br></p></div>",
    );
    let out = eval(
        &mut page,
        r#"
        const s = getSelection(), out = [];
        const at = () => (s.rangeCount ? [s.anchorNode.nodeName, s.anchorNode.data ?? s.anchorNode.id ?? "", s.anchorOffset].join(":") : "none");
        e.focus(); out.push(at());
        f.focus(); s.collapse(f.lastChild.firstChild, 1); f.blur(); f.focus(); out.push(at());
        g.focus(); out.push(at() + ":" + (s.anchorNode === g.firstChild));
        h.focus(); out.push(at() + ":" + (s.anchorNode === h.firstChild));
        out.join("|")
        "#,
    );
    assert_eq!(out, "#text: abc:1|#text:y:1|DIV::0:true|P::0:true");
}

/// Moving by character steps over rendered content: collapsed white space does not count, a block
/// or `br` boundary is a step of its own (the next line's first character is not skipped), and a
/// non-editable element inside an editing host is one unit.
#[test]
fn moving_by_character_steps_over_rendered_content() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<div id=e contenteditable><p id=a>abc  <span>def</span></p><p id=b>ghi</p>\
         <p id=c>x<span contenteditable=false>ne</span>y</p></div><pre id=pre>foo<br>bar</pre>",
    );
    let out = eval(
        &mut page,
        r#"
        const s = getSelection(), out = [];
        const at = () => [s.focusNode.nodeName, s.focusNode.data ?? "", s.focusOffset].join(":");
        const mv = (node, offset, dir) => { s.collapse(node, offset); s.modify("move", dir, "character"); out.push(at()); };
        const def = a.querySelector("span").firstChild;
        mv(def, 0, "left");
        mv(def, 3, "right");
        mv(b.firstChild, 0, "left");
        mv(c.firstChild, 1, "right");
        mv(pre.firstChild, 3, "right");
        mv(pre.lastChild, 0, "left");
        out.join("|")
        "#,
    );
    assert_eq!(
        out,
        "#text:abc  :3|#text:ghi:0|#text:def:3|P::2|#text:bar:0|#text:foo:3"
    );
}

#[test]
fn moving_by_word_steps_over_rendered_words() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<div id=e contenteditable>\n  one  two<b>three</b> four\n</div>\
         <div id=k contenteditable>\u{D76C}\u{C9C4}DJ</div><div id=p contenteditable>~>>>>>p+++</div>",
    );
    let out = eval(
        &mut page,
        r#"
        const s = getSelection(), out = [];
        const at = () => [s.focusNode.data.trim(), s.focusOffset].join(":");
        const mv = (node, offset, dir) => { s.collapse(node, offset); s.modify("move", dir, "word"); out.push(at()); };
        const t = e.firstChild, three = e.querySelector("b").firstChild, four = e.lastChild;
        mv(t, 0, "forward");
        mv(t, 8, "forward");
        mv(three, 5, "forward");
        mv(four, 1, "backward");
        mv(k.firstChild, 0, "forward");
        mv(k.firstChild, 4, "backward");
        mv(p.firstChild, 6, "backward");
        mv(p.firstChild, 7, "forward");
        out.join("|")
        "#,
    );
    // (collapsed white space is not a stop; "two<b>three</b>" is one word; a change of script is a
    // word boundary; a run of punctuation is one unit)
    assert_eq!(
        out,
        "one  two:7|four:1|four:5|one  two:8|\u{D76C}\u{C9C4}DJ:2|\u{D76C}\u{C9C4}DJ:2|~>>>>>p+++:0|~>>>>>p+++:10"
    );
}

#[test]
fn moving_a_range_by_lines_keeps_the_focus_column() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<div id=e contenteditable style='font: 16px monospace'>text1<br>text2</div>\
         <div id=v contenteditable style='font: 16px monospace; writing-mode: vertical-lr; \
         height: 200px'>text1<br>text2</div>\
         <div id=w contenteditable style='font: 16px monospace; writing-mode: vertical-rl; \
         height: 200px'>text1<br>text2</div>",
    );
    let out = eval(
        &mut page,
        r#"
        const s = getSelection(), out = [];
        let one, two;
        const mv = (node, anchor, focus, dir, unit) => {
            s.setBaseAndExtent(node, anchor, node, focus);
            s.modify("move", dir, unit);
            out.push([s.isCollapsed, s.focusNode == one ? 1 : 2, s.focusOffset].join(":"));
        };
        [one, , two] = e.childNodes;
        mv(two, 0, 5, "backward", "line");
        mv(two, 5, 0, "backward", "line");
        mv(one, 0, 5, "forward", "line");
        mv(one, 5, 0, "forward", "paragraph");
        s.setBaseAndExtent(one, 1, one, 4);
        s.modify("move", "forward", "character");
        out.push(s.focusOffset);
        // (lines stack rightwards in vertical-lr, leftwards in vertical-rl)
        for (const host of [v, w]) {
            [one, , two] = host.childNodes;
            mv(one, 5, 1, "forward", "line");
            mv(two, 0, 4, "backward", "line");
        }
        out.join("|")
        "#,
    );
    assert_eq!(
        out,
        "true:1:5|true:1:0|true:2:5|true:2:0|4|true:2:1|true:1:4|true:2:1|true:1:4"
    );
}

#[test]
fn a_text_controls_selection_change_fires_selectionchange_at_it() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<input id=i value=abc><textarea id=t>xyz</textarea>");
    eval(
        &mut page,
        r#"
        window.log = [];
        for (const el of [i, t, document])
            el.addEventListener("selectionchange", e =>
                log.push((e.target.id || "doc") + ">" + (e.currentTarget.id || "doc")));
        i.setSelectionRange(0, 0);
        i.setSelectionRange(1, 2);
        i.setSelectionRange(1, 2);
        t.select();
        window.sync = log.length;
        "#,
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    // (queued, one per control however many changes, bubbling; none for an unchanged selection)
    assert_eq!(
        eval(&mut page, "sync + ':' + log.join('|')"),
        "0:i>i|i>doc|t>t|t>doc"
    );
    eval(
        &mut page,
        "log.length = 0; i.selectionStart = 1; t.setSelectionRange(0, 3)",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    assert_eq!(eval(&mut page, "log.join('|')"), "");
}

#[test]
fn selection_to_string_leaves_out_unselectable_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<div id=a>\n  a<span style='user-select: none'>b</span>c\n</div>\
         <div id=b>\n  start <span style='user-select: none'>unselectable <i>nested</i>\n text</span> end\n</div>\
         <div id=c style='user-select: none'>\n <span style='user-select: text'>sel</span> not\n \
         <span style='user-select: text'>text</span></div>",
    );
    let out = eval(
        &mut page,
        r#"
        const s = getSelection();
        [a, b, c].map(el => { s.selectAllChildren(el); return s.toString(); }).join("|")
        "#,
    );
    // (the unselectable text still takes part in white space collapsing)
    assert_eq!(out, "ac|start  end|seltext");
}

#[test]
fn the_selection_api() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><p id=p>Hello <b>bold</b> world</p>\
         <script>window.changes = 0; document.addEventListener('selectionchange', () => changes++);\
         window.s = getSelection(); window.t = document.getElementById('p').firstChild;</script>",
    );
    assert_eq!(
        eval(
            &mut page,
            "[s === document.getSelection(), s.rangeCount, s.type, s.direction].join()"
        ),
        "true,0,None,none"
    );
    eval(&mut page, "s.collapse(t, 2); s.extend(t, 5)");
    assert_eq!(
        eval(
            &mut page,
            "[s.anchorOffset, s.focusOffset, s.isCollapsed, s.type, s.direction, String(s)].join()"
        ),
        "2,5,false,Range,forward,llo"
    );
    // Extending backwards past the anchor.
    eval(&mut page, "s.extend(t, 0)");
    assert_eq!(
        eval(
            &mut page,
            "[s.anchorOffset, s.focusOffset, s.direction, s.toString()].join()"
        ),
        "2,0,backward,He"
    );
    // The range is the selection's own: changing it changes the selection.
    eval(
        &mut page,
        "s.selectAllChildren(document.getElementById('p'))",
    );
    assert_eq!(eval(&mut page, "s.toString()"), "Hello bold world");
    eval(&mut page, "s.getRangeAt(0).setStart(t, 6)");
    assert_eq!(eval(&mut page, "s.toString()"), "bold world");
    assert_eq!(
        eval(&mut page, "s.containsNode(document.querySelector('b'))"),
        "true"
    );
    let r = eval(
        &mut page,
        "const r = document.createRange(); r.setStart(t, 1); r.setEnd(t, 3); s.removeAllRanges(); \
         s.addRange(r); [s.getRangeAt(0) === r, s.toString()].join()",
    );
    assert_eq!(r, "true,el");
    eval(
        &mut page,
        "s.collapse(t, 0); s.modify('extend', 'forward', 'word')",
    );
    assert_eq!(eval(&mut page, "s.toString()"), "Hello ");
    assert_eq!(
        eval(&mut page, "try { s.getRangeAt(1) } catch (e) { e.name }"),
        "IndexSizeError"
    );
    // selectionchange: queued, coalesced.
    page.run_until_settled(Duration::from_secs(2));
    let n: u32 = eval(&mut page, "changes").parse().unwrap();
    assert!((1..10).contains(&n), "{n} selectionchange events");
}

#[test]
fn selected_text_is_painted_in_its_own_elements_selection_colors() {
    // CSS Pseudo-Elements 4 §3.5: each text takes the `::selection` style of its originating
    // element, and a color that style does not declare comes from the parent's highlight. §3.3.1:
    // the user agent's colors apply only where the author gives neither `color` nor
    // `background-color`, so a color alone leaves no background. §3.6.3: `currentcolor` (with no
    // highlight below) is the text's own color.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; color: white; background: rgb(32, 32, 32) }\
         p { margin: 0 }\
         #p::selection { background: rgb(255, 255, 0); color: rgb(1, 2, 3) }\
         #p b::selection { color: rgb(255, 0, 0) }\
         #q::selection { color: rgb(0, 255, 255) }\
         #r::selection { text-shadow: none }\
         #s::selection { background: rgb(0, 128, 0); color: currentcolor }\
         </style><p id=p>aaaa<b>bbbb</b></p><p id=q>cccc</p><p id=r>dddd</p><p id=s>eeee</p>\
         <script>var r = document.createRange();\
         r.setStart(document.getElementById('p').firstChild, 2);\
         r.setEnd(document.getElementById('s').firstChild, 2);\
         getSelection().addRange(r);</script>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let rgb = |c: &snowlark_paint::Color| (c.r, c.g, c.b);
    let fills: Vec<(u8, u8, u8)> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } => Some(rgb(color)),
            _ => None,
        })
        .filter(|c| *c != (32, 32, 32))
        .collect();
    // `aa` and `bbbb` on #p's yellow, `cccc` on nothing, `dddd` on the default, `ee` on green.
    assert_eq!(
        fills,
        vec![(255, 255, 0), (255, 255, 0), (0, 86, 205), (0, 128, 0)]
    );
    let runs: Vec<(u8, u8, u8)> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Text(t) => Some(rgb(&t.color)),
            _ => None,
        })
        .collect();
    let white = (255, 255, 255);
    assert_eq!(
        runs,
        vec![
            // `aa|aa`: the part not selected, then the selected one
            white,
            (1, 2, 3),
            (255, 0, 0),
            (0, 255, 255),
            // `dddd` (the default highlight) and `ee|ee` (currentcolor) keep their color
            white,
            white,
        ]
    );
}

#[test]
fn selected_text_without_selection_colors_is_highlighttext_on_highlight() {
    // CSS Pseudo-Elements 4 §3.3: where no `::selection` colors are declared, the selected text
    // is painted in `HighlightText` on `Highlight` (Windows' highlight (0, 120, 215), translucent
    // so that it looks the same over white: rgba(0, 86, 205, 0.8) paints (51, 120, 215) there).
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; color: rgb(0, 0, 0); background: rgb(255, 255, 255) }\
         </style><p id=p>aaaa</p>\
         <script>var r = document.createRange();\
         r.setStart(document.getElementById('p').firstChild, 2);\
         r.setEnd(document.getElementById('p').firstChild, 4);\
         getSelection().addRange(r);</script>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let rgba = |c: &snowlark_paint::Color| (c.r, c.g, c.b, c.a);
    let fills: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } => Some(rgba(color)),
            _ => None,
        })
        .filter(|c| *c != (255, 255, 255, 255))
        .collect();
    assert_eq!(fills, vec![(0, 86, 205, 204)]);
    let runs: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Text(t) => Some(rgba(&t.color)),
            _ => None,
        })
        .collect();
    // `aa|aa`: the part not selected keeps its black, the selected part is white
    assert_eq!(runs, vec![(0, 0, 0, 255), (255, 255, 255, 255)]);
}

#[test]
fn a_selection_highlights_the_whole_line_box() {
    // CSS Pseudo-Elements 4 §3.4: the highlighted area of selected text may be its line box; it
    // is the text's own box united with the line box in the block direction, so a tall line is
    // filled from top to bottom and a short one still covers the glyphs. Vertical text the same
    // way across the line.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; color: rgb(0, 0, 0); background: rgb(255, 255, 255) }\
         p { margin: 0 }\
         #a { line-height: 50px }\
         #b { line-height: 4px; position: absolute; top: 100px }\
         #c { line-height: 50px; writing-mode: vertical-rl; position: absolute; top: 200px; left: 0 }\
         </style><p id=a>aaaa</p><p id=b>bbbb</p><p id=c>cccc</p>\
         <script>var r = document.createRange();\
         r.setStart(document.getElementById('a').firstChild, 0);\
         r.setEnd(document.getElementById('c').firstChild, 4);\
         getSelection().addRange(r);</script>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let mut fills = vec![];
    let mut transform = snowlark_paint::Matrix::IDENTITY;
    for i in &list.items {
        match i {
            Item::PushTransform { matrix, .. } => transform = *matrix,
            Item::PopTransform => transform = snowlark_paint::Matrix::IDENTITY,
            Item::FillRect { rect, color, .. } if (color.r, color.g, color.b) == (0, 86, 205) => {
                fills.push(transform.apply_rect(rect))
            }
            _ => {}
        }
    }
    assert_eq!(fills.len(), 3, "{fills:?}");
    // #a: the full 50 px line (the text's own box is about 23 px tall, centred in it)
    assert_eq!((fills[0].y, fills[0].height), (0.0, 50.0), "{fills:?}");
    // #b: a 4 px line inside the glyphs' box: the glyphs' box
    assert!(
        fills[1].y < 100.0 && fills[1].y + fills[1].height > 104.0 && fills[1].height < 30.0,
        "{fills:?}"
    );
    // #c: vertical, the full 50 px column
    assert_eq!((fills[2].x, fills[2].width), (0.0, 50.0), "{fills:?}");
    // The selection goes on past the ends of #a's and #b's lines, which take a space's width more
    // (a monospace font: five advances); it ends with #c's text, whose line takes none (four).
    let advance = fills[2].height / 4.0;
    assert!((fills[0].width - 5.0 * advance).abs() < 0.5, "{fills:?}");
    assert!((fills[1].width - 5.0 * advance).abs() < 0.5, "{fills:?}");
}

#[test]
fn a_selected_replaced_element_is_covered_by_its_highlight() {
    // CSS Pseudo-Elements 4 §3.6.4: a selected replaced element gets its highlight's background
    // over its content, translucent when that background is opaque (so the content shows), and as
    // tall as its line box; a block-level one over its own box. An element outside the selection
    // gets none.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; color: rgb(0, 0, 0); background: rgb(255, 255, 255) }\
         p { margin: 0; line-height: 50px }\
         canvas { width: 30px; height: 30px; vertical-align: middle }\
         #r::selection { background: rgb(255, 0, 0) }\
         </style><p id=a>a<canvas></canvas>a</p><canvas id=r style='display: block'></canvas>\
         <p id=b>b<canvas></canvas></p>\
         <script>var r = document.createRange();\
         r.setStart(document.getElementById('a').firstChild, 0);\
         r.setEnd(document.getElementById('b').firstChild, 1);\
         getSelection().addRange(r);</script>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let overlays: Vec<_> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { rect, color } if color.a < 255 && color.a > 0 => {
                Some((*rect, (color.r, color.g, color.b, color.a)))
            }
            _ => None,
        })
        .collect();
    let canvases: Vec<_> = overlays.iter().filter(|(r, _)| r.width == 30.0).collect();
    assert_eq!(canvases.len(), 2, "{overlays:?}");
    // the inline canvas: the default highlight over its 30 px, as tall as the 50 px line
    let (inline, color) = canvases[0];
    assert_eq!(
        (inline.y, inline.height, *color),
        (0.0, 50.0, (0, 86, 205, 204))
    );
    // the block canvas: its `::selection` red at 80%, over its own 30 px box
    let (block, color) = canvases[1];
    assert_eq!(
        (block.y, block.height, *color),
        (50.0, 30.0, (255, 0, 0, 204))
    );
}

#[test]
fn a_selection_ending_inside_a_line_does_not_cover_its_end() {
    // The space's width at a line's end is for a selection that goes on to the next line: one that
    // ends at the last character of the line, or within the node, does not take it.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; color: rgb(0, 0, 0); background: rgb(255, 255, 255) }\
         p { margin: 0 }\
         </style><p id=a>aaaa</p><p id=b>bbbb</p>\
         <script>var r = document.createRange();\
         r.setStart(document.getElementById('a').firstChild, 0);\
         r.setEnd(document.getElementById('a').firstChild, 4);\
         getSelection().addRange(r);</script>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let fills: Vec<snowlark_paint::Rect> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { rect, color } if (color.r, color.g, color.b) == (0, 86, 205) => {
                Some(*rect)
            }
            _ => None,
        })
        .collect();
    assert_eq!(fills.len(), 1, "{fills:?}");
    // four advances of the 20 px monospace font (0.6 em each in the common ones)
    assert!(fills[0].width > 40.0 && fills[0].width < 52.0, "{fills:?}");
}

#[test]
fn selection_colors_follow_a_style_change_alone() {
    // A rule added through the CSSOM changes no node: the selection's highlight colors still come
    // from the styles that are painted (CSS Pseudo-Elements 4 §3.5), in the same rendering update.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; color: rgb(0, 0, 0); background: rgb(255, 255, 255) }\
         </style><p id=p>aaaa</p>\
         <script>getSelection().selectAllChildren(document.getElementById('p'));</script>",
    );
    let fills = |page: &mut Page| -> Vec<(u8, u8, u8)> {
        page.update_the_rendering();
        let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
        list.items
            .iter()
            .filter_map(|i| match i {
                Item::FillRect { color, .. } => Some((color.r, color.g, color.b)),
                _ => None,
            })
            .filter(|c| *c != (255, 255, 255))
            .collect()
    };
    assert_eq!(fills(&mut page), vec![(0, 86, 205)]);
    eval(
        &mut page,
        "document.styleSheets[0].insertRule('#p::selection { background: rgb(0, 255, 0) }', 1)",
    );
    assert_eq!(fills(&mut page), vec![(0, 255, 0)]);
}

#[test]
fn the_mouse_and_keyboard_select_page_text() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0;font:20px monospace'>\
         <p id=a style='margin:0'>alpha beta gamma</p><p id=b style='margin:0'>second line</p>",
    );
    let w = {
        page.update_the_rendering();
        let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
        list.items
            .iter()
            .find_map(|i| match i {
                Item::Text(t) => Some(list.glyphs(t.glyphs).1[0]),
                _ => None,
            })
            .unwrap()
    };
    let none = Modifiers::default();
    // A drag from inside "beta" to inside "gamma".
    page.pointer_pressed(Point::new(w * 6.4, 10.0), 0, none, 1);
    page.pointer_moved(Point::new(w * 13.4, 10.0), none);
    page.pointer_released(Point::new(w * 13.4, 10.0), 0, none, 1);
    assert_eq!(eval(&mut page, "getSelection().toString()"), "beta ga");
    // The selection is painted.
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let highlighted = list.items.iter().any(
        |i| matches!(i, Item::FillRect { color, rect } if (color.r, color.g, color.b) == (0, 86, 205) && rect.width > w * 6.0),
    );
    assert!(highlighted, "a highlight under the selected text");
    // A double-click selects a word, a triple-click the paragraph.
    click(&mut page, w * 1.5, 10.0, 2, none);
    assert_eq!(eval(&mut page, "getSelection().toString()"), "alpha ");
    click(&mut page, w * 1.5, 30.0, 3, none);
    assert_eq!(eval(&mut page, "getSelection().toString()"), "second line");
    // Shift+click extends.
    click(&mut page, w * 2.0, 10.0, 1, none);
    click(&mut page, w * 3.0, 30.0, 1, SHIFT);
    assert_eq!(
        eval(&mut page, "getSelection().toString()"),
        "pha beta gamma\n\nsec"
    );
    // Ctrl+A selects the page, Ctrl+C copies its rendered text.
    press(&mut page, b'A' as u16, CTRL);
    let out = press(&mut page, b'C' as u16, CTRL);
    assert_eq!(out.copy.as_deref(), Some("alpha beta gamma\n\nsecond line"));
}

#[test]
fn contenteditable_hosts_edit_with_input_events() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0;font:20px monospace'>\
         <div id=e contenteditable style='height:100px'>abc</div>\
         <script>window.log = []; const e = document.getElementById('e'); \
         e.addEventListener('beforeinput', ev => log.push('b:' + ev.inputType + ':' + ev.data)); \
         e.addEventListener('input', ev => log.push('i:' + ev.inputType));</script>",
    );
    let none = Modifiers::default();
    // A click at the end of the text puts the caret there.
    click(&mut page, 300.0, 10.0, 1, none);
    assert_eq!(eval(&mut page, "document.activeElement.id"), "e");
    assert_eq!(
        eval(
            &mut page,
            "const s = getSelection(); [s.focusNode.data, s.focusOffset].join()"
        ),
        "abc,3"
    );
    type_text(&mut page, "de");
    assert_eq!(eval(&mut page, "e.innerHTML"), "abcde");
    assert_eq!(
        eval(&mut page, "log.join()"),
        "b:insertText:d,i:insertText,b:insertText:e,i:insertText"
    );
    // Enter splits: the line becomes a div and the rest goes to a new one (insertParagraph).
    press(&mut page, 0x25, none);
    press(&mut page, 0x25, none);
    press(&mut page, 0x0D, none);
    assert_eq!(
        eval(&mut page, "e.innerHTML"),
        "<div>abc</div><div>de</div>"
    );
    type_text(&mut page, "X");
    assert_eq!(
        eval(&mut page, "e.innerHTML"),
        "<div>abc</div><div>Xde</div>"
    );
    // Backspace at a block's start merges it into the text before.
    press(&mut page, 0x25, none);
    press(&mut page, 0x08, none);
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>abcXde</div>");
    press(&mut page, 0x08, none);
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>abXde</div>");
    // Delete forward, Shift+Enter a line break.
    press(&mut page, 0x2E, none);
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>abde</div>");
    press(&mut page, 0x0D, SHIFT);
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>ab<br>de</div>");
    // A canceled beforeinput changes nothing.
    eval(
        &mut page,
        "e.addEventListener('beforeinput', ev => ev.preventDefault(), { once: true })",
    );
    type_text(&mut page, "Q");
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>ab<br>de</div>");
    // Select all, bold (the line break joins the bold run, as editing/data/bold.js wraps a
    // `br`), then type over.
    press(&mut page, b'A' as u16, CTRL);
    assert_eq!(eval(&mut page, "document.execCommand('bold')"), "true");
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div><b>ab<br>de</b></div>");
    assert_eq!(
        eval(&mut page, "document.queryCommandState('bold')"),
        "true"
    );
    assert_eq!(eval(&mut page, "document.execCommand('bold')"), "true");
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>ab<br>de</div>");
    // (the `div`'s contents: a selection of the whole `div` would delete it too)
    eval(
        &mut page,
        "getSelection().selectAllChildren(e.firstChild);\
         document.execCommand('insertText', false, 'new')",
    );
    assert_eq!(eval(&mut page, "e.innerHTML"), "<div>new</div>");
    // Paste as plain text: lines after the first become paragraphs.
    let mut paste = key(b'V' as u16, None, CTRL);
    paste.paste = Some("one\ntwo".into());
    page.key_down(&paste);
    assert_eq!(
        eval(&mut page, "e.innerHTML"),
        "<div>newone</div><div>two</div>"
    );
    assert_eq!(eval(&mut page, "log[log.length - 1]"), "i:insertFromPaste");
    // The focused host paints its caret.
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let caret = list.items.iter().any(
        |i| matches!(i, Item::FillRect { rect, .. } if rect.width == 1.0 && rect.height > 10.0),
    );
    assert!(caret, "a caret");
}

#[test]
fn design_mode_makes_the_document_editable() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0;font:20px monospace'><p id=p>text</p>",
    );
    assert_eq!(eval(&mut page, "document.designMode"), "off");
    eval(&mut page, "document.designMode = 'on'");
    assert_eq!(
        eval(
            &mut page,
            "[document.designMode, document.body.isContentEditable].join()"
        ),
        "on,true"
    );
    click(&mut page, 200.0, 10.0, 1, Modifiers::default());
    type_text(&mut page, "!");
    assert_eq!(
        eval(&mut page, "document.getElementById('p').textContent"),
        "text!"
    );
}

/// The highlight covers the text nodes from the selection's start to its end in one walk: an end in
/// an element (after its children) includes the text before that point, and nothing after it.
#[test]
fn a_selection_ending_in_an_element_highlights_the_text_before_its_end() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0;font:20px monospace'>\
         <p id=a style='margin:0'>one</p><p id=b style='margin:0'><span>two</span><span>three</span></p>\
         <p id=c style='margin:0'>four</p>",
    );
    let highlights = |page: &mut Page| {
        page.update_the_rendering();
        let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
        list.items
            .iter()
            .filter_map(|i| match i {
                Item::FillRect { color, rect } if (color.r, color.g, color.b) == (0, 86, 205) => {
                    Some(rect.y.round() as i32)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    // From the start of "one" to (p#b, 1): "one" and "two" are selected, "three" and "four" not.
    eval(
        &mut page,
        "getSelection().setBaseAndExtent(a.firstChild, 0, b, 1)",
    );
    assert_eq!(eval(&mut page, "getSelection().toString()"), "one\n\ntwo");
    let ys = highlights(&mut page);
    assert_eq!(ys.len(), 2, "{ys:?}");
    // To (p#b, 2): "three" too.
    eval(
        &mut page,
        "getSelection().setBaseAndExtent(a.firstChild, 0, b, 2)",
    );
    assert_eq!(highlights(&mut page).len(), 3);
}

#[test]
fn a_selection_paints_its_text_shadows_and_recolors_the_decorations() {
    // CSS Pseudo-Elements 4 §3.6.3: over the highlight's background, the selected part gets its
    // `::selection` `text-shadow`s, and the originating element's decorations take the
    // highlight's `color` there (they kept theirs, under the background); the unselected parts
    // keep the text's own decoration color and shadow.
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><style>\
         body { margin: 0; font: 20px monospace; background: rgb(255, 255, 255) }\
         p { margin: 0; color: rgb(0, 0, 0); text-decoration: underline rgb(255, 0, 0);\
         text-shadow: 0 40px rgb(128, 128, 128) }\
         #p::selection { color: rgb(0, 128, 0); background: rgb(255, 255, 0);\
         text-shadow: 0 60px rgb(0, 0, 255) }\
         </style><p id=p>abcdef</p>\
         <script>var t = document.getElementById('p').firstChild, r = document.createRange();\
         r.setStart(t, 2); r.setEnd(t, 4); getSelection().addRange(r);</script>",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let rgb = |c: &snowlark_paint::Color| (c.r, c.g, c.b);
    let painted: Vec<(&str, (u8, u8, u8))> = list
        .items
        .iter()
        .filter_map(|i| match i {
            Item::FillRect { color, .. } => Some(("fill", rgb(color))),
            Item::Text(t) => Some(("text", rgb(&t.color))),
            _ => None,
        })
        .filter(|(_, c)| *c != (255, 255, 255))
        .collect();
    let first = |what: (&str, (u8, u8, u8))| painted.iter().position(|p| *p == what);
    let background = first(("fill", (255, 255, 0))).expect("the highlight background");
    // the text's own shadow (and its underline's), below the highlight
    assert!(
        first(("text", (128, 128, 128))).is_some_and(|i| i < background),
        "{painted:?}"
    );
    // the selection's shadow over the highlight, the underline green there and red elsewhere
    assert!(
        first(("text", (0, 0, 255))).is_some_and(|i| i > background),
        "{painted:?}"
    );
    assert!(
        first(("fill", (0, 128, 0))).is_some_and(|i| i > background),
        "{painted:?}"
    );
    assert!(painted.contains(&("fill", (255, 0, 0))), "{painted:?}");
    assert!(painted.contains(&("text", (0, 128, 0))), "{painted:?}");
}

#[test]
fn a_drag_over_a_gap_takes_its_focus_from_what_is_under_the_pointer() {
    // The focus of a drag is the position for the point in the element under the pointer: over
    // the gap between two paragraphs of an article, text in a side column at the same height is
    // not a candidate (it was, being the only text on the pointer's line).
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0;font:20px/20px monospace'>\
         <div id=art style='position:absolute;left:0;top:0;width:300px'>\
         <p id=a style='margin:0 0 60px'>alpha beta</p><p id=b style='margin:0'>second</p></div>\
         <div id=side style='position:absolute;left:400px;top:40px'>teaser text</div>",
    );
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(70.0, 10.0), 0, none, 1);
    page.pointer_moved(Point::new(40.0, 50.0), none);
    page.pointer_released(Point::new(40.0, 50.0), 0, none, 1);
    let focus = eval(&mut page, "getSelection().focusNode.parentNode.id");
    assert!(focus == "a" || focus == "b", "focus in #{focus}");
    assert!(!eval(&mut page, "getSelection().toString()").contains("teaser"));
}
