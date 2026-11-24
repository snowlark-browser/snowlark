// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The window and what hangs off it (HTML §7.2, §8): `Window`, `Location`, `Navigator`, `BarProp`,
//! `WindowOrWorkerGlobalScope` (timers, microtasks, base64, structuredClone, reportError),
//! `AnimationFrameProvider`, idle callbacks, and the `console` namespace (Console Standard).

use super::view::finite;
use std::collections::HashMap;
use std::time::Instant;

use snowlark_bindings::generated::{
    AnimationFrameProviderImpl, AutoplayPolicy, AutoplayPolicyMediaType, BarPropImpl, ConsoleImpl,
    EventOrUndefined, ExternalImpl, FrameRequestCallback, GlobalPrivacyControlImpl,
    HandwritingModelConstraint, IdleDeadlineImpl, IdleRequestCallback, IdleRequestOptions,
    InstallParams, LocationImpl, NavigatorAutomationInformationImpl, NavigatorBadgeImpl,
    NavigatorConcurrentHardwareImpl, NavigatorContentUtilsImpl, NavigatorCookiesImpl,
    NavigatorIDImpl, NavigatorImpl, NavigatorLanguageImpl, NavigatorOnLineImpl, ScrollBehavior,
    ScrollToOptions, SpatialNavigationDirection, StringOrFunctionOrTrustedScript,
    StructuredSerializeOptions, UnsignedLongOrUnsignedLongSequence, VoidFunction, WindowImpl,
    WindowOrWorkerGlobalScopeImpl, WindowPostMessageOptions,
};
use snowlark_bindings::rt::{
    self, Cx, DomStr, ExceptionName, Handle, JsObject, JsPromise, JsValue,
};
use snowlark_dom::{local_name, ns, NodeId};

use super::element::{rejected, resolved};
use super::{exception, node_handle, Engine, Object};
use crate::event_loop::TimerHandler;

/// An element's border box relative to the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct LayoutBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Serves a body and MIME type in place of the network for some URLs.
pub type ResourceOverride = Box<dyn Fn(&url::Url) -> Option<(Vec<u8>, String)>>;

/// Where console messages go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ConsoleOutput {
    /// Kept only for [`crate::Page::console_messages`].
    #[default]
    Quiet,
    Stderr,
    Stdout,
    /// Each message as a [`crate::devtools::ConsoleEntry::tagged_line`] on stderr when it is
    /// printed (a worker's console in `SNOWLARK_CONSOLE=tagged` runs).
    Tagged,
}

/// Console state (Console Standard §2: count map, timer table, group stack).
#[derive(Default)]
pub struct ConsoleState {
    pub counts: HashMap<String, u64>,
    pub timers: HashMap<String, Instant>,
    pub group: usize,
    pub output: ConsoleOutput,
    /// Every message, as (log level, text), for embedders and tests (while the developer tools'
    /// sink is off).
    pub messages: Vec<(&'static str, String)>,
    /// The developer tools' sink ([`crate::devtools`]), when on.
    pub devtools: Option<crate::devtools::ConsoleSink>,
}

/// Where the page's viewport and its window sit on the screen, in the window's device-independent
/// px (CSSOM View §4.2 `screenX`/`screenY`, `outerWidth`/`outerHeight`; UI Events §5.3.1's screen
/// coordinates of a mouse event). Headless, and in a window that has not said: the origin is 0 and
/// the window is the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScreenPlacement {
    /// The viewport's top-left corner on the screen.
    pub origin: (i32, i32),
    /// The window's rectangle on the screen: left, top, width, height; `None` for the viewport.
    pub outer: Option<(i32, i32, i32, i32)>,
    /// The page zoom (CSS px to device-independent px); 0 stands for 1.
    pub zoom: f32,
}

impl ScreenPlacement {
    /// Screen coordinates of a point in the viewport's CSS px.
    pub fn to_screen(&self, x: f64, y: f64) -> (i32, i32) {
        let zoom = if self.zoom > 0.0 {
            self.zoom as f64
        } else {
            1.0
        };
        (
            self.origin.0 + (x * zoom).round() as i32,
            self.origin.1 + (y * zoom).round() as i32,
        )
    }
}

/// What the engine needs from the page around it.
#[derive(Default)]
pub struct PageHooks {
    pub net: Option<snowlark_net::NetClient>,
    pub console: ConsoleState,
    pub name: String,
    pub status: String,
    pub viewport: (i32, i32),
    /// The pointing devices the page sees (`PageOptions::input`).
    pub input: snowlark_style::InputDevices,
    pub screen: ScreenPlacement,
    /// Bodies served in place of the network for some URLs (the WPT runner's testharness hooks).
    pub resource_override: Option<ResourceOverride>,
}

/// `document.lastModified` for a document without a Last-Modified header: now, as
/// `MM/DD/YYYY hh:mm:ss` local time (here UTC, which is what headless runs use).
pub fn last_modified_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // Civil from days (Howard Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{m:02}/{d:02}/{y:04} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

impl Engine {
    /// A window's Location object.
    pub(crate) fn location_handle(&self, window: Handle) -> Handle {
        self.window_singleton(window, "location", || Object::Location { window })
    }

    fn navigator_handle(&self, window: Handle) -> Handle {
        self.window_singleton(window, "navigator", || Object::Navigator)
    }

    /// The window a Location object belongs to.
    fn location_window(&self, location: Handle) -> Handle {
        match &*self.obj(location) {
            Object::Location { window } => *window,
            _ => panic!("not a Location"),
        }
    }

    /// The border box of an element's first fragment relative to the viewport (zeros without a box).
    pub(crate) fn layout_box(&self, el: NodeId) -> LayoutBox {
        self.with_layout(el, |r, st| {
            r.geometry(el).map(|g| LayoutBox {
                x: (g.border_box.x - st.scroll.x) as f64,
                y: (g.border_box.y - st.scroll.y) as f64,
                width: g.border_box.width as f64,
                height: g.border_box.height as f64,
            })
        })
        .flatten()
        .unwrap_or_default()
    }

    /// `scrollX`, `scrollY`: the viewport's scroll position (0 for windows that are not rendered).
    pub(crate) fn window_scroll(&self, window: Handle) -> (f64, f64) {
        let Some(doc) = self.document_of_window(window) else {
            return (0.0, 0.0);
        };
        // (after style and layout, as Blink reads it: a scroll anchoring adjustment a change made
        // is in the position read, CSS Scroll Anchoring 1 §2.2's suppression window; at the
        // origin there is no anchor, §2.1 step 2, and no layout is needed)
        let p = self
            .with_doc_renderer(doc, |_, st| st.scroll)
            .unwrap_or_default();
        let p = if p == snowlark_paint::Point::ZERO {
            p
        } else {
            self.with_layout(doc, |_, st| st.scroll).unwrap_or(p)
        };
        (p.x as f64, p.y as f64)
    }

    /// Scrolls a window's viewport (when its document is rendered).
    /// CSSOM View §4 `window.scroll()`: performs a scroll of the window's viewport with
    /// `behavior`; the returned promise settles when the scroll completes.
    fn window_scroll_with(
        &self,
        cx: &mut Cx,
        window: Handle,
        x: Option<f64>,
        y: Option<f64>,
        behavior: ScrollBehavior,
        snap: super::view::SnapIntent,
    ) -> rt::Result<JsPromise> {
        let p = JsPromise::new(cx).ok_or(rt::Error::Pending)?;
        let mut pending = Some(p.clone());
        if let Some(doc) = self.document_of_window(window) {
            self.perform_scroll(
                cx,
                super::view::Scroller::Viewport(doc),
                x,
                y,
                behavior,
                &mut pending,
                Some(snap),
            );
        }
        Ok(super::element::scroll_promise(cx, p, pending))
    }

    /// `getSelection()`: the selection of the window's document.
    pub(crate) fn window_selection(&self, window: Handle) -> Option<Handle> {
        let doc = self.document_of_window(window)?;
        self.selection_of(doc)
    }

    /// `innerWidth`/`innerHeight`: a frame's viewport, else the top-level one.
    pub(crate) fn window_viewport(&self, window: Handle) -> (i32, i32) {
        if let Some(doc) = self.document_of_window(window) {
            if self.view.get().is_some_and(|(d, _)| d == doc) {
                // (a hosted window whose view is in place: its state is the top-level one)
                let v = self.render.borrow().viewport;
                return (v.width as i32, v.height as i32);
            }
            self.sync_frame_viewport(doc);
            if let Some(v) = self.frame_renders.borrow().get(&doc).map(|s| s.viewport) {
                return (v.width as i32, v.height as i32);
            }
        }
        self.hooks.borrow().viewport
    }

    /// The URL of a window's document, parsed.
    fn window_url(&self, window: Handle) -> Option<url::Url> {
        let doc = self.document_of_window(window)?;
        let dom = self.dom();
        // (a removed frame's document may be gone before its Window)
        if !dom.is_alive(doc) {
            return None;
        }
        self.parsed_document_url(&dom, doc)
    }

    /// `document.cookie` (HTML §3.1.2 / RFC 6265bis "cookie-string" for a non-HTTP API).
    pub(crate) fn document_cookie(&self, doc: NodeId) -> String {
        let Ok(url) = url::Url::parse(self.dom().document_url(doc)) else {
            return String::new();
        };
        if !matches!(url.scheme(), "http" | "https") {
            return String::new();
        }
        let hooks = self.hooks.borrow();
        match &hooks.net {
            Some(net) => net.document_cookie(&script_cookie_context(url)),
            None => String::new(),
        }
    }

    pub(crate) fn set_document_cookie(&self, doc: NodeId, value: &str) {
        let Ok(url) = url::Url::parse(self.dom().document_url(doc)) else {
            return;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        let hooks = self.hooks.borrow();
        if let Some(net) = &hooks.net {
            net.set_document_cookie(value, &script_cookie_context(url));
        }
    }

    /// Prints a console message with the current group indentation.
    pub(crate) fn console_print(&self, level: &'static str, text: &str) {
        self.console_print_at(level, text, None);
    }

    /// Prints a console message from `source` (shown after the text in the plain output; the
    /// developer tools' sink keeps it apart, or the console call's caller without one).
    pub(crate) fn console_print_at(
        &self,
        level: &'static str,
        text: &str,
        source: Option<crate::devtools::SourceLocation>,
    ) {
        let mut hooks = self.hooks.borrow_mut();
        let group = hooks.console.group;
        let indent = "  ".repeat(group);
        let line = match &source {
            Some(s) => format!("{indent}{text} ({s})"),
            None => format!("{indent}{text}"),
        };
        let output = hooks.console.output;
        match output {
            ConsoleOutput::Quiet | ConsoleOutput::Tagged => {}
            ConsoleOutput::Stderr => eprintln!("{line}"),
            ConsoleOutput::Stdout => println!("{line}"),
        }
        match hooks.console.devtools.as_mut() {
            Some(sink) => {
                let source = source.or_else(|| sink.caller.take());
                let entry = crate::devtools::ConsoleEntry {
                    level,
                    text: text.to_owned(),
                    source,
                    group,
                    realm: sink.realm.take(),
                    styles: std::mem::take(&mut sink.styles),
                };
                if output == ConsoleOutput::Tagged {
                    eprintln!("{}", entry.tagged_line());
                } else {
                    sink.push(entry);
                }
            }
            None => hooks.console.messages.push((level, line)),
        }
    }

    /// An error message (uncaught exceptions).
    pub(crate) fn console_error(&self, text: &str) {
        self.console_print("error", text);
    }

    /// Console Standard §2.1 Logger: the Formatter (§2.2.1) when the first argument is a string and
    /// more follow, then the Printer's text of what is left. The console methods that take a
    /// format string (log, debug, info, warn, error, trace, group, groupCollapsed, assert's data).
    /// A conversion the Formatter makes that throws propagates; the Printer's never do.
    fn console_format(&self, cx: &mut Cx, data: &[JsValue]) -> rt::Result<String> {
        self.devtools_note_caller(cx);
        let scope = cx.scope();
        let values: Vec<v8::Local<v8::Value>> = data
            .iter()
            .map(|v| v8::Local::new(scope, v.global()))
            .collect();
        let mut out = String::new();
        let mut rest = 0;
        let mut styles = Vec::new();
        if values.len() > 1 && values[0].is_string() {
            let target = values[0].to_rust_string_lossy(scope);
            let mut next = 1;
            format_into(scope, &target, &values, &mut next, &mut out, &mut styles, 0)?;
            rest = next;
        }
        let formatted = out.len();
        for v in &values[rest..] {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&console_text(scope, *v));
        }
        if let Some(sink) = self.hooks.borrow_mut().console.devtools.as_mut() {
            // (a style applies to the rest of the format string's output, not to the arguments
            // printed after it)
            let mut parsed: Vec<_> = styles
                .iter()
                .map(|(at, css)| (*at as u32, crate::devtools::ConsoleStyle::parse(css)))
                .collect();
            if !parsed.is_empty() && formatted < out.len() {
                parsed.push((formatted as u32, Default::default()));
            }
            sink.styles = parsed;
        }
        Ok(out)
    }

    /// The Printer's text of `data` with no Formatter (dir, dirxml, table, timeLog's data): each
    /// argument's [`console_text`], joined by spaces.
    fn console_plain(&self, cx: &mut Cx, data: &[JsValue]) -> String {
        self.devtools_note_caller(cx);
        let scope = cx.scope();
        let mut out = String::new();
        for v in data {
            let v = v8::Local::new(scope, v.global());
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&console_text(scope, v));
        }
        out
    }

    /// Queues the abort of `signal` after `ms` (`AbortSignal.timeout`).
    pub(crate) fn queue_signal_timeout(&self, signal: Handle, ms: u64) {
        let steps: crate::event_loop::TaskFn = Box::new(move |e: &Engine, cx: &mut Cx| {
            let reason = e.dom_exception_value(cx, "TimeoutError", "signal timed out");
            e.signal_abort(cx, signal, Some(reason));
        });
        let id = self.start_timer(TimerHandler::Internal(Some(steps)), ms as f64, false, None);
        if let Object::AbortSignal(s) = &mut *self.obj_mut(signal) {
            s.timeout = Some(id as u32);
        }
    }
}

/// The cookie context of `document.cookie` (a non-HTTP API, same-site with itself).
fn script_cookie_context(url: url::Url) -> snowlark_net::cookies::CookieContext {
    snowlark_net::cookies::CookieContext {
        url,
        http_api: false,
        same_site: true,
        top_level_navigation: false,
        safe_method: true,
    }
}

/// "Generic JavaScript object formatting" for the console: strings as-is at the top level, quoted
/// inside containers; arrays and plain objects shallowly; others by their string conversion.
pub(crate) fn inspect(
    scope: &mut v8::PinScope,
    v: v8::Local<v8::Value>,
    depth: u32,
    optimal: bool,
) -> String {
    if v.is_string() {
        let s = v.to_rust_string_lossy(scope);
        return if depth > 0 { format!("\"{s}\"") } else { s };
    }
    if v.is_symbol() {
        let s: v8::Local<v8::Symbol> = v.try_into().unwrap();
        let d = s.description(scope);
        return if d.is_undefined() {
            "Symbol()".into()
        } else {
            format!("Symbol({})", d.to_rust_string_lossy(scope))
        };
    }
    // Errors show their stack, as developer consoles do.
    if v.is_native_error() && depth == 0 {
        let o: v8::Local<v8::Object> = v.try_into().unwrap();
        let key = rt::name(scope, "stack");
        if let Some(stack) = o.get(scope, key.into()).filter(|s| s.is_string()) {
            return stack.to_rust_string_lossy(scope);
        }
    }
    if v.is_function() {
        let f: v8::Local<v8::Function> = v.try_into().unwrap();
        let name = f.get_name(scope).to_rust_string_lossy(scope);
        return if name.is_empty() {
            "ƒ anonymous()".into()
        } else {
            format!("ƒ {name}()")
        };
    }
    if v.is_array() && depth < 2 {
        let a: v8::Local<v8::Array> = v.try_into().unwrap();
        let mut parts = Vec::new();
        for i in 0..a.length().min(100) {
            let item = a
                .get_index(scope, i)
                .unwrap_or_else(|| v8::undefined(scope).into());
            parts.push(inspect(scope, item, depth + 1, optimal));
        }
        return format!("[{}]", parts.join(", "));
    }
    if v.is_object()
        && !v.is_native_error()
        && depth < 2
        && (optimal || v.is_proxy() || is_plain(scope, v))
    {
        let o: v8::Local<v8::Object> = v.try_into().unwrap();
        if let Some(keys) = o.get_own_property_names(scope, Default::default()) {
            let mut parts = Vec::new();
            for i in 0..keys.length().min(50) {
                let Some(k) = keys.get_index(scope, i) else {
                    continue;
                };
                let Some(val) = o.get(scope, k) else { continue };
                let ks = k.to_rust_string_lossy(scope);
                parts.push(format!("{ks}: {}", inspect(scope, val, depth + 1, optimal)));
            }
            return format!("{{{}}}", parts.join(", "));
        }
    }
    v.to_rust_string_lossy(scope)
}

fn is_plain(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> bool {
    let Ok(o) = v8::Local::<v8::Object>::try_from(v) else {
        return false;
    };
    let name = o.get_constructor_name().to_rust_string_lossy(scope);
    name == "Object"
}

/// Console Standard §2.2.1 Formatter over `target`, the arguments from `values[*next]` on: `%s` is
/// `String(arg)`, `%d` and `%i` `%parseInt%(arg, 10)`, `%f` `%parseFloat%(arg)` (a Symbol is NaN for
/// the three), `%o` and `%O` the argument's [`console_text`], `%c` (CSS) takes its argument and
/// prints nothing (its CSS, when a string, goes to `styles` with the byte offset in `out` it
/// applies from), `%%` is `%`. A `%s` result is formatted in turn, as the algorithm's recursion
/// over its target does. The string conversions run the page's `toString` and `@@toPrimitive`,
/// as the spec's calls do; nothing else of the argument is read.
fn format_into(
    scope: &mut v8::PinScope,
    target: &str,
    values: &[v8::Local<v8::Value>],
    next: &mut usize,
    out: &mut String,
    styles: &mut Vec<(usize, String)>,
    depth: u32,
) -> rt::Result<()> {
    let mut chars = target.chars().peekable();
    while let Some(c) = chars.next() {
        let spec = match chars.peek() {
            Some(&s) if c == '%' && *next < values.len() => s,
            _ => {
                out.push(c);
                continue;
            }
        };
        match spec {
            's' | 'd' | 'i' | 'f' | 'o' | 'O' | 'c' => {
                chars.next();
                let v = values[*next];
                *next += 1;
                match spec {
                    's' => {
                        let s = if v.is_symbol() {
                            symbol_text(scope, v)
                        } else {
                            v.to_string(scope)
                                .ok_or(rt::Error::Pending)?
                                .to_rust_string_lossy(scope)
                        };
                        if depth < 32 {
                            format_into(scope, &s, values, next, out, styles, depth + 1)?;
                        } else {
                            out.push_str(&s);
                        }
                    }
                    'd' | 'i' | 'f' => {
                        let n = if v.is_symbol() {
                            f64::NAN
                        } else {
                            let s = v.to_string(scope).ok_or(rt::Error::Pending)?;
                            let s = s.to_rust_string_lossy(scope);
                            if spec == 'f' {
                                parse_float(&s)
                            } else {
                                parse_int(&s)
                            }
                        };
                        out.push_str(&rt::number_to_string(scope, n));
                    }
                    'o' | 'O' => out.push_str(&console_text(scope, v)),
                    _ => {
                        // (any other value is no CSS: the style goes back to none)
                        let css = if v.is_string() {
                            v.to_rust_string_lossy(scope)
                        } else {
                            String::new()
                        };
                        styles.push((out.len(), css));
                    }
                }
            }
            '%' => {
                chars.next();
                out.push('%');
            }
            _ => out.push(c),
        }
    }
    Ok(())
}

/// ECMA-262 `parseInt(string, 10)` (§19.2.5): leading white space, a sign, then the longest run of
/// decimal digits; NaN without one.
fn parse_int(s: &str) -> f64 {
    let s = s.trim_start_matches(is_js_space);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return f64::NAN;
    }
    let n: f64 = s[..digits].parse().unwrap_or(f64::NAN);
    if neg {
        -n
    } else {
        n
    }
}

/// ECMA-262 `parseFloat(string)` (§19.2.4): leading white space, then the longest prefix that is a
/// StrDecimalLiteral (a sign, `Infinity` or decimal digits with an optional fraction and exponent);
/// NaN without one.
fn parse_float(s: &str) -> f64 {
    let s = s.trim_start_matches(is_js_space);
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    if s[i..].starts_with("Infinity") {
        return if b[0] == b'-' {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let int = b[i..].iter().take_while(|c| c.is_ascii_digit()).count();
    let mut end = i + int;
    let mut frac = 0;
    if b.get(end) == Some(&b'.') {
        frac = b[end + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if int + frac > 0 {
            end += 1 + frac;
        }
    }
    if int + frac == 0 {
        return f64::NAN;
    }
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let mut j = end + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let exp = b[j..].iter().take_while(|c| c.is_ascii_digit()).count();
        if exp > 0 {
            end = j + exp;
        }
    }
    // (Rust's float syntax takes every StrDecimalLiteral but a trailing "." before an exponent)
    let lit = s[..end].replace(".e", "e").replace(".E", "E");
    lit.trim_end_matches('.').parse().unwrap_or(f64::NAN)
}

/// ECMA-262 StrWhiteSpaceChar: WhiteSpace and LineTerminator.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn symbol_text(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
    let s: v8::Local<v8::Symbol> = v.try_into().unwrap();
    let d = s.description(scope);
    if d.is_undefined() {
        "Symbol()".into()
    } else {
        format!("Symbol({})", d.to_rust_string_lossy(scope))
    }
}

/// The Printer's text of one console argument (Console Standard §2.3 leaves it to the
/// implementation). What it reads of the page's objects is what the engines' consoles read with
/// the developer tools closed: a string, number, boolean, BigInt or Symbol as itself, an array as its
/// elements joined with commas (null and undefined empty), a function, regular expression, date or
/// error by its string conversion, and any other object by its `Object.prototype.toString` class
/// string (its `@@toStringTag`); a proxy is not looked into. Accessors are never called otherwise,
/// so logging an object shows nothing to the object that only an open console would. A plain
/// object is then shown by its own data properties, read without running script;
/// an error with the stack V8 captured for it. Exceptions are swallowed: a conversion that throws
/// leaves its part empty.
pub(crate) fn console_text(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
    v8::tc_scope!(let tc, scope);
    let v = v8::Local::new(tc, v);
    let mut out = String::new();
    let mut seen = Vec::new();
    let mut budget = 10_000u32;
    text_into(tc, v, &mut out, &mut seen, &mut budget, false);
    out
}

fn text_into<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    v: v8::Local<'s, v8::Value>,
    out: &mut String,
    seen: &mut Vec<v8::Local<'s, v8::Object>>,
    budget: &mut u32,
    in_array: bool,
) -> bool {
    if in_array && (v.is_null() || v.is_undefined()) {
        return true;
    }
    if v.is_symbol() {
        out.push_str(&symbol_text(scope, v));
        return true;
    }
    if v.is_big_int() {
        if let Some(s) = v.to_string(scope) {
            out.push_str(&s.to_rust_string_lossy(scope));
            out.push('n');
        }
        return true;
    }
    if !v.is_object() {
        if let Some(s) = v.to_string(scope) {
            out.push_str(&s.to_rust_string_lossy(scope));
        }
        return true;
    }
    if v.is_proxy() {
        out.push_str("[object Proxy]");
        return true;
    }
    let o: v8::Local<v8::Object> = v.try_into().unwrap();
    if v.is_array() {
        let a: v8::Local<v8::Array> = v.try_into().unwrap();
        if seen.contains(&o) {
            return true;
        }
        let len = a.length();
        if len > *budget || seen.len() > 32 {
            return false;
        }
        *budget -= len;
        seen.push(o);
        let mut ok = true;
        for i in 0..len {
            if i > 0 {
                out.push(',');
            }
            let item = a.get_index(scope, i);
            if let Some(item) = item {
                if !text_into(scope, item, out, seen, budget, true) {
                    ok = false;
                    break;
                }
            }
        }
        seen.pop();
        return ok;
    }
    if v.is_boolean_object()
        || v.is_number_object()
        || v.is_string_object()
        || v.is_big_int_object()
        || v.is_symbol_object()
    {
        // (the wrapped value; no binding reads it without running script)
        out.push_str(&class_string(scope, o, false));
        return true;
    }
    if v.is_date() || v.is_function() || v.is_native_error() || v.is_reg_exp() {
        if let Some(s) = v.to_string(scope) {
            out.push_str(&s.to_rust_string_lossy(scope));
        }
        if v.is_native_error() && !in_array {
            out.push_str(&captured_stack(scope, v));
        }
        return true;
    }
    let class = class_string(scope, o, true);
    if class == "[object Object]" && !in_array && is_plain(scope, v) {
        out.push_str(&data_preview(scope, o));
    } else {
        out.push_str(&class);
    }
    true
}

/// `Object.prototype.toString` (ECMA-262 §20.1.3.6) of a non-proxy object: its builtin tag, or its
/// `@@toStringTag` when that is a string (read with [[Get]], as the method does, when `tagged`).
fn class_string(scope: &mut v8::PinScope, o: v8::Local<v8::Object>, tagged: bool) -> String {
    let v: v8::Local<v8::Value> = o.into();
    let builtin = if v.is_array() {
        "Array"
    } else if v.is_arguments_object() {
        "Arguments"
    } else if v.is_function() {
        "Function"
    } else if v.is_native_error() {
        "Error"
    } else if v.is_boolean_object() {
        "Boolean"
    } else if v.is_number_object() {
        "Number"
    } else if v.is_string_object() {
        "String"
    } else if v.is_date() {
        "Date"
    } else if v.is_reg_exp() {
        "RegExp"
    } else {
        "Object"
    };
    if tagged {
        let key = v8::Symbol::get_to_string_tag(scope);
        if let Some(tag) = o.get(scope, key.into()).filter(|t| t.is_string()) {
            return format!("[object {}]", tag.to_rust_string_lossy(scope));
        }
    }
    format!("[object {builtin}]")
}

/// A plain object's own enumerable data properties, `{a: 1, b: "x"}`, read by their descriptors (no
/// accessor runs; one shows as `(...)`); nested objects are not looked into.
fn data_preview(scope: &mut v8::PinScope, o: v8::Local<v8::Object>) -> String {
    let Some(keys) = o.get_own_property_names(scope, Default::default()) else {
        return "{}".into();
    };
    let value_key = rt::name(scope, "value");
    let get_key = rt::name(scope, "get");
    let mut parts = Vec::new();
    for i in 0..keys.length().min(50) {
        let Some(k) = keys.get_index(scope, i) else {
            continue;
        };
        let Ok(name) = v8::Local::<v8::Name>::try_from(k) else {
            continue;
        };
        let Some(desc) = o.get_own_property_descriptor(scope, name) else {
            continue;
        };
        let Ok(desc) = v8::Local::<v8::Object>::try_from(desc) else {
            continue;
        };
        // (a descriptor object from V8 is a plain object of data properties: reading it runs nothing)
        let shown = if desc
            .get(scope, get_key.into())
            .is_some_and(|g| !g.is_undefined())
        {
            "(...)".to_owned()
        } else {
            match desc.get(scope, value_key.into()) {
                Some(val) => shallow(scope, val),
                None => continue,
            }
        };
        parts.push(format!("{}: {shown}", k.to_rust_string_lossy(scope)));
    }
    format!("{{{}}}", parts.join(", "))
}

/// A property value in a preview, without running script.
fn shallow(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
    if v.is_string() {
        return format!("\"{}\"", v.to_rust_string_lossy(scope));
    }
    if v.is_symbol() {
        return symbol_text(scope, v);
    }
    if v.is_big_int() {
        return format!("{}n", v.to_rust_string_lossy(scope));
    }
    if !v.is_object() {
        return v.to_rust_string_lossy(scope);
    }
    if v.is_function() {
        return "ƒ".into();
    }
    if v.is_array() {
        let a: v8::Local<v8::Array> = v.try_into().unwrap();
        return format!("Array({})", a.length());
    }
    "{…}".into()
}

/// The stack V8 captured when an error was made (`\n    at f (url:line:column)` per frame), read
/// from the error's internal record: its `stack` property is not touched.
pub(crate) fn captured_stack(scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> String {
    let Some(trace) = v8::Exception::get_stack_trace(scope, v) else {
        return String::new();
    };
    let mut out = String::new();
    for i in 0..trace.get_frame_count() {
        let Some(f) = trace.get_frame(scope, i) else {
            continue;
        };
        let name = f
            .get_function_name(scope)
            .map(|n| n.to_rust_string_lossy(scope))
            .filter(|n| !n.is_empty());
        let url = f
            .get_script_name_or_source_url(scope)
            .map(|n| n.to_rust_string_lossy(scope))
            .unwrap_or_default();
        let at = format!("{url}:{}:{}", f.get_line_number(), f.get_column());
        match name {
            Some(n) => out.push_str(&format!("\n    at {n} ({at})")),
            None => out.push_str(&format!("\n    at {at}")),
        }
    }
    out
}

impl Engine {
    /// Whether some element of `doc` may be named `name` for the Window's named properties, from the
    /// document's id and name index (script asks the global for names it does not define far more
    /// often than for ones it does).
    fn may_be_window_named(&self, doc: NodeId, name: &str) -> bool {
        // (every named object has the name as its id or its name attribute)
        self.dom().may_have_id_or_name(doc, name)
    }
}

impl WindowImpl for Engine {
    /// HTML §7.2.2.3 named access on the Window object: a child navigable's WindowProxy, the one
    /// named element, or an HTMLCollection of them.
    fn named_getter(&self, this: Handle, name: &str) -> Option<Handle> {
        if name.is_empty() {
            return None;
        }
        if let Some(w) = self.named_child_window(this, name) {
            return Some(w);
        }
        // (the window's associated Document, also after a navigation replaced the window)
        let doc = self
            .associated_document(this)
            .filter(|&d| self.dom().is_alive(d))?;
        if !self.may_be_window_named(doc, name) {
            return None;
        }
        let found: Vec<NodeId> = {
            let dom = self.dom();
            dom.descendants(doc)
                .filter(|&d| {
                    dom.is_element(d) && super::collections::is_window_named(&dom, d, name)
                })
                .take(2)
                .collect()
        };
        match found.as_slice() {
            [] => None,
            [one] => Some(node_handle(*one)),
            _ => Some(self.collection(
                doc,
                super::collections::Filter::WindowNamed(name.to_owned()),
            )),
        }
    }
    fn supported_property_names(&self, this: Handle) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in self.child_windows(this) {
            let n = self.window_name(w);
            if !out.contains(&n) && self.named_child_window(this, &n) == Some(w) {
                out.push(n);
            }
        }
        let Some(doc) = self
            .associated_document(this)
            .filter(|&d| self.dom().is_alive(d))
        else {
            return out;
        };
        let dom = self.dom();
        for d in dom.descendants(doc) {
            if !dom.is_element(d) {
                continue;
            }
            for attr in [local_name!("name"), local_name!("id")] {
                if let Some(v) = dom.attr(d, &ns!(), &attr) {
                    if super::collections::is_window_named(&dom, d, v)
                        && !out.iter().any(|o| o == v)
                    {
                        out.push(v.to_owned());
                    }
                }
            }
        }
        out
    }
    fn window(&self, this: Handle) -> Handle {
        this
    }
    fn self_(&self, this: Handle) -> Handle {
        this
    }
    fn document(&self, this: Handle) -> Handle {
        node_handle(
            self.associated_document(this)
                .expect("a window has a document"),
        )
    }
    fn name(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.window_name(this))
    }
    fn set_name(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.set_window_name(this, value);
        Ok(())
    }
    fn location(&self, this: Handle) -> Handle {
        self.location_handle(this)
    }
    fn custom_elements(&self, this: Handle) -> Handle {
        self.registry_handle(this)
    }
    fn locationbar(&self, this: Handle) -> Handle {
        self.window_singleton(this, "locationbar", || Object::BarProp(this))
    }
    fn menubar(&self, this: Handle) -> Handle {
        self.window_singleton(this, "menubar", || Object::BarProp(this))
    }
    fn personalbar(&self, this: Handle) -> Handle {
        self.window_singleton(this, "personalbar", || Object::BarProp(this))
    }
    fn scrollbars(&self, this: Handle) -> Handle {
        self.window_singleton(this, "scrollbars", || Object::BarProp(this))
    }
    fn statusbar(&self, this: Handle) -> Handle {
        self.window_singleton(this, "statusbar", || Object::BarProp(this))
    }
    fn toolbar(&self, this: Handle) -> Handle {
        self.window_singleton(this, "toolbar", || Object::BarProp(this))
    }
    fn external(&self, this: Handle) -> Handle {
        self.window_singleton(this, "external", || Object::External)
    }
    fn status(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.hooks.borrow().status.clone())
    }
    fn set_status(&self, _cx: &mut Cx, _this: Handle, value: &str) -> rt::Result<()> {
        self.hooks.borrow_mut().status = value.to_owned();
        Ok(())
    }
    fn closed(&self, this: Handle) -> bool {
        self.window_closed(this)
    }
    fn frames(&self, this: Handle) -> Handle {
        this
    }
    fn length(&self, this: Handle) -> u32 {
        self.child_windows(this).len() as u32
    }
    fn top(&self, this: Handle) -> Option<Handle> {
        self.top_window(this)
    }
    fn opener(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsValue> {
        self.window_opener(cx, this)
    }
    fn set_opener(&self, cx: &mut Cx, this: Handle, value: JsValue) -> rt::Result<()> {
        self.set_window_opener(cx, this, value)
    }
    fn parent(&self, this: Handle) -> Option<Handle> {
        self.parent_window(this)
    }
    /// HTML §7.2.2.2: the container, unless its document is not same origin-domain with the current
    /// settings object's origin.
    fn frame_element(&self, cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let Some(container) = self.frame_element_of(this) else {
            return Ok(None);
        };
        let doc = self.dom().node_document(container);
        let current = self.current_document(cx)?;
        Ok(self
            .same_origin_domain_documents(doc, current)
            .then(|| node_handle(container)))
    }
    fn history(&self, this: Handle) -> Handle {
        self.history_handle(this)
    }
    fn navigator(&self, this: Handle) -> Handle {
        self.navigator_handle(this)
    }
    fn client_information(&self, this: Handle) -> Handle {
        self.navigator_handle(this)
    }
    fn origin_agent_cluster(&self, _this: Handle) -> bool {
        false
    }
    fn credentialless(&self, _this: Handle) -> bool {
        false
    }
    fn orientation(&self, _this: Handle) -> i16 {
        0
    }
    fn inner_width(&self, this: Handle) -> i32 {
        self.window_viewport(this).0
    }
    fn inner_height(&self, this: Handle) -> i32 {
        self.window_viewport(this).1
    }
    /// CSSOM §6.7.1 `getComputedStyle(elt, pseudoElt)`.
    fn get_computed_style(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        elt: Handle,
        pseudo_elt: Option<&str>,
    ) -> rt::Result<Handle> {
        Ok(Engine::get_computed_style(self, elt, pseudo_elt))
    }
    /// Selection API `getSelection()`.
    fn get_selection(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        Ok(self.window_selection(this))
    }
    /// CSSOM View §4.2 `matchMedia(query)`.
    fn match_media(&self, _cx: &mut Cx, this: Handle, query: &str) -> rt::Result<Handle> {
        Ok(Engine::match_media(self, this, query))
    }
    fn scroll_x(&self, this: Handle) -> f64 {
        self.window_scroll(this).0
    }
    fn page_x_offset(&self, this: Handle) -> f64 {
        self.window_scroll(this).0
    }
    fn scroll_y(&self, this: Handle) -> f64 {
        self.window_scroll(this).1
    }
    fn page_y_offset(&self, this: Handle) -> f64 {
        self.window_scroll(this).1
    }
    fn screen(&self, this: Handle) -> Handle {
        self.screen_handle(this)
    }
    fn visual_viewport(&self, this: Handle) -> Option<Handle> {
        self.visual_viewport_handle(this)
    }
    // CSSOM View §4.2: the window's position and size on the screen (the viewport's when the
    // page runs without a window).
    // (a window this page hosts besides its own sits where its view's window is: `screen_of`)
    fn screen_x(&self, this: Handle) -> i32 {
        self.screen_of(this).0.outer.map_or(0, |o| o.0)
    }
    fn screen_left(&self, this: Handle) -> i32 {
        self.screen_x(this)
    }
    fn screen_y(&self, this: Handle) -> i32 {
        self.screen_of(this).0.outer.map_or(0, |o| o.1)
    }
    fn screen_top(&self, this: Handle) -> i32 {
        self.screen_y(this)
    }
    fn outer_width(&self, this: Handle) -> i32 {
        let (screen, viewport) = self.screen_of(this);
        screen.outer.map_or(viewport.0, |o| o.2)
    }
    fn outer_height(&self, this: Handle) -> i32 {
        let (screen, viewport) = self.screen_of(this);
        screen.outer.map_or(viewport.1, |o| o.3)
    }
    fn device_pixel_ratio(&self, _this: Handle) -> f64 {
        self.render
            .borrow()
            .setup
            .as_ref()
            .map_or(1.0, |s| s.device_pixel_ratio as f64)
    }
    /// HTML §7.2.2.1: this Window's current event. (A frame's unload handler can run inside its
    /// parent's listener: the parent's event is not for it to see.)
    fn event(&self, this: Handle) -> EventOrUndefined {
        match self.current_event.get() {
            Some(e) if self.current_event_global.get() == Some(this) => EventOrUndefined::Event(e),
            _ => EventOrUndefined::Undefined(()),
        }
    }
    fn close(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.window_close(cx, this);
        Ok(())
    }
    fn stop(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.stop_loading(this);
        Ok(())
    }
    fn focus(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn blur(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    /// HTML §7.2.2.1 `open(url, target, features)`: "the window open steps".
    fn open(
        &self,
        cx: &mut Cx,
        _this: Handle,
        url: &str,
        target: &str,
        features: &str,
    ) -> rt::Result<Option<Handle>> {
        self.window_open(cx, url, target, features)
    }
    fn alert_0(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn alert_message(&self, _cx: &mut Cx, _this: Handle, message: &str) -> rt::Result<()> {
        self.console_print("info", &format!("alert: {message}"));
        Ok(())
    }
    fn confirm(&self, _cx: &mut Cx, _this: Handle, _message: &str) -> rt::Result<bool> {
        Ok(false)
    }
    fn prompt(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _message: &str,
        _default: &str,
    ) -> rt::Result<Option<DomStr<'_>>> {
        Ok(None)
    }
    fn print(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn post_message_message_target_origin_transfer(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        target_origin: &str,
        transfer: Vec<JsObject>,
    ) -> rt::Result<()> {
        self.window_post_message(cx, this, message, target_origin, &transfer)
    }
    fn post_message_message_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        options: WindowPostMessageOptions,
    ) -> rt::Result<()> {
        self.window_post_message(cx, this, message, &options.target_origin, &options.transfer)
    }
    fn navigate(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _dir: SpatialNavigationDirection,
    ) -> rt::Result<()> {
        Ok(())
    }
    fn request_resize(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    // CSSOM View §4: a window script opened moves and resizes (the outer rectangle, as Chromium
    // reads `resizeTo`), when the embedder lets it (`window_move_or_resize`).
    fn move_to(&self, _cx: &mut Cx, this: Handle, x: i32, y: i32) -> rt::Result<()> {
        self.window_move_or_resize(this, |(_, _, w, h)| (x, y, w, h));
        Ok(())
    }
    fn move_by(&self, _cx: &mut Cx, this: Handle, x: i32, y: i32) -> rt::Result<()> {
        self.window_move_or_resize(this, |(l, t, w, h)| {
            (l.saturating_add(x), t.saturating_add(y), w, h)
        });
        Ok(())
    }
    fn resize_to(&self, _cx: &mut Cx, this: Handle, w: i32, h: i32) -> rt::Result<()> {
        self.window_move_or_resize(this, |(l, t, _, _)| (l, t, w, h));
        Ok(())
    }
    fn resize_by(&self, _cx: &mut Cx, this: Handle, x: i32, y: i32) -> rt::Result<()> {
        self.window_move_or_resize(this, |(l, t, w, h)| {
            (l, t, w.saturating_add(x), h.saturating_add(y))
        });
        Ok(())
    }
    /// CSSOM View §4 `scroll(options)`.
    fn scroll_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ScrollToOptions,
    ) -> rt::Result<JsPromise> {
        self.window_scroll_with(
            cx,
            this,
            options.left.map(finite),
            options.top.map(finite),
            options.behavior,
            super::view::SnapIntent::End,
        )
    }
    fn scroll_x_y(&self, cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<JsPromise> {
        self.window_scroll_with(
            cx,
            this,
            Some(finite(x)),
            Some(finite(y)),
            ScrollBehavior::Auto,
            super::view::SnapIntent::End,
        )
    }
    fn scroll_to_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ScrollToOptions,
    ) -> rt::Result<JsPromise> {
        WindowImpl::scroll_options(self, cx, this, options)
    }
    fn scroll_to_x_y(&self, cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<JsPromise> {
        WindowImpl::scroll_x_y(self, cx, this, x, y)
    }
    /// CSSOM View §4 `scrollBy(options)`: relative to the current position.
    fn scroll_by_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: ScrollToOptions,
    ) -> rt::Result<JsPromise> {
        let (sx, sy) = self.window_scroll(this);
        self.window_scroll_with(
            cx,
            this,
            Some(sx + options.left.map_or(0.0, finite)),
            Some(sy + options.top.map_or(0.0, finite)),
            options.behavior,
            super::view::SnapIntent::By,
        )
    }
    fn scroll_by_x_y(&self, cx: &mut Cx, this: Handle, x: f64, y: f64) -> rt::Result<JsPromise> {
        let (sx, sy) = self.window_scroll(this);
        self.window_scroll_with(
            cx,
            this,
            Some(sx + finite(x)),
            Some(sy + finite(y)),
            ScrollBehavior::Auto,
            super::view::SnapIntent::By,
        )
    }
    fn capture_events(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn release_events(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn request_idle_callback(
        &self,
        _cx: &mut Cx,
        this: Handle,
        callback: IdleRequestCallback,
        options: IdleRequestOptions,
    ) -> rt::Result<u32> {
        // requestIdleCallback §5.2: append to the list of idle request callbacks; with a positive
        // `timeout`, invoke it once that many milliseconds pass if it has not run by then.
        let id = {
            let mut el = self.event_loop.borrow_mut();
            let id = el.next_idle;
            el.next_idle = el.next_idle.wrapping_add(1).max(1);
            id
        };
        let timeout = options.timeout.unwrap_or(0);
        let timeout_timer = (timeout > 0).then(|| {
            self.start_timer(
                crate::event_loop::TimerHandler::Internal(Some(Box::new(move |e, cx| {
                    e.invoke_idle_callback_timeout(cx, id)
                }))),
                f64::from(timeout),
                false,
                None,
            )
        });
        self.event_loop
            .borrow_mut()
            .idle
            .push(crate::event_loop::IdleRequest {
                id,
                callback: callback.0,
                window: this,
                timeout_timer,
            });
        Ok(id)
    }
    /// requestIdleCallback §5.3: removes the callback from both lists.
    fn cancel_idle_callback(&self, _cx: &mut Cx, _this: Handle, handle: u32) -> rt::Result<()> {
        let timers: Vec<i32> = {
            let mut el = self.event_loop.borrow_mut();
            let mut timers = Vec::new();
            let mut keep = |r: &crate::event_loop::IdleRequest| {
                if r.id == handle {
                    timers.extend(r.timeout_timer);
                    false
                } else {
                    true
                }
            };
            el.idle.retain(&mut keep);
            el.idle_runnable.retain(&mut keep);
            timers
        };
        for t in timers {
            self.clear_timer(t);
        }
        Ok(())
    }
    fn minimize(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
    fn maximize(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
    fn restore(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
    fn set_resizable(&self, cx: &mut Cx, _this: Handle, _resizable: bool) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
}

impl Engine {
    /// HTML §9.3.3 "window post message steps". The message leaves here as bytes with the true origin
    /// and source frame stamped on it ([`PostedMessage`]); what happens to it after that needs no
    /// object of the sender's.
    fn window_post_message(
        &self,
        cx: &mut Cx,
        this: Handle,
        message: JsValue,
        target_origin: &str,
        transfer: &[JsObject],
    ) -> rt::Result<()> {
        // (a Window's postMessage always has a Window's realm as its incumbent; were it not so, the
        // message has no true sender to name, and it must not name its target)
        let Some(source) = self.incumbent_window(cx) else {
            return Err(exception(
                ExceptionName::InvalidStateError,
                "No incumbent realm",
            ));
        };
        // 3-5. "/" is the incumbent settings object's origin; a parsed URL's opaque origin is same
        // origin with nothing.
        let target = match target_origin {
            "*" => MessageTarget::Any,
            "/" => match self.window_origin(source) {
                Some(o) => MessageTarget::Origin(o),
                None => MessageTarget::Nothing,
            },
            _ => {
                let Ok(u) = url::Url::parse(target_origin) else {
                    return Err(exception(
                        ExceptionName::SyntaxError,
                        "Invalid target origin",
                    ));
                };
                match u.origin() {
                    o @ url::Origin::Tuple(..) => MessageTarget::Origin(o.ascii_serialization()),
                    url::Origin::Opaque(_) => MessageTarget::Nothing,
                }
            }
        };
        // 6.
        let (bytes, ports) = self.serialize_with_ports(cx, &message, transfer)?;
        let posted = PostedMessage {
            target: this,
            target_origin: target,
            origin: self.window_origin_serialization(source),
            source,
            bytes,
            ports,
        };
        // 7.
        self.queue_task(
            crate::event_loop::TaskSource::PostedMessage,
            move |e, cx| e.deliver_posted_message(cx, posted),
        );
        Ok(())
    }

    /// The task of the window post message steps.
    fn deliver_posted_message(&self, cx: &mut Cx, posted: PostedMessage) {
        let PostedMessage {
            target,
            target_origin,
            origin,
            source,
            bytes,
            ports,
        } = posted;
        // (a Window whose document is gone runs no more tasks)
        if self.context_discarded(target) {
            return;
        }
        // 7.1. Checked now, against the document the target shows now.
        let allowed = match &target_origin {
            MessageTarget::Any => true,
            MessageTarget::Origin(o) => self.window_has_origin(target, o),
            MessageTarget::Nothing => false,
        };
        if !allowed {
            return;
        }
        // (deserialized in the target window's realm)
        let realm = self
            .context_of_window(target)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, |cx| {
            // (the transferred ports arrive in this realm, wherever the data mentions them or not:
            // their messages are deserialized in it from now on)
            for &p in &ports {
                self.receive_port(cx, p);
            }
            match self.deserialize_value(cx, &bytes) {
                Ok(data) => self.fire_message_event(cx, target, data, &origin, source, ports),
                Err(()) => {
                    let ev = self.trusted_event(
                        snowlark_bindings::InterfaceId::MessageEvent,
                        "messageerror",
                        false,
                        false,
                        super::events::EventExt::Message {
                            data: None,
                            origin: origin.clone(),
                            last_event_id: String::new(),
                            source: Some(source),
                            ports: Vec::new(),
                        },
                    );
                    self.dispatch(cx, ev, target, false);
                }
            }
        });
    }
}

/// The URL Standard's basic URL parser on `value` followed by ":", with the scheme start state as
/// state override: the new scheme, lowercase, or `None` for failure (a value that does not start
/// with an ASCII letter, or has a code point a scheme cannot have before its first ":"). ASCII tab
/// and newline are removed first, as the parser does.
fn scheme_state_override(value: &str) -> Option<String> {
    let mut chars = value.chars().filter(|c| !matches!(c, '\t' | '\n' | '\r'));
    let mut scheme = String::new();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => scheme.push(c.to_ascii_lowercase()),
        _ => return None,
    }
    for c in chars {
        match c {
            ':' => return Some(scheme),
            c if c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.') => {
                scheme.push(c.to_ascii_lowercase())
            }
            _ => return None,
        }
    }
    // (the ":" appended to the value ends it)
    Some(scheme)
}

/// What `postMessage`'s `targetOrigin` asks of the target's origin.
enum MessageTarget {
    Any,
    /// The identity of an origin.
    Origin(String),
    /// An opaque origin of a parsed URL, or the origin of a gone Window.
    Nothing,
}

/// A message on its way to a Window: everything is data, the origin and source the sender's true ones.
struct PostedMessage {
    target: Handle,
    target_origin: MessageTarget,
    /// The serialization of the incumbent settings object's origin.
    origin: String,
    /// The incumbent settings object's global object.
    source: Handle,
    bytes: Vec<u8>,
    ports: Vec<Handle>,
}

impl LocationImpl for Engine {
    /// HTML §7.2.4 `ancestorOrigins`: the Location's empty DOMStringList without a relevant
    /// Document, else that Document's ancestor origins list (HTML §3.1.3: the serializations of its
    /// internal ancestor origin objects list), one object per Document. (The same origin-domain
    /// check is the cross-origin access check's: the member is not cross-origin exposed.)
    fn ancestor_origins(&self, this: Handle) -> Handle {
        let window = self.location_window(this);
        let Some(doc) = self.location_document(this) else {
            return self.window_singleton(window, "ancestorOrigins:empty", || {
                Object::StringList(Vec::new(), true)
            });
        };
        // (a Window kept for the document that replaced its initial `about:blank` one has the
        // list of the document it was made for until then: `set_context_document` drops it)
        self.window_singleton(window, "ancestorOrigins", || {
            let origins = self
                .internal_ancestor_origins(doc)
                .into_iter()
                .map(|o| match o {
                    Some(o) if !o.starts_with("opaque:") => o,
                    _ => "null".to_owned(),
                })
                .collect();
            Object::StringList(origins, true)
        })
    }

    fn href(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(String::from(self.location_url_or_blank(this)))
    }
    /// HTML §7.2.4 the `href` setter (and `assign()`): nothing without a relevant Document, else
    /// the value encoding-parsed relative to the entry settings object, navigated to.
    fn set_href(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        if self.location_document(this).is_none() {
            return Ok(());
        }
        let url = self.location_parse_entry(cx, value)?;
        self.location_navigate(cx, self.location_window(this), url)
    }
    fn origin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.location_url_or_blank(this)
                .origin()
                .ascii_serialization(),
        )
    }
    fn protocol(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(format!("{}:", self.location_url_or_blank(this).scheme()))
    }
    /// HTML §7.2.4 the `protocol` setter: a value that is not a scheme throws; a URL whose scheme
    /// is not HTTP(S) afterwards is not navigated to.
    fn set_protocol(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let Some(mut u) = self.location_url(this) else {
            return Ok(());
        };
        let Some(scheme) = scheme_state_override(value) else {
            return Err(exception(ExceptionName::SyntaxError, "Not a scheme"));
        };
        // (a scheme the parser may not switch to, special to non-special or the reverse, leaves the
        // URL as it is: the state override's "return")
        let _ = u.set_scheme(&scheme);
        if !matches!(u.scheme(), "http" | "https") {
            return Ok(());
        }
        self.location_navigate(cx, self.location_window(this), u)
    }
    fn host(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(super::url::host_with_port(
            &self.location_url_or_blank(this),
        ))
    }
    fn set_host(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.location_update(cx, this, |u| {
            // (a URL with an opaque path has no host to set)
            !u.cannot_be_a_base() && {
                super::url::set_host(u, value);
                true
            }
        })
    }
    fn hostname(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.location_url_or_blank(this)
                .host_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }
    fn set_hostname(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.location_update(cx, this, |u| {
            !u.cannot_be_a_base() && {
                super::url::set_hostname(u, value);
                true
            }
        })
    }
    fn port(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.location_url_or_blank(this)
                .port()
                .map(|p| p.to_string())
                .unwrap_or_default(),
        )
    }
    fn set_port(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.location_update(cx, this, |u| {
            // "cannot have a username/password/port": no host, an empty host or `file`
            let can_have_port = u.host_str().is_some_and(|h| !h.is_empty()) && u.scheme() != "file";
            can_have_port && {
                if value.is_empty() {
                    let _ = u.set_port(None);
                } else {
                    super::url::set_port(u, value);
                }
                true
            }
        })
    }
    fn pathname(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.location_url_or_blank(this).path().to_owned())
    }
    fn set_pathname(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.location_update(cx, this, |u| {
            !u.cannot_be_a_base() && {
                super::url::set_pathname(u, value);
                true
            }
        })
    }
    fn search(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.location_url_or_blank(this)
                .query()
                .filter(|q| !q.is_empty())
                .map(|q| format!("?{q}"))
                .unwrap_or_default(),
        )
    }
    /// HTML §7.2.4 the `search` setter: the empty string removes the query; anything else (one
    /// leading `?` removed) is parsed as the query in the relevant Document's encoding.
    fn set_search(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let Some(doc) = self.location_document(this) else {
            return Ok(());
        };
        let encoding = self.dom().encoding(doc);
        self.location_update(cx, this, |u| {
            if value.is_empty() {
                u.set_query(None);
                return true;
            }
            let input = value.strip_prefix('?').unwrap_or(value);
            if encoding.output_encoding() == encoding_rs::UTF_8 {
                // (not `url::quirks::set_search`: for a value of "?" the query is the empty
                // string, not null)
                u.set_query(Some(input));
                return true;
            }
            // (the query state with an encoding: parsed as a relative "?query" against the URL, `#`
            // being part of the query here)
            let relative = format!("?{}", input.replace('#', "%23"));
            if let Some(parsed) = snowlark_render::encoding_parse_url(u, encoding, &relative) {
                u.set_query(Some(parsed.query().unwrap_or("")));
            }
            true
        })
    }
    fn hash(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(
            self.location_url_or_blank(this)
                .fragment()
                .filter(|f| !f.is_empty())
                .map(|f| format!("#{f}"))
                .unwrap_or_default(),
        )
    }
    /// HTML §7.10.4 the `hash` setter: the value without one leading `#` parsed into an empty
    /// fragment (so `""` makes an empty fragment, which scrolls to the top, not none), and no
    /// navigation at all when the fragment comes out as it is (setting the same hash again).
    fn set_hash(&self, cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let Some(mut u) = self.location_url(this) else {
            return Ok(());
        };
        // (an empty fragment is the same as none here: `hash = ""` on a URL without a fragment
        // navigates nowhere, as the browsers do and WPT expects)
        let current = u.fragment().unwrap_or("").to_owned();
        let input = value.strip_prefix('#').unwrap_or(value);
        u.set_fragment(Some(input));
        if u.fragment().unwrap_or("") == current {
            return Ok(());
        }
        self.location_navigate(cx, self.location_window(this), u)
    }
    fn assign(&self, cx: &mut Cx, this: Handle, url: &str) -> rt::Result<()> {
        LocationImpl::set_href(self, cx, this, url)
    }
    /// HTML §7.2.4 `replace()`: as `assign()`, with history handling "replace".
    fn replace(&self, cx: &mut Cx, this: Handle, url: &str) -> rt::Result<()> {
        if self.location_document(this).is_none() {
            return Ok(());
        }
        let url = self.location_parse_entry(cx, url)?;
        self.location_navigate_with(cx, self.location_window(this), url, true)
    }
    fn reload(&self, cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let window = self.location_window(this);
        if self.location_document(this).is_none() {
            return Ok(());
        }
        if let Some(u) = self.window_url(window) {
            let source = self.current_global_document(cx);
            self.check_sandboxed_navigation(source, window)?;
            self.request_reload(window, u, source);
        }
        Ok(())
    }
}

impl Engine {
    /// HTML §7.2.4 a Location's relevant Document: its Window's browsing context's active
    /// document, null once the browsing context is gone.
    fn location_document(&self, location: Handle) -> Option<NodeId> {
        let window = self.location_window(location);
        if !self.has_browsing_context(window) {
            return None;
        }
        self.document_of_window(window)
            .filter(|&doc| self.dom().is_alive(doc))
    }

    /// HTML §7.2.4 a Location's url: its relevant Document's URL, `about:blank` when that is null.
    fn location_url_or_blank(&self, location: Handle) -> url::Url {
        self.location_url(location)
            .unwrap_or_else(|| url::Url::parse("about:blank").expect("about:blank"))
    }

    /// A copy of a Location's url, when it has a relevant Document (each setter's first steps).
    fn location_url(&self, location: Handle) -> Option<url::Url> {
        self.location_document(location)?;
        self.window_url(self.location_window(location))
    }

    /// HTML §2.4.2 "encoding-parsing a URL" relative to the entry settings object: its document's
    /// base URL and character encoding; failure throws a "SyntaxError".
    fn location_parse_entry(&self, cx: &mut Cx, value: &str) -> rt::Result<url::Url> {
        let entry = self.entry_document(cx)?;
        let (base, encoding) = {
            let dom = self.dom();
            (self.document_base_url(&dom, entry), dom.encoding(entry))
        };
        snowlark_render::encoding_parse_url(&base, encoding, value)
            .ok_or_else(|| exception(ExceptionName::SyntaxError, "Invalid URL"))
    }

    /// A Location setter's steps on a copy of its url: `f` changes the copy and says whether it is
    /// navigated to (false: the setter returns, as for a URL with an opaque path).
    fn location_update(
        &self,
        cx: &mut Cx,
        this: Handle,
        f: impl FnOnce(&mut url::Url) -> bool,
    ) -> rt::Result<()> {
        let Some(mut u) = self.location_url(this) else {
            return Ok(());
        };
        if !f(&mut u) {
            return Ok(());
        }
        self.location_navigate(cx, self.location_window(this), u)
    }

    /// HTML §7.2.4 "Location-object navigate": history handling "replace" when asked for, or
    /// while the Location's document is not completely loaded and the incumbent global object has
    /// no transient activation. A fragment change updates the URL and fires `hashchange`; anything
    /// else is a navigation, which the page performs after the current task.
    fn location_navigate(&self, cx: &mut Cx, window: Handle, url: url::Url) -> rt::Result<()> {
        self.location_navigate_with(cx, window, url, false)
    }

    fn location_navigate_with(
        &self,
        cx: &mut Cx,
        window: Handle,
        url: url::Url,
        replace: bool,
    ) -> rt::Result<()> {
        let source = self.current_global_document(cx);
        self.check_sandboxed_navigation(source, window)?;
        if let Some(id) = self.context_of_window(window) {
            self.stop_lazy_loading(id);
        }
        let loading = self
            .document_of_window(window)
            .is_some_and(|doc| !self.is_completely_loaded(doc));
        let activated = self
            .incumbent_window(cx)
            .and_then(|w| self.associated_document(w))
            .is_some_and(|doc| self.has_transient_activation(doc));
        let replace = replace || (loading && !activated);
        if !self.navigate_to_fragment(window, &url, replace) {
            if let Some(doc) = self.document_of_window(window) {
                self.password_probable_navigation(doc);
            }
            let history = if replace {
                super::frames::HistoryHandling::Replace
            } else {
                super::frames::HistoryHandling::Auto
            };
            self.request_navigation(window, url, source, history);
        }
        Ok(())
    }

    /// Throws when `source` is not allowed by sandboxing to navigate `window`'s browsing context.
    fn check_sandboxed_navigation(&self, source: Option<NodeId>, window: Handle) -> rt::Result<()> {
        let Some(target) = self.context_of_window(window) else {
            return Ok(());
        };
        // (a realm with no document left has no flags to ask: it navigates nothing)
        if !source.is_some_and(|doc| self.allowed_by_sandboxing_to_navigate(doc, target)) {
            return Err(exception(
                ExceptionName::SecurityError,
                "The document is sandboxed and may not navigate this browsing context",
            ));
        }
        Ok(())
    }

    /// HTML §7.4.2.3.3 "navigate to a fragment" when `url` differs from the window's document URL
    /// only in its fragment: the URL changes, a history entry is pushed, the indicated part is
    /// scrolled to and `hashchange` fires. Returns false for any other URL.
    pub(crate) fn navigate_to_fragment(
        &self,
        window: Handle,
        url: &url::Url,
        replace: bool,
    ) -> bool {
        let Some(current) = self.window_url(window) else {
            return false;
        };
        let mut a = current.clone();
        let mut b = url.clone();
        a.set_fragment(None);
        b.set_fragment(None);
        if a == b && url.fragment().is_some() {
            if let Some(doc) = self.document_of_window(window) {
                self.ensure_doc_history(doc);
                self.dom_mut().set_document_url(doc, url.as_str());
                self.fragment_history_entry(doc, url.as_str(), replace);
                self.scroll_to_fragment(doc, url.fragment().unwrap_or(""));
            }
            let old = String::from(current);
            let new = String::from(url.clone());
            if old != new {
                self.queue_task(
                    crate::event_loop::TaskSource::DomManipulation,
                    move |e, cx| {
                        let w = window;
                        let ev = e.trusted_event(
                            snowlark_bindings::InterfaceId::HashChangeEvent,
                            "hashchange",
                            false,
                            false,
                            super::events::EventExt::HashChange {
                                old_url: old,
                                new_url: new,
                            },
                        );
                        e.dispatch(cx, ev, w, false);
                    },
                );
            }
            return true;
        }
        false
    }

    /// HTML §7.4.6.4 "scroll to the fragment": the element the fragment indicates (an id, else an `a`
    /// with that name) becomes the document's target element and is scrolled to; for an empty
    /// fragment, "top" or no such element the target element is null (the top is scrolled to for
    /// the first two).
    pub(crate) fn scroll_to_fragment(&self, doc: NodeId, fragment: &str) {
        let decoded =
            url::form_urlencoded::parse(format!("f={}", fragment.replace('+', "%2B")).as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
        let target = {
            let dom = self.dom();
            dom.document_element(doc).and_then(|root| {
                dom.descendants(root)
                    .find(|&n| dom.is_element(n) && dom.get_attribute(n, "id") == Some(&*decoded))
                    .or_else(|| {
                        dom.descendants(root).find(|&n| {
                            dom.is_html_element_named(n, &local_name!("a"))
                                && dom.get_attribute(n, "name") == Some(&*decoded)
                        })
                    })
            })
        };
        self.set_target_element(doc, target);
        if !self.is_rendered_document(doc) {
            return;
        }
        match target {
            // HTML "scroll to the fragment": the ancestor details revealing algorithm runs first. A
            // task, as the `open` changes (and their `toggle` events) need a script context.
            Some(el) if !self.closed_details_hiding(el).is_empty() => {
                self.queue_task(
                    crate::event_loop::TaskSource::DomManipulation,
                    move |e, cx| {
                        if e.dom().is_alive(el) {
                            e.reveal_ancestor_details(cx, el);
                            e.view_scroll_into_view(el, "start", "nearest");
                        }
                    },
                );
            }
            Some(el) => self.view_scroll_into_view(el, "start", "nearest"),
            None if decoded.is_empty() || decoded.eq_ignore_ascii_case("top") => {
                self.view_scroll_viewport_to(doc, Some(0.0), Some(0.0));
            }
            None => {}
        }
    }

    /// Sets `doc`'s target element, moving `:target` to it.
    pub(crate) fn set_target_element(&self, doc: NodeId, target: Option<NodeId>) {
        let old = self.doc_state(doc, |s| std::mem::replace(&mut s.target_element, target));
        if old == target {
            return;
        }
        if let Some(old) = old.filter(|&n| self.dom().is_alive(n)) {
            self.set_element_state(old, snowlark_style::ElementState::URLTARGET, false);
        }
        if let Some(new) = target {
            self.set_element_state(new, snowlark_style::ElementState::URLTARGET, true);
        }
    }

    /// `doc`'s target element (HTML §7.4.6.4), while it is in `doc`: what `:target` matches.
    pub(crate) fn target_element(&self, doc: NodeId) -> Option<NodeId> {
        let target = self.doc_state(doc, |s| s.target_element)?;
        let dom = self.dom();
        (dom.is_alive(target) && dom.is_connected(target) && dom.node_document(target) == doc)
            .then_some(target)
    }
}

impl NavigatorImpl for Engine {
    fn cpu_performance(&self, _this: Handle) -> u16 {
        0
    }
    /// `navigator.scheduling` (one per navigator).
    fn scheduling(&self, this: Handle) -> Handle {
        let window = self.navigator_window(this);
        self.window_singleton(window, "scheduling", || Object::Scheduling)
    }
    /// Credential Management §2.4 `navigator.credentials` (one per navigator).
    fn credentials(&self, this: Handle) -> Handle {
        let window = self.navigator_window(this);
        self.window_singleton(window, "credentials", || Object::CredentialsContainer)
    }
    /// HTML §6.4.4 `navigator.userActivation` (one per navigator).
    fn user_activation(&self, this: Handle) -> Handle {
        let window = self.navigator_window(this);
        self.window_singleton(window, "userActivation", || Object::UserActivation(window))
    }
    /// Media Capture and Streams §9.2 `navigator.mediaDevices` (one per navigator).
    fn media_devices(&self, this: Handle) -> Handle {
        let window = self.navigator_window(this);
        self.window_singleton(window, "mediaDevices", || Object::MediaDevices(window))
    }
    fn do_not_track(&self, _this: Handle) -> Option<DomStr<'_>> {
        Engine::do_not_track(self)
    }
    fn get_battery(&self, cx: &mut Cx, this: Handle) -> rt::Result<JsPromise> {
        Engine::get_battery(self, cx, this)
    }
    fn get_gamepads(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<Option<Handle>>> {
        Engine::get_gamepads(self, this)
    }
    /// EME `navigator.requestMediaKeySystemAccess`.
    fn request_media_key_system_access(
        &self,
        cx: &mut Cx,
        this: Handle,
        key_system: &str,
        supported_configurations: Vec<snowlark_bindings::generated::MediaKeySystemConfiguration>,
    ) -> rt::Result<JsPromise> {
        let document = self.associated_document(self.navigator_window(this));
        Engine::request_media_key_system_access(
            self,
            cx,
            document,
            key_system,
            supported_configurations,
        )
    }
    /// Media Capabilities `navigator.mediaCapabilities` (one per navigator).
    fn media_capabilities(&self, this: Handle) -> Handle {
        Engine::media_capabilities(self, this)
    }
    /// Pointer Events §11: the touch contacts the device supports, the same device knowledge as
    /// the `pointer` media feature and the legacy touch event APIs' exposure.
    fn max_touch_points(&self, _this: Handle) -> i32 {
        self.hooks.borrow().input.max_touch_points as i32
    }
    fn get_autoplay_policy_type(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _t: AutoplayPolicyMediaType,
    ) -> rt::Result<AutoplayPolicy> {
        Ok(AutoplayPolicy::Disallowed)
    }
    fn get_autoplay_policy_element(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _e: Handle,
    ) -> rt::Result<AutoplayPolicy> {
        Ok(AutoplayPolicy::Disallowed)
    }
    /// Autoplay Policy Detection: an AudioContext's is whether it is allowed to start.
    fn get_autoplay_policy_context(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        context: Handle,
    ) -> rt::Result<AutoplayPolicy> {
        Ok(if self.audio_allowed_to_start(context) {
            AutoplayPolicy::Allowed
        } else {
            AutoplayPolicy::Disallowed
        })
    }
    fn ad_auction_components(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _n: u16,
    ) -> rt::Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn get_installed_related_apps(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotSupportedError, "Not supported")
    }
    fn query_handwriting_recognizer(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _c: HandwritingModelConstraint,
    ) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotSupportedError, "Not supported")
    }
    fn install_0(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
    fn install_options(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _o: InstallParams,
    ) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
    fn vibrate(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _p: UnsignedLongOrUnsignedLongSequence,
    ) -> rt::Result<bool> {
        Ok(false)
    }
    /// Beacon §3.1 `sendBeacon(url, data)`: a keepalive POST, queued without waiting for a response.
    fn send_beacon(
        &self,
        cx: &mut Cx,
        _this: Handle,
        url: &str,
        data: Option<snowlark_bindings::generated::ReadableStreamOrBlobOrArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrFormDataOrURLSearchParamsOrString>,
    ) -> rt::Result<bool> {
        let parsed = self
            .current_base_url(cx)?
            .join(url)
            .map_err(|_| rt::Error::Type("Invalid URL".into()))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(rt::Error::Type("Beacons go to http(s) URLs only".into()));
        }
        // Beacon §3.1 step 6: a stream body can't be sent.
        if matches!(
            data,
            Some(snowlark_bindings::generated::ReadableStreamOrBlobOrArrayBufferOrInt8ArrayOrInt16ArrayOrInt32ArrayOrUint8ArrayOrUint16ArrayOrUint32ArrayOrUint8ClampedArrayOrBigInt64ArrayOrBigUint64ArrayOrFloat16ArrayOrFloat32ArrayOrFloat64ArrayOrDataViewOrFormDataOrURLSearchParamsOrString::ReadableStream(_))
        ) {
            return Err(rt::Error::Type("Beacons can't send a ReadableStream".into()));
        }
        let body = data.map(|d| self.extract_body(cx, d));
        let doc = self.current_document(cx)?;
        let mut req = snowlark_net::Request::new(parsed);
        req.method = "POST".into();
        req.keepalive = true;
        req.mode = snowlark_net::Mode::NoCors;
        req.credentials = snowlark_net::CredentialsMode::Include;
        req.initiator_type = "beacon".into();
        req.client = Some(self.request_client(doc));
        if let Some((bytes, ctype)) = body {
            if let Some(t) = ctype {
                req.headers.append("Content-Type", &t);
            }
            req.body = Some(snowlark_net::Body::new(bytes));
        }
        // Beacon §3.1: false when the data is over what the user agent queues, here the keepalive
        // quota beacons share with `fetch(…, {keepalive: true})` (as in Blink, where both go through
        // the document's `ResourceFetcher`).
        if self.keepalive_quota(doc, &req).is_none() {
            return Ok(false);
        }
        // The response is read and dropped by the event loop.
        self.start_api_fetch(doc, req, super::fetch::JobOwner::Beacon);
        Ok(true)
    }
    fn deprecated_replace_in_urn(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _u: &str,
        _r: rt::Record<String>,
    ) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotSupportedError, "Not supported")
    }
    fn deprecated_ur_nto_url(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _u: &str,
        _s: bool,
    ) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotSupportedError, "Not supported")
    }
    fn share(
        &self,
        cx: &mut Cx,
        _this: Handle,
        _d: snowlark_bindings::generated::ShareData,
    ) -> rt::Result<JsPromise> {
        rejected(cx, ExceptionName::NotAllowedError, "Not allowed")
    }
    fn can_share(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _d: snowlark_bindings::generated::ShareData,
    ) -> rt::Result<bool> {
        Ok(false)
    }
    fn clipboard(&self, this: Handle) -> Handle {
        self.navigator_clipboard(this)
    }
    fn permissions(&self, this: Handle) -> Handle {
        self.navigator_permissions(this)
    }
}

impl NavigatorIDImpl for Engine {
    fn app_code_name(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("Mozilla")
    }
    fn app_name(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("Netscape")
    }
    fn app_version(&self, this: Handle) -> DomStr<'_> {
        let ua = self.navigator_user_agent(this);
        DomStr::Owned(ua.strip_prefix("Mozilla/").unwrap_or(&ua).to_owned())
    }
    fn platform(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.ua_profile().nav_platform())
    }
    fn product(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("Gecko")
    }
    fn product_sub(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.ua_profile().product_sub())
    }
    fn user_agent(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.navigator_user_agent(this))
    }
    fn vendor(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.ua_profile().vendor())
    }
    fn vendor_sub(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("")
    }
    fn oscpu(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed(self.ua_profile().oscpu())
    }
    fn taint_enabled(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<bool> {
        Ok(false)
    }
}

impl Engine {
    /// The user agent in force (the network client's, which the browser can change).
    pub(crate) fn user_agent(&self) -> std::sync::Arc<snowlark_net::ua::UserAgent> {
        match &self.hooks.borrow().net {
            Some(n) => n.user_agent(),
            None => std::sync::Arc::new(snowlark_net::ua::UserAgent::default()),
        }
    }

    /// The user agent string without regard to a site (the default identity's when the browser keeps
    /// per-site compatibility strings).
    pub(crate) fn user_agent_string(&self) -> String {
        self.user_agent().header_value()
    }

    /// `navigator.userAgent` of the navigator `navigator` (a Window's or a worker's): the string the
    /// `User-Agent` header carries to its document's or worker's origin, so a site that gets the
    /// compatibility string in its requests reads the same one in its script.
    pub(crate) fn navigator_user_agent(&self, navigator: Handle) -> String {
        self.navigator_ua(navigator).header_value()
    }

    /// The user agent of the navigator (or a navigator's `userAgentData`) `navigator`: the one the
    /// network client sends to its document's or worker's origin.
    pub(crate) fn navigator_ua(&self, navigator: Handle) -> snowlark_net::ua::UserAgent {
        let ua = self.user_agent();
        if !ua.site_compat {
            return (*ua).clone();
        }
        match self.navigator_origin_url(navigator) {
            Some(url) => ua.for_url(&url).into_owned(),
            None => (*ua).clone(),
        }
    }

    /// What `navigator`'s members derive from, for the navigator (or its `userAgentData`)
    /// `navigator`.
    pub(crate) fn navigator_ua_profile(&self, navigator: Handle) -> snowlark_net::ua::Profile {
        self.navigator_ua(navigator).profile()
    }

    /// The origin of a navigator's (or a navigator singleton's) global as a URL: its document's (HTML §7.2.2, the associated
    /// document; `about:` and inheriting documents take their creator's) or its worker's.
    fn navigator_origin_url(&self, navigator: Handle) -> Option<url::Url> {
        let global = self.navigator_window(navigator);
        if let Object::WorkerGlobal(g) = &*self.obj(global) {
            return Some(g.url.clone());
        }
        let doc = self.associated_document(global)?;
        url::Url::parse(&self.origin_of_document(doc)).ok()
    }

    /// What every place a site can read the user agent derives from (`navigator`'s fields, the
    /// client hints).
    pub(crate) fn ua_profile(&self) -> snowlark_net::ua::Profile {
        self.user_agent().profile()
    }
}

impl NavigatorLanguageImpl for Engine {
    fn language(&self, _this: Handle) -> DomStr<'_> {
        DomStr::Borrowed("en-US")
    }
    fn languages(&self, _this: Handle) -> Vec<String> {
        snowlark_render::svg_geometry::USER_LANGUAGES
            .iter()
            .map(|l| (*l).into())
            .collect()
    }
}

impl NavigatorOnLineImpl for Engine {
    fn on_line(&self, _this: Handle) -> bool {
        true
    }
}

impl NavigatorCookiesImpl for Engine {
    fn cookie_enabled(&self, _this: Handle) -> bool {
        true
    }
}

impl NavigatorConcurrentHardwareImpl for Engine {
    fn hardware_concurrency(&self, _this: Handle) -> u64 {
        std::thread::available_parallelism().map_or(4, |n| n.get() as u64)
    }
}

impl NavigatorContentUtilsImpl for Engine {
    fn register_protocol_handler(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _s: &str,
        _u: &str,
    ) -> rt::Result<()> {
        Err(exception(
            ExceptionName::SecurityError,
            "Protocol handlers are not supported",
        ))
    }
    fn unregister_protocol_handler(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        _s: &str,
        _u: &str,
    ) -> rt::Result<()> {
        Err(exception(
            ExceptionName::SecurityError,
            "Protocol handlers are not supported",
        ))
    }
}

impl NavigatorAutomationInformationImpl for Engine {
    fn webdriver(&self, _this: Handle) -> bool {
        false
    }
}

impl NavigatorBadgeImpl for Engine {
    fn set_app_badge(&self, cx: &mut Cx, _this: Handle, _c: Option<u64>) -> rt::Result<JsPromise> {
        resolved(cx)
    }
    fn clear_app_badge(&self, cx: &mut Cx, _this: Handle) -> rt::Result<JsPromise> {
        resolved(cx)
    }
}

impl GlobalPrivacyControlImpl for Engine {
    fn global_privacy_control(&self, _this: Handle) -> bool {
        false
    }
}

/// HTML §16.3 (obsolete features): "The AddSearchProvider() and IsSearchProviderInstalled() methods
/// must do nothing."
impl ExternalImpl for Engine {
    fn add_search_provider(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
    fn is_search_provider_installed(&self, _cx: &mut Cx, _this: Handle) -> rt::Result<()> {
        Ok(())
    }
}

impl BarPropImpl for Engine {
    /// HTML §7.2.6 "Browser interface elements": false when the relevant global object's browsing
    /// context is null, otherwise the negation of its top-level browsing context's "is popup".
    fn visible(&self, this: Handle) -> bool {
        let window = match &*self.obj(this) {
            Object::BarProp(window) => *window,
            _ => return false,
        };
        self.has_browsing_context(window) && !self.window_is_popup(window)
    }
}

impl IdleDeadlineImpl for Engine {
    fn did_timeout(&self, this: Handle) -> bool {
        matches!(
            &*self.obj(this),
            Object::IdleDeadline {
                did_timeout: true,
                ..
            }
        )
    }
    /// requestIdleCallback §4.2: the time until the deadline, computed now ("compute deadline"
    /// shortens it when a frame or a timer is requested during the callback); 0 after a timeout.
    fn time_remaining(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f64> {
        let (start, did_timeout) = match &*self.obj(this) {
            Object::IdleDeadline {
                period_start,
                did_timeout,
            } => (*period_start, *did_timeout),
            _ => return Ok(0.0),
        };
        if did_timeout {
            return Ok(0.0);
        }
        let deadline = if self.event_loop.borrow().idle_period_start == start {
            self.idle_deadline()
        } else {
            // (a deadline object kept past its idle period)
            start + crate::event_loop::IDLE_PERIOD_MS
        };
        // (at the clock's 5 µs granularity: the subtraction must not exceed a timer's own delay)
        Ok(((deadline - self.now()).max(0.0) * 200.0).floor() / 200.0)
    }
}

impl AnimationFrameProviderImpl for Engine {
    fn request_animation_frame(
        &self,
        _cx: &mut Cx,
        this: Handle,
        callback: FrameRequestCallback,
    ) -> rt::Result<u32> {
        let mut el = self.event_loop.borrow_mut();
        let id = el.next_raf;
        el.next_raf += 1;
        el.raf.push((id, callback.0, this));
        Ok(id)
    }
    fn cancel_animation_frame(&self, _cx: &mut Cx, _this: Handle, handle: u32) -> rt::Result<()> {
        let mut el = self.event_loop.borrow_mut();
        el.raf.retain(|(id, _, _)| *id != handle);
        el.raf_cancelled.push(handle);
        Ok(())
    }
}

impl WindowOrWorkerGlobalScopeImpl for Engine {
    /// Prioritized Task Scheduling: the global's Scheduler (one per global).
    fn scheduler(&self, this: Handle) -> Handle {
        self.window_singleton(this, "scheduler", || Object::Scheduler)
    }
    fn crypto(&self, this: Handle) -> Handle {
        self.crypto_handle(this)
    }
    /// IndexedDB §4.3: the global's `IDBFactory`.
    fn indexed_db(&self, this: Handle) -> Handle {
        self.idb_factory(this)
    }
    fn trusted_types(&self, this: Handle) -> Handle {
        self.trusted_types_of(this)
    }
    fn origin(&self, this: Handle) -> DomStr<'_> {
        // (a document a CSP sandbox gave an opaque origin)
        if self
            .document_of_window(this)
            .is_some_and(|d| self.sandboxed_origin(d).is_some())
        {
            return DomStr::Borrowed("null");
        }
        // HTML §8.1.3.1: the serialization of the relevant settings object's origin, which an
        // `about:` document takes from its creator.
        match self.document_of_window(this) {
            Some(d) if self.dom().is_alive(d) => {
                DomStr::Owned(self.net_origin_of_document(d).ascii_serialization())
            }
            _ => DomStr::Borrowed("null"),
        }
    }
    fn is_secure_context(&self, this: Handle) -> bool {
        self.window_url(this)
            .is_some_and(|u| snowlark_net::origin::is_url_potentially_trustworthy(&u))
    }
    fn cross_origin_isolated(&self, _this: Handle) -> bool {
        false
    }
    fn performance(&self, this: Handle) -> Handle {
        self.performance_of(this)
    }
    /// HTML §8.1.4.7 `reportError(e)`: reports `e` for this's relevant global object (a method
    /// taken from another window reports to the window it is called on).
    fn report_error(&self, cx: &mut Cx, this: Handle, e: JsValue) -> rt::Result<()> {
        let realm = self
            .context_of_window(this)
            .and_then(|id| self.realm_of_context(id));
        self.in_realm(cx, realm, |cx| {
            self.report_exception(cx, e.global().clone(), None)
        });
        Ok(())
    }
    /// HTML §8.3 `btoa(data)`: forgiving-base64 encode of the code points as bytes.
    fn btoa(&self, _cx: &mut Cx, _this: Handle, data: &str) -> rt::Result<DomStr<'_>> {
        let mut bytes = Vec::with_capacity(data.len());
        for c in data.chars() {
            let c = c as u32;
            if c > 0xFF {
                return Err(exception(
                    ExceptionName::InvalidCharacterError,
                    "The string contains characters outside of the Latin1 range",
                ));
            }
            bytes.push(c as u8);
        }
        Ok(DomStr::Owned(base64_encode(&bytes)))
    }
    /// HTML §8.3 `atob(data)`: forgiving-base64 decode, each byte a code point.
    fn atob(&self, _cx: &mut Cx, _this: Handle, data: &str) -> rt::Result<DomStr<'_>> {
        let bytes = snowlark_net::data_url::forgiving_base64_decode(data).ok_or_else(|| {
            exception(
                ExceptionName::InvalidCharacterError,
                "The string to be decoded is not correctly encoded",
            )
        })?;
        Ok(DomStr::Owned(bytes.into_iter().map(char::from).collect()))
    }
    fn set_timeout(
        &self,
        cx: &mut Cx,
        this: Handle,
        handler: StringOrFunctionOrTrustedScript,
        timeout: i32,
        arguments: Vec<JsValue>,
    ) -> rt::Result<i32> {
        let h = self.timer_handler(cx, this, handler, arguments);
        if self.timer_code_blocked(cx, this, &h) {
            return Ok(0);
        }
        Ok(self.start_timer(h, timeout as f64, false, None))
    }
    fn clear_timeout(&self, _cx: &mut Cx, _this: Handle, id: i32) -> rt::Result<()> {
        self.clear_timer(id);
        Ok(())
    }
    fn set_interval(
        &self,
        cx: &mut Cx,
        this: Handle,
        handler: StringOrFunctionOrTrustedScript,
        timeout: i32,
        arguments: Vec<JsValue>,
    ) -> rt::Result<i32> {
        let h = self.timer_handler(cx, this, handler, arguments);
        if self.timer_code_blocked(cx, this, &h) {
            return Ok(0);
        }
        Ok(self.start_timer(h, timeout as f64, true, None))
    }
    fn clear_interval(&self, _cx: &mut Cx, _this: Handle, id: i32) -> rt::Result<()> {
        self.clear_timer(id);
        Ok(())
    }
    fn queue_microtask(
        &self,
        cx: &mut Cx,
        _this: Handle,
        callback: VoidFunction,
    ) -> rt::Result<()> {
        let scope = cx.scope();
        let f = v8::Local::new(scope, callback.0.global());
        if let Ok(f) = v8::Local::<v8::Function>::try_from(f) {
            scope.enqueue_microtask(f);
        }
        Ok(())
    }
    fn structured_clone(
        &self,
        cx: &mut Cx,
        _this: Handle,
        value: JsValue,
        _options: StructuredSerializeOptions,
    ) -> rt::Result<JsValue> {
        self.structured_clone_value(cx, &value)
    }
    fn fetch(
        &self,
        cx: &mut Cx,
        _this: Handle,
        input: snowlark_bindings::generated::RequestOrString,
        init: snowlark_bindings::generated::RequestInit,
    ) -> rt::Result<JsPromise> {
        self.fetch_steps(cx, input, init)
    }
}

impl Engine {
    /// HTML timer initialization steps: a string handler goes through HostEnsureCanCompileStrings
    /// (CSP's EnsureCSPDoesNotBlockStringCompilation) when the timer is set; a blocked one sets no
    /// timer (and the call returns 0).
    fn timer_code_blocked(&self, cx: &mut Cx, window: Handle, handler: &TimerHandler) -> bool {
        let TimerHandler::Code(code, _, _) = handler else {
            return false;
        };
        self.policy.borrow().any
            && self
                .document_of_window(window)
                .is_some_and(|doc| self.eval_blocked(cx, doc, code))
    }

    fn timer_handler(
        &self,
        cx: &mut Cx,
        window: Handle,
        handler: StringOrFunctionOrTrustedScript,
        args: Vec<JsValue>,
    ) -> TimerHandler {
        match handler {
            StringOrFunctionOrTrustedScript::Function(f) => {
                TimerHandler::Function(f.0, args, window)
            }
            StringOrFunctionOrTrustedScript::String(s) => {
                TimerHandler::Code(s, window, self.timer_referrer(cx))
            }
            StringOrFunctionOrTrustedScript::TrustedScript(h) => {
                let s = match &*self.obj(h) {
                    Object::TrustedScript(s) => s.clone(),
                    _ => String::new(),
                };
                TimerHandler::Code(s, window, self.timer_referrer(cx))
            }
        }
    }
}

/// Base64 (RFC 4648 §4) with padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

impl ConsoleImpl for Engine {
    fn assert(&self, cx: &mut Cx, condition: bool, data: Vec<JsValue>) -> rt::Result<()> {
        if condition {
            return Ok(());
        }
        let msg = self.console_format(cx, &data)?;
        let text = if msg.is_empty() {
            "Assertion failed".to_owned()
        } else {
            format!("Assertion failed: {msg}")
        };
        if let Some(sink) = self.hooks.borrow_mut().console.devtools.as_mut() {
            let shift = (text.len() - msg.len()) as u32;
            for (at, _) in &mut sink.styles {
                *at += shift;
            }
        }
        self.console_print("error", &text);
        Ok(())
    }
    fn clear(&self, cx: &mut Cx) -> rt::Result<()> {
        self.hooks.borrow_mut().console.group = 0;
        // ("if possible for the environment, clear the console")
        if self.hooks.borrow().console.devtools.is_some() {
            self.devtools_note_caller(cx);
            self.console_print("clear", "Console was cleared");
        }
        Ok(())
    }
    fn debug(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        self.console_print("debug", &m);
        Ok(())
    }
    fn error(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        self.console_print("error", &m);
        Ok(())
    }
    fn info(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        self.console_print("info", &m);
        Ok(())
    }
    fn log(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        self.console_print("log", &m);
        Ok(())
    }
    fn table(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        // (the engines take any arguments here and print them as they are, with no Formatter)
        let m = self.console_plain(cx, &data);
        self.console_print("log", &m);
        Ok(())
    }
    fn trace(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        let stack = {
            let scope = cx.scope();
            v8::StackTrace::current_stack_trace(scope, 10).map(|t| {
                let mut lines = Vec::new();
                for i in 0..t.get_frame_count() {
                    if let Some(f) = t.get_frame(scope, i) {
                        let name = f
                            .get_function_name(scope)
                            .map(|n| n.to_rust_string_lossy(scope))
                            .unwrap_or_default();
                        let file = f
                            .get_script_name(scope)
                            .map(|n| n.to_rust_string_lossy(scope))
                            .unwrap_or_default();
                        lines.push(format!(
                            "    at {name} ({file}:{}:{})",
                            f.get_line_number(),
                            f.get_column()
                        ));
                    }
                }
                lines.join("\n")
            })
        };
        let text = if m.is_empty() {
            "console.trace".to_owned()
        } else {
            m
        };
        self.console_print("log", &format!("{text}\n{}", stack.unwrap_or_default()));
        Ok(())
    }
    fn warn(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        self.console_print("warn", &m);
        Ok(())
    }
    fn dir(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        // (the engines take any arguments here and print them as they are, with no Formatter)
        let m = self.console_plain(cx, &data);
        self.console_print("log", &m);
        Ok(())
    }
    fn dirxml(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        // (as dir: the engines run no Formatter for dirxml)
        let m = self.console_plain(cx, &data);
        self.console_print("log", &m);
        Ok(())
    }
    fn count(&self, cx: &mut Cx, label: &str) -> rt::Result<()> {
        self.devtools_note_caller(cx);
        let n = {
            let mut hooks = self.hooks.borrow_mut();
            let c = hooks.console.counts.entry(label.to_owned()).or_insert(0);
            *c += 1;
            *c
        };
        self.console_print("info", &format!("{label}: {n}"));
        Ok(())
    }
    fn count_reset(&self, cx: &mut Cx, label: &str) -> rt::Result<()> {
        self.devtools_note_caller(cx);
        let found = {
            let mut hooks = self.hooks.borrow_mut();
            match hooks.console.counts.get_mut(label) {
                Some(c) => {
                    *c = 0;
                    true
                }
                None => false,
            }
        };
        if !found {
            self.console_print("warn", &format!("Count for '{label}' does not exist"));
        }
        Ok(())
    }
    fn group(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        let m = self.console_format(cx, &data)?;
        if !m.is_empty() {
            self.console_print("log", &m);
        } else if let Some(sink) = self.hooks.borrow_mut().console.devtools.as_mut() {
            // (no message for the styles)
            sink.styles.clear();
        }
        self.hooks.borrow_mut().console.group += 1;
        Ok(())
    }
    fn group_collapsed(&self, cx: &mut Cx, data: Vec<JsValue>) -> rt::Result<()> {
        ConsoleImpl::group(self, cx, data)
    }
    fn group_end(&self, _cx: &mut Cx) -> rt::Result<()> {
        let mut hooks = self.hooks.borrow_mut();
        hooks.console.group = hooks.console.group.saturating_sub(1);
        Ok(())
    }
    fn time(&self, cx: &mut Cx, label: &str) -> rt::Result<()> {
        self.devtools_note_caller(cx);
        let exists = self.hooks.borrow().console.timers.contains_key(label);
        if exists {
            self.console_print("warn", &format!("Timer '{label}' already exists"));
            return Ok(());
        }
        self.hooks
            .borrow_mut()
            .console
            .timers
            .insert(label.to_owned(), Instant::now());
        Ok(())
    }
    fn time_log(&self, cx: &mut Cx, label: &str, data: Vec<JsValue>) -> rt::Result<()> {
        let start = self.hooks.borrow().console.timers.get(label).copied();
        match start {
            None => self.console_print("warn", &format!("Timer '{label}' does not exist")),
            Some(s) => {
                let extra = self.console_plain(cx, &data);
                let ms = s.elapsed().as_secs_f64() * 1000.0;
                let text = if extra.is_empty() {
                    format!("{label}: {ms} ms")
                } else {
                    format!("{label}: {ms} ms {extra}")
                };
                self.console_print("log", &text);
            }
        }
        Ok(())
    }
    fn time_end(&self, cx: &mut Cx, label: &str) -> rt::Result<()> {
        self.devtools_note_caller(cx);
        let start = self.hooks.borrow_mut().console.timers.remove(label);
        match start {
            None => self.console_print("warn", &format!("Timer '{label}' does not exist")),
            Some(s) => {
                let ms = s.elapsed().as_secs_f64() * 1000.0;
                self.console_print("info", &format!("{label}: {ms} ms"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn parse_int_and_parse_float_take_the_longest_numeric_prefix() {
        assert_eq!(
            parse_int(
                " 
 42px"
            ),
            42.0
        );
        assert_eq!(parse_int("-7.9"), -7.0);
        assert!(parse_int("px").is_nan());
        assert!(parse_int("").is_nan());
        assert_eq!(parse_float(" 1.5e3x"), 1500.0);
        assert_eq!(parse_float("1."), 1.0);
        assert_eq!(parse_float(".5"), 0.5);
        assert_eq!(parse_float("1.e3"), 1000.0);
        assert_eq!(parse_float("2e"), 2.0);
        assert_eq!(parse_float("-Infinityx"), f64::NEG_INFINITY);
        assert!(parse_float("+.e1").is_nan());
        assert!(parse_float("e5").is_nan());
    }

    #[test]
    fn last_modified_shape() {
        let s = last_modified_now();
        assert_eq!(s.len(), 19, "{s}");
        assert_eq!(&s[2..3], "/");
        assert_eq!(&s[13..14], ":");
    }
}
