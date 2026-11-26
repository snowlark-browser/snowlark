// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Emission of `generated/types.rs`: enumerations, dictionaries, unions, callback functions and callback
//! interfaces used by the generated members.

use super::conv::Cat;
use super::names::*;
use super::plan::*;
use crate::ast::*;
use std::fmt::Write;

impl Plan<'_> {
    pub fn is_any(&self, ty: &Type) -> bool {
        matches!(self.prune(ty).kind, TypeKind::Prim(Prim::Any))
    }

    /// Whether a nullable member's field tells null from absent (`Option<Option<T>>`): its
    /// dictionary is in [`NULL_PRESENT`] for it.
    pub fn null_present(&self, m: &DictMember) -> bool {
        NULL_PRESENT.contains(&m.name.as_str()) && !m.required && m.default.is_none()
    }

    /// The Rust type of a dictionary member field.
    pub fn member_type(&self, m: &DictMember) -> String {
        let t = self.rust_arg(&m.ty, true);
        let optional = !m.required && (m.default.is_none() || self.is_any(&m.ty));
        if optional && (!t.starts_with("Option<") || self.null_present(m)) {
            format!("Option<{t}>")
        } else {
            t
        }
    }

    /// Dictionary members in conversion order: inherited dictionaries first, each lexicographically.
    /// Members of types that are not generated are left out; their types are pruned.
    pub fn dictionary_members(&self, name: &str) -> Vec<DictMember> {
        let mut out = Vec::new();
        for d in self.model.dictionary_chain(name) {
            let mut ms: Vec<DictMember> = d
                .members
                .iter()
                .filter(|m| {
                    self.unsupported(&m.ty).is_none()
                        && super::plan::left_out(&d.name, &m.name).is_none()
                })
                .cloned()
                .map(|mut m| {
                    m.ty = self.prune(&m.ty);
                    m
                })
                .collect();
            ms.sort_by(|a, b| a.name.cmp(&b.name));
            out.extend(ms);
        }
        out
    }

    fn dictionary_has_default(&self, name: &str) -> bool {
        self.dictionary_members(name).iter().all(|m| !m.required)
    }

    pub fn emit_types(&self) -> String {
        let mut o = String::new();
        o.push_str(HEADER);
        o.push_str("//! Enumerations, dictionaries, unions and callbacks used by the generated interfaces.\n\n");
        o.push_str("#![allow(unused_imports)]\n\n");
        o.push_str("use super::ids;\nuse crate::rt::{self, Cx, DomStr, Handle, JsObject, JsPromise, JsValue, Record, Result, ToJs};\n\n");
        for e in &self.enums {
            self.emit_enum(&mut o, e);
        }
        for d in &self.dictionaries {
            self.emit_dictionary(&mut o, d);
        }
        for u in self.unions.values() {
            self.emit_union(&mut o, u);
        }
        for c in &self.callbacks {
            self.emit_callback(&mut o, c);
        }
        for c in &self.callback_interfaces {
            self.emit_callback_interface(&mut o, c);
        }
        o
    }

    fn emit_enum(&self, o: &mut String, name: &str) {
        let e = &self.model.enums[name];
        let variants: Vec<String> = e
            .values
            .iter()
            .map(|v| self.enum_variant(name, v))
            .collect();
        writeln!(o, "/// `enum {name}` ({}).", e.span.file).unwrap();
        writeln!(
            o,
            "#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]\npub enum {name} {{"
        )
        .unwrap();
        for (v, s) in variants.iter().zip(&e.values) {
            writeln!(o, "    /// `\"{s}\"`\n    {v},").unwrap();
        }
        o.push_str("}\n\n");
        writeln!(o, "impl {name} {{").unwrap();
        writeln!(
            o,
            "    pub const VALUES: &'static [&'static str] = &[{}];",
            e.values
                .iter()
                .map(|v| lit(v))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        writeln!(
            o,
            "    const ALL: &'static [{name}] = &[{}];",
            variants
                .iter()
                .map(|v| format!("{name}::{v}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        o.push_str("    /// The IDL value.\n    pub fn as_str(self) -> &'static str {\n        Self::VALUES[self as usize]\n    }\n\n");
        o.push_str("    /// The variant of an IDL value.\n    pub fn parse(value: &str) -> Option<Self> {\n        Self::VALUES.iter().position(|v| *v == value).map(|i| Self::ALL[i])\n    }\n\n");
        writeln!(
            o,
            "    pub(crate) fn from_js(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<Self> {{\n        rt::to_enum(scope, v, Self::VALUES, {}).map(|i| Self::ALL[i])\n    }}\n}}\n",
            lit(name)
        )
        .unwrap();
        writeln!(
            o,
            "impl ToJs for {name} {{\n    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {{\n        self.as_str().to_js(scope)\n    }}\n}}\n"
        )
        .unwrap();
    }

    fn emit_dictionary(&self, o: &mut String, name: &str) {
        let members = self.dictionary_members(name);
        let d = &self.model.dictionaries[name];
        writeln!(
            o,
            "/// `dictionary {name}{}` ({}).",
            d.inherits
                .as_ref()
                .map(|p| format!(" : {p}"))
                .unwrap_or_default(),
            d.span.file
        )
        .unwrap();
        writeln!(o, "#[derive(Clone)]\npub struct {name} {{").unwrap();
        for m in &members {
            writeln!(
                o,
                "    /// `{}{} {}{}`\n    pub {}: {},",
                if m.required { "required " } else { "" },
                m.ty.idl(),
                m.name,
                m.default
                    .as_ref()
                    .map(|d| format!(" = {}", literal_idl(d)))
                    .unwrap_or_default(),
                snake(&m.name),
                self.member_type(m)
            )
            .unwrap();
        }
        o.push_str("}\n\n");
        let default_of = |m: &DictMember| -> String {
            match &m.default {
                Some(_) if self.is_any(&m.ty) => "None".into(),
                Some(d) => self.default_value(&m.ty, d),
                None => "None".into(),
            }
        };
        if self.dictionary_has_default(name) {
            writeln!(
                o,
                "// The member defaults, written out (some equal the types' defaults).\n#[allow(clippy::derivable_impls)]\nimpl Default for {name} {{\n    fn default() -> Self {{\n        {name} {{"
            )
            .unwrap();
            for m in &members {
                writeln!(o, "            {}: {},", snake(&m.name), default_of(m)).unwrap();
            }
            o.push_str("        }\n    }\n}\n\n");
        }
        writeln!(o, "impl {name} {{").unwrap();
        writeln!(
            o,
            "    pub(crate) fn from_js(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<Self> {{"
        )
        .unwrap();
        writeln!(
            o,
            "        let {} = rt::dictionary_object(scope, v, {})?;",
            if members.is_empty() { "_obj" } else { "obj" },
            lit(name)
        )
        .unwrap();
        for m in &members {
            let field = snake(&m.name);
            let conv = self.conv(&m.ty, "v", &format!("{name}.{}", m.name));
            let optional_field = self.member_type(m).starts_with("Option<")
                && (!self.rust_arg(&m.ty, true).starts_with("Option<") || self.null_present(m));
            let present = if optional_field {
                format!("Some({conv}?)")
            } else {
                format!("{conv}?")
            };
            let absent = if m.required {
                format!(
                    "return rt::type_error(scope, \"Failed to read the '{}' property from '{name}': Required member is undefined.\")",
                    m.name
                )
            } else {
                default_of(m)
            };
            writeln!(
                o,
                "        let m_{field} = match obj {{ Some(o) => {{ let v = rt::get_named_member(scope, o, {})?; if v.is_undefined() {{ {absent} }} else {{ {present} }} }} None => {{ {absent} }} }};",
                name_key(&m.name)
            )
            .unwrap();
        }
        writeln!(
            o,
            "        Some({name} {{ {} }})\n    }}",
            members
                .iter()
                .map(|m| format!("{0}: m_{0}", snake(&m.name)))
                .collect::<Vec<_>>()
                .join(", ")
        )
        .unwrap();
        if super::plan::ENGINE_CONVERTED_DICTIONARIES.contains(&name) {
            writeln!(
                o,
                "\n    /// Converts a value the engine holds to this dictionary (Web IDL §3.2.17), for an algorithm\n    /// that converts an `any` argument itself; `Err(Pending)` with the exception thrown.\n    pub fn from_value(cx: &mut Cx, v: &JsValue) -> Result<Self> {{\n        let scope = cx.scope();\n        let v = v8::Local::new(scope, v.global());\n        Self::from_js(scope, v).ok_or(rt::Error::Pending)\n    }}"
            )
            .unwrap();
        }
        o.push_str("}\n\n");
        writeln!(
            o,
            "impl ToJs for {name} {{\n    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {{\n        let o = v8::Object::new(scope);"
        )
        .unwrap();
        // (in conversion order; members that can only be null are written as null)
        let mut order: Vec<(String, Option<&DictMember>)> = Vec::new();
        for d in self.model.dictionary_chain(name) {
            let mut names: Vec<(String, Option<&DictMember>)> = d
                .members
                .iter()
                .filter_map(|dm| {
                    if let Some(m) = members.iter().find(|m| m.name == dm.name) {
                        Some((dm.name.clone(), Some(m)))
                    } else if dm.required && self.always_null(&dm.ty) {
                        Some((dm.name.clone(), None))
                    } else {
                        None
                    }
                })
                .collect();
            names.sort_by(|a, b| a.0.cmp(&b.0));
            order.extend(names);
        }
        for (member_name, m) in order {
            let Some(m) = m else {
                writeln!(
                    o,
                    "        {{ let v = v8::null(scope).into(); o.create_data_property(scope, rt::cached_name(scope, {}).into(), v)?; }}",
                    name_key(&member_name)
                )
                .unwrap();
                continue;
            };
            let field = snake(&m.name);
            if self.member_type(m).starts_with("Option<")
                && !self.rust_arg(&m.ty, true).starts_with("Option<")
            {
                writeln!(
                    o,
                    "        if let Some(x) = &self.{field} {{ let v = x.to_js(scope)?; o.create_data_property(scope, rt::cached_name(scope, {}).into(), v)?; }}",
                    name_key(&m.name)
                )
                .unwrap();
            } else {
                writeln!(
                    o,
                    "        {{ let v = self.{field}.to_js(scope)?; o.create_data_property(scope, rt::cached_name(scope, {}).into(), v)?; }}",
                    name_key(&m.name)
                )
                .unwrap();
            }
        }
        o.push_str("        Some(o.into())\n    }\n}\n\n");
    }

    fn emit_union(&self, o: &mut String, u: &UnionPlan) {
        let name = &u.rust;
        let idl = format!(
            "({})",
            u.members
                .iter()
                .map(|t| t.idl())
                .collect::<Vec<_>>()
                .join(" or ")
        );
        // (union values are arguments, converted and consumed at once: a dictionary member makes a
        // variant large without the enum living long)
        writeln!(
            o,
            "/// `{idl}`\n#[derive(Clone)]\n#[allow(clippy::large_enum_variant)]\npub enum {name} {{"
        )
        .unwrap();
        for t in &u.members {
            let ty = Type {
                nullable: false,
                ..t.clone()
            };
            writeln!(
                o,
                "    {}({}),",
                self.variant_label(t),
                self.rust_arg(&ty, true)
            )
            .unwrap();
        }
        o.push_str("}\n\n");
        writeln!(o, "impl {name} {{").unwrap();
        writeln!(
            o,
            "    pub(crate) fn from_js(scope: &v8::PinScope<'_, '_>, v: v8::Local<v8::Value>) -> Option<Self> {{"
        )
        .unwrap();
        let candidates: Vec<(usize, Type)> = u
            .members
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (
                    i,
                    Type {
                        nullable: false,
                        ..t.clone()
                    },
                )
            })
            .collect();
        let pick = |i: usize| {
            let t = &candidates[i].1;
            let conv = self.conv(t, "v", "The value");
            let variant = self.variant_label(t);
            match infallible(&conv) {
                Some(inner) => format!("Some({name}::{variant}({inner}))"),
                None => format!("({conv}).map({name}::{variant})"),
            }
        };
        let fallback = format!(
            "rt::type_error(scope, {})",
            lit(&format!("The value is not of type '{idl}'"))
        );
        // Undefined and null go to a dictionary member, if any (step 2).
        let body = self.dispatch(&candidates, &[], "v", &pick, &fallback);
        o.push_str(&body);
        o.push_str("    }\n}\n\n");
        writeln!(
            o,
            "impl ToJs for {name} {{\n    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {{\n        match self {{"
        )
        .unwrap();
        for t in &u.members {
            let v = self.variant_label(t);
            if self.category(t) == Cat::Sequence
                && matches!(self.prune(t).kind, TypeKind::FrozenArray(_))
            {
                writeln!(
                    o,
                    "            {name}::{v}(x) => rt::Frozen(x.clone()).to_js(scope),"
                )
                .unwrap();
            } else {
                writeln!(o, "            {name}::{v}(x) => x.to_js(scope),").unwrap();
            }
        }
        o.push_str("        }\n    }\n}\n\n");
    }

    /// Parameters of a callback's invoke method, and the statement building `argv` (the JS arguments).
    fn callback_params(&self, args: &[Argument]) -> (Vec<String>, String) {
        let mut params = Vec::new();
        let mut items = Vec::new();
        let mut rest = None;
        for a in args {
            let pname = snake(&a.name);
            let t = if a.variadic {
                format!("Vec<{}>", self.rust_arg(&a.ty, true))
            } else if a.optional && !self.rust_arg(&a.ty, false).starts_with("Option<") {
                format!("Option<{}>", self.rust_arg(&a.ty, false))
            } else {
                self.rust_arg(&a.ty, false)
            };
            params.push(format!("{pname}: {t}"));
            if a.variadic {
                rest = Some(pname);
            } else if a.optional && !self.rust_arg(&a.ty, false).starts_with("Option<") {
                items.push(format!(
                    "match &{pname} {{ Some(x) => x.to_js(scope).ok_or(rt::Error::Pending)?, None => v8::undefined(scope).into() }}"
                ));
            } else {
                items.push(format!("{pname}.to_js(scope).ok_or(rt::Error::Pending)?"));
            }
        }
        let argv = match rest {
            None => format!(
                "let argv: [v8::Local<v8::Value>; {}] = [{}];",
                items.len(),
                items.join(", ")
            ),
            Some(r) => format!(
                "let mut argv: Vec<v8::Local<v8::Value>> = vec![{}];\n        for x in &{r} {{ argv.push(x.to_js(scope).ok_or(rt::Error::Pending)?); }}",
                items.join(", ")
            ),
        };
        (params, argv)
    }

    fn return_conv(&self, ret: &Type) -> (String, String) {
        if ret.is_undefined() {
            return ("()".into(), "{ let _ = r; Ok(()) }".into());
        }
        let rust = self.rust_arg(ret, true);
        (
            rust,
            format!(
                "({}).ok_or(rt::Error::Pending)",
                self.conv(ret, "r", "The callback's result")
            ),
        )
    }

    fn emit_callback(&self, o: &mut String, name: &str) {
        let c = &self.model.callbacks[name];
        let (params, argv) = self.callback_params(&c.args);
        let ret = self.prune(&c.ret);
        let (rust_ret, ret_conv) = self.return_conv(&ret);
        writeln!(
            o,
            "/// `callback {name} = {} ({});` ({}).\n#[derive(Clone)]\npub struct {name}(pub JsObject);\n",
            c.ret.idl(),
            c.args.iter().map(|a| format!("{} {}", a.ty.idl(), a.name)).collect::<Vec<_>>().join(", "),
            c.span.file
        )
        .unwrap();
        writeln!(o, "impl {name} {{").unwrap();
        writeln!(
            o,
            "    /// Invokes the callback (Web IDL §3.12 \"invoke a callback function\"). A pending JS exception is\n    /// `Err(Error::Pending)`.\n    pub fn invoke(&self, cx: &mut Cx, this_arg: Option<&dyn ToJs>{}) -> Result<{rust_ret}> {{",
            params.iter().map(|p| format!(", {p}")).collect::<String>()
        )
        .unwrap();
        o.push_str("        rt::in_callback_realm(cx, &self.0, None, |cx| {\n");
        o.push_str("        let scope: &v8::PinScope = cx.scope;\n");
        o.push_str("        let callee: v8::Local<v8::Value> = v8::Local::new(scope, &self.0 .0).into();\n");
        o.push_str("        let recv = rt::callback_this(scope, this_arg);\n");
        writeln!(
            o,
            "        let Ok(f) = v8::Local::<v8::Function>::try_from(callee) else {{ let r: v8::Local<v8::Value> = v8::undefined(scope).into(); return {ret_conv}; }};"
        )
        .unwrap();
        writeln!(o, "        {argv}").unwrap();
        o.push_str("        let r = f.call(scope, recv, &argv).ok_or(rt::Error::Pending)?;\n");
        writeln!(o, "        {ret_conv}\n        }})\n    }}\n}}\n").unwrap();
        writeln!(
            o,
            "impl ToJs for {name} {{\n    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {{\n        self.0.to_js(scope)\n    }}\n}}\n"
        )
        .unwrap();
    }

    fn emit_callback_interface(&self, o: &mut String, name: &str) {
        let ops = self.callback_interface_ops(name);
        let op = ops[0];
        let op_name = op.name.clone().unwrap();
        let (params, argv) = self.callback_params(&op.args);
        let ret = self.prune(&op.ret);
        let (rust_ret, ret_conv) = self.return_conv(&ret);
        let i = &self.model.interfaces[name];
        writeln!(
            o,
            "/// `callback interface {name}` ({}).\n#[derive(Clone)]\npub struct {name}(pub JsObject);\n",
            i.span.file
        )
        .unwrap();
        writeln!(o, "impl {name} {{").unwrap();
        let method = snake(&op_name);
        let typed = params.iter().map(|p| format!(", {p}")).collect::<String>();
        let names = params
            .iter()
            .map(|p| format!(", {}", p.split(':').next().unwrap()))
            .collect::<String>();
        writeln!(
            o,
            "    /// Calls the user object's `{op_name}` operation (Web IDL §3.11 \"call a user object's operation\"):\n    /// the object itself when callable (with `this_arg` as `this`), else its `{op_name}` method.\n    pub fn {method}(&self, cx: &mut Cx, this_arg: Option<&dyn ToJs>{typed}) -> Result<{rust_ret}> {{\n        rt::in_callback_realm(cx, &self.0, None, |cx| self.{method}_in_realm(cx, this_arg{names}))\n    }}\n"
        )
        .unwrap();
        writeln!(
            o,
            "    /// [`Self::{method}`] for a caller already in [`rt::in_callback_realm`].\n    pub fn {method}_in_realm(&self, cx: &mut Cx, this_arg: Option<&dyn ToJs>{typed}) -> Result<{rust_ret}> {{"
        )
        .unwrap();
        o.push_str("        let scope: &v8::PinScope = cx.scope;\n");
        o.push_str("        let obj = v8::Local::new(scope, &self.0 .0);\n");
        writeln!(
            o,
            "        let (f, recv) = rt::callback_interface_method(scope, obj, {}, this_arg)?;",
            name_key(&op_name)
        )
        .unwrap();
        writeln!(o, "        {argv}").unwrap();
        o.push_str("        let r = f.call(scope, recv, &argv).ok_or(rt::Error::Pending)?;\n");
        writeln!(o, "        {ret_conv}\n    }}\n}}\n").unwrap();
        writeln!(
            o,
            "impl ToJs for {name} {{\n    fn to_js<'s>(&self, scope: &v8::PinScope<'s, '_>) -> Option<v8::Local<'s, v8::Value>> {{\n        self.0.to_js(scope)\n    }}\n}}\n"
        )
        .unwrap();
    }
}

/// The value of a conversion that cannot fail (`Some(expr)` with `expr` a single call), if it is one.
pub fn infallible(conv: &str) -> Option<&str> {
    let inner = conv.strip_prefix("Some(")?.strip_suffix(')')?;
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    (depth == 0 && !inner.contains('?')).then_some(inner)
}

/// The top of every generated file: the licence notice (as `tools/license_headers.py` writes it), then the
/// banner.
pub const HEADER: &str = "// This Source Code Form is subject to the terms of the Mozilla Public\n\
// License, v. 2.0. If a copy of the MPL was not distributed with this\n\
// file, You can obtain one at https://mozilla.org/MPL/2.0/.\n\n\
// Generated by snowlark-idl from the Web IDL in idl/. Do not edit; run `snowlark-idl gen`.\n\n";
