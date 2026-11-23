// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The password manager's form parser over a table of form shapes found on real sites.

use snowlark_page::passwords::{parse, FieldData, FormData, Mode, ParsedForm};

/// A field: `type`, `name`, `autocomplete`, value. Focusable, typed in when it has a value.
fn f(kind: &str, name: &str, autocomplete: &str, value: &str) -> FieldData {
    FieldData {
        kind: kind.into(),
        name: name.into(),
        autocomplete: autocomplete.into(),
        value: value.into(),
        user_typed: !value.is_empty(),
        focusable: true,
        ..FieldData::default()
    }
}

fn form(fields: Vec<FieldData>) -> FormData {
    let fields = fields
        .into_iter()
        .enumerate()
        .map(|(i, mut x)| {
            x.id = i as u64 + 1;
            x
        })
        .collect();
    FormData {
        form: 99,
        action: "https://example.test/login".into(),
        fields,
    }
}

/// The parse as field names: username, password, new password, confirmation.
fn names(form: &FormData, mode: Mode) -> Option<[Option<String>; 4]> {
    let p: ParsedForm = parse(form, mode)?;
    let n = |i: Option<usize>| i.map(|i| form.fields[i].name.clone());
    Some([
        n(p.username),
        n(p.password),
        n(p.new_password),
        n(p.confirmation),
    ])
}

fn s(x: &str) -> Option<String> {
    Some(x.to_owned())
}

/// What a form shape parses to: what it is, the mode, the form, the expected names.
type Case = (&'static str, Mode, FormData, Option<[Option<String>; 4]>);

#[test]
fn form_shapes() {
    use Mode::*;
    let cases: Vec<Case> = vec![
        (
            "email and password",
            Saving,
            form(vec![
                f("email", "email", "", "a@b.test"),
                f("password", "pass", "", "pw1"),
            ]),
            Some([s("email"), s("pass"), None, None]),
        ),
        (
            "a remember-me checkbox and a submit button are not fields",
            Saving,
            form(vec![
                f("text", "login", "", "alice"),
                f("password", "pw", "", "pw1"),
                f("checkbox", "remember", "", "on"),
                f("submit", "go", "", "Sign in"),
            ]),
            Some([s("login"), s("pw"), None, None]),
        ),
        (
            "autocomplete names a username that is not the closest field",
            Saving,
            form(vec![
                f("text", "user", "username", "alice"),
                f("text", "nickname", "", "al"),
                f("password", "pw", "current-password", "pw1"),
            ]),
            Some([s("user"), s("pw"), None, None]),
        ),
        (
            "autocomplete with a section and webauthn tokens",
            Filling,
            form(vec![
                f("text", "u", "section-login username webauthn", ""),
                f("password", "p", "section-login current-password", ""),
            ]),
            Some([s("u"), s("p"), None, None]),
        ),
        (
            "sign-up: two equal passwords are a new one and its confirmation",
            Saving,
            form(vec![
                f("text", "name", "", "Alice Smith"),
                f("email", "email", "", "a@b.test"),
                f("password", "pw", "", "s3cret"),
                f("password", "pw2", "", "s3cret"),
            ]),
            Some([s("email"), None, s("pw"), s("pw2")]),
        ),
        (
            "change password: current, new, confirmation",
            Saving,
            form(vec![
                f("password", "old", "", "old1"),
                f("password", "new", "", "new1"),
                f("password", "again", "", "new1"),
            ]),
            Some([None, s("old"), s("new"), s("again")]),
        ),
        (
            "two new-password fields that differ: autocomplete is ignored, order decides",
            Saving,
            form(vec![
                f("text", "user", "", "alice"),
                f("password", "a", "new-password", "x1"),
                f("password", "b", "new-password", "x2"),
            ]),
            Some([s("user"), s("a"), s("b"), None]),
        ),
        (
            "a one-time code field after the password is not a password",
            Saving,
            form(vec![
                f("text", "user", "", "alice"),
                f("password", "pw", "", "pw1"),
                f("password", "otp_code", "", "123456"),
            ]),
            Some([s("user"), s("pw"), None, None]),
        ),
        (
            "a payment form's security code is not a login",
            Saving,
            form(vec![
                f("text", "cardnumber", "cc-number", "4111111111111111"),
                f("password", "cvc", "", "123"),
            ]),
            None,
        ),
        (
            "an autocomplete one-time-code password field is not a login",
            Saving,
            form(vec![f("password", "code", "one-time-code", "123456")]),
            None,
        ),
        (
            "a masked value is the page's, not the user's",
            Saving,
            form(vec![
                f("text", "user", "", "alice"),
                f("password", "pw", "", "\u{2022}\u{2022}\u{2022}\u{2022}"),
            ]),
            None,
        ),
        (
            "an empty password is not saved",
            Saving,
            form(vec![
                f("text", "user", "", "alice"),
                f("password", "pw", "", ""),
            ]),
            None,
        ),
        (
            "one or two digits are not a username",
            Saving,
            form(vec![
                f("text", "user", "", "alice"),
                f("text", "age", "", "42"),
                f("password", "pw", "", "pw1"),
            ]),
            Some([s("user"), s("pw"), None, None]),
        ),
        (
            "fields after the password are not the username",
            Saving,
            form(vec![
                f("password", "pw", "", "pw1"),
                f("text", "captcha", "", "xk2f9"),
            ]),
            Some([None, s("pw"), None, None]),
        ),
        (
            "filling: empty fields still parse",
            Filling,
            form(vec![f("text", "user", "", ""), f("password", "pw", "", "")]),
            Some([s("user"), s("pw"), None, None]),
        ),
        (
            "no password field: not a password form",
            Filling,
            form(vec![f("text", "q", "", ""), f("search", "s", "", "")]),
            None,
        ),
        (
            "three different passwords: the first is the current one",
            Saving,
            form(vec![
                f("password", "a", "", "1a"),
                f("password", "b", "", "2b"),
                f("password", "c", "", "3c"),
            ]),
            Some([None, s("a"), None, None]),
        ),
    ];
    let mut failed = Vec::new();
    for (what, mode, form, want) in cases {
        let got = names(&form, mode);
        if got != want {
            failed.push(format!("{what}: got {got:?}, want {want:?}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn typed_fields_win_over_prefilled_hidden_ones() {
    // A page's hidden, prefilled password field and the one the user typed in.
    let mut hidden = f("password", "stale", "", "old-value");
    hidden.user_typed = false;
    hidden.focusable = false;
    let mut user_hidden = f("text", "hidden_user", "", "bot");
    user_hidden.user_typed = false;
    user_hidden.focusable = false;
    let form = form(vec![
        user_hidden,
        hidden,
        f("text", "user", "", "alice"),
        f("password", "pw", "", "pw1"),
    ]);
    assert_eq!(
        names(&form, Mode::Saving),
        Some([s("user"), s("pw"), None, None])
    );
}

#[test]
fn readonly_password_counts_only_when_typed_in() {
    let mut ro = f("password", "pw", "", "pw1");
    ro.readonly = true;
    ro.user_typed = false;
    assert_eq!(names(&form(vec![ro.clone()]), Mode::Saving), None);
    ro.user_typed = true;
    assert_eq!(
        names(&form(vec![ro]), Mode::Saving),
        Some([None, s("pw"), None, None])
    );
}

mod page {
    use std::time::Duration;

    use snowlark_net::{NetConfig, NetThread};
    use snowlark_page::input::Modifiers;
    use snowlark_page::passwords::{PasswordEvent, PasswordFill};
    use snowlark_page::{Page, PageOptions, RenderSetup};
    use snowlark_paint::Point;

    const LOGIN: &str = "<!doctype html><style>body{margin:0}input{display:block;width:200px;height:30px}</style>\
        <form id=f action=/next><input id=u name=user><input id=p name=pass type=password></form>\
        <script>var log = []; p.addEventListener('input', () => log.push('input:' + p.value));</script>";

    fn page_with(html: &str, on: bool) -> Page {
        static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
        let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
        let mut page = Page::new(PageOptions {
            net: Some(net.client()),
            viewport: (800, 600),
            render: Some(RenderSetup::new().unwrap()),
            password_forms: on,
            ..Default::default()
        });
        page.load_html(html, &url::Url::parse("https://login.example/").unwrap());
        assert!(page.run_until_settled(Duration::from_secs(5)), "settles");
        page
    }

    fn eval(page: &mut Page, js: &str) -> String {
        match page.eval(js, "test.js") {
            Ok(v) => v,
            Err(e) => panic!("{js}: threw {e}"),
        }
    }

    fn rendered(page: &Page) -> Vec<snowlark_page::passwords::FormData> {
        page.engine()
            .take_password_events()
            .into_iter()
            .find_map(|e| match e {
                PasswordEvent::Rendered(f) => Some(f),
                _ => None,
            })
            .expect("a Rendered report after load")
    }

    fn fill_for(form: &snowlark_page::passwords::FormData, origin: &str) -> PasswordFill {
        PasswordFill {
            origin: origin.into(),
            username_field: Some(form.fields[0].id),
            password_field: form.fields[1].id,
            username: "alice".into(),
            password: "hunter2".into(),
        }
    }

    #[test]
    fn off_by_default_reports_nothing() {
        let page = page_with(LOGIN, false);
        assert!(page.engine().take_password_events().is_empty());
    }

    #[test]
    fn load_reports_the_login_form_without_values() {
        let mut page = page_with(LOGIN, true);
        eval(&mut page, "u.value = 'x'");
        let forms = rendered(&page);
        assert_eq!(forms.len(), 1);
        let names: Vec<&str> = forms[0].fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["user", "pass"]);
        assert!(forms[0].fields.iter().all(|f| f.value.is_empty()));
        assert!(forms[0].fields.iter().all(|f| f.focusable && !f.user_typed));
    }

    #[test]
    fn submission_reports_the_values() {
        let mut page = page_with(LOGIN, true);
        let _ = rendered(&page);
        eval(
            &mut page,
            "u.value = 'alice'; p.value = 'pw1'; f.requestSubmit()",
        );
        let submitted = page
            .engine()
            .take_password_events()
            .into_iter()
            .find_map(|e| match e {
                PasswordEvent::Submitted(f) => Some(f),
                _ => None,
            })
            .expect("a Submitted report");
        let values: Vec<&str> = submitted.fields.iter().map(|f| f.value.as_str()).collect();
        assert_eq!(values, ["alice", "pw1"]);
    }

    #[test]
    fn a_cancelled_submission_reports_nothing() {
        let mut page = page_with(LOGIN, true);
        let _ = rendered(&page);
        eval(
            &mut page,
            "f.onsubmit = e => e.preventDefault(); p.value = 'pw1'; f.requestSubmit()",
        );
        assert!(page.engine().take_password_events().is_empty());
    }

    #[test]
    fn filled_values_are_hidden_from_script_until_the_user_activates_the_page() {
        let mut page = page_with(LOGIN, true);
        let forms = rendered(&page);
        assert!(page
            .engine()
            .fill_password(&fill_for(&forms[0], "https://login.example")));
        assert!(page.run_until_settled(Duration::from_secs(5)));
        assert_eq!(eval(&mut page, "u.value + '|' + p.value"), "|");
        assert_eq!(eval(&mut page, "new FormData(f).get('pass')"), "");
        // A click in the page (on the username field).
        let none = Modifiers::default();
        let at = Point::new(20.0, 15.0);
        page.pointer_moved(at, none);
        page.pointer_pressed(at, 0, none, 1);
        page.pointer_released(at, 0, none, 1);
        assert!(page.run_until_settled(Duration::from_secs(5)));
        assert_eq!(eval(&mut page, "u.value + '|' + p.value"), "alice|hunter2");
        assert_eq!(eval(&mut page, "log.join()"), "input:hunter2");
    }

    #[test]
    fn a_fill_for_another_origin_or_a_stale_field_is_dropped() {
        let mut page = page_with(LOGIN, true);
        let forms = rendered(&page);
        assert!(!page
            .engine()
            .fill_password(&fill_for(&forms[0], "https://evil.example")));
        let mut wrong = fill_for(&forms[0], "https://login.example");
        wrong.password_field = forms[0].fields[0].id;
        assert!(
            !page.engine().fill_password(&wrong),
            "the password goes only into a password field"
        );
        eval(&mut page, "p.remove()");
        assert!(!page
            .engine()
            .fill_password(&fill_for(&forms[0], "https://login.example")));
    }

    #[test]
    fn script_setting_the_value_drops_the_suggestion() {
        let mut page = page_with(LOGIN, true);
        let forms = rendered(&page);
        page.engine()
            .fill_password(&fill_for(&forms[0], "https://login.example"));
        eval(&mut page, "p.value = 'page'");
        let none = Modifiers::default();
        let at = Point::new(20.0, 15.0);
        page.pointer_pressed(at, 0, none, 1);
        page.pointer_released(at, 0, none, 1);
        assert!(page.run_until_settled(Duration::from_secs(5)));
        assert_eq!(eval(&mut page, "p.value"), "page");
    }
}

#[test]
fn signature_recognises_the_same_form() {
    let a = form(vec![
        f("text", "user", "", "alice"),
        f("password", "pw", "", "pw1"),
    ]);
    let b = form(vec![f("text", "user", "", ""), f("password", "pw", "", "")]);
    let c = form(vec![f("text", "user", "", ""), f("text", "pw", "", "")]);
    assert_eq!(a.signature(), b.signature());
    assert_ne!(a.signature(), c.signature());
}
