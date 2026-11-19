// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Form controls' state without submission (HTML §4.10): the input element's value modes across
//! `type` changes, and the text control selection APIs' initial state.

use std::time::Duration;

use snowlark_page::{Page, PageOptions};

fn page() -> Page {
    let mut page = Page::new(PageOptions::default());
    page.load_html(
        "<!doctype html><body>",
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

/// HTML §4.10.5 "When an input element's type attribute changes state": the value follows the
/// value modes and is sanitized for the new state.
#[test]
fn a_type_change_moves_and_sanitizes_the_value() {
    let mut page = page();
    let cases = [
        // (from, to, the value after the change)
        ("text", "hidden", "  foo  "),
        ("text", "checkbox", "  foo  "),
        ("text", "url", "foo"),
        ("text", "number", ""),
        ("text", "range", "50"),
        ("text", "color", "#000000"),
    ];
    for (from, to, want) in cases {
        let js = format!(
            "(() => {{ const i = document.createElement('input'); i.type = '{from}'; \
             i.value = '  foo  '; i.type = '{to}'; return i.value; }})()"
        );
        assert_eq!(eval(&mut page, &js), want, "{from} -> {to}");
    }
    // Back to the value mode: the value content attribute's, not dirty (it follows the attribute).
    let js = "(() => { const i = document.createElement('input'); i.type = 'button'; \
              i.value = 'b'; i.type = 'text'; const a = i.value; i.defaultValue = 'c'; \
              return a + ',' + i.value; })()";
    assert_eq!(eval(&mut page, js), "b,c");
}

/// HTML §4.10.21.1 the validity states: type, pattern (a JavaScript regular expression with the
/// "v" flag, anchored), range and step mismatches, whatever the control's mutability; the
/// constraint checks only for controls that are candidates for validation.
#[test]
fn constraint_validation_checks_type_pattern_range_and_step() {
    let mut page = page();
    let flags = |page: &mut Page, html: &str| {
        let js = format!(
            "(() => {{ const d = document.createElement('div'); d.innerHTML = '{html}'; \
             const v = d.firstChild.validity; \
             return ['typeMismatch','patternMismatch','rangeUnderflow','rangeOverflow', \
             'stepMismatch','valueMissing','valid'].filter(k => v[k]).join(); }})()"
        );
        eval(page, &js)
    };
    assert_eq!(
        flags(&mut page, "<input type=email value=nope>"),
        "typeMismatch"
    );
    assert_eq!(flags(&mut page, "<input type=email value=a@b.c>"), "valid");
    assert_eq!(
        flags(&mut page, "<input type=email multiple value=\"a@b.c, d\">"),
        "typeMismatch"
    );
    assert_eq!(
        flags(&mut page, "<input type=url value=nope>"),
        "typeMismatch"
    );
    assert_eq!(
        flags(&mut page, "<input pattern=[a-z]+ value=abc1>"),
        "patternMismatch"
    );
    // (anchored: a matching substring is not enough; disabled controls still have the flags)
    assert_eq!(
        flags(&mut page, "<input disabled pattern=a value=aa>"),
        "patternMismatch"
    );
    // (a pattern that does not compile is no constraint)
    assert_eq!(flags(&mut page, "<input pattern=( value=x>"), "valid");
    assert_eq!(
        flags(&mut page, "<input type=number min=5 value=4>"),
        "rangeUnderflow"
    );
    assert_eq!(
        flags(&mut page, "<input type=number max=5 value=6>"),
        "rangeOverflow"
    );
    assert_eq!(
        flags(&mut page, "<input type=number min=1 step=2 value=4>"),
        "stepMismatch"
    );
    assert_eq!(
        flags(&mut page, "<input type=number step=0.1 value=0.3>"),
        "valid"
    );
    // valueMissing: a required checkbox counts even when disabled, a text field only while mutable.
    assert_eq!(
        flags(&mut page, "<input type=checkbox required disabled>"),
        "valueMissing"
    );
    assert_eq!(flags(&mut page, "<input required readonly>"), "valid");
    // The constraint check skips controls barred from validation.
    let js = "(() => { const i = document.createElement('input'); i.type = 'email'; \
              i.value = 'x'; const a = i.checkValidity(); i.disabled = true; \
              return [a, i.checkValidity()].join(); })()";
    assert_eq!(eval(&mut page, js), "false,true");
}

/// HTML §4.10.5.1.7-13 and §4.10.5.4: the date, month, week, time, datetime-local, number and
/// range types as numbers (`valueAsNumber`, `valueAsDate`, `stepUp`/`stepDown`, range and step
/// constraints), and a range control's value kept in its range and on its step.
#[test]
fn numeric_input_types_convert_step_and_constrain() {
    let mut page = page();
    let run = |page: &mut Page, js: &str| eval(page, &format!("(() => {{ {js} }})()"));
    let make = "const i = document.createElement('input');";
    assert_eq!(
        run(
            &mut page,
            &format!("{make} i.type = 'date'; i.value = '2019-12-10'; return i.valueAsNumber;")
        ),
        "1575936000000"
    );
    assert_eq!(
        run(
            &mut page,
            &format!("{make} i.type = 'month'; i.valueAsNumber = 599; return i.value;")
        ),
        "2019-12"
    );
    assert_eq!(
        run(
            &mut page,
            &format!(
                "{make} i.type = 'week'; i.value = '2019-W50'; return i.valueAsDate.toISOString();"
            )
        ),
        "2019-12-09T00:00:00.000Z"
    );
    assert_eq!(
        run(
            &mut page,
            &format!("{make} i.type = 'time'; i.valueAsNumber = 3723004; return i.value;")
        ),
        "01:02:03.004"
    );
    assert_eq!(
        run(&mut page, &format!("{make} i.type = 'datetime-local'; i.value = '2019-12-10 10:00:00'; return i.value;")),
        "2019-12-10T10:00"
    );
    // (2020 has 53 weeks, 2019 52)
    assert_eq!(
        run(&mut page, &format!("{make} i.type = 'week'; i.value = '2019-W53'; const a = i.value; i.value = '2020-W53'; return a + ',' + i.value;")),
        ",2020-W53"
    );
    // Stepping: n steps on a step, to the next step off one, into the range from nothing.
    assert_eq!(
        run(
            &mut page,
            &format!(
                "{make} i.type = 'date'; i.value = '2019-12-31'; i.stepUp(2); return i.value;"
            )
        ),
        "2020-01-02"
    );
    assert_eq!(
        run(&mut page, &format!("{make} i.type = 'number'; i.step = '0.1'; i.value = '0.1'; i.stepUp(); i.stepUp(); return i.value;")),
        "0.3"
    );
    assert_eq!(
        run(
            &mut page,
            &format!("{make} i.type = 'number'; i.min = '7'; i.stepDown(); return i.value;")
        ),
        "7"
    );
    assert_eq!(
        run(&mut page, &format!("{make} i.type = 'time'; i.max = '10:00'; i.value = '11:00'; return i.validity.rangeOverflow;")),
        "true"
    );
    // A range control: the middle by default, clamped, on its step (up on a tie).
    let range = "<input type=range min=0 max=7 step=2>";
    assert_eq!(
        run(&mut page, &format!("const d = document.createElement('div'); d.innerHTML = '{range}'; return d.firstChild.value;")),
        "4"
    );
    assert_eq!(
        run(
            &mut page,
            &format!("{make} i.type = 'range'; i.min = 0; i.max = 5; i.value = 7; return i.value;")
        ),
        "5"
    );
}

/// HTML §4.10.20 "set the selection range": a change of the selection queues a `select` event
/// (bubbling, not synchronous); the same selection again fires nothing; a clone's selection starts
/// afresh.
#[test]
fn a_selection_change_queues_a_select_event() {
    let mut page = page();
    let js = "window.n = 0; window.c = 0; const i = document.createElement('textarea'); \
              i.value = 'abc'; i.onselect = e => { if (e.bubbles && !e.cancelable) n++; }; \
              i.select(); window.sync = n; i.select(); i.selectionDirection = 'none'; \
              const k = i.cloneNode(true); k.onselect = () => c++; \
              window.cloneEnd = k.selectionEnd; k.select(); 0";
    eval(&mut page, js);
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    assert_eq!(eval(&mut page, "[sync, n, cloneEnd, c].join()"), "0,1,0,1");
    eval(
        &mut page,
        "const t = document.createElement('input'); t.value = 'abcd'; \
                     t.onselect = () => n++; t.setSelectionRange(1, 2); t.selectionStart = 1; \
                     t.setRangeText('x', 0, 0); 0",
    );
    assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
    assert_eq!(eval(&mut page, "n"), "3");
}

/// HTML §4.10.18.7.1 the autofill processing model: the `autocomplete` IDL attribute is the
/// IDL-exposed autofill value (normalized tokens in order; "" for anything the model rejects).
#[test]
fn autocomplete_is_the_idl_exposed_autofill_value() {
    let mut page = page();
    let cases = [
        ("input", " NAME\t", "name"),
        ("input", "foobar", ""),
        ("input", "foo off", ""),
        ("textarea", " HOME\ntel", "home tel"),
        (
            "textarea",
            "  section-FOO shipping  bday",
            "section-foo shipping bday",
        ),
        ("textarea", "\tusername webauthn", "username webauthn"),
        ("select", "billing work email", "billing work email"),
        ("select", "foo section-bar billing work tel", ""),
    ];
    for (tag, value, want) in cases {
        let js = format!(
            "(() => {{ const e = document.createElement('{tag}'); \
             e.setAttribute('autocomplete', {value:?}); return e.autocomplete; }})()"
        );
        assert_eq!(eval(&mut page, &js), want, "{tag} {value:?}");
    }
    let js = "(() => { const e = document.createElement('input'); e.type = 'hidden'; \
              e.setAttribute('autocomplete', 'on'); return '[' + e.autocomplete + ']'; })()";
    assert_eq!(eval(&mut page, js), "[]");
}

/// HTML §4.10.5.4 and §4.10.11: setting a text control's value to the value it has (as a controlled
/// input does on every keystroke) leaves the cursor where it is; a new value moves it to the end.
#[test]
fn setting_the_same_value_keeps_the_cursor() {
    let mut page = page();
    for tag in ["input", "textarea"] {
        let js = format!(
            "(() => {{ const e = document.createElement('{tag}'); e.value = 'abcdef'; \
             e.setSelectionRange(2, 3); e.value = 'abcdef'; const a = [e.selectionStart, e.selectionEnd]; \
             e.value = 'abcdefg'; return a.concat([e.selectionStart, e.selectionEnd]).join(); }})()"
        );
        assert_eq!(eval(&mut page, &js), "2,3,7,7", "{tag}");
    }
}

/// HTML §4.10.5.1.5: an email field with `multiple` sanitizes its value as a comma-separated list
/// (each address stripped of whitespace), also when `multiple` is set after the value.
#[test]
fn a_multiple_email_field_sanitizes_its_list() {
    let mut page = page();
    let js = "(() => { const i = document.createElement('input'); i.type = 'email'; \
              i.multiple = true; i.value = ' a@b.c  , d@e.f '; const a = i.value; \
              const j = document.createElement('input'); j.type = 'email'; j.value = 'a@b.c , d@e.f'; \
              j.multiple = true; return [a, j.value].join('|'); })()";
    assert_eq!(eval(&mut page, js), "a@b.c,d@e.f|a@b.c,d@e.f");
}

/// HTML §4.10.10 `option.text`: the descendant text without HTML and SVG `script` elements' (a
/// MathML or null-namespace element named "script" counts), whitespace stripped and collapsed.
#[test]
fn option_text_skips_only_html_and_svg_scripts() {
    let mut page = page();
    let js = "(() => { const o = document.createElement('option'); o.append('a '); \
              const s = document.createElement('script'); s.text = 'x'; o.append(s); \
              const m = document.createElementNS('http://www.w3.org/1998/Math/MathML', 'script'); \
              m.textContent = 'm'; o.append(m); \
              const n = document.createElementNS(null, 'script'); n.textContent = ' n'; o.append(n); \
              return o.text; })()";
    assert_eq!(eval(&mut page, js), "a m n");
}

/// HTML §4.10.7: `selectedIndex = -1`, or a `value` no option has, leaves a select with nothing
/// selected until the selectedness setting algorithm runs again (an option removed or inserted).
#[test]
fn a_select_emptied_by_script_stays_empty_until_its_options_change() {
    let mut page = page();
    let js = "(() => { const s = document.createElement('select'); \
              s.append(new Option('a', 'a'), new Option('b', 'b'), new Option('c', 'c')); \
              const out = []; s.selectedIndex = -1; out.push(s.selectedIndex, s.value); \
              s.value = 'b'; out.push(s.selectedIndex); s.value = 'zzz'; out.push(s.selectedIndex); \
              s.options[2].remove(); out.push(s.selectedIndex); return out.join(); })()";
    assert_eq!(eval(&mut page, js), "-1,,1,-1,0");
}

/// HTML §4.10.7: a required select's placeholder label option is the first option only as a child
/// of the select itself, and only with a display size of 1.
#[test]
fn a_selects_placeholder_label_option() {
    let mut page = page();
    let missing = |page: &mut Page, html: &str| {
        let js = format!(
            "(() => {{ const d = document.createElement('div'); d.innerHTML = '{html}'; \
             return d.firstChild.validity.valueMissing; }})()"
        );
        eval(page, &js)
    };
    assert_eq!(
        missing(
            &mut page,
            "<select required><option value=\"\">-</option><option>a</option></select>"
        ),
        "true"
    );
    assert_eq!(
        missing(&mut page, "<select required><optgroup><option value=\"\">-</option></optgroup><option>a</option></select>"),
        "false"
    );
    assert_eq!(
        missing(&mut page, "<select required size=2><option value=\"\" selected>-</option><option>a</option></select>"),
        "false"
    );
}

/// HTML §4.10.7 `selectedOptions`: a live HTMLCollection that follows the options' selectedness,
/// whether script changes it through `selected`, `selectedIndex` or the `selected` attribute.
#[test]
fn selected_options_is_live() {
    let mut page = page();
    let js = "(() => { const s = document.createElement('select'); s.multiple = true; \
              const a = new Option('a'), b = new Option('b'); s.append(a, b); \
              const c = s.selectedOptions; const out = [c instanceof HTMLCollection, c.length]; \
              a.selected = true; b.setAttribute('selected', ''); out.push(c.length); \
              s.selectedIndex = 1; out.push(c.length, c[0] === b); return out.join(); })()";
    assert_eq!(eval(&mut page, js), "true,0,2,1,true");
}

/// HTML §4.10.18.3 "reset the form owner": the `form` attribute applies to connected controls; a
/// disconnected one is owned by its nearest ancestor form all the same.
#[test]
fn a_disconnected_control_with_a_form_attribute_has_its_ancestor_form() {
    let mut page = page();
    let js = "(() => { const f = document.createElement('form'); f.id = 'f1'; \
              const i = document.createElement('input'); i.setAttribute('form', 'elsewhere'); \
              f.append(i); const a = i.form === f; document.body.append(f); \
              return [a, i.form === null].join(); })()";
    assert_eq!(eval(&mut page, js), "true,true");
}

/// HTML §4.10.10: `new Option()` sets an option's selectedness, not its dirtiness, so its `selected`
/// attribute still moves the select's selection.
#[test]
fn a_constructed_option_follows_its_selected_attribute() {
    let mut page = page();
    let js = "(() => { const s = document.createElement('select'); \
              const a = new Option('a'), b = new Option('b'); s.append(a, b); \
              const out = [s.selectedIndex]; b.setAttribute('selected', ''); out.push(s.selectedIndex); \
              b.removeAttribute('selected'); out.push(s.selectedIndex); return out.join(); })()";
    assert_eq!(eval(&mut page, js), "0,1,0");
}

/// HTML §4.10.20: a text control's selection direction starts as "none", and a control that
/// becomes selectable through a type change has its cursor at the start.
#[test]
fn a_control_that_becomes_selectable_starts_at_the_beginning() {
    let mut page = page();
    let js = "(() => { const i = document.createElement('input'); const d = i.selectionDirection; \
              i.type = 'hidden'; i.value = 'abc'; i.type = 'text'; \
              return [d, i.selectionStart, i.selectionEnd, i.selectionDirection].join(); })()";
    assert_eq!(eval(&mut page, js), "none,0,0,none");
    let js = "document.createElement('textarea').selectionDirection";
    assert_eq!(eval(&mut page, js), "none");
}

/// Types `text` into the focused control as the user does (one key per character).
fn type_text(page: &mut Page, text: &str) {
    use snowlark_page::keyboard::KeyInput;
    for c in text.chars() {
        let k = KeyInput {
            vk: c.to_ascii_uppercase() as u16,
            scan: 0,
            text: Some(c),
            mods: Default::default(),
            repeat: false,
            paste: None,
            paste_html: None,
        };
        page.key_down(&k);
        page.key_char(c, &k);
        page.key_up(&k);
    }
}

/// A number field takes from what the user types only the characters that can still make a number:
/// no letters but the exponent, no second decimal point or exponent, signs only at the start or
/// after the exponent.
#[test]
fn a_number_field_takes_only_characters_that_can_make_a_number() {
    use snowlark_page::keyboard::KeyInput;
    let mut page = page();
    eval(
        &mut page,
        "document.body.innerHTML = '<input id=n type=number>'; n.focus(); 'ok'",
    );
    type_text(&mut page, "1a.2.3x");
    assert_eq!(eval(&mut page, "n.value"), "1.23");
    let left = |page: &mut Page, times: usize| {
        let k = KeyInput {
            vk: 0x25,
            scan: 0,
            text: None,
            mods: Default::default(),
            repeat: false,
            paste: None,
            paste_html: None,
        };
        for _ in 0..times {
            page.key_down(&k);
            page.key_up(&k);
        }
    };
    for (moves, key, expected) in [
        (2, "-", "10"),
        (2, "+", "10"),
        (4, "+", "10"),
        (4, "-", "-10"),
    ] {
        eval(&mut page, "n.value = ''; 'ok'");
        type_text(&mut page, "1e+1");
        left(&mut page, moves);
        type_text(&mut page, key);
        assert_eq!(
            eval(&mut page, "n.valueAsNumber + '|' + n.validity.valid"),
            format!("{expected}|true"),
            "{key} after {moves} moves left"
        );
    }
}

/// HTML §4.10.5.3: `maxlength` applies to text, search, url, tel, email and password inputs (and
/// textarea), so it does not limit what the user types into a number field.
#[test]
fn maxlength_does_not_limit_a_number_field() {
    let mut page = page();
    eval(
        &mut page,
        "document.body.innerHTML = '<input id=n type=number maxlength=1><input id=t maxlength=1>';
         n.focus(); 'ok'",
    );
    type_text(&mut page, "1234");
    eval(&mut page, "t.focus(); 'ok'");
    type_text(&mut page, "1234");
    assert_eq!(eval(&mut page, "n.value + '|' + t.value"), "1234|1");
}

/// HTML §4.10.5.1.12: the user's input in a number field becomes its value as the number it
/// converts to; input that does not convert leaves the value empty and the field suffering from
/// bad input, until the value is set.
#[test]
fn a_number_fields_user_input_is_converted_or_bad_input() {
    let mut page = page();
    eval(
        &mut page,
        "document.body.innerHTML = '<input id=n type=number step=any><input id=r type=number required>';
         n.focus(); 'ok'",
    );
    let state = "n.value + '|' + n.validity.badInput + '|' + n.validity.valid";
    type_text(&mut page, "1.e");
    assert_eq!(eval(&mut page, state), "|true|false");
    assert_eq!(
        eval(&mut page, "n.validationMessage"),
        "Please enter a number."
    );
    eval(&mut page, "n.value = '1.e'; 'ok'");
    assert_eq!(eval(&mut page, state), "|false|true");
    type_text(&mut page, "1.");
    assert_eq!(eval(&mut page, state), "1|false|true");
    type_text(&mut page, "5");
    assert_eq!(eval(&mut page, state), "1.5|false|true");
    eval(&mut page, "r.focus(); 'ok'");
    type_text(&mut page, "-");
    assert_eq!(
        eval(
            &mut page,
            "r.value + '|' + r.validity.badInput + '|' + r.validity.valueMissing"
        ),
        "|true|true"
    );
}

/// HTML "the constraint validation API": an `object` is barred from constraint validation, yet
/// `setCustomValidity()` sets its custom validity error message, which `validity.customError`
/// reports; it stays valid for `checkValidity()` and has no validation message.
#[test]
fn an_objects_custom_validity_is_reported() {
    let mut page = page();
    let js = "(() => { const o = document.createElement('object'); const before = \
              o.validity.customError; o.setCustomValidity('custom error'); return [before, \
              o.validity.customError, o.validity.valid, o.willValidate, o.checkValidity(), \
              o.validationMessage === ''].join(); })()";
    assert_eq!(eval(&mut page, js), "false,true,false,false,true,true");
}
