// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTML §6.11.5 the drag-and-drop processing model driven by pointer input: a press on a draggable
//! element that moves past the threshold fires `dragstart`, suppresses the pointer events, and
//! each move is an iteration (`drag`, `dragenter`, `dragleave`, `dragover`); the release drops
//! (`drop`, `dragend`). And drags from outside the page (the window's drop target), with files.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::dnd::{DragOperation, DroppedFile, ExternalDrag};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

fn page_with(html: &str) -> Page {
    static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
    let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
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

/// `a` (draggable, 100x50 at 0,0) and `b` (a drop zone, 100x50 at 0,100); the drag-and-drop,
/// pointer and mouse events are logged as `type@id`, the DND events with what their DataTransfer
/// shows (`types`, `getData('text')`, `dropEffect`).
const PAGE: &str = "<!doctype html><style>body{margin:0}div{width:100px;height:50px}\
    #b{margin-top:50px}</style>\
    <div id=a draggable=true></div><div id=b></div><script>var log = [];\
    for (const type of ['dragstart','drag','dragenter','dragleave','dragover','drop','dragend'])\
      document.addEventListener(type, e => { if (!e.target.id) return;\
        const dt = e.dataTransfer;\
        log.push(type + '@' + e.target.id + '[' + dt.types + '|' + dt.getData('text') + '|' + dt.dropEffect + ']') }, true);\
    for (const type of ['pointercancel','pointerout','pointerleave','mouseup','click'])\
      document.addEventListener(type, e => { if (e.target.id) log.push(type + '@' + e.target.id) }, true);\
    a.addEventListener('dragstart', e => { e.dataTransfer.setData('text/plain', 'hi');\
      e.dataTransfer.effectAllowed = 'copyMove' });\
    b.addEventListener('dragover', e => e.preventDefault());\
    b.addEventListener('drop', e => e.preventDefault());</script>";

#[test]
fn a_drag_fires_the_dnd_events_with_the_store_in_each_mode() {
    let mut page = page_with(PAGE);
    let none = Modifiers::default();
    page.pointer_moved(Point::new(50.0, 25.0), none);
    page.pointer_pressed(Point::new(50.0, 25.0), 0, none, 1);
    eval(&mut page, "log = []");
    // (under the threshold: no drag yet)
    page.pointer_moved(Point::new(52.0, 26.0), none);
    assert_eq!(eval(&mut page, "log.join()"), "");
    // (the move's own events first, then `dragstart` at the press and the pointer stream
    // suppressed where the pointer is now)
    page.pointer_moved(Point::new(50.0, 125.0), none);
    assert!(page.dragging());
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "pointerout@a pointerleave@a dragstart@a[||none] pointercancel@a pointerout@b \
         pointerleave@b drag@a[text/plain||none] dragenter@b[text/plain||copy] \
         dragover@b[text/plain||copy]"
    );
    eval(&mut page, "log = []");
    page.pointer_released(Point::new(50.0, 125.0), 0, none, 1);
    assert!(!page.dragging());
    // (the data is readable in `drop` only; no mouseup or click after a drag)
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "drag@a[text/plain||none] drop@b[text/plain|hi|copy] dragend@a[text/plain||copy]"
    );
}

#[test]
fn leaving_an_element_fires_dragleave_with_the_new_target() {
    let mut page = page_with(PAGE);
    let none = Modifiers::default();
    eval(
        &mut page,
        "var rel = []; for (const t of ['dragenter','dragleave'])\
         document.addEventListener(t, e => rel.push(t + '@' + e.target.id + '>' + (e.relatedTarget && e.relatedTarget.id)), true)",
    );
    page.pointer_pressed(Point::new(50.0, 25.0), 0, none, 1);
    page.pointer_moved(Point::new(50.0, 40.0), none);
    page.pointer_moved(Point::new(50.0, 125.0), none);
    assert_eq!(
        eval(&mut page, "rel.join()"),
        "dragenter@a>null,dragenter@b>a,dragleave@a>b"
    );
}

#[test]
fn escape_cancels_a_drag() {
    let mut page = page_with(PAGE);
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(50.0, 25.0), 0, none, 1);
    page.pointer_moved(Point::new(50.0, 125.0), none);
    eval(&mut page, "log = []");
    page.key_down(&KeyInput {
        vk: 0x1B,
        scan: 0x01,
        text: None,
        mods: none,
        repeat: false,
        paste: None,
        paste_html: None,
    });
    assert!(!page.dragging());
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "dragleave@b[text/plain||none] dragend@a[text/plain||none]"
    );
}

#[test]
fn a_canceled_dragstart_leaves_the_press_a_click() {
    let mut page = page_with(PAGE);
    let none = Modifiers::default();
    eval(
        &mut page,
        "a.addEventListener('dragstart', e => e.preventDefault())",
    );
    page.pointer_pressed(Point::new(50.0, 25.0), 0, none, 1);
    eval(&mut page, "log = []");
    page.pointer_moved(Point::new(50.0, 40.0), none);
    assert!(!page.dragging());
    page.pointer_released(Point::new(50.0, 40.0), 0, none, 1);
    assert_eq!(
        eval(&mut page, "log.join(' ')"),
        "dragstart@a[||none] mouseup@a click@a"
    );
}

#[test]
fn a_press_in_a_text_field_of_a_draggable_element_selects_instead() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}</style><div draggable=true id=d>\
         <input id=i value='some text' style='width:200px'><button id=b>button</button></div>\
         <script>var starts = []; d.addEventListener('dragstart', e => starts.push(e.target.id));</script>",
    );
    let none = Modifiers::default();
    let rect = |page: &mut Page, id: &str| -> (f32, f32) {
        let v = eval(
            page,
            &format!(
                "var r = {id}.getBoundingClientRect(); (r.left + 5) + ',' + (r.top + r.height / 2)"
            ),
        );
        let (x, y) = v.split_once(',').unwrap();
        (x.parse().unwrap(), y.parse().unwrap())
    };
    let (x, y) = rect(&mut page, "i");
    page.pointer_pressed(Point::new(x, y), 0, none, 1);
    page.pointer_moved(Point::new(x + 30.0, y), none);
    page.pointer_released(Point::new(x + 30.0, y), 0, none, 1);
    assert_eq!(eval(&mut page, "starts.join()"), "");
    // (a button in it does not stop the drag of the draggable element)
    let (x, y) = rect(&mut page, "b");
    page.pointer_pressed(Point::new(x, y), 0, none, 1);
    page.pointer_moved(Point::new(x + 30.0, y + 30.0), none);
    page.pointer_released(Point::new(x + 30.0, y + 30.0), 0, none, 1);
    assert_eq!(eval(&mut page, "starts.join()"), "d");
}

#[test]
fn draggable_image_buttons_and_moved_press_targets_drag() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}#e{width:40px;height:40px}#inner{width:30px;height:30px}</style>\
         <div id=e draggable=true><div id=inner></div></div><div id=e2></div>\
         <input type=image id=im draggable=true style='width:50px;height:50px;border:1px solid;border-top-width:20px'>\
         <script>var starts = []; document.addEventListener('dragstart', e => { starts.push(e.target.id); e.preventDefault() });\
         e.addEventListener('mousedown', () => e2.appendChild(inner));</script>",
    );
    let none = Modifiers::default();
    // (the `mousedown` listener takes the pressed element out from under the press)
    page.pointer_pressed(Point::new(5.0, 5.0), 0, none, 1);
    page.pointer_moved(Point::new(15.0, 15.0), none);
    page.pointer_released(Point::new(15.0, 15.0), 0, none, 1);
    let v = eval(
        &mut page,
        "var r = im.getBoundingClientRect(); (r.left + 20) + ',' + (r.top + 10)",
    );
    let (x, y) = v.split_once(',').unwrap();
    let (x, y): (f32, f32) = (x.parse().unwrap(), y.parse().unwrap());
    page.pointer_pressed(Point::new(x, y), 0, none, 1);
    page.pointer_moved(Point::new(x, y + 20.0), none);
    page.pointer_released(Point::new(x, y + 20.0), 0, none, 1);
    assert_eq!(eval(&mut page, "starts.join()"), "e,im");
}

#[test]
fn a_link_drag_carries_its_url() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}a{display:block;width:100px;height:50px}\
         #t{width:100px;height:50px;margin-top:50px}</style><a href='/x?y'>link</a><div id=t></div>\
         <script>var got; t.ondragover = e => e.preventDefault();\
         t.ondrop = e => { e.preventDefault(); got = e.dataTransfer.getData('text/uri-list') + ' ' + e.dataTransfer.getData('text') + ' ' + e.dataTransfer.dropEffect };</script>",
    );
    let none = Modifiers::default();
    page.pointer_pressed(Point::new(20.0, 25.0), 0, none, 1);
    page.pointer_moved(Point::new(20.0, 125.0), none);
    page.pointer_released(Point::new(20.0, 125.0), 0, none, 1);
    assert_eq!(
        eval(&mut page, "got"),
        "http://example.test/x?y http://example.test/x?y link"
    );
}

#[test]
fn files_dragged_in_from_outside_are_in_the_drop() {
    let mut page = page_with(
        "<!doctype html><style>body{margin:0}#z{width:200px;height:200px}</style><div id=z></div>\
         <script>var log = []; z.ondragenter = e => log.push('enter:' + e.dataTransfer.types + ':' + e.dataTransfer.files.length);\
         z.ondragover = e => { e.preventDefault(); e.dataTransfer.dropEffect = 'copy' };\
         z.ondrop = e => { e.preventDefault(); const f = e.dataTransfer.files[0];\
           log.push('drop:' + f.name + ':' + f.type + ':' + f.size + ':' + e.dataTransfer.items[0].kind);\
           f.text().then(t => log.push('text:' + t)) };\
         z.ondragleave = e => log.push('leave');</script>",
    );
    let none = Modifiers::default();
    // (the contents come with the drop)
    let data = ExternalDrag {
        files: vec![DroppedFile {
            name: "notes.txt".into(),
            type_: "text/plain".into(),
            bytes: Vec::new(),
            modified: 0,
        }],
        allowed: "copy".into(),
        ..Default::default()
    };
    let mut full = data.clone();
    full.files[0].bytes = b"hello".to_vec();
    // (outside the zone: nothing takes it)
    assert_eq!(
        page.drag_over(Some(&data), Point::new(300.0, 300.0), none),
        DragOperation::None
    );
    assert_eq!(
        page.drag_over(None, Point::new(50.0, 50.0), none),
        DragOperation::Copy
    );
    assert_eq!(
        page.drop_dragged(Some(&full), Point::new(50.0, 50.0), none),
        DragOperation::Copy
    );
    assert!(page.run_until_settled(Duration::from_secs(5)));
    assert_eq!(
        eval(&mut page, "log.join()"),
        "enter:Files:0,drop:notes.txt:text/plain:5:file,text:hello"
    );
    // A drag that leaves the page fires `dragleave` at the target.
    eval(&mut page, "log = []");
    page.drag_over(Some(&data), Point::new(50.0, 50.0), none);
    page.drag_leave();
    assert!(!page.dragging());
    assert_eq!(eval(&mut page, "log.join()"), "enter:Files:0,leave");
}

#[test]
fn a_dnd_events_data_transfer_is_disabled_after_its_dispatch() {
    let mut page = page_with(PAGE);
    let none = Modifiers::default();
    eval(
        &mut page,
        "var kept; b.addEventListener('drop', e => kept = e.dataTransfer)",
    );
    page.pointer_pressed(Point::new(50.0, 25.0), 0, none, 1);
    page.pointer_moved(Point::new(50.0, 125.0), none);
    page.pointer_released(Point::new(50.0, 125.0), 0, none, 1);
    assert_eq!(
        eval(
            &mut page,
            "[kept.types.length, kept.items.length, kept.getData('text'), kept.files.length].join()"
        ),
        "0,0,,0"
    );
}
