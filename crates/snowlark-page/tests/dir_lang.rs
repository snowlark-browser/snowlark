// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `:dir()` and `:lang()` in the DOM's own selector matching (Selectors 4 §8): the element's
//! directionality (HTML §3.2.6.4, text fields by the value the page holds) and language ranges
//! by extended filtering.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

/// HTML §4.2.5.3 the content language state: a `meta http-equiv=content-language` sets the
/// pragma-set default language (its first token; none when its content has a comma), the language
/// of nodes without `lang`, which stays when the `meta` is removed; a `meta` in a shadow tree sets
/// nothing.
#[test]
fn a_content_language_pragma_sets_the_default_language() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><meta http-equiv=Content-Language content=' de-DE x'>\
         <p id=p lang=en></p>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    let got = eval(
        &mut page,
        "document.querySelectorAll('meta').forEach(m => m.remove()); \
         [document.documentElement.matches(':lang(de-DE)'), p.matches(':lang(de)'), \
         document.body.matches(':lang(de)')].join()",
    );
    assert_eq!(got, "true,false,true", "kept after its removal");
    for (content, root) in [("de-DE ,nl-NL", "false"), ("  ", "false")] {
        page.load_html(
            &format!(
                "<!doctype html><meta http-equiv=content-language content='{content}'>\
                 <div id=h></div><script>h.attachShadow({{mode: 'open'}}).innerHTML = \
                 \"<meta http-equiv=content-language content=de>\";</script>"
            ),
            &url::Url::parse("http://example.test/").unwrap(),
        );
        assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
        assert_eq!(
            eval(&mut page, "document.documentElement.matches(':lang(de)')"),
            root,
            "{content}"
        );
    }
}

#[test]
fn dir_and_lang_match_directionality_and_language_ranges() {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body><div dir=rtl><input id=t type=tel><input id=x dir=auto>\
         <bdi id=b>\u{5EA}</bdi><span id=s></span></div>\
         <p lang=fr-x-standard id=f><i id=i></i></p>",
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    let got = eval(
        &mut page,
        "[t.matches(':dir(ltr)'), x.matches(':dir(ltr)'), s.matches(':dir(rtl)'), \
         (x.value = '\\u05EA', x.matches(':dir(rtl)')), b.matches(':dir(rtl)'), \
         document.querySelectorAll(':dir(foo)').length, \
         i.matches(':lang(fr-x)'), i.matches(':lang(fr-standard)'), i.matches(':lang(\"*-x\")'), \
         document.querySelector('p:lang(fr, de)') === f].join()",
    );
    assert_eq!(got, "true,true,true,true,true,0,true,false,true,true");
}
