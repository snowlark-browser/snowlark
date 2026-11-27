// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The form pseudo-classes (HTML §4.16.3, Selectors 4 §11): `:valid`/`:invalid` on controls,
//! forms and fieldsets, `:in-range`/`:out-of-range`, `:user-valid`/`:user-invalid`, as style and
//! `matches()` see them after each change.

use std::time::Duration;

use snowlark_net::{NetConfig, NetThread};
use snowlark_page::{Page, PageOptions, RenderSetup};

fn rendered(html: &str) -> Page {
    let net = NetThread::start(NetConfig::default()).unwrap();
    let mut page = Page::new(PageOptions {
        net: Some(net.client()),
        viewport: (800, 600),
        render: Some(RenderSetup::new().unwrap()),
        ..Default::default()
    });
    page.load_html(
        &format!("<!doctype html>{html}"),
        &url::Url::parse("http://example.test/").unwrap(),
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    page
}

fn eval(page: &mut Page, js: &str) -> String {
    match page.eval(js, "test.js") {
        Ok(v) => v,
        Err(e) => panic!("{js}: threw {e}"),
    }
}

const STYLE: &str = "<style>\
    #s1 { color: black } #f:has(:invalid) #s1 { color: red }\
    #s2 { color: black } #f:valid #s2 { color: green }\
    #s3 { color: black } #fs:invalid ~ #s3 { color: red }\
    #s4 { color: black } div:has(> #n:in-range) #s4 { color: green }\
    #s5 { color: black } div:has(> #n:out-of-range) #s5 { color: blue }\
    </style>";

fn colors(page: &mut Page) -> String {
    eval(
        page,
        "['s1','s2','s3','s4','s5'].map(id => getComputedStyle(document.getElementById(id)).color \
         .replace('rgb(0, 0, 0)', 'black').replace('rgb(255, 0, 0)', 'red') \
         .replace('rgb(0, 128, 0)', 'green').replace('rgb(0, 0, 255)', 'blue')).join(' ')",
    )
}

/// A select's options' `:checked` comes from one run of the selectedness setting algorithm (HTML
/// §4.10.7) for all of them: 40,000 blank options made by the `length` setter (HTML
/// `HTMLOptionsCollection`, up to 100,000) are styled in well under a second, not once per option
/// over every option (about 12 s before).
#[test]
fn a_select_of_many_options_is_styled_in_linear_time() {
    let mut page = rendered(
        "<style>option:checked { color: green }</style><select id=s><option>a<option>b</select>",
    );
    let started = std::time::Instant::now();
    assert_eq!(
        eval(
            &mut page,
            "s.options.length = 40000; s.options[39999].selected = true; document.body.offsetTop;\
             [s.options.length, getComputedStyle(s.options[39999]).color,\
              getComputedStyle(s.options[0]).color].join()"
        ),
        "40000,rgb(0, 128, 0),rgb(0, 0, 0)"
    );
    let took = started.elapsed();
    assert!(took < Duration::from_secs(3), "{took:?}");
}

/// A required field's validity, its form's and fieldset's, and a number field's range follow
/// value, attribute and tree changes, for style (through `:has()`) and for `matches()`.
#[test]
fn validity_pseudo_classes_follow_values_attributes_and_the_tree() {
    let mut page = rendered(&format!(
        "{STYLE}<form id=f><fieldset id=fs><input id=t required></fieldset>\
         <div><input id=n type=number min=1 max=10 value=5><p id=s4></p><p id=s5></p></div>\
         <p id=s1></p><p id=s2></p><p id=s3></p></form>"
    ));
    assert_eq!(colors(&mut page), "red black red green black", "initially");
    assert_eq!(
        eval(
            &mut page,
            "[t.matches(':invalid'), f.matches(':invalid'), fs.matches(':invalid'), \
             n.matches(':in-range'), n.matches(':valid')].join()"
        ),
        "true,true,true,true,true"
    );
    eval(&mut page, "t.value = 'x'; n.value = '12'");
    // (the number field overflows: the form has an invalid control, the fieldset none)
    assert_eq!(
        colors(&mut page),
        "red black black black blue",
        "after values"
    );
    assert_eq!(
        eval(
            &mut page,
            "[t.matches(':valid'), n.matches(':out-of-range'), n.matches(':invalid')].join()"
        ),
        "true,true,true"
    );
    // (a barred control is neither valid nor invalid, nor in or out of range)
    eval(&mut page, "n.readOnly = true; t.value = ''");
    assert_eq!(
        colors(&mut page),
        "red black red black black",
        "readonly, empty"
    );
    eval(&mut page, "t.remove()");
    assert_eq!(
        colors(&mut page),
        "black green black black black",
        "removed"
    );
    eval(
        &mut page,
        "const p = document.createElement('input'); p.pattern = '[a-z]+'; p.value = 'A1'; \
         fs.append(p)",
    );
    assert_eq!(
        colors(&mut page),
        "red black red black black",
        "pattern mismatch"
    );
    eval(&mut page, "fs.disabled = true");
    assert_eq!(
        colors(&mut page),
        "black green black black black",
        "disabled fieldset"
    );
    assert_eq!(eval(&mut page, "fs.lastChild.matches(':disabled')"), "true");
}

/// `:user-invalid` waits for the user (a committed change, a submission) and a reset clears it.
#[test]
fn user_validity_starts_with_a_submission_and_ends_with_a_reset() {
    let mut page = rendered(
        "<form id=f onsubmit='return false'><input id=t required><input type=reset id=r></form>",
    );
    let state = |page: &mut Page| {
        eval(
            page,
            "[t.matches(':user-invalid'), t.matches(':user-valid'), t.matches(':invalid')].join()",
        )
    };
    assert_eq!(state(&mut page), "false,false,true");
    eval(&mut page, "f.requestSubmit()");
    assert_eq!(state(&mut page), "true,false,true", "after a submission");
    eval(&mut page, "t.value = 'x'");
    assert_eq!(state(&mut page), "false,true,false", "valid");
    eval(&mut page, "r.click()");
    assert_eq!(state(&mut page), "false,false,true", "after a reset");
}

/// The state is computed when style, layout or a selector that reads it needs it, not at each DOM
/// operation: a list of controls rebuilt many times by script before anything reads it computes
/// the controls that are still in the document once, and the replaced ones only if a selector match
/// asks for them. A `pattern` (which needs a script context) is still current for a style read
/// right after the insertion.
#[test]
fn form_states_are_computed_when_read_not_at_each_insertion() {
    let mut page = rendered(
        "<style>input { color: black } input:invalid { color: red }</style><ul id=l></ul>",
    );
    let before = page.engine().form_states_computed();
    // (one script: no rendering update in between)
    assert_eq!(
        eval(
            &mut page,
            "for (let i = 0; i < 50; i++) \
               l.innerHTML = '<li><input required><input value=x></li>'.repeat(10); \
             window.first = l.querySelector('input'); \
             l.innerHTML = '<li><input required></li>'.repeat(10); \
             getComputedStyle(l.querySelector('input')).color"
        ),
        "rgb(255, 0, 0)",
        "style sees the inserted control's validity"
    );
    let computed = page.engine().form_states_computed() - before;
    assert!(
        computed <= 20,
        "{computed} control states computed for 10 controls in the document"
    );
    // (a replaced control: computed when a selector reads it)
    assert_eq!(
        eval(
            &mut page,
            "[first.isConnected, first.matches(':invalid')].join()"
        ),
        "false,true"
    );
    assert_eq!(
        eval(
            &mut page,
            "l.innerHTML = '<input pattern=a value=b>'; getComputedStyle(l.firstChild).color"
        ),
        "rgb(255, 0, 0)",
        "a pattern mismatch, for a style read"
    );
}
