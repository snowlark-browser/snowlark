// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The select UI the page asks its embedder for: a drop-down's list (opened by a press or the keyboard,
//! answered with the user's choice), and list box selection by clicks with Ctrl and Shift.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::Point;

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

fn click(page: &mut Page, x: f32, y: f32, mods: Modifiers) {
    page.pointer_pressed(Point::new(x, y), 0, mods, 1);
    page.pointer_released(Point::new(x, y), 0, mods, 1);
}

#[test]
fn drop_down_lists_open_and_report_the_choice() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <select id=s style='position:absolute;left:10px;top:20px;width:100px;height:24px;font-size:16px'>\
         <option>One<option selected>Two<optgroup label=G><option>Three<option disabled>Four</optgroup></select>\
         <script>window.log = []; const s = document.getElementById('s'); \
         s.oninput = () => log.push('input ' + s.value); s.onchange = () => log.push('change ' + s.value);</script>",
    );
    click(&mut page, 30.0, 30.0, Modifiers::default());
    let p = page.engine().take_select_popup().expect("a list to show");
    assert_eq!(p.anchor, snowlark_paint::Rect::new(10.0, 20.0, 100.0, 24.0));
    assert_eq!(p.font_size, 16.0);
    let labels: Vec<(&str, bool, bool)> = p
        .items
        .iter()
        .map(|i| (i.label.as_str(), i.group, i.disabled))
        .collect();
    assert_eq!(
        labels,
        [
            ("One", false, false),
            ("Two", false, false),
            ("G", true, false),
            ("Three", false, false),
            ("Four", false, true)
        ]
    );
    assert_eq!(p.selected, Some(1));
    // Choosing the third option (the group's first).
    page.select_popup_choose(p.select, p.option_of_row(3).unwrap());
    assert_eq!(eval(&mut page, "log.join()"), "input Three,change Three");
    // The same option again changes nothing; a disabled one is refused.
    page.select_popup_choose(p.select, 2);
    page.select_popup_choose(p.select, 3);
    assert_eq!(eval(&mut page, "s.value + ' ' + log.length"), "Three 2");
    // The keyboard opens it too (Alt+Down on the focused select).
    let key = KeyInput {
        vk: 0x28,
        scan: 0x150,
        text: None,
        mods: Modifiers {
            alt: true,
            ..Default::default()
        },
        repeat: false,
        paste: None,
        paste_html: None,
    };
    page.key_down(&key);
    assert!(page.engine().take_select_popup().is_some());
}

#[test]
fn list_boxes_select_by_click_with_ctrl_and_shift() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <select id=m multiple size=6 style='position:absolute;left:0;top:0;width:120px;font-size:16px;\
          line-height:20px;padding:0;border:0'>\
         <option>a<option>b<option>c<option disabled>d<option>e<option>f</select>\
         <script>window.changes = 0; document.getElementById('m').onchange = () => changes++;</script>",
    );
    let sel = |page: &mut Page| {
        eval(
            page,
            "[...document.getElementById('m').options].filter(o => o.selected).map(o => o.text).join('')",
        )
    };
    // Rows are a line apart; the rows' middles are at 10, 30, 50...
    let row = |i: usize| 10.0 + 20.0 * i as f32;
    let none = Modifiers::default();
    click(&mut page, 20.0, row(1), none);
    assert_eq!(sel(&mut page), "b");
    let ctrl = Modifiers {
        ctrl: true,
        ..Default::default()
    };
    click(&mut page, 20.0, row(4), ctrl);
    assert_eq!(sel(&mut page), "be");
    click(&mut page, 20.0, row(1), ctrl);
    assert_eq!(sel(&mut page), "e");
    // Shift: a range from the last clicked row, skipping the disabled option.
    click(&mut page, 20.0, row(5), none);
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    click(&mut page, 20.0, row(2), shift);
    assert_eq!(sel(&mut page), "cef");
    // A disabled option takes no click.
    click(&mut page, 20.0, row(3), none);
    assert_eq!(sel(&mut page), "cef");
    assert_eq!(eval(&mut page, "changes"), "5");
    // Drop-down lists are not offered for list boxes.
    assert!(page.engine().take_select_popup().is_none());
}

#[test]
fn pointer_outcomes_carry_the_advisory_information() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <div title='Outer' style='height:100px'><div style='height:40px'>a</div>\
         <div title='' style='height:40px'>b</div></div><p style='height:40px'>c",
    );
    let title = |page: &mut Page, y: f32| {
        page.pointer_moved(Point::new(10.0, y), Modifiers::default())
            .title
    };
    // Inherited from the nearest ancestor with a title; an empty title means none.
    assert_eq!(title(&mut page, 20.0).as_deref(), Some("Outer"));
    assert_eq!(title(&mut page, 60.0), None);
    assert_eq!(title(&mut page, 120.0), None);
}

#[test]
fn text_fields_take_the_caret_and_selections_from_the_mouse() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(
        &net,
        "<!doctype html><body style='margin:0'>\
         <input id=i value='hello brave new world' style='position:absolute;left:0;top:0;width:400px;\
          height:30px;padding:0;border:0;font:20px monospace'>\
         <input id=l value='0123456789012345678901234567890123456789' style='position:absolute;left:0;\
          top:50px;width:100px;height:30px;padding:0;border:0;font:20px monospace'>",
    );
    let sel = |page: &mut Page, id: &str| {
        eval(
            page,
            &format!(
                "(() => {{ const e = document.getElementById('{id}'); \
                 return [e.selectionStart, e.selectionEnd, e.selectionDirection].join() }})()"
            ),
        )
    };
    let none = Modifiers::default();
    // The field's clusters are as wide as its glyphs' advances.
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let w = list
        .items
        .iter()
        .find_map(|i| match i {
            snowlark_paint::Item::Text(t) => Some(list.glyphs(t.glyphs).1[0]),
            _ => None,
        })
        .unwrap();
    click(&mut page, w * 3.4, 15.0, none);
    assert_eq!(sel(&mut page, "i"), "3,3,none");
    // Shift+press extends.
    let shift = Modifiers {
        shift: true,
        ..Default::default()
    };
    click(&mut page, w * 8.1, 15.0, shift);
    assert_eq!(sel(&mut page, "i"), "3,8,forward");
    // A drag selects backwards.
    page.pointer_pressed(Point::new(w * 10.0, 15.0), 0, none, 1);
    page.pointer_moved(Point::new(w * 2.0, 15.0), none);
    page.pointer_released(Point::new(w * 2.0, 15.0), 0, none, 1);
    assert_eq!(sel(&mut page, "i"), "2,10,backward");
    // A double-click selects a word (with its space), a triple-click the whole value.
    page.pointer_pressed(Point::new(w * 7.5, 15.0), 0, none, 2);
    page.pointer_released(Point::new(w * 7.5, 15.0), 0, none, 2);
    assert_eq!(sel(&mut page, "i"), "6,12,none");
    page.pointer_pressed(Point::new(w * 7.5, 15.0), 0, none, 3);
    page.pointer_released(Point::new(w * 7.5, 15.0), 0, none, 3);
    assert_eq!(sel(&mut page, "i"), "0,21,none");
    // A long value scrolls so that the caret at its end is in view.
    click(&mut page, 50.0, 65.0, none);
    eval(
        &mut page,
        "document.getElementById('l').setSelectionRange(40, 40)",
    );
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    // The text's runs paint left of the field's start (scrolled).
    let leftmost = list
        .items
        .iter()
        .filter_map(|i| match i {
            snowlark_paint::Item::Text(t) if t.origin.y > 50.0 => Some(t.origin.x),
            _ => None,
        })
        .fold(f32::INFINITY, f32::min);
    assert!(leftmost < -100.0, "scrolled text starts at {leftmost}");
    // A press at the field's left edge now lands well into the value.
    click(&mut page, 1.0, 65.0, none);
    let start: u32 = eval(&mut page, "document.getElementById('l').selectionStart")
        .parse()
        .unwrap();
    assert!(start > 20, "caret at {start}");
}
