// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The console's Formatter and Printer as a page can observe them (Console Standard §2.2, §2.3): which
//! of the arguments' methods and accessors a console call runs, which calls throw, and the text.

use snowlark_page::{Page, PageOptions};

fn page() -> Page {
    Page::new(PageOptions::default())
}

fn eval(page: &mut Page, src: &str) -> String {
    page.eval(src, "console-test.js").unwrap()
}

/// What logging a baited object or error runs: the Formatter's string conversions (`%s`, `%d`,
/// `%i`, `%f` call `toString`, never `valueOf`), an object's `@@toStringTag` and an error's string
/// conversion for the printed text, and nothing else: no own getter, no `stack`. The calls the
/// developer-tools probes make (`dir` and `table` with a format string and more arguments) do not
/// throw.
#[test]
fn logging_runs_only_the_conversions_the_format_calls_for() {
    let mut p = page();
    let out = eval(
        &mut p,
        r#"
        const hits = [];
        function bait() {
          const o = {};
          o.valueOf = () => { hits.push("valueOf"); return 1; };
          o.toString = () => { hits.push("toString"); return "x"; };
          Object.defineProperty(o, "id", { get() { hits.push("get id"); return 1; }, enumerable: true });
          Object.defineProperty(o, Symbol.toStringTag, { get() { hits.push("tag"); return "T"; } });
          return o;
        }
        function errBait() {
          const e = new Error("m");
          Object.defineProperty(e, "stack", { get() { hits.push("stack"); return ""; } });
          Object.defineProperty(e, "message", { get() { hits.push("message"); return ""; } });
          return e;
        }
        const seen = {};
        for (const m of ["log", "debug", "info", "warn", "error", "trace", "dir", "dirxml", "table"]) {
          for (const f of ["%c%d", "%c%i", "%c%f", "%c%s", "%c%o", "%c%O"]) {
            for (const [kind, make] of [["obj", bait], ["err", errBait]]) {
              hits.length = 0;
              try { console[m](f, "font-size:0;color:transparent", make()); } catch (e) { hits.push("threw " + e.name); }
              seen[`${m} ${f} ${kind}`] = hits.join(",");
            }
          }
          hits.length = 0;
          console[m](bait()); console[m](errBait());
          seen[`${m} plain`] = hits.join(",");
        }
        JSON.stringify(seen)
        "#,
    );
    let seen: std::collections::BTreeMap<String, String> = serde_json::from_str(&out).unwrap();
    let formatting = ["log", "debug", "info", "warn", "error", "trace"];
    for (call, hits) in &seen {
        let mut parts = call.split(' ');
        let (method, format, kind) = (parts.next().unwrap(), parts.next().unwrap(), parts.next());
        let expected = match (kind, format) {
            (None, _) => "tag,message",
            (Some("err"), _) => "message",
            (Some(_), "%c%o" | "%c%O") => "tag",
            (Some(_), _) if formatting.contains(&method) => "toString",
            (Some(_), _) => "tag",
        };
        assert_eq!(hits, expected, "{call}");
    }
}

/// The Formatter's conversions: `%d`/`%i` are `parseInt(String(x), 10)`, `%f` `parseFloat`, a Symbol
/// is NaN, `%s` is `String(x)` and its result is formatted in turn, `%c` prints nothing, `%%` is `%`;
/// a conversion that throws propagates out of the console call. The Printer shows a plain object by
/// its data properties (an accessor as `(...)`, unread) and an array by its elements.
#[test]
fn the_formatter_converts_as_the_standard_says() {
    let mut p = page();
    eval(
        &mut p,
        r#"
        console.log("%d|%i|%f|%d", " 42px", "-7.9", " 1.5e3x", Symbol("s"));
        console.log("%s and %s", { toString() { return "%d"; } }, "12", "rest");
        console.log("%c100%% %o", "color:red", { a: 1, b: "x", get c() { throw 1; } });
        console.log([1, null, [2, 3]], 5n, Symbol("t"));
        "#,
    );
    let threw = eval(
        &mut p,
        r#"
        let r = "no";
        try { console.log("%s", { toString() { throw new RangeError("t"); } }); } catch (e) { r = e.name; }
        try { console.log({ get [Symbol.toStringTag]() { throw 1; } }, "after"); r += ",printed"; } catch (e) { r += ",threw"; }
        r
        "#,
    );
    assert_eq!(threw, "RangeError,printed");
    let texts: Vec<String> = p.console_messages().into_iter().map(|(_, t)| t).collect();
    assert_eq!(texts[0], "42|-7|1500|NaN");
    assert_eq!(texts[1], "12 and rest");
    assert_eq!(texts[2], "100% {a: 1, b: \"x\", c: (...)}");
    assert_eq!(texts[3], "1,,2,3 5n Symbol(t)");
}

/// Console Standard 2.2.1 `%c`: the developer tools get each style with the byte of the text it
/// applies from; the style ends where the format string's output ends (the arguments printed after
/// it are plain), and an argument that is not a string is no style. assert's prefix moves them.
#[test]
fn percent_c_styles_reach_the_developer_tools() {
    use snowlark_page::devtools::ConsoleStyle;
    let mut p = page();
    p.engine().devtools_console(true);
    eval(
        &mut p,
        r#"
        console.log("%c\n██╗\n╚═╝\n", "color: #0066CC; font-weight: bold;");
        console.log("a%cb%cc", "background: rgb(1, 2, 3) none; font-style: italic", 7, "tail");
        console.assert(false, "%cx", "color: red !important");
        console.log("plain", "text");
        "#,
    );
    let entries = p.engine().take_console_entries();
    let blue = ConsoleStyle {
        color: Some([0, 0x66, 0xCC, 255]),
        bold: true,
        ..Default::default()
    };
    assert_eq!(entries[0].text, "\n██╗\n╚═╝\n");
    assert_eq!(entries[0].styles, vec![(0, blue)]);
    assert_eq!(entries[1].text, "abc tail");
    let italic = ConsoleStyle {
        background: Some([1, 2, 3, 255]),
        italic: true,
        ..Default::default()
    };
    assert_eq!(
        entries[1].styles,
        vec![
            (1, italic),
            (2, ConsoleStyle::default()),
            (3, ConsoleStyle::default())
        ]
    );
    assert_eq!(entries[2].text, "Assertion failed: x");
    let red = ConsoleStyle {
        color: Some([255, 0, 0, 255]),
        ..Default::default()
    };
    assert_eq!(entries[2].styles, vec![(18, red)]);
    assert!(entries[3].styles.is_empty());
}

/// While the developer tools are open, their line for an uncaught exception or an unhandled
/// rejection of an error carries the error's call frames after its message, as V8 captured them
/// when the error was made (the page's own `stack` getter is not run); a thrown value that is not
/// an error has none.
#[test]
fn uncaught_errors_reach_the_developer_tools_with_their_stack() {
    let mut p = page();
    p.engine().devtools_console(true);
    p.capture_error_stacks(true);
    eval(
        &mut p,
        r#"
        var hits = [];
        function boom() { throw new Error("6"); }
        function reject() { return Promise.reject(new TypeError("7")); }
        setTimeout(() => boom(), 0);
        setTimeout(() => {
          const e = new RangeError("8");
          Object.defineProperty(e, "stack", { get() { hits.push("stack"); return ""; } });
          throw e;
        }, 0);
        setTimeout(() => { throw "plain"; }, 0);
        setTimeout(() => { reject(); }, 0);
        "#,
    );
    p.run_until_settled(std::time::Duration::from_secs(5));
    let entries = p.engine().take_console_entries();
    let errors: Vec<&str> = entries
        .iter()
        .filter(|e| e.level == "error")
        .map(|e| e.text.as_str())
        .collect();
    assert_eq!(errors.len(), 4, "{errors:?}");
    assert!(
        errors[0].starts_with("Uncaught Error: 6\n    at boom (console-test.js:"),
        "{:?}",
        errors[0]
    );
    assert!(
        errors[1].starts_with("Uncaught RangeError: 8\n    at console-test.js:"),
        "{:?}",
        errors[1]
    );
    assert_eq!(errors[2], "Uncaught plain");
    assert!(
        errors[3]
            .starts_with("Uncaught (in promise) TypeError: 7\n    at reject (console-test.js:"),
        "{:?}",
        errors[3]
    );
    assert_eq!(eval(&mut p, "hits.join()"), "");
}
