// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Trusted Types (W3C Trusted Types): `trustedTypes` (`TrustedTypePolicyFactory`), policies and
//! the trusted objects they make, and the enforcement of CSP's `require-trusted-types-for 'script'`
//! and `trusted-types` directives at the injection sinks ("get Trusted Type compliant string": a
//! trusted value passes, a string goes through the default policy or is a violation, which an
//! enforced policy turns into a TypeError).

use snowlark_bindings::generated::ids;
use snowlark_bindings::generated::{
    CreateHTMLCallback, CreateParserOptionsCallback, CreateScriptCallback, CreateScriptURLCallback,
    ParseHTMLUnsafeOptionsOrTrustedHTMLParserOptions, SanitizerOrSanitizerConfigOrSanitizerPresets,
    SetHTMLUnsafeOptions, SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions,
    TrustedHTMLOrNullEmptyString, TrustedHTMLOrString,
    TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString, TrustedHTMLParserOptionsImpl,
    TrustedScriptOrNullEmptyString, TrustedScriptOrString, TrustedScriptURLOrString,
    TrustedTypePolicyFactoryImpl, TrustedTypePolicyImpl, TrustedTypePolicyOptions,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, Handle, JsValue, ToJs};
use snowlark_dom::{ns, NodeId};

use super::element::InsertMode;
use super::held::Held;
use super::sanitizer::{Config, SanitizerInput};
use super::{Engine, Object};

/// A trusted type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtKind {
    Html,
    Script,
    ScriptUrl,
}

impl TtKind {
    fn name(self) -> &'static str {
        match self {
            TtKind::Html => "TrustedHTML",
            TtKind::Script => "TrustedScript",
            TtKind::ScriptUrl => "TrustedScriptURL",
        }
    }
}

/// A global's `TrustedTypePolicyFactory`: its global, the names of the policies made, and the default
/// policy.
pub struct FactoryData {
    pub global: Handle,
    pub created: Vec<String>,
    pub default: Option<Handle>,
}

/// A `TrustedTypePolicy`: its name and its options' callbacks (engine/held.rs: kept by its wrapper
/// once its realm is discarded).
pub struct PolicyData {
    pub name: String,
    pub html: Option<Held<v8::Object>>,
    pub script: Option<Held<v8::Object>>,
    pub script_url: Option<Held<v8::Object>>,
    pub parser_options: Option<Held<v8::Object>>,
}

/// A `TrustedHTMLParserOptions` (Trusted Types §2.2.4): its run scripts flag and sanitizer
/// configuration, fixed when it is made.
pub struct ParserOptionsData {
    pub run_scripts: bool,
    pub config: Option<Box<Config>>,
}

/// The options an unsafe HTML parsing sink was given ("SetHTMLUnsafeOptions or
/// TrustedHTMLParserOptions", or `ParseHTMLUnsafeOptions`), or what Trusted Types made of them.
pub(crate) enum ParserOptions {
    Dictionary {
        run_scripts: bool,
        sanitizer: Option<SanitizerInput>,
    },
    Trusted(Handle),
}

impl From<SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions> for ParserOptions {
    fn from(v: SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions) -> ParserOptions {
        match v {
            SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions::SetHTMLUnsafeOptions(o) => {
                ParserOptions::Dictionary {
                    run_scripts: o.run_scripts,
                    sanitizer: o.sanitizer.map(Into::into),
                }
            }
            SetHTMLUnsafeOptionsOrTrustedHTMLParserOptions::TrustedHTMLParserOptions(h) => {
                ParserOptions::Trusted(h)
            }
        }
    }
}

impl From<ParseHTMLUnsafeOptionsOrTrustedHTMLParserOptions> for ParserOptions {
    fn from(v: ParseHTMLUnsafeOptionsOrTrustedHTMLParserOptions) -> ParserOptions {
        match v {
            ParseHTMLUnsafeOptionsOrTrustedHTMLParserOptions::ParseHTMLUnsafeOptions(o) => {
                ParserOptions::Dictionary {
                    run_scripts: false,
                    sanitizer: o.sanitizer.map(Into::into),
                }
            }
            ParseHTMLUnsafeOptionsOrTrustedHTMLParserOptions::TrustedHTMLParserOptions(h) => {
                ParserOptions::Trusted(h)
            }
        }
    }
}

impl ParserOptions {
    /// Trusted Types "needs vetting": scripts run, or a sanitizer that is not an empty
    /// `SanitizerConfig`.
    fn needs_vetting(&self) -> bool {
        match self {
            ParserOptions::Dictionary {
                run_scripts,
                sanitizer,
            } => {
                *run_scripts
                    || match sanitizer {
                        None => false,
                        Some(SanitizerInput::Config(c)) => !super::sanitizer::is_empty(c),
                        Some(_) => true,
                    }
            }
            ParserOptions::Trusted(_) => false,
        }
    }
}

impl PolicyData {
    /// From now on the policy's wrapper keeps its callbacks ([`Held::trace`]).
    pub(crate) fn trace(&mut self, scope: &mut v8::PinScope<'_, '_>, owner: Handle) {
        for cb in [
            &mut self.html,
            &mut self.script,
            &mut self.script_url,
            &mut self.parser_options,
        ]
        .into_iter()
        .flatten()
        {
            cb.trace(scope, owner);
        }
    }
}

/// A value at a sink: a trusted object, or a string.
pub enum TtValue {
    Trusted(Handle),
    String(String),
}

impl From<TrustedHTMLOrString> for TtValue {
    fn from(v: TrustedHTMLOrString) -> TtValue {
        match v {
            TrustedHTMLOrString::TrustedHTML(h) => TtValue::Trusted(h),
            TrustedHTMLOrString::String(s) => TtValue::String(s),
        }
    }
}

impl From<TrustedHTMLOrNullEmptyString> for TtValue {
    fn from(v: TrustedHTMLOrNullEmptyString) -> TtValue {
        match v {
            TrustedHTMLOrNullEmptyString::TrustedHTML(h) => TtValue::Trusted(h),
            TrustedHTMLOrNullEmptyString::String(s) => TtValue::String(s),
        }
    }
}

impl From<TrustedScriptOrString> for TtValue {
    fn from(v: TrustedScriptOrString) -> TtValue {
        match v {
            TrustedScriptOrString::TrustedScript(h) => TtValue::Trusted(h),
            TrustedScriptOrString::String(s) => TtValue::String(s),
        }
    }
}

impl From<TrustedScriptOrNullEmptyString> for TtValue {
    fn from(v: TrustedScriptOrNullEmptyString) -> TtValue {
        match v {
            TrustedScriptOrNullEmptyString::TrustedScript(h) => TtValue::Trusted(h),
            TrustedScriptOrNullEmptyString::String(s) => TtValue::String(s),
        }
    }
}

impl From<TrustedScriptURLOrString> for TtValue {
    fn from(v: TrustedScriptURLOrString) -> TtValue {
        match v {
            TrustedScriptURLOrString::TrustedScriptURL(h) => TtValue::Trusted(h),
            TrustedScriptURLOrString::String(s) => TtValue::String(s),
        }
    }
}

impl From<TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString> for TtValue {
    fn from(v: TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString) -> TtValue {
        use TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString as T;
        match v {
            T::TrustedHTML(h) | T::TrustedScript(h) | T::TrustedScriptURL(h) => TtValue::Trusted(h),
            T::String(s) => TtValue::String(s),
        }
    }
}

/// Trusted Types §4.3.? "get Trusted Types-compliant attribute value" table: the trusted type an
/// element's attribute takes (`None`: any string).
fn attribute_type(element_ns: &str, local: &str, attr_ns: &str, attr: &str) -> Option<TtKind> {
    let html = element_ns == &*ns!(html);
    let svg = element_ns == &*ns!(svg);
    if attr_ns.is_empty() && (html || svg || element_ns == &*ns!(mathml)) {
        // Event handler content attributes (of the event handlers there are).
        if attr
            .strip_prefix("on")
            .and_then(super::html::event_handler_name)
            .is_some()
        {
            return Some(TtKind::Script);
        }
    }
    if html && attr_ns.is_empty() {
        return match (local, attr) {
            ("iframe", "srcdoc") => Some(TtKind::Html),
            ("script", "src") => Some(TtKind::ScriptUrl),
            _ => None,
        };
    }
    if svg && local == "script" && attr == "href" && (attr_ns.is_empty() || attr_ns == &*ns!(xlink))
    {
        return Some(TtKind::ScriptUrl);
    }
    None
}

/// The trusted type of an element property (`getPropertyType`).
fn property_type(element_ns: &str, local: &str, property: &str) -> Option<TtKind> {
    if element_ns == &*ns!(html) {
        match (local, property) {
            ("iframe", "srcdoc") => return Some(TtKind::Html),
            ("script", "innerText" | "textContent" | "text") => return Some(TtKind::Script),
            ("script", "src") => return Some(TtKind::ScriptUrl),
            _ => {}
        }
    }
    matches!(property, "innerHTML" | "outerHTML").then_some(TtKind::Html)
}

impl Engine {
    fn factory<R>(&self, h: Handle, f: impl FnOnce(&mut FactoryData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::TrustedTypePolicyFactory(d) => f(d),
            _ => panic!("not a TrustedTypePolicyFactory"),
        }
    }

    /// The `trustedTypes` of a global (made on first use).
    pub(crate) fn trusted_types_of(&self, global: Handle) -> Handle {
        self.window_singleton(global, "trustedTypes", || {
            Object::TrustedTypePolicyFactory(Box::new(FactoryData {
                global,
                created: Vec::new(),
                default: None,
            }))
        })
    }

    fn trusted_data(&self, h: Handle) -> Option<(TtKind, String)> {
        match &*self.obj(h) {
            Object::TrustedHtml(s) => Some((TtKind::Html, s.clone())),
            Object::TrustedScript(s) => Some((TtKind::Script, s.clone())),
            Object::TrustedScriptUrl(s) => Some((TtKind::ScriptUrl, s.clone())),
            _ => None,
        }
    }

    fn new_trusted(&self, kind: TtKind, data: String) -> Handle {
        self.alloc(match kind {
            TtKind::Html => Object::TrustedHtml(data),
            TtKind::Script => Object::TrustedScript(data),
            TtKind::ScriptUrl => Object::TrustedScriptUrl(data),
        })
    }

    /// Whether `value` is a trusted object of `kind` (`isHTML()` and its siblings).
    fn is_trusted(&self, cx: &mut Cx, value: &JsValue, kind: TtKind) -> bool {
        let scope = cx.scope();
        let v = v8::Local::new(scope, value.global());
        let Ok(o) = v8::Local::<v8::Object>::try_from(v) else {
            return false;
        };
        // (the brand check of the three trusted interfaces: a node's handle is not an object's)
        rt::unwrap(&o, ids::TRUSTED_HTML, ids::TRUSTED_SCRIPT_URL_END)
            .and_then(|h| self.trusted_data(h))
            .is_some_and(|(k, _)| k == kind)
    }

    /// Trusted Types "get Trusted Type policy value": the policy's callback for `kind` with `value`
    /// and `arguments` (`None` when the callback returned null or undefined, or there is none and
    /// `throw_if_missing` is false).
    fn policy_value(
        &self,
        cx: &mut Cx,
        policy: Handle,
        kind: TtKind,
        value: &str,
        arguments: Vec<JsValue>,
        throw_if_missing: bool,
    ) -> rt::Result<Option<String>> {
        enum Cb {
            Html(CreateHTMLCallback),
            Script(CreateScriptCallback),
            ScriptUrl(CreateScriptURLCallback),
        }
        let (name, cb) = {
            let scope = cx.scope();
            match &*self.obj(policy) {
                Object::TrustedTypePolicy(p) => (
                    p.name.clone(),
                    match kind {
                        TtKind::Html => p
                            .html
                            .as_ref()
                            .and_then(|c| c.get(scope))
                            .map(|o| Cb::Html(CreateHTMLCallback(o))),
                        TtKind::Script => p
                            .script
                            .as_ref()
                            .and_then(|c| c.get(scope))
                            .map(|o| Cb::Script(CreateScriptCallback(o))),
                        TtKind::ScriptUrl => p
                            .script_url
                            .as_ref()
                            .and_then(|c| c.get(scope))
                            .map(|o| Cb::ScriptUrl(CreateScriptURLCallback(o))),
                    },
                ),
                _ => return Ok(None),
            }
        };
        let Some(cb) = cb else {
            if throw_if_missing {
                let member = match kind {
                    TtKind::Html => "createHTML",
                    TtKind::Script => "createScript",
                    TtKind::ScriptUrl => "createScriptURL",
                };
                return Err(Error::Type(
                    format!("Policy {name}'s TrustedTypePolicyOptions did not specify a '{member}' member.")
                        .into(),
                ));
            }
            return Ok(None);
        };
        let null = {
            let scope = cx.scope();
            JsValue::new(scope, v8::null(scope).into())
        };
        let this: Option<&dyn ToJs> = Some(&null);
        match cb {
            Cb::Html(f) => f.invoke(cx, this, value, arguments),
            Cb::Script(f) => f.invoke(cx, this, value, arguments),
            Cb::ScriptUrl(f) => f.invoke(cx, this, value, arguments),
        }
    }

    /// The document whose CSP list governs a sink of the global `global` (none for a worker here).
    fn global_document(&self, global: Handle) -> Option<NodeId> {
        self.associated_document(global)
    }

    /// Trusted Types §4.? "Get Trusted Type compliant string" for `value` at `sink` of `doc`.
    pub(crate) fn tt_compliant(
        &self,
        cx: &mut Cx,
        value: TtValue,
        kind: TtKind,
        doc: NodeId,
        sink: &str,
    ) -> rt::Result<String> {
        let input = match value {
            TtValue::Trusted(h) => match self.trusted_data(h) {
                Some((k, data)) if k == kind => return Ok(data),
                Some((_, data)) => data,
                None => String::new(),
            },
            TtValue::String(s) => s,
        };
        let Some(csp) = self.doc_csp(doc) else {
            return Ok(input);
        };
        let (enforced, report_only) = csp.requires_trusted_types_for_script();
        if !enforced && !report_only {
            return Ok(input);
        }
        // "Process value with a default policy".
        let default = self
            .window_of_document(doc)
            .map(|w| self.trusted_types_of(w))
            .and_then(|f| self.factory(f, |d| d.default));
        if let Some(policy) = default {
            let args = {
                let scope = cx.scope();
                let t = rt::name(scope, kind.name());
                let s = v8::String::new(scope, sink).ok_or(Error::Pending)?;
                vec![JsValue::new(scope, t.into()), JsValue::new(scope, s.into())]
            };
            if let Some(converted) = self.policy_value(cx, policy, kind, &input, args, false)? {
                return Ok(converted);
            }
        }
        let verdict = csp.trusted_types_sink_violation(sink, &input);
        self.report_verdict(cx, doc, &verdict);
        if verdict.blocked {
            return Err(Error::Type(
                format!("This document requires '{}' assignment.", kind.name()).into(),
            ));
        }
        Ok(input)
    }

    /// [`Engine::tt_compliant`] for an HTML sink of a node's document.
    pub(crate) fn tt_html(
        &self,
        cx: &mut Cx,
        value: impl Into<TtValue>,
        node: NodeId,
        sink: &str,
    ) -> rt::Result<String> {
        let doc = self.dom().node_document(node);
        self.tt_compliant(cx, value.into(), TtKind::Html, doc, sink)
    }

    /// Trusted Types "create a parser options callback argument": the run scripts flag, and the
    /// sanitizer as a new `Sanitizer` ("get a sanitizer from dictionary").
    fn parser_options_argument(
        &self,
        run_scripts: bool,
        sanitizer: Option<SanitizerInput>,
    ) -> rt::Result<SetHTMLUnsafeOptions> {
        let sanitizer = match sanitizer {
            None => None,
            Some(input) => {
                let c = self.sanitizer_configuration(input)?;
                let h = self.alloc(Object::Sanitizer(Box::new(c)));
                Some(SanitizerOrSanitizerConfigOrSanitizerPresets::Sanitizer(h))
            }
        };
        Ok(SetHTMLUnsafeOptions {
            run_scripts,
            sanitizer,
        })
    }

    /// A new `TrustedHTMLParserOptions` from a policy's options ("get a sanitizer config copy
    /// from dictionary" and its run scripts flag).
    fn new_parser_options(&self, value: SetHTMLUnsafeOptions) -> rt::Result<Handle> {
        let config = match value.sanitizer {
            None => None,
            Some(s) => Some(Box::new(self.sanitizer_configuration(s.into())?)),
        };
        Ok(self.alloc(Object::TrustedHtmlParserOptions(Box::new(
            ParserOptionsData {
                run_scripts: value.run_scripts,
                config,
            },
        ))))
    }

    /// The default policy's `createParserOptions` callback of the global whose document is `doc`.
    fn default_parser_options_callback(
        &self,
        cx: &mut Cx,
        doc: NodeId,
    ) -> Option<CreateParserOptionsCallback> {
        let policy = self
            .window_of_document(doc)
            .map(|w| self.trusted_types_of(w))
            .and_then(|f| self.factory(f, |d| d.default))?;
        let scope = cx.scope();
        match &*self.obj(policy) {
            Object::TrustedTypePolicy(p) => p
                .parser_options
                .as_ref()
                .and_then(|c| c.get(scope))
                .map(CreateParserOptionsCallback),
            _ => None,
        }
    }

    /// Options a sink of `doc` rejects for want of a policy: a violation, which throws unless the
    /// CSP list only reports it.
    fn parser_options_violation(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        sink: &str,
        input: ParserOptions,
    ) -> rt::Result<ParserOptions> {
        let Some(csp) = self.doc_csp(doc) else {
            return Ok(input);
        };
        let verdict = csp.trusted_types_sink_violation(sink, "");
        self.report_verdict(cx, doc, &verdict);
        if verdict.blocked {
            return Err(Error::Type(
                "This document requires 'TrustedHTMLParserOptions' for these options.".into(),
            ));
        }
        Ok(input)
    }

    /// Trusted Types "get trusted type compliant parser options" (`throwIfMissing` false) for a
    /// sink of `doc`'s global.
    fn tt_parser_options(
        &self,
        cx: &mut Cx,
        input: ParserOptions,
        doc: NodeId,
        sink: &str,
        from_author: bool,
    ) -> rt::Result<ParserOptions> {
        if matches!(input, ParserOptions::Trusted(_)) {
            return Ok(input);
        }
        let Some(csp) = self.doc_csp(doc) else {
            return Ok(input);
        };
        let (enforced, report_only) = csp.requires_trusted_types_for_script();
        if !enforced && !report_only {
            return Ok(input);
        }
        let require_policy = from_author && input.needs_vetting();
        let Some(callback) = self.default_parser_options_callback(cx, doc) else {
            if require_policy {
                return self.parser_options_violation(cx, doc, sink, input);
            }
            return Ok(input);
        };
        let ParserOptions::Dictionary {
            run_scripts,
            sanitizer,
        } = input
        else {
            return Ok(input);
        };
        let argument = self.parser_options_argument(run_scripts, sanitizer)?;
        let (null, args) = {
            let scope = cx.scope();
            let t = rt::name(scope, "TrustedHTMLParserOptions");
            let s = v8::String::new(scope, sink).ok_or(Error::Pending)?;
            (
                JsValue::new(scope, v8::null(scope).into()),
                vec![JsValue::new(scope, t.into()), JsValue::new(scope, s.into())],
            )
        };
        let this: Option<&dyn ToJs> = Some(&null);
        let fallback = ParserOptions::Dictionary {
            run_scripts: argument.run_scripts,
            sanitizer: argument.sanitizer.clone().map(Into::into),
        };
        match callback.invoke(cx, this, argument, args)? {
            None => self.parser_options_violation(cx, doc, sink, fallback),
            Some(v) => Ok(ParserOptions::Trusted(self.new_parser_options(v)?)),
        }
    }

    /// The document of the global whose Trusted Types requirements govern a sink called on
    /// `node` (its relevant global): the node's document when that has a Window, otherwise (an
    /// inert document: template contents, a parsed or created document) the current global's.
    pub(crate) fn tt_document(&self, cx: &mut Cx, node: NodeId) -> NodeId {
        let doc = self.dom().node_document(node);
        if self.window_of_document(doc).is_some() {
            return doc;
        }
        self.current_document(cx).unwrap_or(doc)
    }

    /// Trusted Types "get trusted type compliant input" for an unsafe HTML parsing sink of
    /// `doc`'s global (an HTML document): the markup, and the fragment parser mode its options
    /// give.
    pub(crate) fn tt_html_input(
        &self,
        cx: &mut Cx,
        value: impl Into<TtValue>,
        options: impl Into<ParserOptions>,
        doc: NodeId,
        sink: &str,
    ) -> rt::Result<(String, InsertMode)> {
        let value = value.into();
        let options = self.tt_parser_options(cx, options.into(), doc, sink, true)?;
        let markup = match (&options, value) {
            (ParserOptions::Trusted(_), TtValue::Trusted(h)) => {
                self.trusted_data(h).map(|(_, d)| d).unwrap_or_default()
            }
            (ParserOptions::Trusted(_), TtValue::String(s)) => s,
            (_, value) => self.tt_compliant(cx, value, TtKind::Html, doc, sink)?,
        };
        let mode = match options {
            ParserOptions::Dictionary {
                run_scripts,
                sanitizer,
            } => InsertMode::Unsafe {
                run_scripts,
                input: sanitizer,
            },
            ParserOptions::Trusted(h) => match &*self.obj(h) {
                Object::TrustedHtmlParserOptions(d) => InsertMode::Unsafe {
                    run_scripts: d.run_scripts,
                    input: d
                        .config
                        .as_ref()
                        .map(|c| SanitizerInput::Resolved((**c).clone())),
                },
                _ => InsertMode::Unsafe {
                    run_scripts: false,
                    input: None,
                },
            },
        };
        Ok((markup, mode))
    }

    /// Trusted Types "get Trusted Types-compliant attribute value" for `setAttribute()` and
    /// `setAttributeNS()` of `element`.
    pub(crate) fn tt_attribute(
        &self,
        cx: &mut Cx,
        element: NodeId,
        attr_ns: &str,
        attr_local: &str,
        value: TrustedHTMLOrTrustedScriptOrTrustedScriptURLOrString,
        sink: &str,
    ) -> rt::Result<String> {
        let (kind, doc) = {
            let dom = self.dom();
            let Some(el) = dom.element(element) else {
                return Ok(String::new());
            };
            (
                attribute_type(&el.name().ns, &el.name().local, attr_ns, attr_local),
                dom.node_document(element),
            )
        };
        let value: TtValue = value.into();
        match kind {
            Some(kind) => self.tt_compliant(cx, value, kind, doc, sink),
            None => Ok(match value {
                TtValue::String(s) => s,
                TtValue::Trusted(h) => self.trusted_data(h).map(|d| d.1).unwrap_or_default(),
            }),
        }
    }

    /// A TrustedScript's data (for the compilation hook).
    pub(crate) fn trusted_script_data(&self, h: Handle) -> Option<String> {
        match &*self.obj(h) {
            Object::TrustedScript(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// HTML's EnsureCSPDoesNotBlockStringCompilation, its Trusted Types step: whether `source` may
    /// be compiled (`eval`, `new Function`) when it is a string: it passes only through a default
    /// policy that returns it as it is. (TrustedScripts, and a `Function` made only of them, never
    /// get here: V8 tells the compilation hook they are code like.)
    pub(crate) fn tt_eval_allowed(&self, cx: &mut Cx, doc: NodeId, source: &str) -> bool {
        match self.tt_compliant(
            cx,
            TtValue::String(source.to_owned()),
            TtKind::Script,
            doc,
            "eval",
        ) {
            Ok(s) => s == source,
            Err(_) => {
                // (the TypeError is the compilation's EvalError: drop it)
                false
            }
        }
    }
}

impl TrustedTypePolicyFactoryImpl for Engine {
    fn empty_html(&self, _this: Handle) -> Handle {
        self.new_trusted(TtKind::Html, String::new())
    }
    fn empty_script(&self, _this: Handle) -> Handle {
        self.new_trusted(TtKind::Script, String::new())
    }
    fn default_policy(&self, this: Handle) -> Option<Handle> {
        self.factory(this, |d| d.default)
    }
    /// Trusted Types §2.3.1 `createPolicy()`.
    fn create_policy(
        &self,
        cx: &mut Cx,
        this: Handle,
        policy_name: &str,
        policy_options: TrustedTypePolicyOptions,
    ) -> rt::Result<Handle> {
        let (global, created, has_default) =
            self.factory(this, |d| (d.global, d.created.clone(), d.default.is_some()));
        // 1. The CSP list's `trusted-types` directives.
        if let Some(doc) = self.global_document(global) {
            if let Some(csp) = self.doc_csp(doc) {
                let verdict = csp.should_block_trusted_type_policy(policy_name, &created);
                self.report_verdict(cx, doc, &verdict);
                if verdict.blocked {
                    return Err(Error::Type(
                        format!("Policy \"{policy_name}\" disallowed.").into(),
                    ));
                }
            }
        }
        // 2.
        if policy_name == "default" && has_default {
            return Err(Error::Type("Policy \"default\" already exists.".into()));
        }
        // 3-6.
        let policy = self.alloc(Object::TrustedTypePolicy(Box::new(PolicyData {
            name: policy_name.to_owned(),
            html: policy_options.create_html.map(|c| Held::object(c.0)),
            script: policy_options.create_script.map(|c| Held::object(c.0)),
            script_url: policy_options.create_script_url.map(|c| Held::object(c.0)),
            parser_options: policy_options
                .create_parser_options
                .map(|c| Held::object(c.0)),
        })));
        self.factory(this, |d| {
            if policy_name == "default" {
                d.default = Some(policy);
            }
            d.created.push(policy_name.to_owned());
        });
        Ok(policy)
    }
    fn is_html(&self, cx: &mut Cx, _this: Handle, value: JsValue) -> rt::Result<bool> {
        Ok(self.is_trusted(cx, &value, TtKind::Html))
    }
    fn is_script(&self, cx: &mut Cx, _this: Handle, value: JsValue) -> rt::Result<bool> {
        Ok(self.is_trusted(cx, &value, TtKind::Script))
    }
    fn is_script_url(&self, cx: &mut Cx, _this: Handle, value: JsValue) -> rt::Result<bool> {
        Ok(self.is_trusted(cx, &value, TtKind::ScriptUrl))
    }
    /// Trusted Types §2.3.1 `getAttributeType()`.
    fn get_attribute_type(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        tag_name: &str,
        attribute: &str,
        element_ns: Option<&str>,
        attr_ns: Option<&str>,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let local = tag_name.to_ascii_lowercase();
        let attr = attribute.to_ascii_lowercase();
        let element_ns = match element_ns {
            Some("") | None => ns!(html).to_string(),
            Some(n) => n.to_owned(),
        };
        let attr_ns = attr_ns.unwrap_or("");
        Ok(attribute_type(&element_ns, &local, attr_ns, &attr).map(|k| DomStr::Borrowed(k.name())))
    }
    /// Trusted Types §2.3.1 `getPropertyType()`.
    fn get_property_type(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        tag_name: &str,
        property: &str,
        element_ns: Option<&str>,
    ) -> rt::Result<Option<DomStr<'_>>> {
        let local = tag_name.to_ascii_lowercase();
        let element_ns = match element_ns {
            Some("") | None => ns!(html).to_string(),
            Some(n) => n.to_owned(),
        };
        Ok(property_type(&element_ns, &local, property).map(|k| DomStr::Borrowed(k.name())))
    }
}

impl TrustedTypePolicyImpl for Engine {
    fn name(&self, this: Handle) -> DomStr<'_> {
        match &*self.obj(this) {
            Object::TrustedTypePolicy(p) => DomStr::Owned(p.name.clone()),
            _ => DomStr::Borrowed(""),
        }
    }
    /// Trusted Types §2.4.? "create a trusted type".
    fn create_html(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: &str,
        arguments: Vec<JsValue>,
    ) -> rt::Result<Handle> {
        let data = self.policy_value(cx, this, TtKind::Html, input, arguments, true)?;
        Ok(self.new_trusted(TtKind::Html, data.unwrap_or_default()))
    }
    fn create_script(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: &str,
        arguments: Vec<JsValue>,
    ) -> rt::Result<Handle> {
        let data = self.policy_value(cx, this, TtKind::Script, input, arguments, true)?;
        Ok(self.new_trusted(TtKind::Script, data.unwrap_or_default()))
    }
    fn create_script_url(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: &str,
        arguments: Vec<JsValue>,
    ) -> rt::Result<Handle> {
        let data = self.policy_value(cx, this, TtKind::ScriptUrl, input, arguments, true)?;
        Ok(self.new_trusted(TtKind::ScriptUrl, data.unwrap_or_default()))
    }
    /// Trusted Types "create a trusted type" for "TrustedHTMLParserOptions".
    fn create_parser_options(
        &self,
        cx: &mut Cx,
        this: Handle,
        options: SetHTMLUnsafeOptions,
        arguments: Vec<JsValue>,
    ) -> rt::Result<Handle> {
        let (name, callback) = {
            let scope = cx.scope();
            match &*self.obj(this) {
                Object::TrustedTypePolicy(p) => (
                    p.name.clone(),
                    p.parser_options
                        .as_ref()
                        .and_then(|c| c.get(scope))
                        .map(CreateParserOptionsCallback),
                ),
                _ => return Err(Error::Type("Not a TrustedTypePolicy".into())),
            }
        };
        let Some(callback) = callback else {
            return Err(Error::Type(
                format!(
                    "Policy {name}'s TrustedTypePolicyOptions did not specify a \
                     'createParserOptions' member."
                )
                .into(),
            ));
        };
        let argument =
            self.parser_options_argument(options.run_scripts, options.sanitizer.map(Into::into))?;
        let null = {
            let scope = cx.scope();
            JsValue::new(scope, v8::null(scope).into())
        };
        let this_arg: Option<&dyn ToJs> = Some(&null);
        match callback.invoke(cx, this_arg, argument, arguments)? {
            None => Err(Error::Type(
                format!("Policy {name}'s createParserOptions returned null.").into(),
            )),
            Some(v) => self.new_parser_options(v),
        }
    }
}

impl TrustedHTMLParserOptionsImpl for Engine {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_and_property_types() {
        let html = ns!(html).to_string();
        let svg = ns!(svg).to_string();
        assert_eq!(
            attribute_type(&html, "div", "", "onclick"),
            Some(TtKind::Script)
        );
        assert_eq!(
            attribute_type(&html, "iframe", "", "srcdoc"),
            Some(TtKind::Html)
        );
        assert_eq!(
            attribute_type(&html, "script", "", "src"),
            Some(TtKind::ScriptUrl)
        );
        assert_eq!(
            attribute_type(&svg, "script", "", "href"),
            Some(TtKind::ScriptUrl)
        );
        assert_eq!(attribute_type(&html, "div", "", "title"), None);
        assert_eq!(attribute_type(&html, "div", "", "one"), None);
        assert_eq!(property_type(&html, "div", "innerHTML"), Some(TtKind::Html));
        assert_eq!(property_type(&html, "script", "text"), Some(TtKind::Script));
        assert_eq!(property_type(&html, "div", "text"), None);
    }
}
