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

    #[test]
    fn a_login_form_script_adds_after_load_is_reported_once() {
        let mut page = page_with("<!doctype html><body><p>app</p>", true);
        assert!(rendered(&page).is_empty());
        eval(
            &mut page,
            "document.body.insertAdjacentHTML('beforeend', \
             '<form id=f><input id=u name=user><input id=p name=pass type=password></form>'); \
             u.value = 'x'",
        );
        assert!(page.run_until_settled(Duration::from_secs(5)));
        let added = page.engine().take_password_events();
        let forms = match added.as_slice() {
            [PasswordEvent::Added(forms)] => forms.clone(),
            other => panic!("one report of added forms, not {other:?}"),
        };
        assert_eq!(forms.len(), 1);
        assert!(forms[0].fields.iter().all(|f| f.value.is_empty()));
        // More inputs, in the same form or none with a password: nothing new to report.
        eval(
            &mut page,
            "f.append(document.createElement('input')); \
             document.body.append(document.createElement('input'))",
        );
        assert!(page.run_until_settled(Duration::from_secs(5)));
        assert!(page.engine().take_password_events().is_empty());
        // The form is filled like one that was there at `load`.
        assert!(page
            .engine()
            .fill_password(&fill_for(&forms[0], "https://login.example")));
    }

    /// Sign-ins by script: a server with the login page at `/`, `/ok` answering 200 and `/bad`
    /// 401, and the page loaded from it with the user's typing in its two fields.
    mod by_script {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        use snowlark_page::keyboard::KeyInput;
        use snowlark_page::passwords::FormData;

        use super::*;

        fn serve() -> String {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    let _ = reader.read_line(&mut first);
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                            break;
                        }
                    }
                    let target = first.split(' ').nth(1).unwrap_or("/");
                    let (status, body) = match target {
                        "/" => ("200 OK", LOGIN),
                        "/bad" => ("401 Unauthorized", "no"),
                        _ => ("200 OK", "<!doctype html><title>ok</title>ok"),
                    };
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                }
            });
            format!("http://{addr}")
        }

        fn typed_login() -> Page {
            static NET: std::sync::OnceLock<NetThread> = std::sync::OnceLock::new();
            let net = NET.get_or_init(|| NetThread::start(NetConfig::default()).unwrap());
            let mut page = Page::new(PageOptions {
                net: Some(net.client()),
                viewport: (800, 600),
                render: Some(RenderSetup::new().unwrap()),
                password_forms: true,
                ..Default::default()
            });
            page.navigate(url::Url::parse(&format!("{}/", serve())).unwrap());
            page.run_until(std::time::Instant::now() + Duration::from_secs(10), |p| {
                p.load_complete()
            });
            assert!(page.run_until_settled(Duration::from_secs(5)));
            let _ = rendered(&page);
            for (y, text) in [(15.0, "alice"), (45.0, "pw1")] {
                let at = Point::new(20.0, y);
                let none = Modifiers::default();
                page.pointer_moved(at, none);
                page.pointer_pressed(at, 0, none, 1);
                page.pointer_released(at, 0, none, 1);
                for c in text.chars() {
                    let k = KeyInput {
                        vk: c.to_ascii_uppercase() as u16,
                        scan: 0,
                        text: Some(c),
                        mods: none,
                        repeat: false,
                        paste: None,
                        paste_html: None,
                    };
                    page.key_down(&k);
                    page.key_char(c, &k);
                    page.key_up(&k);
                }
            }
            assert!(page.run_until_settled(Duration::from_secs(5)));
            assert_eq!(eval(&mut page, "u.value + '|' + p.value"), "alice|pw1");
            assert!(page.engine().take_password_events().is_empty());
            page
        }

        /// Runs `js`, which sets the title to "done" when it is through, and what follows it (a
        /// rendering update): the page's reports.
        fn run(page: &mut Page, js: &str) -> Vec<PasswordEvent> {
            eval(page, js);
            page.run_until(std::time::Instant::now() + Duration::from_secs(5), |p| {
                p.eval("document.title", "t.js").is_ok_and(|t| t == "done")
            });
            assert!(page.run_until_settled(Duration::from_secs(5)));
            page.engine().take_password_events()
        }

        fn values(f: &FormData) -> Vec<&str> {
            f.fields.iter().map(|f| f.value.as_str()).collect()
        }

        #[test]
        fn a_sign_in_by_fetch_that_takes_the_form_away_is_reported() {
            let mut page = typed_login();
            let events = run(
                &mut page,
                "fetch('/ok').then(r => r.text()).then(() => { f.remove(); document.title = 'done' })",
            );
            assert_eq!(eval(&mut page, "document.title"), "done");
            match events.as_slice() {
                [PasswordEvent::SubmittedInPlace(f)] => assert_eq!(values(f), ["alice", "pw1"]),
                other => panic!("one sign-in in place, not {other:?}"),
            }
        }

        #[test]
        fn a_form_hidden_after_an_xhr_is_reported() {
            let mut page = typed_login();
            let events = run(
                &mut page,
                "const x = new XMLHttpRequest(); x.open('POST', '/ok');\
                 x.onload = () => setTimeout(() => { f.style.display = 'none'; document.title = 'done' }, 50);\
                 x.send('a=b')",
            );
            assert_eq!(eval(&mut page, "document.title"), "done");
            assert!(
                matches!(events.as_slice(), [PasswordEvent::SubmittedInPlace(_)]),
                "{events:?}"
            );
        }

        #[test]
        fn a_failed_request_is_no_sign_in() {
            let mut page = typed_login();
            let events = run(
                &mut page,
                "fetch('/bad').then(() => { f.remove(); document.title = 'done' })",
            );
            assert_eq!(eval(&mut page, "document.title"), "done");
            assert!(events.is_empty(), "{events:?}");
        }

        #[test]
        fn a_form_that_stays_is_no_sign_in() {
            let mut page = typed_login();
            let events = run(
                &mut page,
                "fetch('/ok').then(() => { document.title = 'done' })",
            );
            assert_eq!(eval(&mut page, "document.title"), "done");
            assert!(events.is_empty(), "{events:?}");
        }

        #[test]
        fn a_script_navigation_after_typing_is_a_probable_submission() {
            let mut page = typed_login();
            eval(&mut page, "location.href = '/next'");
            let events = page.engine().take_password_events();
            match events.as_slice() {
                [PasswordEvent::Submitted(f)] => assert_eq!(values(f), ["alice", "pw1"]),
                other => panic!("one submission, not {other:?}"),
            }
        }

        #[test]
        fn typing_in_another_form_forgets_the_login_form() {
            let mut page = typed_login();
            eval(
                &mut page,
                "document.body.insertAdjacentHTML('afterbegin', \
                 '<form><input id=q style=\"display:block;height:30px;width:200px\"></form>')",
            );
            assert!(page.run_until_settled(Duration::from_secs(5)));
            let at = Point::new(20.0, 15.0);
            let none = Modifiers::default();
            page.pointer_pressed(at, 0, none, 1);
            page.pointer_released(at, 0, none, 1);
            let k = KeyInput {
                vk: u16::from(b'S'),
                scan: 0,
                text: Some('s'),
                mods: none,
                repeat: false,
                paste: None,
                paste_html: None,
            };
            page.key_down(&k);
            page.key_char('s', &k);
            page.key_up(&k);
            assert_eq!(eval(&mut page, "q.value"), "s");
            let events = run(
                &mut page,
                "fetch('/ok').then(() => { f.remove(); document.title = 'done' })",
            );
            assert!(events.is_empty(), "{events:?}");
        }
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
