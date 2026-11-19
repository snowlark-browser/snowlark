// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The keyboard and the clipboard in editing hosts go through the HTML Editing APIs' commands
//! (Input Events Level 2): typing, Backspace, Delete, Enter and paste fire `beforeinput` with the
//! ranges they change, the clipboard events fire, IME composition edits the host, and Ctrl+Z and
//! Ctrl+Y undo and redo.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::input::Modifiers;
use snowlark_page::keyboard::{KeyInput, KeyOutcome};
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

fn press(page: &mut Page, vk: u16, mods: Modifiers) -> KeyOutcome {
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

const NONE: Modifiers = Modifiers {
    ctrl: false,
    shift: false,
    alt: false,
    meta: false,
};
const CTRL: Modifiers = Modifiers {
    ctrl: true,
    shift: false,
    alt: false,
    meta: false,
};

/// Two hosts with the same markup: `a` edited by keys, `b` by `execCommand`.
const TWO_HOSTS: &str = "<div contenteditable id=a></div><div contenteditable id=b></div>\
    <script>var log = [];\
    a.addEventListener('beforeinput', e => log.push(e.inputType + ':' + \
      e.getTargetRanges().map(r => r.startContainer.nodeName + r.startOffset + '-' + \
      r.endContainer.nodeName + r.endOffset).join()));\
    a.addEventListener('input', e => log.push('input:' + e.inputType));\
    function setUp(html, node, offset) {\
      for (const h of [a, b]) { h.innerHTML = html; }\
      a.focus();\
      getSelection().collapse(node(a), offset);\
    }\
    function onB(f) { b.focus(); getSelection().collapse(window.nodeB, window.offB); f(); }\
    </script>";

/// Typing, Backspace, Delete, Enter and Shift+Enter give what the `insertText`, `delete`,
/// `forwardDelete`, `insertParagraph` and `insertLineBreak` commands give.
#[test]
fn editing_keys_run_the_editing_commands() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_HOSTS);
    // A typed space at the end of a line is the command's no-break space.
    eval(&mut page, "setUp('ab', h => h.firstChild, 2)");
    type_text(&mut page, "c ");
    eval(
        &mut page,
        "b.focus(); getSelection().collapse(b.firstChild, 2);\
         document.execCommand('insertText', false, 'c');\
         document.execCommand('insertText', false, ' ')",
    );
    assert_eq!(eval(&mut page, "a.innerHTML"), "abc&nbsp;");
    assert_eq!(eval(&mut page, "a.innerHTML == b.innerHTML"), "true");
    // Backspace at the start of a list item outdents it, as `delete` does.
    let list = "<ul><li>one</li><li>two</li></ul>";
    eval(
        &mut page,
        &format!("setUp('{list}', h => h.querySelectorAll('li')[1].firstChild, 0)"),
    );
    press(&mut page, 0x08, NONE);
    eval(
        &mut page,
        "b.focus(); getSelection().collapse(b.querySelectorAll('li')[1].firstChild, 0);\
         document.execCommand('delete')",
    );
    assert_eq!(eval(&mut page, "a.innerHTML == b.innerHTML"), "true");
    // Enter in a list item makes a new item.
    eval(
        &mut page,
        &format!("setUp('{list}', h => h.querySelector('li').firstChild, 3)"),
    );
    press(&mut page, 0x0D, NONE);
    assert_eq!(
        eval(&mut page, "a.innerHTML"),
        "<ul><li>one</li><li><br></li><li>two</li></ul>"
    );
    // Delete across a bold boundary.
    eval(&mut page, "setUp('a<b>bc</b>d', h => h.firstChild, 1)");
    press(&mut page, 0x2E, NONE);
    eval(
        &mut page,
        "b.focus(); getSelection().collapse(b.firstChild, 1); document.execCommand('forwardDelete')",
    );
    assert_eq!(eval(&mut page, "a.innerHTML"), "a<b>c</b>d");
    assert_eq!(eval(&mut page, "a.innerHTML == b.innerHTML"), "true");
}

/// Text typed where a deletion removed bold or italic text takes that formatting (links
/// excepted), by key and by `execCommand`.
#[test]
fn typing_after_deleting_formatted_text_keeps_its_formatting() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_HOSTS);
    eval(
        &mut page,
        "setUp('foo<b>bar</b>baz', h => h, 0);\
         getSelection().selectAllChildren(a.querySelector('b'))",
    );
    press(&mut page, 0x08, NONE);
    assert_eq!(eval(&mut page, "a.innerHTML"), "foobaz");
    type_text(&mut page, "xy");
    assert_eq!(eval(&mut page, "a.innerHTML"), "foo<b>xy</b>baz");
    eval(
        &mut page,
        "b.innerHTML = '<div>abc<i>def</i></div>'; b.focus();\
         const t = b.querySelector('i').firstChild; getSelection().setBaseAndExtent(t, 0, t, 3);\
         document.execCommand('delete'); document.execCommand('insertText', false, 'd')",
    );
    assert_eq!(eval(&mut page, "b.innerHTML"), "<div>abc<i>d</i></div>");
    // A link's text deleted and typed over is the link's again (the states and values the
    // deletion recorded; editing/data/multitest.js: "foo<a href=...>[bar]</a>baz", delete,
    // insertText "a" gives "foo<a href=...>a</a>baz").
    eval(
        &mut page,
        "setUp('x<a href=/>link</a>', h => h, 0);\
         getSelection().selectAllChildren(a.querySelector('a'))",
    );
    press(&mut page, 0x08, NONE);
    type_text(&mut page, "y");
    assert_eq!(eval(&mut page, "a.innerHTML"), "x<a href=\"/\">y</a>");
}

/// `beforeinput` carries the ranges each edit changes, the same ones during and after its
/// dispatch; `input` carries none.
#[test]
fn beforeinput_has_the_target_ranges() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_HOSTS);
    eval(&mut page, "setUp('abc', h => h.firstChild, 2)");
    press(&mut page, 0x08, NONE);
    press(&mut page, 0x2E, NONE);
    type_text(&mut page, "x");
    assert_eq!(
        eval(&mut page, "log.join('|')"),
        "deleteContentBackward:#text1-#text2|input:deleteContentBackward|\
         deleteContentForward:#text1-#text2|input:deleteContentForward|\
         insertText:#text1-#text1|input:insertText"
    );
    assert_eq!(eval(&mut page, "a.innerHTML"), "ax");
    // Ctrl+Backspace deletes a word.
    eval(&mut page, "setUp('one two', h => h.firstChild, 7)");
    eval(&mut page, "log = []");
    press(&mut page, 0x08, CTRL);
    assert_eq!(eval(&mut page, "a.textContent"), "one\u{a0}");
    assert_eq!(
        eval(&mut page, "log[0]"),
        "deleteWordBackward:#text4-#text7"
    );
    // Ctrl+Delete deletes a word and the collapsed white space after it.
    eval(
        &mut page,
        "setUp('<p>abc def   </p>', h => h.firstChild.firstChild, 4)",
    );
    eval(&mut page, "log = []");
    press(&mut page, 0x2E, CTRL);
    assert_eq!(
        eval(&mut page, "log[0]"),
        "deleteWordForward:#text4-#text10"
    );
}

/// Ctrl+Z undoes typing (one step until the caret moves) and deletes, putting back the very
/// nodes; Ctrl+Y redoes; `historyUndo` and `historyRedo` fire.
#[test]
fn undo_and_redo_of_typing_and_deleting() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_HOSTS);
    eval(&mut page, "setUp('x<b id=bold>B</b>', h => h, 0)");
    eval(&mut page, "window.bold = a.querySelector('b')");
    type_text(&mut page, "hey");
    assert_eq!(eval(&mut page, "a.innerHTML"), "heyx<b id=\"bold\">B</b>");
    press(&mut page, b'Z' as u16, CTRL);
    assert_eq!(eval(&mut page, "a.innerHTML"), "x<b id=\"bold\">B</b>");
    press(&mut page, b'Y' as u16, CTRL);
    assert_eq!(eval(&mut page, "a.innerHTML"), "heyx<b id=\"bold\">B</b>");
    // Deleting the bold text and undoing it brings back the same element.
    eval(&mut page, "getSelection().selectAllChildren(a)");
    press(&mut page, 0x08, NONE);
    assert_eq!(eval(&mut page, "a.textContent"), "");
    eval(&mut page, "log = []");
    press(&mut page, b'Z' as u16, CTRL);
    assert_eq!(eval(&mut page, "a.innerHTML"), "heyx<b id=\"bold\">B</b>");
    assert_eq!(eval(&mut page, "a.querySelector('b') === bold"), "true");
    assert_eq!(
        eval(&mut page, "log.join('|')"),
        "historyUndo:|input:historyUndo"
    );
    // A canceled historyUndo undoes nothing.
    eval(
        &mut page,
        "a.addEventListener('beforeinput', e => e.preventDefault(), { once: true })",
    );
    press(&mut page, b'Z' as u16, CTRL);
    assert_eq!(eval(&mut page, "a.innerHTML"), "heyx<b id=\"bold\">B</b>");
    // A text control undoes its own edits.
    eval(
        &mut page,
        "document.body.insertAdjacentHTML('beforeend', '<input id=t>'); t.focus()",
    );
    type_text(&mut page, "abc");
    press(&mut page, 0x08, NONE);
    assert_eq!(eval(&mut page, "t.value"), "ab");
    press(&mut page, b'Z' as u16, CTRL);
    assert_eq!(eval(&mut page, "t.value"), "abc");
    press(&mut page, b'Z' as u16, CTRL);
    assert_eq!(eval(&mut page, "t.value"), "");
    press(&mut page, b'Y' as u16, CTRL);
    assert_eq!(eval(&mut page, "t.value"), "abc");
}

/// Ctrl+V fires `paste` with the clipboard's data; unless canceled, `beforeinput`
/// (`insertFromPaste`, with a `dataTransfer`) and the markup goes in sanitized.
#[test]
fn paste_fires_the_clipboard_event_and_sanitizes_markup() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_HOSTS);
    eval(
        &mut page,
        "setUp('<p>ab</p>', h => h.firstChild.firstChild, 1);\
         window.seen = [];\
         a.addEventListener('paste', e => seen.push(e.constructor.name + ':' + e.isTrusted + ':' +\
           e.clipboardData.getData('text/plain')));\
         a.addEventListener('beforeinput', e => seen.push(e.dataTransfer.getData('text/html')))",
    );
    let mut paste = key(b'V' as u16, None, CTRL);
    paste.paste = Some("bold".into());
    paste.paste_html = Some(
        "<html><body><!--StartFragment--><b onclick=\"x()\">bold</b><script>bad()</script>\
         <a href=\"javascript:bad()\">l</a><!--EndFragment--></body></html>"
            .into(),
    );
    page.key_down(&paste);
    assert_eq!(
        eval(&mut page, "a.innerHTML"),
        "<p>a<b>bold</b><a>l</a>b</p>"
    );
    assert_eq!(eval(&mut page, "seen[0]"), "ClipboardEvent:true:bold");
    assert!(eval(&mut page, "seen[1]").contains("<b onclick"));
    // A canceled paste inserts nothing; the event's data cannot be changed.
    eval(
        &mut page,
        "a.addEventListener('paste', e => { e.clipboardData.setData('text/plain', 'no');\
           window.after = e.clipboardData.getData('text/plain'); e.preventDefault() }, { once: true })",
    );
    page.key_down(&paste);
    assert_eq!(
        eval(&mut page, "a.innerHTML"),
        "<p>a<b>bold</b><a>l</a>b</p>"
    );
    assert_eq!(eval(&mut page, "after"), "bold");
    // Plain text: lines after the first are paragraphs.
    eval(
        &mut page,
        "setUp('<p>ab</p>', h => h.firstChild.firstChild, 1)",
    );
    let mut text = key(b'V' as u16, None, CTRL);
    text.paste = Some("x\r\ny".into());
    page.key_down(&text);
    assert_eq!(eval(&mut page, "a.innerHTML"), "<p>ax</p><p>yb</p>");
}

/// Ctrl+C fires `copy`: the selection's text and markup go to the clipboard, or, when a listener
/// sets data and cancels it, that data.
#[test]
fn copy_fires_the_clipboard_event() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, "<p id=p>plain <b>bold</b></p>");
    eval(&mut page, "getSelection().selectAllChildren(p)");
    let out = press(&mut page, b'C' as u16, CTRL);
    assert_eq!(out.copy.as_deref(), Some("plain bold"));
    assert_eq!(out.copy_html.as_deref(), Some("plain <b>bold</b>"));
    eval(
        &mut page,
        "document.addEventListener('copy', e => { e.clipboardData.setData('text/plain', 'mine');\
           e.preventDefault() })",
    );
    let out = press(&mut page, b'C' as u16, CTRL);
    assert_eq!(out.copy.as_deref(), Some("mine"));
    assert_eq!(out.copy_html, None);
}

/// An IME composition in an editing host shows its text in place, then commits it, with the
/// composition events and `insertCompositionText`.
#[test]
fn composition_in_an_editing_host() {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = rendered(&net, TWO_HOSTS);
    eval(
        &mut page,
        "setUp('<p>ab</p>', h => h.firstChild.firstChild, 1); log = [];\
         for (const t of ['compositionstart', 'compositionupdate', 'compositionend'])\
           a.addEventListener(t, e => log.push(t + ':' + e.data))",
    );
    page.composition_start();
    page.composition_update("\u{306b}", 1);
    assert_eq!(eval(&mut page, "a.innerHTML"), "<p>a\u{306b}b</p>");
    page.composition_update("\u{65e5}\u{672c}", 2);
    assert_eq!(eval(&mut page, "a.innerHTML"), "<p>a\u{65e5}\u{672c}b</p>");
    page.composition_commit("\u{65e5}\u{672c}");
    assert_eq!(eval(&mut page, "a.innerHTML"), "<p>a\u{65e5}\u{672c}b</p>");
    type_text(&mut page, "c");
    assert_eq!(eval(&mut page, "a.innerHTML"), "<p>a\u{65e5}\u{672c}cb</p>");
    let log = eval(&mut page, "log.join('|')");
    assert!(log.starts_with("compositionstart:|"), "{log}");
    assert!(log.contains("compositionend:\u{65e5}\u{672c}"), "{log}");
    assert!(log.contains("input:insertCompositionText"), "{log}");
}
