// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Emission of value conversions: JS → Rust (Web IDL §3.2 "converted to an IDL value"), Rust → JS, default
//! values, and the type tests shared by union conversion and overload resolution (§3.2.24, §3.7.6).

use super::names::*;
use super::plan::*;
use crate::ast::*;
use crate::model::*;

/// Categories of IDL types for distinguishing values (Web IDL §2.5.6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cat {
    Undefined,
    Boolean,
    Numeric,
    String,
    Object,
    Interface,
    Callback,
    Dictionary,
    Record,
    Sequence,
    CallbackInterface,
    Promise,
    Buffer,
    Any,
}

impl Plan<'_> {
    pub fn category(&self, ty: &Type) -> Cat {
        let ty = self.prune(ty);
        match &ty.kind {
            TypeKind::Prim(p) => match p {
                Prim::Undefined => Cat::Undefined,
                Prim::Boolean => Cat::Boolean,
                p if p.is_numeric() => Cat::Numeric,
                p if p.is_string() => Cat::String,
                Prim::Object => Cat::Object,
                Prim::Any => Cat::Any,
                _ => Cat::Any,
            },
            TypeKind::Named(n) => match self.model.kind_of(n) {
                NamedKind::Interface => Cat::Interface,
                NamedKind::Enum => Cat::String,
                NamedKind::Dictionary => Cat::Dictionary,
                NamedKind::Callback => Cat::Callback,
                NamedKind::CallbackInterface => Cat::CallbackInterface,
                NamedKind::Buffer => Cat::Buffer,
                _ => Cat::Any,
            },
            TypeKind::Sequence(_) | TypeKind::FrozenArray(_) => Cat::Sequence,
            TypeKind::Record(..) => Cat::Record,
            TypeKind::Promise(_) => Cat::Promise,
            _ => Cat::Any,
        }
    }

    fn legacy_null_callback(&self, ty: &Type) -> bool {
        let ty = self.prune(ty);
        match &ty.kind {
            TypeKind::Named(n) => self
                .model
                .callbacks
                .get(n)
                .is_some_and(|c| has_ext(&c.ext, "LegacyTreatNonObjectAsNull")),
            _ => false,
        }
    }

    /// An expression of type `Option<T>` converting the `v8::Local<v8::Value>` expression `v` to the owned
    /// Rust value of `ty` (`None`: an exception is pending). `scope` is in scope as a `&PinScope`.
    pub fn conv(&self, ty: &Type, v: &str, what: &str) -> String {
        let ty = self.prune(ty);
        if ty.nullable && !matches!(ty.kind, TypeKind::Prim(Prim::Any)) {
            if self.legacy_null_callback(&ty) {
                let TypeKind::Named(n) = &ty.kind else {
                    unreachable!()
                };
                return format!(
                    "rt::to_callback_function(scope, {v}, true, {}).map(|o| o.map({n}))",
                    lit(what)
                );
            }
            let inner = Type {
                nullable: false,
                ..ty.clone()
            };
            let inner_conv = self.conv(&inner, "v", what);
            let bind = if v == "v" {
                String::new()
            } else {
                format!("let v = {v}; ")
            };
            // A conversion that cannot fail (`Some(..)`) wraps directly.
            let present = match inner_conv.strip_prefix("Some(") {
                Some(rest) if rest.ends_with(')') && !inner_conv.contains("?") => {
                    format!("Some({inner_conv})")
                }
                _ => format!("({inner_conv}).map(Some)"),
            };
            return format!(
                "{{ {bind}if v.is_null_or_undefined() {{ Some(None) }} else {{ {present} }} }}"
            );
        }
        let w = lit(what);
        match &ty.kind {
            TypeKind::Prim(p) => {
                let mode = if ty.has_ext("EnforceRange") {
                    "rt::IntMode::EnforceRange"
                } else if ty.has_ext("Clamp") {
                    "rt::IntMode::Clamp"
                } else {
                    "rt::IntMode::Wrap"
                };
                match p {
                    Prim::Boolean => format!("Some(rt::to_boolean(scope, {v}))"),
                    Prim::Byte => format!("rt::to_byte(scope, {v}, {mode})"),
                    Prim::Octet => format!("rt::to_octet(scope, {v}, {mode})"),
                    Prim::Short => format!("rt::to_short(scope, {v}, {mode})"),
                    Prim::UnsignedShort => format!("rt::to_unsigned_short(scope, {v}, {mode})"),
                    Prim::Long => format!("rt::to_long(scope, {v}, {mode})"),
                    Prim::UnsignedLong => format!("rt::to_unsigned_long(scope, {v}, {mode})"),
                    Prim::LongLong => format!("rt::to_long_long(scope, {v}, {mode})"),
                    Prim::UnsignedLongLong => {
                        format!("rt::to_unsigned_long_long(scope, {v}, {mode})")
                    }
                    Prim::Float => format!("rt::to_float(scope, {v}, true)"),
                    Prim::UnrestrictedFloat => format!("rt::to_float(scope, {v}, false)"),
                    Prim::Double => format!("rt::to_double(scope, {v}, true)"),
                    Prim::UnrestrictedDouble => format!("rt::to_double(scope, {v}, false)"),
                    Prim::DOMString => {
                        if ty.has_ext("LegacyNullToEmptyString") {
                            format!("rt::to_dom_string_null_empty(scope, {v})")
                        } else {
                            format!("rt::to_dom_string(scope, {v})")
                        }
                    }
                    Prim::USVString => format!("rt::to_usv_string(scope, {v})"),
                    Prim::ByteString => format!("rt::to_byte_string(scope, {v})"),
                    Prim::Object => format!("rt::to_object(scope, {v}, {w})"),
                    Prim::Any | Prim::Symbol => format!("Some(rt::to_any(scope, {v}))"),
                    Prim::Undefined => "Some(())".into(),
                    Prim::BigInt => unreachable!("bigint is not supported"),
                }
            }
            TypeKind::Named(n) => match self.model.kind_of(n) {
                NamedKind::Interface => {
                    let id = upper(n);
                    format!(
                        "rt::to_interface(scope, {v}, ids::{id}, ids::{id}_END, {w}, {})",
                        lit(n)
                    )
                }
                NamedKind::Buffer => {
                    let allow_shared = n == "SharedArrayBuffer" || ty.has_ext("AllowShared");
                    format!("rt::to_buffer(scope, {v}, {allow_shared}, {w})")
                }
                NamedKind::Enum | NamedKind::Dictionary => format!("{n}::from_js(scope, {v})"),
                NamedKind::Callback => {
                    let legacy = self.legacy_null_callback(&ty);
                    format!(
                        "rt::to_callback_function(scope, {v}, {legacy}, {w}).and_then(|o| o.map({n}))"
                    )
                }
                NamedKind::CallbackInterface => {
                    format!("rt::to_callback_interface(scope, {v}, {w}).map({n})")
                }
                _ => unreachable!("{n}"),
            },
            TypeKind::Sequence(t) | TypeKind::FrozenArray(t) => {
                let item = self.conv(t, "item", what);
                format!(
                    "{{ let mut out = Vec::new(); rt::iterate(scope, {v}, |scope, item| {{ out.push({item}?); Some(()) }}).map(|_| out) }}"
                )
            }
            TypeKind::Record(k, t) => {
                let item = self.conv(t, "item", what);
                let key = match k {
                    Prim::USVString => "rt::to_usv_string",
                    Prim::ByteString => "rt::to_byte_string",
                    _ => "rt::to_dom_string",
                };
                // Web IDL §3.2.20, key by key: [[GetOwnProperty]], the key's conversion, [[Get]],
                // the value's conversion.
                format!(
                    "rt::record_entries(scope, {v}, {key}, |scope, item| {item}).map(rt::Record)"
                )
            }
            TypeKind::Promise(_) => format!("rt::to_promise(scope, {v})"),
            TypeKind::AsyncSequence(_) => format!("rt::to_async_sequence(scope, {v})"),
            TypeKind::Union(_) => format!("{}::from_js(scope, {v})", self.rust_arg(&ty, true)),
            _ => unreachable!(),
        }
    }

    /// The Rust expression of an IDL default value for `ty` (owned).
    pub fn default_value(&self, ty: &Type, d: &Literal) -> String {
        let ty = self.prune(ty);
        if matches!(d, Literal::Null) {
            return if matches!(ty.kind, TypeKind::Prim(Prim::Any)) {
                "rt::null_value(scope)".into()
            } else {
                "None".into()
            };
        }
        let wrap = |s: String| {
            if ty.nullable && !matches!(ty.kind, TypeKind::Prim(Prim::Any)) {
                format!("Some({s})")
            } else {
                s
            }
        };
        let inner = match (&ty.kind, d) {
            (TypeKind::Prim(Prim::Any), Literal::Undefined) => "rt::undefined_value(scope)".into(),
            (TypeKind::Prim(Prim::Any), _) => "rt::undefined_value(scope)".into(),
            (TypeKind::Prim(_), Literal::Bool(b)) => b.to_string(),
            (TypeKind::Prim(p), Literal::Integer(_)) => {
                let v = d.integer_value().unwrap();
                if p.is_float() {
                    format!("{v}.0")
                } else {
                    format!("{v}")
                }
            }
            (TypeKind::Prim(_), Literal::Decimal(s)) => {
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    s.clone()
                } else {
                    format!("{s}.0")
                }
            }
            (TypeKind::Prim(p), Literal::Infinity) => format!("{}::INFINITY", prim_rust(*p)),
            (TypeKind::Prim(p), Literal::NegInfinity) => format!("{}::NEG_INFINITY", prim_rust(*p)),
            (TypeKind::Prim(p), Literal::NaN) => format!("{}::NAN", prim_rust(*p)),
            (TypeKind::Prim(_), Literal::String(s)) => format!("String::from({})", lit(s)),
            (TypeKind::Named(n), Literal::String(s)) => {
                format!("{n}::{}", self.enum_variant(n, s))
            }
            (TypeKind::Named(n), Literal::EmptyDictionary) => format!("{n}::default()"),
            (TypeKind::Union(ts), Literal::EmptyDictionary) => {
                let dict = ts
                    .iter()
                    .find(|t| self.category(t) == Cat::Dictionary)
                    .expect("{} default for a union without a dictionary");
                let name = self.rust_arg(
                    &Type {
                        nullable: false,
                        ..ty.clone()
                    },
                    true,
                );
                let TypeKind::Named(dn) = &self.prune(dict).kind else {
                    unreachable!()
                };
                format!("{name}::{}({dn}::default())", self.variant_label(dict))
            }
            (TypeKind::Union(ts), lit_) => {
                // A default for a union picks the member of the literal's type.
                let want = match lit_ {
                    Literal::Bool(_) => Cat::Boolean,
                    Literal::String(_) => Cat::String,
                    // (`(DOMString or sequence<DOMString>) protocols = []`)
                    Literal::EmptySequence => Cat::Sequence,
                    _ => Cat::Numeric,
                };
                let member = ts
                    .iter()
                    .find(|t| self.category(t) == want)
                    .expect("union default without a matching member");
                let name = self.rust_arg(
                    &Type {
                        nullable: false,
                        ..ty.clone()
                    },
                    true,
                );
                format!(
                    "{name}::{}({})",
                    self.variant_label(member),
                    self.default_value(member, lit_)
                )
            }
            (TypeKind::Sequence(_) | TypeKind::FrozenArray(_), Literal::EmptySequence) => {
                "Vec::new()".into()
            }
            (_, Literal::Undefined) => "Default::default()".into(),
            other => panic!("unsupported default {other:?}"),
        };
        wrap(inner)
    }

    /// The Rust variant name of an enumeration value.
    pub fn enum_variant(&self, enum_name: &str, value: &str) -> String {
        let e = &self.model.enums[enum_name];
        let mut seen = std::collections::BTreeSet::new();
        for v in &e.values {
            let mut c = camel(v);
            while !seen.insert(c.clone()) {
                c.push('_');
            }
            if v == value {
                return c;
            }
        }
        panic!("{value} is not a value of {enum_name}")
    }

    /// A boolean expression testing whether the JS value `v` belongs to a type's category, for the
    /// platform-object, callable, object and primitive steps of union conversion and overload resolution.
    pub fn platform_test(&self, ty: &Type, v: &str) -> Option<String> {
        let ty = self.prune(ty);
        match &ty.kind {
            TypeKind::Named(n) if self.model.kind_of(n) == NamedKind::Interface => {
                let id = upper(n);
                Some(format!(
                    "rt::unwrap_value({v}, ids::{id}, ids::{id}_END).is_some()"
                ))
            }
            _ => None,
        }
    }

    /// A boolean expression testing whether the JS value `v` has the internal slot of a buffer source
    /// type (`[[ArrayBufferData]]`, `[[DataView]]`, `[[TypedArrayName]]`), for step 4 of union conversion.
    pub fn buffer_test(&self, ty: &Type, v: &str) -> Option<String> {
        let ty = self.prune(ty);
        let TypeKind::Named(n) = &ty.kind else {
            return None;
        };
        let method = match n.as_str() {
            "ArrayBuffer" => "is_array_buffer",
            "SharedArrayBuffer" => "is_shared_array_buffer",
            "DataView" => "is_data_view",
            "Int8Array" => "is_int8_array",
            "Int16Array" => "is_int16_array",
            "Int32Array" => "is_int32_array",
            "Uint8Array" => "is_uint8_array",
            "Uint16Array" => "is_uint16_array",
            "Uint32Array" => "is_uint32_array",
            "Uint8ClampedArray" => "is_uint8_clamped_array",
            "BigInt64Array" => "is_big_int64_array",
            "BigUint64Array" => "is_big_uint64_array",
            "Float16Array" => "is_float16_array",
            "Float32Array" => "is_float32_array",
            "Float64Array" => "is_float64_array",
            _ => return None,
        };
        Some(format!("{v}.{method}()"))
    }

    /// Emits the selection among `candidates` (an index and its type each) for the JS value `v`, per the
    /// Web IDL order; `pick(i)` is the expression for candidate `i`. `optional[i]` marks candidates
    /// whose argument is optional (they take `undefined`). The code is a block's statements ending in a
    /// tail expression: the last candidate's, or `fallback`.
    pub fn dispatch(
        &self,
        candidates: &[(usize, Type)],
        optional: &[bool],
        v: &str,
        pick: &dyn Fn(usize) -> String,
        fallback: &str,
    ) -> String {
        let mut out = String::new();
        // A union candidate (overload resolution) stands for each of its member types.
        let cats: Vec<(usize, Cat, Type)> = candidates
            .iter()
            .flat_map(|(i, t)| {
                let t = self.prune(t);
                match &t.kind {
                    TypeKind::Union(members) => members
                        .iter()
                        .map(|m| {
                            let mut m = self.prune(m);
                            m.nullable |= t.nullable;
                            (*i, self.category(&m), m)
                        })
                        .collect::<Vec<_>>(),
                    _ => vec![(*i, self.category(&t), t)],
                }
            })
            .collect();
        let find = |pred: &dyn Fn(&Cat, &Type) -> bool| {
            cats.iter().find(|(_, c, t)| pred(c, t)).map(|(i, _, _)| *i)
        };
        // 1. undefined and an optional argument.
        for (k, (i, _)) in candidates.iter().enumerate() {
            if optional.get(k).copied().unwrap_or(false) {
                out.push_str(&format!(
                    "if {v}.is_undefined() {{ return {}; }}\n",
                    pick(*i)
                ));
                break;
            }
        }
        // 2. null or undefined: a nullable type or a dictionary.
        if let Some(i) = find(&|c, t| t.nullable || *c == Cat::Dictionary) {
            out.push_str(&format!(
                "if {v}.is_null_or_undefined() {{ return {}; }}\n",
                pick(i)
            ));
        }
        // 3. platform objects.
        for (i, c, t) in &cats {
            if *c == Cat::Interface {
                let test = self.platform_test(t, v).unwrap();
                out.push_str(&format!("if {test} {{ return {}; }}\n", pick(*i)));
            }
        }
        // 4. buffer source types, by the value's internal slot.
        for (i, c, t) in &cats {
            if *c == Cat::Buffer {
                if let Some(test) = self.buffer_test(t, v) {
                    out.push_str(&format!("if {test} {{ return {}; }}\n", pick(*i)));
                }
            }
        }
        // 5. callable: a callback function.
        if let Some(i) = find(&|c, _| *c == Cat::Callback) {
            out.push_str(&format!("if {v}.is_function() {{ return {}; }}\n", pick(i)));
        }
        // 6. objects: sequences (with @@iterator), then dictionaries, records, callback interfaces, object.
        if let Some(i) = find(&|c, _| *c == Cat::Sequence) {
            out.push_str(&format!(
                "if {v}.is_object() && rt::is_iterable(scope, {v})? {{ return {}; }}\n",
                pick(i)
            ));
        }
        if let Some(i) = find(&|c, _| {
            matches!(
                c,
                Cat::Dictionary | Cat::Record | Cat::CallbackInterface | Cat::Object
            )
        }) {
            out.push_str(&format!("if {v}.is_object() {{ return {}; }}\n", pick(i)));
        }
        // 7. booleans, 8. numbers.
        if let Some(i) = find(&|c, _| *c == Cat::Boolean) {
            out.push_str(&format!("if {v}.is_boolean() {{ return {}; }}\n", pick(i)));
        }
        if let Some(i) = find(&|c, _| *c == Cat::Numeric) {
            out.push_str(&format!("if {v}.is_number() {{ return {}; }}\n", pick(i)));
        }
        // 10.–12. otherwise: a string type, a numeric type, boolean, any.
        if let Some(i) = find(&|c, _| *c == Cat::String) {
            out.push_str(&format!("{}\n", pick(i)));
        } else if let Some(i) = find(&|c, _| *c == Cat::Numeric) {
            out.push_str(&format!("{}\n", pick(i)));
        } else if let Some(i) = find(&|c, _| *c == Cat::Boolean) {
            out.push_str(&format!("{}\n", pick(i)));
        } else if let Some(i) = find(&|c, _| *c == Cat::Any || *c == Cat::Promise) {
            out.push_str(&format!("{}\n", pick(i)));
        } else {
            out.push_str(fallback);
            out.push('\n');
        }
        out
    }
}
