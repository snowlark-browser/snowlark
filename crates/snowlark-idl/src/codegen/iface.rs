// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Emission of the per-interface and per-mixin files: the trait the engine implements, the V8 callbacks
//! (attribute accessors, operations with overload resolution, Fast API getters, interceptors,
//! constructors) and, for interfaces, the function that builds the `FunctionTemplate`.

use super::conv::Cat;
use super::names::*;
use super::plan::*;
use super::types::HEADER;
use crate::ast::*;
use std::collections::BTreeSet;
use std::fmt::Write;

const CB_SIG: &str =
    "scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue";
const GENERICS: &str = "<D: Dom, const LO: u16, const HI: u16>";
const THIS: &str = "let Some(this) = rt::unwrap(&args.this(), LO, HI) else { return rt::throw_illegal_invocation(scope) };";
/// The receiver of a member whose template has a receiver signature: V8 checked it before the call, so it
/// is read with no brand check (Blink's generated bindings use `ToWrappableUnsafe` there).
const THIS_UNCHECKED: &str = "// SAFETY: this member's template has a receiver signature, which V8 checked before the call.\n    let this = unsafe { rt::receiver(&args.this()) };";
/// The brand check of a promise-returning member: a promise rejected with the TypeError.
const THIS_PROMISE: &str = "let Some(this) = rt::unwrap(&args.this(), LO, HI) else { if let Some(p) = rt::rejected_illegal_invocation(scope) { rv.set(p.into()) } return };";

/// The kinds of access to a member, for CrossOriginProperties.
#[derive(Clone, Copy)]
enum Access {
    Get,
    Set,
    Call,
}

/// An expression usable before `else` in `let ... else`: parenthesized when it ends with a block.
fn le(expr: &str) -> String {
    if expr.trim_end().ends_with('}') {
        format!("({expr})")
    } else {
        expr.to_string()
    }
}

/// A string conversion for a value passed on as `&str`, into a stack buffer named `{var}_buf`
/// (declared by the returned statement) so short strings are not allocated: the declaration and the
/// conversion, or `None` for other conversions.
fn buffered_str(conv: &str, rust_param: &str, var: &str) -> Option<(String, String)> {
    if rust_param != "&str" {
        return None;
    }
    let value = conv
        .strip_prefix("rt::to_dom_string(scope, ")
        .or_else(|| conv.strip_prefix("rt::to_usv_string(scope, "))?
        .strip_suffix(')')?;
    Some((
        format!("let mut {var}_buf = [std::mem::MaybeUninit::<u8>::uninit(); 64];"),
        format!("rt::to_dom_str(scope, {value}, &mut {var}_buf)"),
    ))
}

/// How a converted argument is passed to its trait method.
fn pass(rust_param: &str, var: &str) -> String {
    match rust_param {
        "&str" => format!("&{var}"),
        "Option<&str>" => format!("{var}.as_deref()"),
        _ => var.to_string(),
    }
}

/// The first statement of a generated callback: counts the call of `what` when profiling
/// (`rt::prof`, `SNOWLARK_PROFILE=1`).
fn prof(what: &str) -> String {
    format!("let _prof = rt::prof::enter(scope, {});", lit(what))
}

/// Sets a primitive result on a `ReturnValue` without allocating a handle.
fn set_prim(rust: &str, v: &str) -> Option<String> {
    Some(match rust {
        "bool" => format!("rv.set_bool({v});"),
        "i32" => format!("rv.set_int32({v});"),
        "i8" | "i16" => format!("rv.set_int32({v}.into());"),
        "u32" => format!("rv.set_uint32({v});"),
        "u8" | "u16" => format!("rv.set_uint32({v}.into());"),
        "f32" => format!("rv.set_double({v}.into());"),
        "f64" => format!("rv.set_double({v});"),
        "i64" | "u64" => format!("rv.set_double({v} as f64);"),
        "()" => "rv.set_undefined();".into(),
        _ => return None,
    })
}

/// Sets a light getter's result ([`rt::Call`]): in place for primitives and platform objects, through
/// a scope for the rest.
fn light_result(rust: &str, v: &str) -> String {
    match rust {
        "bool" => format!("call.return_bool({v});"),
        "i32" => format!("call.return_i32({v});"),
        "i8" | "i16" | "u8" | "u16" => format!("call.return_i32({v}.into());"),
        "u32" => format!("call.return_u32({v});"),
        "f32" => format!("call.return_f64({v}.into());"),
        "f64" => format!("call.return_f64({v});"),
        "i64" | "u64" => format!("call.return_f64({v} as f64);"),
        "Handle" => format!("call.return_handle(Some({v}));"),
        "Option<Handle>" => format!("call.return_handle({v});"),
        _ => format!("call.return_value(&{v});"),
    }
}

impl Plan<'_> {
    fn trait_path(&self, owner: &str) -> String {
        self.traits[owner].trait_name.clone()
    }

    fn is_frozen(&self, ty: &Type) -> bool {
        matches!(self.prune(ty).kind, crate::ast::TypeKind::FrozenArray(_))
    }

    /// `let Some(js) = <v>.to_js(scope) else { return }; rv.set(js);` (or the primitive setter).
    fn set_result(&self, ty: &Type, rust: &str, v: &str, same_object: Option<&str>) -> String {
        if same_object.is_none() {
            if let Some(s) = set_prim(rust, v) {
                return s;
            }
        }
        // A platform object: its cached wrapper goes straight into the return value (Blink's
        // `DOMDataStore::SetReturnValueFromInlineStorage`).
        if same_object.is_none() && !self.is_frozen(ty) {
            match rust {
                "Handle" => return format!("rt::return_handle(scope, &mut rv, Some({v}));"),
                "Option<Handle>" => return format!("rt::return_handle(scope, &mut rv, {v});"),
                _ => {}
            }
        }
        let conv = if self.is_frozen(ty) && rust.starts_with("Option<") {
            format!("{v}.map(rt::Frozen).to_js(scope)")
        } else if self.is_frozen(ty) {
            format!("rt::Frozen({v}).to_js(scope)")
        } else {
            format!("{v}.to_js(scope)")
        };
        let lec = le(&conv);
        let cache = same_object
            .map(|k| {
                format!(
                    "rt::same_object_set(scope, args.this(), {}, js);",
                    name_key(k)
                )
            })
            .unwrap_or_default();
        format!("let Some(js) = {lec} else {{ return }}; {cache} rv.set(js);")
    }

    fn ce_push(&self, on: bool) -> &'static str {
        if on {
            "<D as rt::Host>::push_ce_reactions(dom);"
        } else {
            ""
        }
    }

    fn ce_pop(&self, on: bool) -> &'static str {
        if on {
            "{ let mut cx = Cx::new(scope); <D as rt::Host>::pop_ce_reactions(dom, &mut cx); }"
        } else {
            ""
        }
    }

    // ---- files ------------------------------------------------------------------------------------

    /// The file of an interface or mixin (owner).
    pub fn emit_owner(&self, owner: &str) -> String {
        let t = &self.traits[owner];
        let mut o = String::new();
        o.push_str(HEADER);
        let def = self
            .model
            .interfaces
            .get(owner)
            .or_else(|| self.model.mixins.get(owner))
            .unwrap();
        writeln!(
            o,
            "//! `{}{owner}{}` ({}).\n",
            if t.is_mixin {
                "interface mixin "
            } else {
                "interface "
            },
            def.inherits
                .as_ref()
                .map(|p| format!(" : {p}"))
                .unwrap_or_default(),
            def.span.file
        )
        .unwrap();
        o.push_str(
            "#![allow(\n    unused_imports,\n    unused_variables,\n    unused_mut,\n    clippy::let_unit_value,\n    // Every callback has the same generic shape, also those that do not call the engine.\n    clippy::extra_unused_type_parameters\n)]\n\n",
        );
        o.push_str("use super::*;\nuse crate::rt::{self, Cx, DomStr, Handle, JsObject, JsPromise, JsValue, Record, Result, ToJs};\n\n");
        self.emit_trait(&mut o, t);

        // Callbacks of this owner's members, once each (a mixin's members serve every inclusion).
        let mut done = BTreeSet::new();
        let mut fast = Vec::new();
        for ip in self.ifaces.iter().chain(&self.namespaces) {
            for a in ip.attrs.iter().filter(|a| a.owner == owner) {
                if done.insert(format!("attr:{}:{}", a.is_static, a.name)) {
                    self.emit_attr(&mut o, a, &mut fast);
                }
            }
            for op in ip.ops.iter().filter(|op| op.owner == owner) {
                if done.insert(format!("op:{}:{}", op.is_static, op.name)) {
                    self.emit_op(&mut o, op);
                }
            }
        }
        if !fast.is_empty() {
            o.push_str("/// Fast API C functions of this file's getters, per receiver brand range.\npub(crate) struct Fast<D, const LO: u16, const HI: u16>(std::marker::PhantomData<D>);\n\n");
            writeln!(o, "impl{GENERICS} Fast<D, LO, HI> {{").unwrap();
            for (name, f, info) in &fast {
                writeln!(
                    o,
                    "    pub(crate) const {name}: &'static [v8::fast_api::CFunction] = &[v8::fast_api::CFunction::new({f}::<D, LO, HI> as *const std::ffi::c_void, &rt::{info})];"
                )
                .unwrap();
            }
            o.push_str("}\n\n");
        }
        if let Some(ip) = self.ifaces.iter().find(|i| i.name == owner) {
            self.emit_interface(&mut o, ip);
        }
        if let Some(ip) = self.namespaces.iter().find(|i| i.name == owner) {
            self.emit_namespace(&mut o, ip);
        }
        o
    }

    /// The namespace object of a namespace (Web IDL §3.13.1): an ordinary object whose [[Prototype]] is
    /// %Object.prototype%, with an operation property per regular operation and `@@toStringTag`.
    fn emit_namespace(&self, o: &mut String, ip: &IfacePlan) {
        writeln!(
            o,
            "/// Creates the `{}` namespace object in the current context.",
            ip.name
        )
        .unwrap();
        o.push_str("pub(crate) fn namespace_object<'s, D: Dom>(scope: &mut v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Object>> {\n    const LO: u16 = 0;\n    const HI: u16 = 0;\n    let t = v8::ObjectTemplate::new(scope);\n");
        writeln!(o, "    rt::to_string_tag(scope, t, {});", lit(&ip.name)).unwrap();
        for op in &ip.ops {
            let file = &self.traits[&op.owner].file;
            let len = op
                .overloads
                .iter()
                .map(|ov| self.required(ov))
                .min()
                .unwrap_or(0);
            writeln!(
                o,
                "    {{\n        let f = rt::method(scope, rt::builder(super::{file}::{}::<D, LO, HI>), {}, {len}, None);\n        rt::operation(scope, &t, {}, f, false);\n    }}",
                self.op_fn(op),
                lit(&op.name),
                lit(&op.name)
            )
            .unwrap();
        }
        o.push_str("    t.new_instance(scope)\n}\n");
    }

    /// The members of a value asynchronously iterable declaration (Web IDL §3.7.10): `values`
    /// (also `@@asyncIterator`, set per context), and the asynchronous iterator prototype object's
    /// `next` and `return` with the step functions they chain on the ongoing promise.
    fn emit_async_iterator(&self, o: &mut String, ip: &IfacePlan) {
        let Some(ai) = &ip.async_iterable else {
            return;
        };
        let tr = self.trait_path(&ip.name);
        let id = upper(&ip.name);
        // `values()`: the arguments, converted as the declaration declares them.
        writeln!(
            o,
            "/// `{0}.prototype.values()`: a new default asynchronous iterator object.\nfn aiter_values<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    {THIS}\n    let argc = args.length();",
            ip.name
        )
        .unwrap();
        let mut passes = String::new();
        for (i, a) in ai.args.iter().enumerate() {
            let what = format!(
                "Failed to execute 'values' on '{}': parameter {}",
                ip.name,
                i + 1
            );
            let conv = self.conv(&a.ty, "v", &what);
            let param_rust = self.rust_arg(&a.ty, false);
            let value = match &a.default {
                Some(d) => {
                    let default = self.default_value(&a.ty, d);
                    format!("if argc > {i} && !v.is_undefined() {{ let Some(x) = {conv} else {{ return }}; x }} else {{ {default} }}")
                }
                None if param_rust.starts_with("Option<") => format!(
                    "if argc > {i} && !v.is_undefined() {{ let Some(x) = {conv} else {{ return }}; x }} else {{ None }}"
                ),
                None => format!(
                    "if argc > {i} && !v.is_undefined() {{ let Some(x) = {conv} else {{ return }}; Some(x) }} else {{ None }}"
                ),
            };
            writeln!(o, "    let a{i} = {{ let v = args.get({i}); {value} }};").unwrap();
            passes.push_str(&format!(", a{i}"));
        }
        writeln!(
            o,
            "    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{}(dom, &mut cx, this{passes}) }};\n    let Some(state) = rt::check(scope, r) else {{ return }};\n    if let Some(it) = rt::async_iterator_new(scope, args.this(), ids::{id}, state) {{ rv.set(it.into()) }}\n}}\n",
            ai.init
        )
        .unwrap();
        writeln!(
            o,
            "/// `%{0}AsyncIteratorPrototype%.next()` (values of type {1}).\nfn aiter_next<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let this: v8::Local<v8::Value> = v8::Local::new(scope, args.this()).into();\n    if let Some(p) = rt::async_iterator_next(scope, this, ids::{id}, aiter_next_steps::<D, LO, HI>) {{ rv.set(p) }}\n}}\n",
            ip.name,
            ai.value.idl().replace('`', "'")
        )
        .unwrap();
        writeln!(
            o,
            "/// The nextSteps of `next()` (its data is the iterator object).\nfn aiter_next_steps<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let data = v8::Local::new(scope, args.data());\n    let Ok(it) = v8::Local::<v8::Object>::try_from(data) else {{ return }};\n    let p = rt::async_iterator_next_steps(scope, it, |scope, target, state| {{\n        let this = rt::unwrap(&target, LO, HI)?;\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{}(dom, &mut cx, this, state) }};\n        let p = rt::check(scope, r)?;\n        rt::promise_of(scope, &p)\n    }});\n    if let Some(p) = p {{ rv.set(p) }}\n}}\n",
            ai.next
        )
        .unwrap();
        if let Some(ret) = &ai.ret {
            writeln!(
                o,
                "/// `%{0}AsyncIteratorPrototype%.return(value)`.\nfn aiter_return<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let this: v8::Local<v8::Value> = v8::Local::new(scope, args.this()).into();\n    let value = v8::Local::new(scope, args.get(0));\n    if let Some(p) = rt::async_iterator_return(scope, this, ids::{id}, value, aiter_return_steps::<D, LO, HI>) {{ rv.set(p) }}\n}}\n",
                ip.name
            )
            .unwrap();
            writeln!(
                o,
                "/// The returnSteps of `return()` (its data is `[iterator, value]`).\nfn aiter_return_steps<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let data = v8::Local::new(scope, args.data());\n    let p = rt::async_iterator_return_steps(scope, data, |scope, target, state, value| {{\n        let this = rt::unwrap(&target, LO, HI)?;\n        let value = JsValue::new(scope, value);\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{ret}(dom, &mut cx, this, state, value) }};\n        let p = rt::check(scope, r)?;\n        rt::promise_of(scope, &p)\n    }});\n    if let Some(p) = p {{ rv.set(p) }}\n}}\n"
            )
            .unwrap();
        }
    }

    /// The default iterator members of a pair iterator declaration (Web IDL §3.7.10), and the `next`
    /// method of the interface's iterator prototype object (§3.7.10.1).
    fn emit_pair_iterator(&self, o: &mut String, ip: &IfacePlan) {
        let tr = self.trait_path(&ip.name);
        // A setlike iterates over (value, value) pairs.
        let pair_call = match (&ip.pair_iterable, &ip.setlike) {
            (Some((_, _, m)), _) => format!("<D as {tr}>::{m}(dom, this, index)"),
            (None, Some(sl)) => {
                // (an interface's values are handles, which are `Copy`)
                let copy = matches!(sl.value.kind, TypeKind::Named(_));
                format!(
                    "<D as {tr}>::{}(dom, this, index).map(|v| ({}, v))",
                    sl.at,
                    if copy { "v" } else { "v.clone()" }
                )
            }
            _ => return,
        };
        if let Some(sl) = &ip.setlike {
            self.emit_setlike(o, ip, sl);
        }
        let id = upper(&ip.name);
        for (fname, kind) in [("entries", 0), ("keys", 1), ("values", 2)] {
            writeln!(
                o,
                "/// `{0}.prototype.{fname}` (a default iterator object of kind {kind}).\nfn iter_{fname}<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    if rt::unwrap(&args.this(), LO, HI).is_none() {{ return rt::throw_illegal_invocation(scope) }}\n    if let Some(it) = rt::pair_iterator_new(scope, args.this(), ids::{id}, {kind}) {{ rv.set(it.into()) }}\n}}\n",
                ip.name
            )
            .unwrap();
        }
        writeln!(
            o,
            "/// `%{0}IteratorPrototype%.next()`.\nfn iter_next<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let Some((target, kind, index)) = rt::pair_iterator_state(scope, args.this(), ids::{id}) else {{ return rt::throw_type_error(scope, \"next() called on an object that is not a {0} iterator\") }};\n    let Some(this) = rt::unwrap(&target, LO, HI) else {{ return rt::throw_illegal_invocation(scope) }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let pair = {pair_call};\n    let value = match pair {{\n        None => {{ rt::pair_iterator_finish(scope, args.this()); None }}\n        Some((k, v)) => {{\n            rt::pair_iterator_set_index(scope, args.this(), index + 1);\n            let Some(k) = k.to_js(scope) else {{ return }};\n            let Some(v) = v.to_js(scope) else {{ return }};\n            Some(rt::pair_iterator_value(scope, kind, k, v))\n        }}\n    }};\n    rv.set(rt::iter_result(scope, value).into());\n}}\n",
            ip.name
        )
        .unwrap();
        writeln!(
            o,
            "/// `{0}.prototype.forEach(callback, thisArg)`.\nfn iter_for_each<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let Some(this) = rt::unwrap(&args.this(), LO, HI) else {{ return rt::throw_illegal_invocation(scope) }};\n    let Ok(callback) = v8::Local::<v8::Function>::try_from(args.get(0)) else {{ return rt::throw_type_error(scope, \"The callback provided as parameter 1 is not a function.\") }};\n    let this_arg = args.get(1);\n    let receiver: v8::Local<v8::Value> = args.this().into();\n    let mut i = 0u32;\n    loop {{\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        let index = i;\n        let Some((k, v)) = {pair_call} else {{ break }};\n        let Some(k) = k.to_js(scope) else {{ return }};\n        let Some(v) = v.to_js(scope) else {{ return }};\n        if callback.call(scope, this_arg, &[v, k, receiver]).is_none() {{ return }}\n        i += 1;\n    }}\n    rv.set_undefined();\n}}\n",
            ip.name
        )
        .unwrap();
    }

    fn emit_setlike(&self, o: &mut String, ip: &IfacePlan, sl: &SetlikePlan) {
        let tr = self.trait_path(&ip.name);
        let head = "    let Some(this) = rt::unwrap(&args.this(), LO, HI) else { return rt::throw_illegal_invocation(scope) };\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n";
        let iface = match &sl.value.kind {
            TypeKind::Named(n) => Some(upper(n)),
            _ => None,
        };
        let arg = match &iface {
            Some(id) => format!("    let Some(value) = rt::unwrap_value(args.get(0), ids::{id}, ids::{id}_END) else {{ return rt::throw_type_error(scope, \"parameter 1 is not of the expected type\") }};\n"),
            None => "    let Some(value) = rt::to_dom_string(scope, args.get(0)) else { return };\n".to_string(),
        };
        let by_ref = if iface.is_some() { "" } else { "&" };
        writeln!(
            o,
            "/// `{0}.prototype.size` (setlike).\nfn setlike_size_get<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n{head}    rv.set_uint32(<D as {tr}>::{1}(dom, this));\n}}\n",
            ip.name, sl.size
        )
        .unwrap();
        writeln!(
            o,
            "/// `{0}.prototype.has(value)` (setlike).\nfn setlike_has<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n{head}{arg}    rv.set_bool(<D as {tr}>::{1}(dom, this, {by_ref}value));\n}}\n",
            ip.name, sl.has
        )
        .unwrap();
        if let (Some(add), Some(delete), Some(clear)) = (&sl.add, &sl.delete, &sl.clear) {
            writeln!(
                o,
                "/// `{0}.prototype.add(value)` (setlike): returns the set.\nfn setlike_add<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n{head}{arg}    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{add}(dom, &mut cx, this, &value) }};\n    if rt::check(scope, r).is_some() {{ rv.set(args.this().into()) }}\n}}\n",
                ip.name
            )
            .unwrap();
            writeln!(
                o,
                "/// `{0}.prototype.delete(value)` (setlike).\nfn setlike_delete<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n{head}{arg}    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{delete}(dom, &mut cx, this, &value) }};\n    if let Some(b) = rt::check(scope, r) {{ rv.set_bool(b) }}\n}}\n",
                ip.name
            )
            .unwrap();
            writeln!(
                o,
                "/// `{0}.prototype.clear()` (setlike).\nfn setlike_clear<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n{head}    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{clear}(dom, &mut cx, this) }};\n    if rt::check(scope, r).is_some() {{ rv.set_undefined() }}\n}}\n",
                ip.name
            )
            .unwrap();
        }
    }

    fn emit_trait(&self, o: &mut String, t: &TraitPlan) {
        writeln!(
            o,
            "/// The engine's implementation of `{}`: one method per attribute getter and setter and per\n/// operation overload, in IDL order. Reflected attributes are not here: they go through `rt::Reflect`.",
            t.owner
        )
        .unwrap();
        // (WebGL extension interfaces are named in the registry's style, `EXT_sRGB`)
        let camel = if t.trait_name.contains('_') {
            ", non_camel_case_types"
        } else {
            ""
        };
        writeln!(
            o,
            "#[allow(clippy::too_many_arguments{camel})]\npub trait {} {{",
            t.trait_name
        )
        .unwrap();
        for m in &t.methods {
            for line in m.doc.lines() {
                writeln!(o, "    /// {}", line.replace('`', "'")).unwrap();
            }
            let mut params = vec!["&self".to_string()];
            if m.cx {
                params.push("cx: &mut Cx".into());
            }
            if m.this {
                params.push("this: Handle".into());
            }
            for (n, t) in &m.params {
                params.push(format!("{n}: {t}"));
            }
            let sig = if m.ret == "()" {
                format!("fn {}({})", m.name, params.join(", "))
            } else {
                format!("fn {}({}) -> {}", m.name, params.join(", "), m.ret)
            };
            writeln!(o, "    #[cfg(not(feature = \"stubs\"))]\n    {sig};").unwrap();
            writeln!(
                o,
                "    #[cfg(feature = \"stubs\")]\n    {sig} {{\n        rt::stub({})\n    }}",
                lit(&format!("{}.{}", t.owner, m.name))
            )
            .unwrap();
        }
        o.push_str("}\n\n");
    }

    // ---- attributes -------------------------------------------------------------------------------

    /// Whether an attribute's getter is a light callback ([`rt::Call`], `rt::light_builder`): V8 checked
    /// its receiver, its engine call takes no `Cx` and cannot throw, and nothing is cached on the
    /// receiver, so the call needs a scope only for a result that must be allocated.
    fn light_getter(&self, a: &AttrPlan) -> bool {
        !a.is_static
            && !a.same_object
            && !a.cached
            && !a.getter_fallible
            && matches!(a.getter, Getter::Trait(_))
            && !self.is_frozen(&a.ty)
            && self.rust_ret(&a.ty) != "()"
            && self.attr_this(a, Access::Get) == THIS_UNCHECKED
    }

    /// The attribute of an interface's stringifier when its getter is a light callback.
    fn light_stringifier<'p>(&self, ip: &'p IfacePlan) -> Option<&'p AttrPlan> {
        let Some(Stringifier::Attribute(a)) = &ip.stringifier else {
            return None;
        };
        let attr = ip.attrs.iter().find(|x| &x.name == a)?;
        self.light_getter(attr).then_some(attr)
    }

    /// Whether an operation is a light callback ([`rt::Call`], `rt::light_builder`): V8 checked its
    /// receiver against the template's signature, and it has one overload with no variadic argument.
    /// It reads its arguments in place and makes one scope, for their conversions and the engine call;
    /// a result that is a platform object or a primitive is stored in place after it.
    fn light_op(&self, op: &OpPlan) -> bool {
        !op.is_static
            && op.name != "constructor"
            && !op.name.starts_with("factory")
            && !op.promise
            && !self.cross_origin_member(&op.owner, &op.name, Access::Call)
            && self.unchecked_receiver(&op.owner)
            && op.overloads.len() == 1
            && !op.overloads[0].args.iter().any(|a| a.variadic)
    }

    /// The builder of an operation's function template.
    fn op_builder(&self, op: &OpPlan) -> &'static str {
        if self.light_op(op) {
            "light_builder"
        } else {
            "builder"
        }
    }

    fn attr_fn(&self, a: &AttrPlan, suffix: &str) -> String {
        // The CSS property attributes come in three spellings of one name (`webkitTransform`,
        // `WebkitTransform`, `-webkit-transform`): the function names keep them apart.
        if matches!(a.getter, Getter::CssProperty(_)) {
            if a.name.contains('-') {
                return format!("attr_prop_dash_{}_{suffix}", a.name.replace('-', "_"));
            }
            let s = snake(&a.name);
            let s = s.trim_end_matches('_');
            if a.name.starts_with(|c: char| c.is_ascii_uppercase()) {
                return format!("attr_prop_upper_{s}_{suffix}");
            }
            return format!("attr_prop_{s}_{suffix}");
        }
        let s = snake(&a.name);
        let s = s.trim_end_matches('_');
        if a.is_static {
            format!("static_{s}_{suffix}")
        } else {
            format!("attr_{s}_{suffix}")
        }
    }

    fn emit_attr(&self, o: &mut String, a: &AttrPlan, fast: &mut Vec<(String, String, String)>) {
        if let Getter::ObservableArray(p) = &a.getter {
            self.emit_observable_array(o, a, p);
            return;
        }
        let tr = self.trait_path(&a.owner);
        let this_line = self.attr_this(a, Access::Get);
        let this_arg = if a.is_static { "" } else { ", this" };
        // Getter.
        writeln!(o, "/// Getter of `{}`.", a.idl.replace('`', "'")).unwrap();
        if self.light_getter(a) {
            let Getter::Trait(m) = &a.getter else {
                unreachable!()
            };
            writeln!(
                o,
                "pub(crate) fn {}{GENERICS}(call: rt::Call) {{\n    let _prof = call.prof({});\n    // SAFETY: this member's template has a receiver signature, which V8 checked before the call.\n    let this = unsafe {{ call.receiver() }};\n    let dom = call.state().engine::<D>();\n    let v = <D as {tr}>::{m}(dom, this);\n    {}\n}}\n",
                self.attr_fn(a, "get"),
                lit(&format!("get {}.{}", a.owner, a.name)),
                light_result(&self.rust_ret(&a.ty), "v")
            )
            .unwrap();
        } else {
            writeln!(
                o,
                "pub(crate) fn {}{GENERICS}({CB_SIG}) {{",
                self.attr_fn(a, "get")
            )
            .unwrap();
            writeln!(o, "    {}", prof(&format!("get {}.{}", a.owner, a.name))).unwrap();
            writeln!(o, "    {this_line}").unwrap();
            let same_key = format!("{}.{}", a.owner, a.name);
            if a.same_object {
                writeln!(
                o,
                "    if let Some(v) = rt::same_object_get(scope, args.this(), {}) {{ rv.set(v); return; }}",
                name_key(&same_key)
            )
            .unwrap();
            }
            o.push_str("    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n");
            let same = a.same_object.then_some(same_key.as_str());
            match &a.getter {
                Getter::Trait(m) => {
                    let rust = self.rust_ret(&a.ty);
                    if a.getter_fallible {
                        writeln!(
                        o,
                        "    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{m}(dom, &mut cx{this_arg}) }};\n    let Some(v) = rt::check(scope, r) else {{ return }};"
                    )
                    .unwrap();
                    } else {
                        writeln!(o, "    let v = <D as {tr}>::{m}(dom{this_arg});").unwrap();
                    }
                    writeln!(o, "    {}", self.set_result(&a.ty, &rust, "v", same)).unwrap();
                }
                Getter::Reflect(rule) => o.push_str(&self.reflect_get(rule, same)),
                Getter::ObservableArray(_) => unreachable!("emitted by emit_observable_array"),
                Getter::CssProperty(property) => {
                    let rust = self.rust_ret(&a.ty);
                    writeln!(
                        o,
                        "    let v = <D as {tr}>::css_property(dom, this, {});",
                        lit(property)
                    )
                    .unwrap();
                    writeln!(o, "    {}", self.set_result(&a.ty, &rust, "v", None)).unwrap();
                }
                Getter::SvgReflect => {
                    writeln!(
                        o,
                        "    let v = <D as {}>::svg_reflect(dom, this, {}, {});",
                        self.trait_path("SVGElement"),
                        lit(&a.owner),
                        lit(&a.name)
                    )
                    .unwrap();
                    writeln!(o, "    {}", self.set_result(&a.ty, "Handle", "v", same)).unwrap();
                }
                Getter::EventHandler(event) => {
                    writeln!(
                    o,
                    "    let r = {{ let mut cx = Cx::new(scope); <D as rt::EventHandlers>::get_event_handler(dom, &mut cx, this, {}) }};\n    let Some(v) = rt::check(scope, r) else {{ return }};\n    match v {{ Some(f) => {{ let Some(js) = f.to_js(scope) else {{ return }}; rv.set(js); }} None => rv.set_null() }}",
                    lit(event)
                )
                .unwrap();
                }
            }
            o.push_str("}\n\n");
        }

        // Fast API getter.
        if let (Some(f), Getter::Trait(m)) = (a.fast, &a.getter) {
            if !a.same_object && !self.security_checked_attr(a) {
                let (ret, info, cast) = match f {
                    FastRet::Bool => ("bool", "FAST_GETTER_BOOL", ""),
                    FastRet::I32 => ("i32", "FAST_GETTER_I32", ".into()"),
                    FastRet::U32 => ("u32", "FAST_GETTER_U32", ".into()"),
                    FastRet::F64 => ("f64", "FAST_GETTER_F64", ".into()"),
                };
                let name = self.attr_fn(a, "fast");
                writeln!(
                    o,
                    "/// Fast API variant of the `{}` getter: called directly from optimized code.",
                    a.name
                )
                .unwrap();
                writeln!(
                    o,
                    "pub(crate) extern \"C\" fn {name}{GENERICS}(recv: v8::Local<v8::Object>, options: *mut v8::fast_api::FastApiCallbackOptions) -> {ret} {{"
                )
                .unwrap();
                writeln!(
                    o,
                    "    // SAFETY: V8 calls this C function only as the fast path of this getter, whose signature it\n    // has checked against the receiver, with the options of the current call.\n    let Some((dom, this)) = (unsafe {{ rt::fast_receiver::<D>(&recv, options) }}) else {{ return Default::default() }};"
                )
                .unwrap();
                let rust = self.rust_ret(&a.ty);
                let cast = if rust == ret { "" } else { cast };
                // (a panic cannot unwind out of a C function: the guard stops it there)
                writeln!(o, "    // SAFETY: `options` is the pointer V8 passed to this fast call.\n    unsafe {{ rt::fast_guard(options, || -> {ret} {{ <D as {tr}>::{m}(dom, this){cast} }}) }}\n}}\n").unwrap();
                fast.push((upper(&a.name), name, info.to_string()));
            }
        }

        // Setter.
        let setter_body = match &a.setter {
            Setter::None => return,
            Setter::ObservableArray => unreachable!("emitted by emit_observable_array"),
            Setter::Trait(m) => {
                let param = self.rust_arg(&a.ty, false);
                let pruned = self.prune(&a.ty);
                let conv = match &pruned.kind {
                    // Web IDL §3.7.6 attribute setter step 4.5: a value that is not one of the
                    // enumeration's values is ignored, not an error.
                    TypeKind::Named(n) if !pruned.nullable && self.model.enums.contains_key(n) => {
                        format!(
                            "rt::to_dom_string(scope, args.get(0)).map(|s| {n}::parse(&s)) else {{ return }};
    let Some(value) = value"
                        )
                    }
                    _ => self.conv(&a.ty, "args.get(0)", "The value"),
                };
                let (decl, conv) = match buffered_str(&conv, &param, "value") {
                    Some((decl, conv)) => (format!("    {decl}\n"), conv),
                    None => (String::new(), conv),
                };
                let lec = le(&conv);
                format!(
                    "{decl}    let Some(value) = {lec} else {{ return }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    {}\n    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{m}(dom, &mut cx{this_arg}, {}) }};\n    {}\n    rt::check(scope, r);\n",
                    self.ce_push(a.ce_reactions),
                    pass(&param, "value"),
                    self.ce_pop(a.ce_reactions)
                )
            }
            Setter::Reflect(rule) => {
                format!(
                    "    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n{}",
                    self.reflect_set(rule, &a.ty, a.ce_reactions)
                )
            }
            Setter::PutForwards(f) => format!(
                "    let Some(receiver) = rt::wrap(scope, this) else {{ return }};\n    let Some(target) = receiver.get(scope, rt::name(scope, {}).into()) else {{ return }};\n    let Ok(target) = v8::Local::<v8::Object>::try_from(target) else {{ return rt::throw_type_error(scope, \"The forwarding target is not an object\") }};\n    target.set(scope, rt::name(scope, {}).into(), args.get(0));\n",
                lit(&a.name),
                lit(f)
            ),
            Setter::CssProperty(property) => {
                let param = self.rust_arg(&a.ty, false);
                let conv = self.conv(&a.ty, "args.get(0)", "The value");
                let lec = le(&conv);
                format!(
                    "    let Some(value) = {lec} else {{ return }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    {}\n    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::set_css_property(dom, &mut cx, this, {}, {}) }};\n    {}\n    rt::check(scope, r);\n",
                    self.ce_push(true),
                    lit(property),
                    pass(&param, "value"),
                    self.ce_pop(true)
                )
            }
            Setter::Replaceable => format!(
                "    args.this().create_data_property(scope, rt::name(scope, {}).into(), args.get(0));\n",
                lit(&a.name)
            ),
            // HTML §8.1.8.1: the setter takes the value as a [LegacyTreatNonObjectAsNull] callback.
            Setter::EventHandler(event) => format!(
                "    let Some(value) = rt::to_callback_function(scope, args.get(0), true, \"The value\") else {{ return }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let r = {{ let mut cx = Cx::new(scope); <D as rt::EventHandlers>::set_event_handler(dom, &mut cx, this, {}, value) }};\n    rt::check(scope, r);\n",
                lit(event)
            ),
        };
        writeln!(o, "/// Setter of `{}`.", a.idl.replace('`', "'")).unwrap();
        writeln!(
            o,
            "pub(crate) fn {}{GENERICS}({CB_SIG}) {{",
            self.attr_fn(a, "set")
        )
        .unwrap();
        writeln!(o, "    {}", prof(&format!("set {}.{}", a.owner, a.name))).unwrap();
        let this_line = self.attr_this(a, Access::Set);
        writeln!(o, "    {this_line}\n{setter_body}}}\n").unwrap();
    }

    /// An `ObservableArray<T>` attribute (Web IDL §3.10): the attribute's backing list as an
    /// `rt::ObservableArray` over the engine's methods, a getter returning the one observable array
    /// exotic object of the platform object, and a setter replacing the backing list.
    fn emit_observable_array(&self, o: &mut String, a: &AttrPlan, p: &ObservablePlan) {
        let tr = self.trait_path(&a.owner);
        let ty = camel(&a.name);
        let owner_ty = camel(&a.owner);
        let oa = format!("Observable{owner_ty}{ty}");
        let conv = self.conv(&p.element, "value", "The value");
        let lec = le(&conv);
        let param = self.rust_arg(&p.element, false);
        let pass_v = pass(&param, "value");
        let pass_old = pass(&param, "old");
        let (len, item, set, delete, store) = (
            &p.length,
            &p.item,
            &p.set_indexed_value,
            &p.delete_indexed_value,
            &p.store,
        );
        let idl = a.idl.replace('`', "'");
        writeln!(
            o,
            "/// The backing list of `{idl}` (Web IDL §3.10 observable array).\npub(crate) struct {oa}<D, const LO: u16, const HI: u16>(std::marker::PhantomData<D>);\n"
        )
        .unwrap();
        writeln!(
            o,
            "impl<D: Dom, const LO: u16, const HI: u16> rt::ObservableArray for {oa}<D, LO, HI> {{"
        )
        .unwrap();
        writeln!(
            o,
            "    fn len(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>) -> Option<u32> {{\n        let Some(this) = rt::unwrap(&obj, LO, HI) else {{ return rt::illegal_invocation(scope) }};\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        Some(<D as {tr}>::{len}(dom, this))\n    }}"
        )
        .unwrap();
        writeln!(
            o,
            "    fn get<'s>(scope: &mut v8::PinScope<'s, '_>, obj: v8::Local<v8::Object>, index: u32) -> Option<v8::Local<'s, v8::Value>> {{\n        let Some(this) = rt::unwrap(&obj, LO, HI) else {{ return rt::illegal_invocation(scope) }};\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        let v = <D as {tr}>::{item}(dom, this, index);\n        v.to_js(scope)\n    }}"
        )
        .unwrap();
        writeln!(
            o,
            "    fn set(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>, index: u32, value: v8::Local<v8::Value>) -> Option<()> {{\n        let Some(this) = rt::unwrap(&obj, LO, HI) else {{ return rt::illegal_invocation(scope) }};\n        let value = {lec}?;\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        if index < <D as {tr}>::{len}(dom, this) {{\n            let old = <D as {tr}>::{item}(dom, this, index);\n            let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{delete}(dom, &mut cx, this, {pass_old}, index) }};\n            rt::check(scope, r)?;\n        }}\n        let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{set}(dom, &mut cx, this, {pass_v}, index) }};\n        rt::check(scope, r)?;\n        <D as {tr}>::{store}(dom, this, index, Some(value));\n        Some(())\n    }}"
        )
        .unwrap();
        writeln!(
            o,
            "    fn delete_last(scope: &mut v8::PinScope<'_, '_>, obj: v8::Local<v8::Object>) -> Option<()> {{\n        let Some(this) = rt::unwrap(&obj, LO, HI) else {{ return rt::illegal_invocation(scope) }};\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        let len = <D as {tr}>::{len}(dom, this);\n        if len == 0 {{\n            return Some(());\n        }}\n        let old = <D as {tr}>::{item}(dom, this, len - 1);\n        let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{delete}(dom, &mut cx, this, {pass_old}, len - 1) }};\n        rt::check(scope, r)?;\n        <D as {tr}>::{store}(dom, this, len - 1, None);\n        Some(())\n    }}\n}}\n"
        )
        .unwrap();
        let key = format!("{}.{}", a.owner, a.name);
        writeln!(
            o,
            "/// Getter of `{idl}`: the attribute's observable array exotic object."
        )
        .unwrap();
        writeln!(
            o,
            "pub(crate) fn {}{GENERICS}({CB_SIG}) {{",
            self.attr_fn(a, "get")
        )
        .unwrap();
        writeln!(o, "    {}", prof(&format!("get {}.{}", a.owner, a.name))).unwrap();
        writeln!(o, "    {THIS}").unwrap();
        writeln!(
            o,
            "    if let Some(v) = rt::same_object_get(scope, args.this(), {k}) {{ rv.set(v); return; }}\n    let Some(oa) = rt::observable_array_new::<{oa}<D, LO, HI>>(scope, args.this()) else {{ return }};\n    rt::same_object_set(scope, args.this(), {k}, oa.into());\n    rv.set(oa.into());\n}}\n",
            k = name_key(&key)
        )
        .unwrap();
        if matches!(a.setter, Setter::None) {
            return;
        }
        let seq = Type {
            kind: TypeKind::Sequence(Box::new(p.element.clone())),
            nullable: false,
            ext: Vec::new(),
        };
        let seq_conv = self.conv(&seq, "args.get(0)", "The value");
        let seq_lec = le(&seq_conv);
        writeln!(o, "/// Setter of `{idl}` (Web IDL §3.7.6): the backing list emptied, then each value set.").unwrap();
        writeln!(
            o,
            "pub(crate) fn {}{GENERICS}({CB_SIG}) {{",
            self.attr_fn(a, "set")
        )
        .unwrap();
        writeln!(o, "    {}", prof(&format!("set {}.{}", a.owner, a.name))).unwrap();
        writeln!(o, "    {THIS}").unwrap();
        writeln!(
            o,
            "    let Some(values) = {seq_lec} else {{ return }};\n    let obj = args.this();\n    let Some(len) = <{oa}<D, LO, HI> as rt::ObservableArray>::len(scope, obj) else {{ return }};\n    for _ in 0..len {{\n        if <{oa}<D, LO, HI> as rt::ObservableArray>::delete_last(scope, obj).is_none() {{ return }}\n    }}\n    for (i, value) in values.into_iter().enumerate() {{\n        let st = rt::state(scope);\n        let dom = st.engine::<D>();\n        let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{set}(dom, &mut cx, this, {pass_v}, i as u32) }};\n        if rt::check(scope, r).is_none() {{ return }}\n        <D as {tr}>::{store}(dom, this, i as u32, Some(value));\n    }}\n}}\n"
        )
        .unwrap();
    }

    fn reflect_get(&self, rule: &ReflectRule, same: Option<&str>) -> String {
        let get = |content: &str| {
            format!(
                "    let value = <D as rt::Reflect>::get_content_attribute(dom, this, {});\n",
                lit(content)
            )
        };
        let set_js = |v: &str| self.set_result(&Type::prim(Prim::DOMString), "DomStr", v, same);
        match rule {
            ReflectRule::String {
                content, nullable, ..
            } => format!(
                "{}    match value {{ Some(v) => {{ {} }} None => {} }}\n",
                get(content),
                set_js("v"),
                if *nullable {
                    "rv.set_null()"
                } else {
                    "rv.set_empty_string()"
                }
            ),
            ReflectRule::Url { content } => format!(
                "{}    match value {{ None => rv.set_empty_string(), Some(v) => {{ let s = <D as rt::Reflect>::resolve_url(dom, this, &v).unwrap_or_else(|| v.into_owned()); {} }} }}\n",
                get(content),
                set_js("s")
            ),
            ReflectRule::Boolean { content } => format!("{}    rv.set_bool(value.is_some());\n", get(content)),
            ReflectRule::Long {
                content,
                default,
                non_negative,
            } => format!(
                "{}    rv.set_int32(rt::{}(value.as_deref(), {default}));\n",
                get(content),
                if *non_negative {
                    "long_non_negative_get"
                } else {
                    "long_get"
                }
            ),
            ReflectRule::Unsigned {
                content,
                kind,
                default,
                range,
            } => format!(
                "{}    rv.set_uint32(rt::unsigned_long_get(value.as_deref(), rt::UnsignedKind::{kind}, {default}, {}));\n",
                get(content),
                match range {
                    Some((a, b)) => format!("Some(({a}, {b}))"),
                    None => "None".into(),
                }
            ),
            ReflectRule::Double {
                content,
                positive,
                default,
            } => format!(
                "{}    rv.set_double(rt::double_get(value.as_deref(), {positive}, {}));\n",
                get(content),
                if default.contains('.') {
                    default.clone()
                } else {
                    format!("{default}.0")
                }
            ),
            ReflectRule::TokenList { content } => format!(
                "    let h = <D as rt::Reflect>::token_list(dom, this, {});\n    {}\n",
                lit(content),
                self.set_result(&Type::prim(Prim::Object), "Handle", "h", same)
            ),
            ReflectRule::Enumerated {
                content,
                keywords,
                missing,
                invalid,
                ..
            } => {
                let kw: Vec<String> = keywords
                    .iter()
                    .map(|(k, c)| format!("({}, {})", lit(k), lit(c)))
                    .collect();
                let opt = |d: &Option<String>| match d {
                    Some(s) => format!("Some({})", lit(s)),
                    None => "None".into(),
                };
                format!(
                    "{}    match rt::enumerated_get(value.as_deref(), &[{}], {}, {}) {{ Some(s) => {{ let Some(js) = s.to_js(scope) else {{ return }}; rv.set(js); }} None => rv.set_null() }}\n",
                    get(content),
                    kw.join(", "),
                    opt(missing),
                    opt(invalid)
                )
            }
        }
    }

    fn reflect_set(&self, rule: &ReflectRule, ty: &Type, ce: bool) -> String {
        let set = |content: &str, value: &str| {
            format!(
                "    {}\n    let r = {{ let mut cx = Cx::new(scope); <D as rt::Reflect>::set_content_attribute(dom, &mut cx, this, {}, {value}) }};\n    {}\n    rt::check(scope, r);\n",
                self.ce_push(ce),
                lit(content),
                self.ce_pop(ce)
            )
        };
        let remove = |content: &str| {
            format!(
                "{{ {}\n    let r = {{ let mut cx = Cx::new(scope); <D as rt::Reflect>::remove_content_attribute(dom, &mut cx, this, {}) }};\n    {}\n    rt::check(scope, r); return; }}",
                self.ce_push(ce),
                lit(content),
                self.ce_pop(ce)
            )
        };
        let index_size = "rt::throw(scope, rt::Error::dom(rt::ExceptionName::IndexSizeError, \"The value is out of range\")); return;";
        let v = "    let v = args.get(0);\n";
        // A nullable string reflection removes the attribute for null (and undefined, which converts to
        // null); otherwise it sets the converted string.
        let string_set = |content: &str, nullable: bool| {
            let base = Type {
                nullable: false,
                ..self.prune(ty)
            };
            let conv = self.conv(&base, "v", "The value");
            let lec = le(&conv);
            let null = if nullable {
                format!("    if v.is_null_or_undefined() {}\n", remove(content))
            } else {
                String::new()
            };
            format!(
                "{v}{null}    let Some(s) = {lec} else {{ return }};\n{}",
                set(content, "&s")
            )
        };
        match rule {
            ReflectRule::String {
                content, nullable, ..
            } => string_set(content, *nullable),
            ReflectRule::Url { content } => format!(
                "{v}    let Some(s) = rt::to_usv_string(scope, v) else {{ return }};\n{}",
                set(content, "&s")
            ),
            ReflectRule::Boolean { content } => format!(
                "{v}    if !rt::to_boolean(scope, v) {}\n{}",
                remove(content),
                set(content, "\"\"")
            ),
            ReflectRule::Long {
                content,
                non_negative,
                ..
            } => format!(
                "{v}    let Some(x) = rt::to_long(scope, v, rt::IntMode::Wrap) else {{ return }};\n{}    let s = x.to_string();\n{}",
                if *non_negative {
                    format!("    if x < 0 {{ {index_size} }}\n")
                } else {
                    String::new()
                },
                set(content, "&s")
            ),
            ReflectRule::Unsigned {
                content,
                kind,
                default,
                ..
            } => format!(
                "{v}    let Some(x) = rt::to_unsigned_long(scope, v, rt::IntMode::Wrap) else {{ return }};\n    let Some(x) = rt::unsigned_long_set(x, rt::UnsignedKind::{kind}, {default}) else {{ {index_size} }};\n    let s = x.to_string();\n{}",
                set(content, "&s")
            ),
            ReflectRule::Double {
                content, positive, ..
            } => {
                let restricted = self.prune(ty).as_prim() == Some(Prim::Double);
                format!(
                    "{v}    let Some(x) = rt::to_double(scope, v, {restricted}) else {{ return }};\n{}    let s = rt::number_to_string(scope, x);\n{}",
                    if *positive {
                        "    if x <= 0.0 { return; }\n"
                    } else {
                        ""
                    },
                    set(content, "&s")
                )
            }
            ReflectRule::Enumerated {
                content, nullable, ..
            } => string_set(content, *nullable),
            ReflectRule::TokenList { .. } => unreachable!("token lists forward their setter"),
        }
    }

    // ---- operations -------------------------------------------------------------------------------

    fn op_fn(&self, op: &OpPlan) -> String {
        let s = snake(&op.name);
        let s = s.trim_end_matches('_');
        if op.name == "constructor" {
            "ctor".into()
        } else if op.is_static {
            format!("static_op_{s}")
        } else {
            format!("op_{s}")
        }
    }

    fn required(&self, ov: &OverloadPlan) -> usize {
        ov.args
            .iter()
            .rposition(|a| !a.optional && !a.variadic)
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    /// Emits an operation's callback, its dispatcher (overload resolution) and one helper per overload.
    fn emit_op(&self, o: &mut String, op: &OpPlan) {
        if self.light_op(op) {
            self.emit_light_op(o, op);
            return;
        }
        let base = self.op_fn(op);
        let has_this = !op.is_static && op.name != "constructor" && !op.name.starts_with("factory");
        let is_ctor = op.name == "constructor";
        let what = format!("{}.{}", op.owner, op.name);
        // Overload helpers.
        for (k, ov) in op.overloads.iter().enumerate() {
            self.emit_overload(o, op, ov, &format!("{base}_{k}"), has_this, is_ctor);
        }
        // Dispatcher.
        let ret = if is_ctor {
            "Option<Handle>".to_string()
        } else {
            "Option<v8::Local<'s, v8::Value>>".to_string()
        };
        writeln!(
            o,
            "/// Overload resolution of `{what}` (Web IDL §3.7.6).\nfn {base}_dispatch<'s, D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope<'s, '_>, args: &v8::FunctionCallbackArguments) -> {ret} {{"
        )
        .unwrap();
        if has_this {
            if self.cross_origin_member(&op.owner, &op.name, Access::Call) {
                o.push_str("    let Some(this) = rt::cross_origin_this(scope, args, LO, HI) else { return rt::illegal_invocation(scope) };\n");
            } else {
                if !op.is_static && !op.promise && self.unchecked_receiver(&op.owner) {
                    writeln!(o, "    {THIS_UNCHECKED}").unwrap();
                } else {
                    o.push_str("    let Some(this) = rt::unwrap(&args.this(), LO, HI) else { return rt::illegal_invocation(scope) };\n");
                }
                if self.security_checked(&op.owner) {
                    o.push_str(
                        "    if !security_check::<LO>(scope, args.this()) { return None }\n",
                    );
                }
            }
        }
        o.push_str("    let argc = args.length();\n");
        let this_arg = if has_this { ", this" } else { "" };
        let call = |k: usize| format!("{base}_{k}::<D, LO, HI>(scope, args{this_arg}, argc)");
        if op.overloads.len() == 1 {
            let req = self.required(&op.overloads[0]);
            if req > 0 {
                writeln!(
                    o,
                    "    if argc < {req} {{ return rt::not_enough_arguments(scope, {}, {req}, argc); }}",
                    lit(&what)
                )
                .unwrap();
            }
            writeln!(o, "    {}", call(0)).unwrap();
        } else {
            let maxarg = op
                .overloads
                .iter()
                .map(|ov| ov.args.len())
                .max()
                .unwrap_or(0);
            writeln!(o, "    match argc.min({maxarg}) {{").unwrap();
            for n in 0..=maxarg {
                let cands: Vec<usize> = op
                    .overloads
                    .iter()
                    .enumerate()
                    .filter(|(_, ov)| {
                        let r = self.required(ov);
                        let variadic = ov.args.last().is_some_and(|a| a.variadic);
                        r <= n && (n <= ov.args.len() || variadic)
                    })
                    .map(|(k, _)| k)
                    .collect();
                let arm = if n == maxarg {
                    "_".to_string()
                } else {
                    n.to_string()
                };
                writeln!(o, "        {arm} => {{").unwrap();
                match cands.len() {
                    0 => {
                        let req = op
                            .overloads
                            .iter()
                            .map(|ov| self.required(ov))
                            .min()
                            .unwrap_or(0);
                        writeln!(
                            o,
                            "            rt::not_enough_arguments(scope, {}, {req}, argc)",
                            lit(&what)
                        )
                        .unwrap();
                    }
                    1 => writeln!(o, "            {}", call(cands[0])).unwrap(),
                    _ => {
                        // The distinguishing argument index: the first position whose types differ.
                        let types_at = |k: usize, i: usize| -> Option<Type> {
                            op.overloads[k]
                                .args
                                .get(i)
                                .map(|a| a.ty.clone())
                                .or_else(|| {
                                    op.overloads[k]
                                        .args
                                        .last()
                                        .filter(|a| a.variadic)
                                        .map(|a| a.ty.clone())
                                })
                        };
                        let d = (0..n)
                            .find(|&i| {
                                let first = types_at(cands[0], i);
                                cands.iter().any(|&k| types_at(k, i) != first)
                            })
                            .unwrap_or(0);
                        let candidates: Vec<(usize, Type)> = cands
                            .iter()
                            .map(|&k| {
                                (
                                    k,
                                    types_at(k, d).unwrap_or_else(|| Type::prim(Prim::Undefined)),
                                )
                            })
                            .collect();
                        let optional: Vec<bool> = cands
                            .iter()
                            .map(|&k| op.overloads[k].args.get(d).is_some_and(|a| a.optional))
                            .collect();
                        writeln!(o, "            let v = args.get({d});").unwrap();
                        let fallback = format!(
                            "rt::type_error(scope, {})",
                            lit(&format!(
                                "Failed to execute '{}': no overload matches the arguments",
                                what
                            ))
                        );
                        let body =
                            self.dispatch(&candidates, &optional, "v", &|k| call(k), &fallback);
                        writeln!(o, "{body}").unwrap();
                    }
                }
                o.push_str("        }\n");
            }
            o.push_str("    }\n");
        }
        o.push_str("}\n\n");
        if is_ctor {
            return;
        }
        // The callback.
        writeln!(o, "/// `{what}`.").unwrap();
        writeln!(o, "pub(crate) fn {base}{GENERICS}({CB_SIG}) {{").unwrap();
        writeln!(o, "    {}", prof(&what)).unwrap();
        // A cross-origin `this` of a Window operation that returns a promise: Web IDL rejects, but
        // these throw (`scroll()` and its kin, whose promise is new in CSSOM View): WPT's
        // cross-origin-objects.html expects it of everything but `fetch` and `createImageBitmap`.
        if op.promise && op.owner == GLOBAL && !op.is_static {
            o.push_str("    if rt::unwrap(&args.this(), LO, HI).is_some() && !security_check::<LO>(scope, args.this()) { return }\n");
        }
        if op.promise {
            writeln!(
                o,
                "    v8::tc_scope!(let tc, scope);\n    match {base}_dispatch::<D, LO, HI>(tc, &args) {{\n        Some(v) => rv.set(v),\n        None => {{ if let Some(e) = tc.exception() {{ if let Some(r) = v8::PromiseResolver::new(tc) {{ r.reject(tc, e); rv.set(r.get_promise(tc).into()); }} }} }}\n    }}"
            )
            .unwrap();
        } else {
            writeln!(
                o,
                "    if let Some(v) = {base}_dispatch::<D, LO, HI>(scope, &args) {{ rv.set(v) }}"
            )
            .unwrap();
        }
        o.push_str("}\n\n");
    }

    /// The conversions of an overload's arguments (Web IDL §3.2), one statement each, with argument `i`
    /// read from `get(i)`; returns the expressions that pass them to the engine.
    fn emit_args(
        &self,
        o: &mut String,
        op: &OpPlan,
        ov: &OverloadPlan,
        ind: &str,
        get: &dyn Fn(&str) -> String,
    ) -> Vec<String> {
        let mut passes = Vec::new();
        for (i, a) in ov.args.iter().enumerate() {
            let var = format!("a{i}");
            let what = format!("Failed to execute '{}': parameter {}", op.name, i + 1);
            let conv = self.conv(&a.ty, "v", &what);
            let param_rust = self.rust_arg(&a.ty, false);
            if a.variadic {
                writeln!(
                    o,
                    "{ind}let mut {var} = Vec::new();\n{ind}for i in {i}..argc {{ let v = {}; {var}.push({conv}?); }}",
                    get("i")
                )
                .unwrap();
                passes.push(var);
                continue;
            }
            let any = self.is_any(&a.ty);
            if !a.optional {
                let conv = match buffered_str(&conv, &param_rust, &var) {
                    Some((decl, conv)) => {
                        writeln!(o, "{ind}{decl}").unwrap();
                        conv
                    }
                    None => conv,
                };
                // (a nullable conversion is a block of its own: here it is the block's value)
                let conv = match conv.strip_prefix("{ ").and_then(|c| c.strip_suffix(" }")) {
                    Some(inner) if inner.starts_with("if v.is_null_or_undefined()") => {
                        inner.to_string()
                    }
                    _ => conv,
                };
                writeln!(
                    o,
                    "{ind}let {var} = {{ let v = {}; {conv} }}?;",
                    get(&i.to_string())
                )
                .unwrap();
                passes.push(pass(&param_rust, &var));
            } else if let Some(d) = &a.default {
                let default = self.default_value(&a.ty, d);
                writeln!(
                    o,
                    "{ind}let {var} = {{ let v = {}; if argc > {i} && !v.is_undefined() {{ {conv}? }} else {{ {default} }} }};",
                    get(&i.to_string())
                )
                .unwrap();
                passes.push(pass(&param_rust, &var));
            } else {
                let nullable = param_rust.starts_with("Option<");
                let present = if nullable && !any {
                    format!("{conv}?")
                } else {
                    format!("Some({conv}?)")
                };
                writeln!(
                    o,
                    "{ind}let {var} = {{ let v = {}; if argc > {i} && !v.is_undefined() {{ {present} }} else {{ None }} }};",
                    get(&i.to_string())
                )
                .unwrap();
                let opt_param = if nullable && !any {
                    param_rust.clone()
                } else {
                    format!("Option<{param_rust}>")
                };
                passes.push(pass(&opt_param, &var));
            }
        }
        passes
    }

    /// Emits a light operation's callback ([`Self::light_op`]), as Blink's generated callbacks are: the
    /// receiver and the arguments read in place, one scope for the conversions and the engine call
    /// (Blink's `ExceptionState`), and a platform object or primitive result stored in place.
    fn emit_light_op(&self, o: &mut String, op: &OpPlan) {
        let base = self.op_fn(op);
        let what = format!("{}.{}", op.owner, op.name);
        let ov = &op.overloads[0];
        let tr = self.trait_path(&op.owner);
        let getter_call = ov.method.strip_prefix("#getter:");
        let index_size =
            getter_call.is_some() && super::plan::INDEX_SIZE_GETTERS.contains(&op.owner.as_str());
        let rust = self.rust_ret(&ov.ret);
        let undefined = ov.ret.is_undefined();
        let in_place = !index_size
            && !undefined
            && !self.is_frozen(&ov.ret)
            && matches!(
                rust.as_str(),
                "bool" | "i32" | "i8" | "i16" | "u8" | "u16" | "u32" | "Handle" | "Option<Handle>"
            );
        writeln!(o, "/// `{what}`: `{}`", ov.idl.replace('`', "'")).unwrap();
        writeln!(o, "pub(crate) fn {base}{GENERICS}(call: rt::Call) {{").unwrap();
        writeln!(o, "    let _prof = call.prof({});", lit(&what)).unwrap();
        o.push_str("    // SAFETY: this member's template has a receiver signature, which V8 checked before the call.\n    let this = unsafe { call.receiver() };\n    let argc = call.length();\n");
        let req = self.required(ov);
        if req > 0 {
            writeln!(
                o,
                "    if argc < {req} {{\n        return call.not_enough_arguments({}, {req});\n    }}",
                lit(&what)
            )
            .unwrap();
        }
        o.push_str("    let dom = call.state().engine::<D>();\n");
        o.push_str("    let v = call.with_scope(|scope, mut rv| {\n");
        let passes = self.emit_args(o, op, ov, "        ", &|i| format!("call.arg({i})"));
        let args_list = passes.iter().map(|p| format!(", {p}")).collect::<String>();
        if let Some(m) = getter_call {
            writeln!(o, "        let v = <D as {tr}>::{m}(dom, this{args_list});").unwrap();
            if index_size {
                o.push_str("        let Some(v) = v else { rt::throw(scope, rt::Error::Dom(rt::ExceptionName::IndexSizeError, \"The index is not in the allowed range.\".into())); return None; };\n");
            }
        } else {
            if op.ce_reactions {
                writeln!(o, "        {}", self.ce_push(true)).unwrap();
            }
            writeln!(
                o,
                "        let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{}(dom, &mut cx, this{args_list}) }};",
                ov.method
            )
            .unwrap();
            if op.ce_reactions {
                writeln!(o, "        {}", self.ce_pop(true)).unwrap();
            }
            o.push_str("        let v = rt::check(scope, r)?;\n");
        }
        if in_place {
            o.push_str("        Some(v)\n    });\n");
            writeln!(
                o,
                "    let Some(v) = v else {{ return }};\n    {}",
                light_result(&rust, "v")
            )
            .unwrap();
        } else {
            if undefined {
                o.push_str("        let _ = v;\n");
            } else {
                let js = if self.is_frozen(&ov.ret) && ov.ret.nullable {
                    "v.map(rt::Frozen).to_js(scope)"
                } else if self.is_frozen(&ov.ret) {
                    "rt::Frozen(v).to_js(scope)"
                } else {
                    "v.to_js(scope)"
                };
                writeln!(o, "        if let Some(js) = {js} {{ rv.set(js) }}").unwrap();
            }
            o.push_str("        Some(())\n    });\n    let _ = v;\n");
        }
        o.push_str("}\n\n");
    }

    fn emit_overload(
        &self,
        o: &mut String,
        op: &OpPlan,
        ov: &OverloadPlan,
        name: &str,
        has_this: bool,
        is_ctor: bool,
    ) {
        let tr = self.trait_path(&op.owner);
        let ret = if is_ctor {
            "Option<Handle>".to_string()
        } else {
            "Option<v8::Local<'s, v8::Value>>".to_string()
        };
        writeln!(o, "/// `{}`", ov.idl.replace('`', "'")).unwrap();
        writeln!(
            o,
            "fn {name}<'s, D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope<'s, '_>, args: &v8::FunctionCallbackArguments{}, argc: i32) -> {ret} {{",
            if has_this { ", this: Handle" } else { "" }
        )
        .unwrap();
        let getter_call = ov.method.strip_prefix("#getter:");
        let passes = self.emit_args(o, op, ov, "    ", &|i| format!("args.get({i})"));
        o.push_str("    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n");
        let this_arg = if has_this { ", this" } else { "" };
        let args_list = passes.iter().map(|p| format!(", {p}")).collect::<String>();
        if let Some(m) = getter_call {
            writeln!(o, "    let v = <D as {tr}>::{m}(dom{this_arg}{args_list});").unwrap();
            if super::plan::INDEX_SIZE_GETTERS.contains(&op.owner.as_str()) {
                o.push_str("    let Some(v) = v else { rt::throw(scope, rt::Error::Dom(rt::ExceptionName::IndexSizeError, \"The index is not in the allowed range.\".into())); return None; };\n");
            }
            o.push_str("    v.to_js(scope)\n}\n\n");
            return;
        }
        writeln!(o, "    {}", self.ce_push(op.ce_reactions)).unwrap();
        writeln!(
            o,
            "    let r = {{ let mut cx = Cx::new(scope); <D as {tr}>::{}(dom, &mut cx{this_arg}{args_list}) }};",
            ov.method
        )
        .unwrap();
        writeln!(o, "    {}", self.ce_pop(op.ce_reactions)).unwrap();
        o.push_str("    let v = rt::check(scope, r)?;\n");
        if is_ctor {
            o.push_str("    Some(v)\n}\n\n");
        } else if op.name.starts_with("factory") {
            o.push_str("    Some(rt::wrap(scope, v)?.into())\n}\n\n");
        } else if ov.ret.is_undefined() {
            o.push_str("    let _ = v;\n    Some(v8::undefined(scope).into())\n}\n\n");
        } else if self.is_frozen(&ov.ret) && ov.ret.nullable {
            o.push_str("    v.map(rt::Frozen).to_js(scope)\n}\n\n");
        } else if self.is_frozen(&ov.ret) {
            o.push_str("    rt::Frozen(v).to_js(scope)\n}\n\n");
        } else {
            o.push_str("    v.to_js(scope)\n}\n\n");
        }
    }

    // ---- interface objects ------------------------------------------------------------------------

    fn emit_interface(&self, o: &mut String, ip: &IfacePlan) {
        let id = upper(&ip.name);
        let tr = self.trait_path(&ip.name);
        // Constructor.
        if let Some(c) = &ip.ctor {
            self.emit_op(o, c);
            writeln!(
                o,
                "/// The `{0}` constructor.\nfn ctor_callback<D: Dom>({CB_SIG}) {{\n    let _prof = rt::prof::enter(scope, \"new {0}\");\n    const LO: u16 = ids::{id};\n    const HI: u16 = ids::{id}_END;\n    if !args.is_construct_call() {{ return rt::throw_type_error(scope, \"Failed to construct '{0}': Please use the 'new' operator\"); }}\n    let Some(h) = ctor_dispatch::<D, LO, HI>(scope, &args) else {{ return }};\n    rt::adopt_wrapper(scope, args.this(), h, LO);\n    rv.set(args.this().into());\n}}\n",
                ip.name
            )
            .unwrap();
        } else if ip.html_constructor {
            writeln!(
                o,
                "/// The `{0}` constructor (`[HTMLConstructor]`).\nfn ctor_callback<D: Dom>({CB_SIG}) {{\n    let _prof = rt::prof::enter(scope, \"new {0}\");\n    if !args.is_construct_call() {{ return rt::throw_type_error(scope, \"Failed to construct '{0}': Please use the 'new' operator\"); }}\n    let Ok(nt) = v8::Local::<v8::Object>::try_from(args.new_target()) else {{ return }};\n    let nt = JsObject(v8::Global::new(scope, nt));\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let r = {{ let mut cx = Cx::new(scope); <D as rt::Host>::html_constructor(dom, &mut cx, InterfaceId::{1}, &nt) }};\n    let Some(h) = rt::check(scope, r) else {{ return }};\n    let Some(w) = rt::wrap(scope, h) else {{ return }};\n    rv.set(w.into());\n}}\n",
                ip.name,
                ip.name
            )
            .unwrap();
        }
        // Legacy factory functions.
        for f in &ip.factories {
            let mut op = f.clone();
            op.name = format!("factory{}", f.name);
            self.emit_op(o, &op);
            let base = self.op_fn(&op);
            writeln!(
                o,
                "/// The legacy factory function `{0}`.\npub(crate) fn factory_{1}_callback<D: Dom>({CB_SIG}) {{\n    const LO: u16 = ids::{id};\n    const HI: u16 = ids::{id}_END;\n    if !args.is_construct_call() {{ return rt::throw_type_error(scope, \"Failed to construct '{0}': Please use the 'new' operator\"); }}\n    let Some(v) = {base}_dispatch::<D, LO, HI>(scope, &args) else {{ return }};\n    rv.set(v);\n}}\n",
                f.name,
                snake(&f.name)
            )
            .unwrap();
        }
        // Stringifier.
        if let Some(attr) = self.light_stringifier(ip) {
            // The attribute's getter is a light callback: so is `toString`.
            writeln!(
                o,
                "/// `toString` (the stringifier).\nfn op_to_string_stringifier<D: Dom, const LO: u16, const HI: u16>(call: rt::Call) {{\n    super::{}::{}::<D, LO, HI>(call)\n}}\n",
                self.traits[&attr.owner].file,
                self.attr_fn(attr, "get")
            )
            .unwrap();
        } else if let Some(s) = &ip.stringifier {
            writeln!(o, "/// `toString` (the stringifier).\nfn op_to_string_stringifier<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{").unwrap();
            match s {
                Stringifier::Attribute(a) => {
                    let attr = ip.attrs.iter().find(|x| &x.name == a).unwrap();
                    writeln!(
                        o,
                        "    super::{}::{}::<D, LO, HI>(scope, args, rv)",
                        self.traits[&attr.owner].file,
                        self.attr_fn(attr, "get")
                    )
                    .unwrap();
                }
                Stringifier::Method(owner, m, false) => {
                    writeln!(
                        o,
                        "    {}\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let v = <D as {}>::{m}(dom, this);\n    let Some(js) = v.to_js(scope) else {{ return }};\n    rv.set(js);",
                        self.checked_this(&ip.name),
                        self.trait_path(owner)
                    )
                    .unwrap();
                }
                Stringifier::Method(owner, m, true) => {
                    writeln!(
                        o,
                        "    {}\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let r = {{ let mut cx = Cx::new(scope); <D as {}>::{m}(dom, &mut cx, this) }};\n    let Some(v) = rt::check(scope, r) else {{ return }};\n    let Some(js) = v.to_js(scope) else {{ return }};\n    rv.set(js);",
                        self.checked_this(&ip.name),
                        self.trait_path(owner)
                    )
                    .unwrap();
                }
            }
            o.push_str("}\n\n");
        }
        if let Some(names) = &ip.default_json {
            let list: Vec<String> = names.iter().map(|n| lit(n)).collect();
            writeln!(
                o,
                "/// `toJSON()` (`[Default]`: Web IDL's default toJSON steps over the attributes of the
/// inheritance stack).
fn op_to_json_default<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{
    let _prof = rt::prof::enter(scope, \"{}.toJSON\");
    if rt::unwrap(&args.this(), LO, HI).is_none() {{ return rt::throw_illegal_invocation(scope) }}
    let Some(o) = rt::default_to_json(scope, args.this(), &[{}]) else {{ return }};
    rv.set(o.into());
}}
",
                ip.name,
                list.join(", ")
            )
            .unwrap();
        }
        if HTML_DDA.contains(&ip.name.as_str()) {
            // [[Call]]: the `item()` operation's steps with the callee object as `this`.
            let item = ip
                .ops
                .iter()
                .find(|op| op.name == "item")
                .expect("an [[IsHTMLDDA]] interface has item()");
            writeln!(
                o,
                "/// [[Call]] of `{0}` objects (HTML: the `item()` steps).\nfn legacy_call<D: Dom, const LO: u16, const HI: u16>({CB_SIG}) {{\n    let _prof = rt::prof::enter(scope, \"{0} [[Call]]\");\n    let Some(this) = rt::unwrap(&args.this(), LO, HI) else {{ return rt::throw_illegal_invocation(scope) }};\n    let a0 = {{\n        let v = args.get(0);\n        if args.length() > 0 && !v.is_undefined() {{\n            let Some(s) = rt::to_dom_string(scope, v) else {{ return }};\n            Some(s)\n        }} else {{\n            None\n        }}\n    }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let r = {{ let mut cx = Cx::new(scope); <D as {1}>::{2}(dom, &mut cx, this, a0.as_deref()) }};\n    let Some(v) = rt::check(scope, r) else {{ return }};\n    if let Some(js) = v.to_js(scope) {{ rv.set(js) }}\n}}\n",
                ip.name,
                self.trait_path(&item.owner),
                item.overloads[0].method
            )
            .unwrap();
        }
        self.emit_interceptors(o, ip, &tr);
        self.emit_pair_iterator(o, ip);
        self.emit_async_iterator(o, ip);

        // The template.
        writeln!(
            o,
            "/// Builds the interface object template of `{}` (Web IDL §3.7), inheriting from `parent`.",
            ip.name
        )
        .unwrap();
        writeln!(
            o,
            "pub(crate) fn template<'s, D: Dom>(scope: &v8::PinScope<'s, '_>, parent: Option<v8::Local<'s, v8::FunctionTemplate>>, options: &rt::InstallOptions) -> v8::Local<'s, v8::FunctionTemplate> {{"
        )
        .unwrap();
        writeln!(
            o,
            "    const LO: u16 = ids::{id};\n    const HI: u16 = ids::{id}_END;"
        )
        .unwrap();
        let ctor_len = ip
            .ctor
            .as_ref()
            .map(|c| {
                c.overloads
                    .iter()
                    .map(|ov| self.required(ov))
                    .min()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        if ip.ctor.is_some() || ip.html_constructor {
            writeln!(
                o,
                "    let t = rt::builder(ctor_callback::<D>).length({ctor_len}).build(scope);"
            )
            .unwrap();
        } else {
            o.push_str("    let t = v8::FunctionTemplate::builder(rt::illegal_constructor).length(0).build(scope);\n");
        }
        writeln!(
            o,
            "    t.set_class_name(rt::name(scope, {}));",
            lit(&ip.name)
        )
        .unwrap();
        o.push_str("    t.read_only_prototype();\n    if let Some(p) = parent {\n        t.inherit(p);\n    }\n");
        o.push_str("    let inst = t.instance_template(scope);\n    inst.set_internal_field_count(rt::WRAPPER_FIELDS);\n");
        if ip.name == "Location" {
            // HTML §7.10.5, a Location object's creation: own valueOf and @@toPrimitive, before
            // its `[LegacyUnforgeable]` members (as Blink orders them).
            o.push_str("    rt::location_own_properties(scope, inst);\n");
        }
        if HTML_DDA.contains(&ip.name.as_str()) {
            o.push_str("    // [[IsHTMLDDA]] and [[Call]] (HTML §2.7.2.1).\n    snowlark_js::hooks::mark_as_undetectable(inst);\n    inst.set_call_as_function_handler(legacy_call::<D, LO, HI>, None);\n");
        }
        if CODE_LIKE.contains(&ip.name.as_str()) {
            o.push_str("    // Code like: eval() and new Function() compile its data.\n    snowlark_js::hooks::mark_as_code_like(inst);\n");
        }
        o.push_str("    let proto = t.prototype_template(scope);\n    let sig = v8::Signature::new(scope, t);\n");
        writeln!(o, "    rt::to_string_tag(scope, proto, {});", lit(&ip.name)).unwrap();
        for c in &ip.consts {
            let value = match &c.value {
                Literal::Integer(_) => {
                    let v = c.value.integer_value().unwrap();
                    if v >= i32::MIN as i128 && v <= i32::MAX as i128 {
                        format!("v8::Integer::new(scope, {v}).into()")
                    } else {
                        format!("v8::Number::new(scope, {v}.0).into()")
                    }
                }
                Literal::Decimal(s) => format!("v8::Number::new(scope, {s}).into()"),
                Literal::Bool(b) => format!("v8::Boolean::new(scope, {b}).into()"),
                Literal::Infinity => "v8::Number::new(scope, f64::INFINITY).into()".into(),
                Literal::NegInfinity => "v8::Number::new(scope, f64::NEG_INFINITY).into()".into(),
                Literal::NaN => "v8::Number::new(scope, f64::NAN).into()".into(),
                _ => continue,
            };
            writeln!(
                o,
                "    rt::constant(scope, t, proto, {}, {value});",
                lit(&c.name)
            )
            .unwrap();
        }
        for a in &ip.attrs {
            let file = &self.traits[&a.owner].file;
            o.push_str(&member_guard(
                a.secure,
                &a.exposure,
                &a.ua_gate,
                a.touch_device,
            ));
            let sig = if a.is_static || a.lenient_this || matches!(a.ty.kind, TypeKind::Promise(_))
            {
                "None"
            } else {
                "Some(sig)"
            };
            let fast = if a.fast.is_some()
                && !a.same_object
                && matches!(a.getter, Getter::Trait(_))
                && !self.security_checked_attr(a)
            {
                format!("Some(super::{file}::Fast::<D, LO, HI>::{})", upper(&a.name))
            } else {
                "None".into()
            };
            if a.cached {
                writeln!(
                    o,
                    "        let g = rt::cached_getter(scope, super::{file}::{}::<D, LO, HI>, {}, {}, {sig});",
                    self.attr_fn(a, "get"),
                    lit(&a.name),
                    lit(&format!("{}.{}", a.owner, a.name))
                )
                .unwrap();
            } else {
                writeln!(
                    o,
                    "        let g = rt::getter(scope, rt::{}(super::{file}::{}::<D, LO, HI>), {}, {sig}, {fast}, options);",
                    if self.light_getter(a) { "light_builder" } else { "builder" },
                    self.attr_fn(a, "get"),
                    lit(&a.name)
                )
                .unwrap();
            }
            if matches!(a.setter, Setter::None) {
                o.push_str("        let s = None;\n");
            } else {
                writeln!(
                    o,
                    "        let s = Some(rt::setter(scope, rt::builder(super::{file}::{}::<D, LO, HI>), {}, {sig}));",
                    self.attr_fn(a, "set"),
                    lit(&a.name)
                )
                .unwrap();
            }
            if a.is_static {
                writeln!(
                    o,
                    "        rt::static_accessor(scope, t, {}, g, s);",
                    lit(&a.name)
                )
                .unwrap();
            } else if ip.global {
                writeln!(
                    o,
                    "        rt::accessor(scope, inst, {}, g, s, {});",
                    lit(&a.name),
                    a.unforgeable
                )
                .unwrap();
            } else if a.unforgeable {
                writeln!(
                    o,
                    "        rt::accessor(scope, inst, {}, g, s, true);",
                    lit(&a.name)
                )
                .unwrap();
            } else {
                writeln!(
                    o,
                    "        rt::accessor(scope, proto, {}, g, s, false);",
                    lit(&a.name)
                )
                .unwrap();
            }
            o.push_str("    }\n");
        }
        for op in &ip.ops {
            let file = &self.traits[&op.owner].file;
            let len = op
                .overloads
                .iter()
                .map(|ov| self.required(ov))
                .min()
                .unwrap_or(0);
            o.push_str(&member_guard(op.secure, &op.exposure, &None, false));
            // (a promise-returning operation checks `this` itself, to reject: Web IDL §3.7.7)
            let sig = if op.is_static || op.promise {
                "None"
            } else {
                "Some(sig)"
            };
            writeln!(
                o,
                "        let f = rt::method(scope, rt::{}(super::{file}::{}::<D, LO, HI>), {}, {len}, {sig});",
                self.op_builder(op),
                self.op_fn(op),
                lit(&op.name)
            )
            .unwrap();
            if op.is_static {
                writeln!(
                    o,
                    "        rt::operation(scope, &t, {}, f, false);",
                    lit(&op.name)
                )
                .unwrap();
            } else if op.unforgeable || ip.global {
                writeln!(
                    o,
                    "        rt::operation(scope, &inst, {}, f, {});",
                    lit(&op.name),
                    op.unforgeable
                )
                .unwrap();
            } else {
                writeln!(
                    o,
                    "        rt::operation(scope, &proto, {}, f, false);",
                    lit(&op.name)
                )
                .unwrap();
            }
            o.push_str("    }\n");
        }
        if ip.default_json.is_some() {
            o.push_str("    {
        let f = rt::method(scope, rt::builder(op_to_json_default::<D, LO, HI>), \"toJSON\", 0, Some(sig));
        rt::operation(scope, &proto, \"toJSON\", f, false);
    }
");
        }
        // Unforgeable members of ancestors: V8 instance templates do not inherit their parents', so each
        // interface's instance template repeats them (Web IDL §3.7.6: they are own properties of every
        // instance).
        let mut parent = ip.parent.clone();
        while let Some(pn) = parent {
            let Some(pp) = self.ifaces.iter().find(|i| i.name == pn) else {
                break;
            };
            for a in pp.attrs.iter().filter(|a| a.unforgeable && !a.is_static) {
                let file = &self.traits[&a.owner].file;
                o.push_str(&member_guard(
                    a.secure,
                    &a.exposure,
                    &a.ua_gate,
                    a.touch_device,
                ));
                writeln!(
                    o,
                    "        let g = rt::getter(scope, rt::{}(super::{file}::{}::<D, LO, HI>), {}, Some(sig), None, options);",
                    if self.light_getter(a) { "light_builder" } else { "builder" },
                    self.attr_fn(a, "get"),
                    lit(&a.name)
                )
                .unwrap();
                if matches!(a.setter, Setter::None) {
                    o.push_str(
                        "        let s = None;
",
                    );
                } else {
                    writeln!(
                        o,
                        "        let s = Some(rt::setter(scope, rt::builder(super::{file}::{}::<D, LO, HI>), {}, Some(sig)));",
                        self.attr_fn(a, "set"),
                        lit(&a.name)
                    )
                    .unwrap();
                }
                writeln!(
                    o,
                    "        rt::accessor(scope, inst, {}, g, s, true);",
                    lit(&a.name)
                )
                .unwrap();
                o.push_str(
                    "    }
",
                );
            }
            for op in pp.ops.iter().filter(|op| op.unforgeable && !op.is_static) {
                let file = &self.traits[&op.owner].file;
                let len = op
                    .overloads
                    .iter()
                    .map(|ov| self.required(ov))
                    .min()
                    .unwrap_or(0);
                o.push_str(&member_guard(op.secure, &op.exposure, &None, false));
                writeln!(
                    o,
                    "        let f = rt::method(scope, rt::{}(super::{file}::{}::<D, LO, HI>), {}, {len}, Some(sig));",
                    self.op_builder(op),
                    self.op_fn(op),
                    lit(&op.name)
                )
                .unwrap();
                writeln!(
                    o,
                    "        rt::operation(scope, &inst, {}, f, true);",
                    lit(&op.name)
                )
                .unwrap();
                o.push_str(
                    "    }
",
                );
            }
            parent = pp.parent.clone();
        }
        if ip.stringifier.is_some() && !ip.ops.iter().any(|op| op.name == "toString") {
            // Web IDL §3.7.6.2: the toString property is on every object when the stringifier is
            // unforgeable (`[LegacyUnforgeable] stringifier attribute`, Location's `href`), else on
            // the interface prototype object.
            let unforgeable = match &ip.stringifier {
                Some(Stringifier::Attribute(a)) => {
                    ip.attrs.iter().any(|at| &at.name == a && at.unforgeable)
                }
                _ => false,
            };
            let (target, flag) = if unforgeable {
                ("inst", "true")
            } else {
                ("proto", "false")
            };
            let builder = if self.light_stringifier(ip).is_some() {
                "light_builder"
            } else {
                "builder"
            };
            writeln!(
                o,
                "    {{\n        let f = rt::method(scope, rt::{builder}(op_to_string_stringifier::<D, LO, HI>), \"toString\", 0, Some(sig));\n        rt::operation(scope, &{target}, \"toString\", f, {flag});\n    }}"
            )
            .unwrap();
        }
        // Value iterators, and interfaces that support indexed properties (Web IDL 3.7.10: their
        // @@iterator is %Array.prototype.values%).
        if ip.value_iterable {
            o.push_str("    rt::value_iterator(scope, proto);\n");
        } else if ip.indexed_getter.is_some() && ip.pair_iterable.is_none() {
            o.push_str("    rt::indexed_iterator(scope, proto);\n");
        }
        if let Some(sl) = &ip.setlike {
            o.push_str("    {\n        let g = rt::getter(scope, rt::builder(setlike_size_get::<D, LO, HI>), \"size\", Some(sig), None, options);\n        rt::accessor(scope, proto, \"size\", g, None, false);\n    }\n");
            let mut members = vec![("has", "setlike_has", 1)];
            if !sl.readonly {
                members.extend([
                    ("add", "setlike_add", 1),
                    ("delete", "setlike_delete", 1),
                    ("clear", "setlike_clear", 0),
                ]);
            }
            for (name, f, len) in members {
                writeln!(
                    o,
                    "    {{\n        let f = rt::method(scope, rt::builder({f}::<D, LO, HI>), {}, {len}, Some(sig));\n        rt::operation(scope, &proto, {}, f, false);\n    }}",
                    lit(name),
                    lit(name)
                )
                .unwrap();
            }
        }
        if let Some(ai) = &ip.async_iterable {
            o.push_str("    {\n        let f = rt::method(scope, rt::builder(aiter_values::<D, LO, HI>), \"values\", 0, Some(sig));\n        rt::operation(scope, &proto, \"values\", f, false);\n        let next = rt::method(scope, rt::builder(aiter_next::<D, LO, HI>), \"next\", 0, None);\n");
            if ai.ret.is_some() {
                o.push_str("        let ret = Some(rt::method(scope, rt::builder(aiter_return::<D, LO, HI>), \"return\", 1, None));\n");
            } else {
                o.push_str("        let ret = None;\n");
            }
            writeln!(
                o,
                "        rt::async_iterator_template(scope, ids::{}, {}, next, ret);\n    }}",
                upper(&ip.name),
                lit(&format!("{} AsyncIterator", ip.name))
            )
            .unwrap();
        }
        if ip.pair_iterable.is_some() || ip.setlike.is_some() {
            for (name, f) in [
                ("entries", "iter_entries"),
                ("keys", "iter_keys"),
                ("values", "iter_values"),
            ] {
                writeln!(
                    o,
                    "    {{\n        let f = rt::method(scope, rt::builder({f}::<D, LO, HI>), {}, 0, Some(sig));\n        rt::operation(scope, &proto, {}, f, false);\n    }}",
                    lit(name),
                    lit(name)
                )
                .unwrap();
            }
            o.push_str("    {\n        let f = rt::method(scope, rt::builder(iter_for_each::<D, LO, HI>), \"forEach\", 1, Some(sig));\n        rt::operation(scope, &proto, \"forEach\", f, false);\n        // @@iterator is the same function object as entries (Web IDL §3.7.9.1); set when the context is\n        // installed. The iterator prototype object's next():\n        let next = rt::method(scope, rt::builder(iter_next::<D, LO, HI>), \"next\", 0, None);\n");
            writeln!(
                o,
                "        rt::pair_iterator_template(scope, ids::{id}, {}, next);\n    }}",
                lit(&format!("{} Iterator", ip.name))
            )
            .unwrap();
        }
        if ip.global && ip.name == "Window" {
            // The WindowProxy's indexed properties: the child navigables (HTML §7.2.3.3).
            o.push_str("    inst.set_indexed_property_handler(v8::IndexedPropertyHandlerConfiguration::new().getter(rt::guarded_indexed_get(window_child_get::<D, LO, HI>)).query(rt::guarded_indexed_query(window_child_query::<D, LO, HI>)).enumerator(rt::guarded_enumerate(window_child_enumerate::<D, LO, HI>)).flags(v8::PropertyHandlerFlags::NONE));\n");
        }
        if ip.indexed_getter.is_some() {
            let mut cfg = "v8::IndexedPropertyHandlerConfiguration::new().getter(rt::guarded_indexed_get(indexed_get::<D, LO, HI>)).query(rt::guarded_indexed_query(indexed_query::<D, LO, HI>)).enumerator(rt::guarded_enumerate(indexed_enumerate::<D, LO, HI>)).setter(rt::guarded_indexed_set(indexed_set::<D, LO, HI>)).deleter(rt::guarded_indexed_delete(indexed_delete::<D, LO, HI>))".to_string();
            cfg.push_str(".flags(v8::PropertyHandlerFlags::NONE)");
            writeln!(o, "    inst.set_indexed_property_handler({cfg});").unwrap();
        }
        if ip.named_getter.is_some() {
            let mut cfg = "v8::NamedPropertyHandlerConfiguration::new().getter(rt::guarded_named_get(named_get::<D, LO, HI>)).query(rt::guarded_named_query(named_query::<D, LO, HI>)).enumerator(rt::guarded_enumerate(named_enumerate::<D, LO, HI>)).deleter(rt::guarded_named_delete(named_delete::<D, LO, HI>))".to_string();
            if ip.named_setter.is_some() {
                cfg.push_str(".setter(rt::guarded_named_set(named_set::<D, LO, HI>))");
            }
            if ip.override_builtins {
                cfg.push_str(".flags(v8::PropertyHandlerFlags::ONLY_INTERCEPT_STRINGS)");
            } else {
                cfg.push_str(".flags(v8::PropertyHandlerFlags::ONLY_INTERCEPT_STRINGS | v8::PropertyHandlerFlags::NON_MASKING)");
            }
            if ip.global {
                // The named properties object (Web IDL §3.7.4) carries the handler, not the global.
                writeln!(
                    o,
                    "    {{\n        let np = v8::FunctionTemplate::builder(rt::illegal_constructor).build(scope);\n        np.set_class_name(rt::name(scope, {0}));\n        let npi = np.instance_template(scope);\n        // The window's handle is in the object's own fields, so the handlers find their window.\n        npi.set_internal_field_count(rt::WRAPPER_FIELDS);\n        rt::to_string_tag(scope, npi, {0});\n        npi.set_named_property_handler({cfg});\n        rt::set_named_properties_template(scope, ids::{1}, np);\n    }}",
                    lit(&format!("{}Properties", ip.name)),
                    upper(&ip.name)
                )
                .unwrap();
            } else {
                writeln!(o, "    inst.set_named_property_handler({cfg});").unwrap();
            }
        }
        if (ip.global && ip.name == GLOBAL) || ip.name == "Location" {
            // WindowProxy's and Location's [[SetPrototypeOf]] (HTML §7.2.3.2, §7.2.4.1):
            // SetImmutablePrototype.
            o.push_str("    inst.set_immutable_proto();\n");
        }
        self.emit_access_check(o, ip);
        o.push_str("    t\n}\n");
    }

    /// The access check of an interface with cross-origin properties (HTML §7.2.1.3.2): the table of
    /// the members, with the callbacks that perform their steps.
    fn emit_access_check(&self, o: &mut String, ip: &IfacePlan) {
        let Some((_, members)) = CROSS_ORIGIN_PROPERTIES.iter().find(|(i, _)| *i == ip.name) else {
            return;
        };
        let file = &self.traits[&ip.name].file;
        writeln!(
            o,
            "    rt::guard_instances::<D>(scope, inst, ids::{}, vec![",
            upper(&ip.name)
        )
        .unwrap();
        for (name, get, set) in *members {
            if !get && !set {
                let op = ip
                    .ops
                    .iter()
                    .find(|op| op.name == *name)
                    .unwrap_or_else(|| panic!("no cross-origin operation {name}"));
                let len = op
                    .overloads
                    .iter()
                    .map(|ov| self.required(ov))
                    .min()
                    .unwrap_or(0);
                writeln!(
                    o,
                    "        rt::CrossOriginProperty {{ name: {}, kind: rt::CrossOriginKind::Method {{ call: rt::callback(super::{file}::{}::<D, LO, HI>), length: {len} }} }},",
                    lit(name),
                    self.op_fn(op)
                )
                .unwrap();
                continue;
            }
            let a = ip
                .attrs
                .iter()
                .find(|a| a.name == *name)
                .unwrap_or_else(|| panic!("no cross-origin attribute {name}"));
            let cb = |wanted: bool, suffix: &str| {
                if wanted {
                    format!(
                        "Some(rt::callback(super::{file}::{}::<D, LO, HI>))",
                        self.attr_fn(a, suffix)
                    )
                } else {
                    "None".to_string()
                }
            };
            writeln!(
                o,
                "        rt::CrossOriginProperty {{ name: {}, kind: rt::CrossOriginKind::Accessor {{ get: {}, set: {} }} }},",
                lit(name),
                cb(*get, "get"),
                cb(*set, "set")
            )
            .unwrap();
        }
        o.push_str("    ]);\n");
    }

    /// Whether `member` of `owner` is in CrossOriginProperties for this kind of access.
    fn cross_origin_member(&self, owner: &str, member: &str, access: Access) -> bool {
        CROSS_ORIGIN_PROPERTIES
            .iter()
            .filter(|(i, _)| *i == owner)
            .flat_map(|(_, members)| members.iter())
            .any(|(name, get, set)| {
                *name == member
                    && match access {
                        Access::Get => *get,
                        Access::Set => *set,
                        Access::Call => !*get && !*set,
                    }
            })
    }

    /// Whether the members of `owner` can have a Window or Location as `this`.
    /// Whether a member of `owner` with a receiver signature may read its receiver unchecked: not when
    /// it is security-checked, or can be called on a global object, whose receiver may arrive through
    /// its global proxy (members of a global interface, its ancestors and their mixins).
    fn unchecked_receiver(&self, owner: &str) -> bool {
        if self.security_checked(owner) {
            return false;
        }
        !self.ifaces.iter().filter(|g| g.global).any(|g| {
            let mut cur = Some(g);
            while let Some(i) = cur {
                if i.name == owner || i.uses.contains(owner) {
                    return true;
                }
                cur = i
                    .parent
                    .as_ref()
                    .and_then(|p| self.ifaces.iter().find(|x| &x.name == p));
            }
            false
        })
    }

    fn security_checked(&self, owner: &str) -> bool {
        SECURITY_CHECKED.contains(&owner)
            || self
                .ifaces
                .iter()
                .find(|i| i.name == GLOBAL)
                .is_some_and(|w| w.uses.contains(owner))
    }

    /// Whether an attribute's getter performs a security check. V8 calls a Fast API getter with no way
    /// to throw from Rust's side of the check, so such an attribute has none.
    fn security_checked_attr(&self, a: &AttrPlan) -> bool {
        self.security_checked(&a.owner) && !self.cross_origin_member(&a.owner, &a.name, Access::Get)
    }

    /// The `this` of a member of `iface` that is not a cross-origin one, with its security check.
    fn checked_this(&self, iface: &str) -> String {
        if self.security_checked(iface) {
            format!("{THIS}\n    if !security_check::<LO>(scope, args.this()) {{ return }}")
        } else {
            THIS.to_string()
        }
    }

    /// The statements that bind `this` in an attribute's getter or setter.
    fn attr_this(&self, a: &AttrPlan, access: Access) -> String {
        if a.is_static {
            return String::new();
        }
        if self.cross_origin_member(&a.owner, &a.name, access) {
            return "let Some(this) = rt::cross_origin_this(scope, &args, LO, HI) else { return rt::throw_illegal_invocation(scope) };".to_string();
        }
        let this = if a.lenient_this {
            "let Some(this) = rt::unwrap(&args.this(), LO, HI) else { return };".to_string()
        } else if matches!(access, Access::Get) && matches!(a.ty.kind, TypeKind::Promise(_)) {
            // Web IDL §3.7.6: a promise-typed attribute's getter rejects instead of throwing when
            // `this` is not one of the interface's objects (its template has no receiver signature).
            THIS_PROMISE.to_string()
        } else if self.unchecked_receiver(&a.owner) {
            // (the template has `Some(sig)`: not static, not `[LegacyLenientThis]`, not a promise)
            THIS_UNCHECKED.to_string()
        } else {
            THIS.to_string()
        };
        if self.security_checked(&a.owner) {
            format!("{this}\n    if !security_check::<LO>(scope, args.this()) {{ return }}")
        } else {
            this
        }
    }

    /// Interceptors of a legacy platform object (Web IDL §3.9): indexed and named properties.
    fn emit_interceptors(&self, o: &mut String, ip: &IfacePlan, _tr: &str) {
        if ip.global && ip.name == "Window" {
            o.push_str("fn window_child_get<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) -> v8::Intercepted {\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else { return v8::Intercepted::kNo };\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    match <D as rt::Host>::window_child(dom, this, index) {\n        Some(w) => { if let Some(js) = rt::wrap(scope, w) { rv.set(js.into()) } v8::Intercepted::kYes }\n        None => v8::Intercepted::kNo,\n    }\n}\n");
            o.push_str("fn window_child_query<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Integer>) -> v8::Intercepted {\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else { return v8::Intercepted::kNo };\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    if <D as rt::Host>::window_child(dom, this, index).is_none() { return v8::Intercepted::kNo }\n    // Read-only, enumerable, configurable.\n    rv.set_int32(1);\n    v8::Intercepted::kYes\n}\n");
            o.push_str("fn window_child_enumerate<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Array>) {\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else { return };\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let mut keys: Vec<v8::Local<v8::Value>> = Vec::new();\n    let mut i = 0u32;\n    while <D as rt::Host>::window_child(dom, this, i).is_some() { keys.push(v8::Integer::new_from_unsigned(scope, i).into()); i += 1; }\n    rv.set(v8::Array::new_with_elements(scope, &keys));\n}\n");
        }
        let holder = "let Some(this) = rt::unwrap(&args.holder(), LO, HI) else { return v8::Intercepted::kNo };\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();";
        if let Some(g) = &ip.indexed_getter {
            let tr = self.trait_path(&g.owner);
            let m = &g.method;
            let iprof = prof(&format!("{}[index]", ip.name));
            writeln!(o, "fn indexed_get<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) -> v8::Intercepted {{\n    {iprof}\n    {holder}\n    let r = match <D as {tr}>::{m}(dom, this, index) {{\n        Some(v) => {{ if let Some(js) = v.to_js(scope) {{ rv.set(js) }} v8::Intercepted::kYes }}\n        None => v8::Intercepted::kNo,\n    }};\n    r\n}}\n").unwrap();
            let writable = if ip.indexed_setter.is_some() { 0 } else { 1 };
            writeln!(o, "fn indexed_query<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Integer>) -> v8::Intercepted {{\n    {holder}\n    if <D as {tr}>::{m}(dom, this, index).is_none() {{ return v8::Intercepted::kNo }}\n    rv.set_int32({writable});\n    v8::Intercepted::kYes\n}}\n").unwrap();
            writeln!(o, "fn indexed_enumerate<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Array>) {{\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else {{ return }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let mut keys: Vec<v8::Local<v8::Value>> = Vec::new();\n    let mut i = 0u32;\n    while <D as {tr}>::{m}(dom, this, i).is_some() {{ keys.push(v8::Integer::new_from_unsigned(scope, i).into()); i += 1; }}\n    rv.set(v8::Array::new_with_elements(scope, &keys));\n}}\n").unwrap();
            writeln!(o, "fn indexed_delete<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Boolean>) -> v8::Intercepted {{\n    {holder}\n    if <D as {tr}>::{m}(dom, this, index).is_none() {{ return v8::Intercepted::kNo }}\n    rv.set_bool(false);\n    v8::Intercepted::kYes\n}}\n").unwrap();
            match &ip.indexed_setter {
                Some(s) => {
                    let str_ = self.trait_path(&s.owner);
                    let conv = self.conv(&s.value, "value", "The value");
                    let lec = le(&conv);
                    let param = self.rust_arg(&s.value, false);
                    writeln!(o, "fn indexed_set<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, value: v8::Local<v8::Value>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Boolean>) -> v8::Intercepted {{\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else {{ return v8::Intercepted::kNo }};\n    let Some(v) = {lec} else {{ return v8::Intercepted::kYes }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    {}\n    let r = {{ let mut cx = Cx::new(scope); <D as {str_}>::{}(dom, &mut cx, this, index, {}) }};\n    {}\n    rt::check(scope, r);\n    v8::Intercepted::kYes\n}}\n", self.ce_push(s.ce_reactions), s.method, pass(&param, "v"), self.ce_pop(s.ce_reactions)).unwrap();
                }
                None => {
                    writeln!(o, "fn indexed_set<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, index: u32, value: v8::Local<v8::Value>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Boolean>) -> v8::Intercepted {{\n    if rt::unwrap(&args.holder(), LO, HI).is_none() {{ return v8::Intercepted::kNo }}\n    // No indexed property setter: [[DefineOwnProperty]] of an array index returns false (V8 throws in
    // strict code).\n    rv.set_bool(false);\n    v8::Intercepted::kYes\n}}\n").unwrap();
                }
            }
        }
        if let Some(g) = &ip.named_getter {
            let tr = self.trait_path(&g.owner);
            let m = &g.method;
            let names_tr = &tr;
            let nprof = prof(&format!("{}[name]", ip.name));
            writeln!(o, "fn named_get<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, key: v8::Local<v8::Name>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Value>) -> v8::Intercepted {{\n    {nprof}\n    {holder}\n    let name = key.to_rust_string_lossy(scope);\n    let r = match <D as {tr}>::{m}(dom, this, &name) {{\n        Some(v) => {{ if let Some(js) = v.to_js(scope) {{ rv.set(js) }} v8::Intercepted::kYes }}\n        None => v8::Intercepted::kNo,\n    }};\n    r\n}}\n").unwrap();
            let mut attrs = 0;
            // A legacy platform object's named properties are writable when it has a named setter
            // (Web IDL §3.9.1); a named properties object's always are (§3.7.4.1), so assigning to
            // the global creates an own property that shadows the named one.
            if ip.named_setter.is_none() && !ip.global {
                attrs |= 1;
            }
            if ip.unenumerable_named {
                attrs |= 2;
            }
            writeln!(o, "fn named_query<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, key: v8::Local<v8::Name>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Integer>) -> v8::Intercepted {{\n    {holder}\n    let name = key.to_rust_string_lossy(scope);\n    if <D as {tr}>::{m}(dom, this, &name).is_none() {{ return v8::Intercepted::kNo }}\n    rv.set_int32({attrs});\n    v8::Intercepted::kYes\n}}\n").unwrap();
            writeln!(o, "fn named_enumerate<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Array>) {{\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else {{ return }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    let names = <D as {names_tr}>::supported_property_names(dom, this);\n    let mut keys: Vec<v8::Local<v8::Value>> = Vec::with_capacity(names.len());\n    for n in &names {{ let Some(k) = rt::str(scope, n) else {{ return }}; keys.push(k.into()); }}\n    rv.set(v8::Array::new_with_elements(scope, &keys));\n}}\n").unwrap();
            match &ip.named_deleter {
                Some(d) => {
                    let dtr = self.trait_path(&d.owner);
                    writeln!(o, "fn named_delete<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, key: v8::Local<v8::Name>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Boolean>) -> v8::Intercepted {{\n    {holder}\n    let name = key.to_rust_string_lossy(scope);\n    if <D as {tr}>::{m}(dom, this, &name).is_none() {{ return v8::Intercepted::kNo }}\n    {}\n    let r = {{ let mut cx = Cx::new(scope); <D as {dtr}>::{}(dom, &mut cx, this, &name) }};\n    {}\n    rt::check(scope, r);\n    rv.set_bool(true);\n    v8::Intercepted::kYes\n}}\n", self.ce_push(d.ce_reactions), d.method, self.ce_pop(d.ce_reactions)).unwrap();
                }
                None => {
                    writeln!(o, "fn named_delete<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, key: v8::Local<v8::Name>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Boolean>) -> v8::Intercepted {{\n    {holder}\n    let name = key.to_rust_string_lossy(scope);\n    if <D as {tr}>::{m}(dom, this, &name).is_none() {{ return v8::Intercepted::kNo }}\n    rv.set_bool(false);\n    v8::Intercepted::kYes\n}}\n").unwrap();
                }
            }
            if let Some(s) = &ip.named_setter {
                let str_ = self.trait_path(&s.owner);
                let conv = self.conv(&s.value, "value", "The value");
                let lec = le(&conv);
                let param = self.rust_arg(&s.value, false);
                writeln!(o, "fn named_set<D: Dom, const LO: u16, const HI: u16>(scope: &mut v8::PinScope, key: v8::Local<v8::Name>, value: v8::Local<v8::Value>, args: v8::PropertyCallbackArguments, mut rv: v8::ReturnValue<v8::Boolean>) -> v8::Intercepted {{\n    let Some(this) = rt::unwrap(&args.holder(), LO, HI) else {{ return v8::Intercepted::kNo }};\n    let name = key.to_rust_string_lossy(scope);\n    let Some(v) = {lec} else {{ return v8::Intercepted::kYes }};\n    let st = rt::state(scope);\n    let dom = st.engine::<D>();\n    {}\n    let r = {{ let mut cx = Cx::new(scope); <D as {str_}>::{}(dom, &mut cx, this, &name, {}) }};\n    {}\n    rt::check(scope, r);\n    v8::Intercepted::kYes\n}}\n", self.ce_push(s.ce_reactions), s.method, pass(&param, "v"), self.ce_pop(s.ce_reactions)).unwrap();
            }
        }
        let _ = Cat::Any;
    }
}

/// The opening of a member's block in an interface template: installed only in secure contexts
/// (`[SecureContext]`), only in the realms whose globals it is exposed in, only where the user
/// agent's browser family has it (`[UaGate=Chromium]`), and only where the user agent has a touch
/// device (`[TouchDevice]`).
fn member_guard(
    secure: bool,
    exposure: &Option<Vec<String>>,
    ua_gate: &Option<String>,
    touch_device: bool,
) -> String {
    let mut conds = Vec::new();
    if let Some(family) = ua_gate {
        conds.push(format!("options.ua_allows({})", lit(family)));
    }
    if touch_device {
        conds.push("options.touch_events".to_string());
    }
    if secure {
        conds.push("options.secure_context".to_string());
    }
    if let Some(names) = exposure {
        let names: Vec<String> = names.iter().map(|n| lit(n)).collect();
        conds.push(format!("options.exposes(&[{}])", names.join(", ")));
    }
    if conds.is_empty() {
        "    {\n".into()
    } else {
        format!("    if {} {{\n", conds.join(" && "))
    }
}
