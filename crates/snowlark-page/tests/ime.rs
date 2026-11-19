// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! IME composition in a text field: the events in their order, the value while composing and after a
//! commit or a cancel, and the composition shown underlined. With `SNOWLARK_SHOT_DIR` set, the
//! composing field is also rasterized to `ime.png` there (the window's IME cannot be driven without an
//! IME installed: this is the same display list the window shows).

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::KeyInput;
use snowlark_page::{Page, PageOptions, RenderSetup};
use snowlark_paint::{Item, Point};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

#[test]
fn compositions_fire_events_and_edit_the_focused_field() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (400, 120),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        "<!doctype html><body style='margin:10px'><input id=i value='ab' \
         style='font:24px sans-serif;width:300px'>\
         <script>window.log = []; const i = document.getElementById('i'); \
         for (const t of ['compositionstart', 'compositionupdate', 'compositionend']) \
           i.addEventListener(t, e => log.push(t + ':' + e.data)); \
         for (const t of ['beforeinput', 'input']) \
           i.addEventListener(t, e => log.push(t + ':' + e.inputType + ':' + e.data + ':' + e.isComposing)); \
         i.addEventListener('keydown', e => log.push('keydown:' + e.key + ':' + e.isComposing));</script>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    page.run_until_settled(Duration::from_secs(5));
    page.pointer_pressed(Point::new(200.0, 25.0), 0, Modifiers::default(), 1);
    page.pointer_released(Point::new(200.0, 25.0), 0, Modifiers::default(), 1);
    eval(&mut page, "i.setSelectionRange(1, 2); log.length = 0");
    // The IME takes a key: keydown "Process", nothing typed.
    page.key_down(&KeyInput {
        vk: 0xE5,
        scan: 0x1E,
        text: None,
        mods: Modifiers::default(),
        repeat: false,
        paste: None,
        paste_html: None,
    });
    page.composition_start();
    page.composition_update("\u{306b}", 1);
    page.composition_update("\u{65e5}\u{672c}", 1);
    assert_eq!(eval(&mut page, "i.value"), "a\u{65e5}\u{672c}");
    assert_eq!(
        eval(&mut page, "[i.selectionStart, i.selectionEnd].join()"),
        "2,2"
    );
    if let Ok(dir) = std::env::var("SNOWLARK_SHOT_DIR") {
        page.update_the_rendering();
        let list = page.engine().paint(Point::ZERO, 2.0).unwrap();
        let (text, images) = page
            .engine()
            .with_renderer(|r, _| (r.text().clone(), r.images().clone()))
            .unwrap();
        let mut raster = snowlark_paint::HeadlessRenderer::new().unwrap();
        let pixmap = raster
            .render(
                &list,
                snowlark_paint::Sources {
                    text: &text,
                    images: &images,
                },
                &snowlark_paint::Viewport {
                    width: 800,
                    height: 240,
                    scale: 2.0,
                    background: snowlark_paint::Color::WHITE,
                },
            )
            .unwrap();
        pixmap
            .write_png(&std::path::Path::new(&dir).join("ime.png"))
            .unwrap();
    }
    // The composition is underlined: a thin fill under the composed text.
    page.update_the_rendering();
    let list = page.engine().paint(Point::ZERO, 1.0).unwrap();
    let underline = list.items.iter().any(|i| {
        matches!(i, Item::FillRect { rect, .. } if rect.height <= 2.5 && rect.width > 10.0 && rect.y > 20.0)
    });
    assert!(underline, "an underline under the composition");
    page.composition_commit("\u{65e5}\u{672c}");
    assert_eq!(eval(&mut page, "i.value"), "a\u{65e5}\u{672c}");
    assert_eq!(
        eval(&mut page, "log.join('|')"),
        "keydown:Process:false|compositionstart:b|\
         beforeinput:insertCompositionText:\u{306b}:true|compositionupdate:\u{306b}|\
         input:insertCompositionText:\u{306b}:true|\
         beforeinput:insertCompositionText:\u{65e5}\u{672c}:true|compositionupdate:\u{65e5}\u{672c}|\
         input:insertCompositionText:\u{65e5}\u{672c}:true|\
         beforeinput:insertCompositionText:\u{65e5}\u{672c}:true|compositionupdate:\u{65e5}\u{672c}|\
         input:insertCompositionText:\u{65e5}\u{672c}:true|compositionend:\u{65e5}\u{672c}"
    );
    // A canceled composition leaves the value as it was.
    eval(&mut page, "log.length = 0");
    page.composition_start();
    page.composition_update("x", 1);
    assert_eq!(eval(&mut page, "i.value"), "a\u{65e5}\u{672c}x");
    page.composition_end();
    assert_eq!(eval(&mut page, "i.value"), "a\u{65e5}\u{672c}");
    assert!(eval(&mut page, "log[log.length - 1]") == "compositionend:");
}
