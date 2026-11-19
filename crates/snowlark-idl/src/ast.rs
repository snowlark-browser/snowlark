// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The syntax tree of a Web IDL file, as written: partials, mixins and typedefs not yet merged or resolved.
//! [`crate::model`] links these into the model the generator reads.

/// Where a definition came from, for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub file: String,
    pub line: u32,
}

/// An extended attribute (Web IDL §2.13). Every form the specs use is kept, so later passes can read
/// whatever they need without the parser knowing their meaning.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtAttr {
    pub name: String,
    pub value: ExtValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExtValue {
    /// `[Name]`
    None,
    /// `[Name=Ident]`, `[Name="string"]` (quotes removed), `[Name=1.0]`.
    Token(String),
    /// `[Name=*]`
    Wildcard,
    /// `[Name=(A, B)]` with identifiers, strings or numbers.
    List(Vec<String>),
    /// `[Name(args)]`
    Args(Vec<Argument>),
    /// `[Name=Ident(args)]`
    NamedArgs(String, Vec<Argument>),
}

impl ExtAttr {
    /// The value as a single token, if it is one.
    pub fn token(&self) -> Option<&str> {
        match &self.value {
            ExtValue::Token(t) => Some(t),
            _ => None,
        }
    }

    /// The value as a list of tokens: a single token is a list of one.
    pub fn tokens(&self) -> Vec<&str> {
        match &self.value {
            ExtValue::Token(t) => vec![t.as_str()],
            ExtValue::List(l) => l.iter().map(String::as_str).collect(),
            ExtValue::Wildcard => vec!["*"],
            _ => Vec::new(),
        }
    }
}

/// Finds an extended attribute by name.
pub fn ext<'a>(list: &'a [ExtAttr], name: &str) -> Option<&'a ExtAttr> {
    list.iter().find(|a| a.name == name)
}

/// Whether an extended attribute is present.
pub fn has_ext(list: &[ExtAttr], name: &str) -> bool {
    ext(list, name).is_some()
}

/// Extended attributes that Web IDL associates with a type rather than with the construct they are written
/// on (§3.3: "applicable to types").
pub const TYPE_EXT_ATTRS: &[&str] = &[
    "AllowResizable",
    "AllowShared",
    "Clamp",
    "EnforceRange",
    "LegacyNullToEmptyString",
];

/// Built-in types (Web IDL §2.13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Prim {
    Any,
    Undefined,
    Boolean,
    Byte,
    Octet,
    Short,
    UnsignedShort,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
    Float,
    UnrestrictedFloat,
    Double,
    UnrestrictedDouble,
    BigInt,
    DOMString,
    ByteString,
    USVString,
    Object,
    Symbol,
}

impl Prim {
    pub fn idl_name(self) -> &'static str {
        match self {
            Prim::Any => "any",
            Prim::Undefined => "undefined",
            Prim::Boolean => "boolean",
            Prim::Byte => "byte",
            Prim::Octet => "octet",
            Prim::Short => "short",
            Prim::UnsignedShort => "unsigned short",
            Prim::Long => "long",
            Prim::UnsignedLong => "unsigned long",
            Prim::LongLong => "long long",
            Prim::UnsignedLongLong => "unsigned long long",
            Prim::Float => "float",
            Prim::UnrestrictedFloat => "unrestricted float",
            Prim::Double => "double",
            Prim::UnrestrictedDouble => "unrestricted double",
            Prim::BigInt => "bigint",
            Prim::DOMString => "DOMString",
            Prim::ByteString => "ByteString",
            Prim::USVString => "USVString",
            Prim::Object => "object",
            Prim::Symbol => "symbol",
        }
    }

    pub fn is_integer(self) -> bool {
        matches!(
            self,
            Prim::Byte
                | Prim::Octet
                | Prim::Short
                | Prim::UnsignedShort
                | Prim::Long
                | Prim::UnsignedLong
                | Prim::LongLong
                | Prim::UnsignedLongLong
        )
    }

    pub fn is_float(self) -> bool {
        matches!(
            self,
            Prim::Float | Prim::UnrestrictedFloat | Prim::Double | Prim::UnrestrictedDouble
        )
    }

    pub fn is_numeric(self) -> bool {
        self.is_integer() || self.is_float()
    }

    pub fn is_string(self) -> bool {
        matches!(self, Prim::DOMString | Prim::ByteString | Prim::USVString)
    }
}

/// The buffer source types, which the grammar treats as keywords.
pub const BUFFER_TYPES: &[&str] = &[
    "ArrayBuffer",
    "SharedArrayBuffer",
    "DataView",
    "Int8Array",
    "Int16Array",
    "Int32Array",
    "Uint8Array",
    "Uint16Array",
    "Uint32Array",
    "Uint8ClampedArray",
    "BigInt64Array",
    "BigUint64Array",
    "Float16Array",
    "Float32Array",
    "Float64Array",
];

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    Prim(Prim),
    /// An identifier: an interface, dictionary, enum, typedef, callback or buffer type, resolved by the model.
    Named(String),
    Sequence(Box<Type>),
    FrozenArray(Box<Type>),
    ObservableArray(Box<Type>),
    AsyncSequence(Box<Type>),
    Promise(Box<Type>),
    Record(Prim, Box<Type>),
    Union(Vec<Type>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    pub kind: TypeKind,
    pub nullable: bool,
    /// Extended attributes that apply to the type (see [`TYPE_EXT_ATTRS`]), including ones written on an
    /// argument, attribute or dictionary member, which Web IDL associates with its type.
    pub ext: Vec<ExtAttr>,
}

impl Type {
    pub fn prim(p: Prim) -> Type {
        Type {
            kind: TypeKind::Prim(p),
            nullable: false,
            ext: Vec::new(),
        }
    }

    pub fn as_prim(&self) -> Option<Prim> {
        match self.kind {
            TypeKind::Prim(p) => Some(p),
            _ => None,
        }
    }

    pub fn is_undefined(&self) -> bool {
        self.as_prim() == Some(Prim::Undefined)
    }

    pub fn has_ext(&self, name: &str) -> bool {
        has_ext(&self.ext, name)
    }

    /// Web IDL spelling, for diagnostics and generated comments.
    pub fn idl(&self) -> String {
        let base = match &self.kind {
            TypeKind::Prim(p) => p.idl_name().to_string(),
            TypeKind::Named(n) => n.clone(),
            TypeKind::Sequence(t) => format!("sequence<{}>", t.idl()),
            TypeKind::FrozenArray(t) => format!("FrozenArray<{}>", t.idl()),
            TypeKind::ObservableArray(t) => format!("ObservableArray<{}>", t.idl()),
            TypeKind::AsyncSequence(t) => format!("async_sequence<{}>", t.idl()),
            TypeKind::Promise(t) => format!("Promise<{}>", t.idl()),
            TypeKind::Record(k, v) => format!("record<{}, {}>", k.idl_name(), v.idl()),
            TypeKind::Union(ts) => format!(
                "({})",
                ts.iter().map(|t| t.idl()).collect::<Vec<_>>().join(" or ")
            ),
        };
        if self.nullable {
            format!("{base}?")
        } else {
            base
        }
    }
}

/// A constant value or default value (Web IDL §2.5.2 and §2.5.3).
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    Bool(bool),
    /// An integer token, as written (decimal, hex or octal; possibly negative).
    Integer(String),
    /// A decimal token, as written.
    Decimal(String),
    Infinity,
    NegInfinity,
    NaN,
    String(String),
    Null,
    Undefined,
    EmptySequence,
    EmptyDictionary,
}

impl Literal {
    /// The value of an integer literal.
    pub fn integer_value(&self) -> Option<i128> {
        let Literal::Integer(s) = self else {
            return None;
        };
        parse_integer(s)
    }
}

/// Parses a Web IDL integer token (decimal, `0x` hex or leading-zero octal).
pub fn parse_integer(s: &str) -> Option<i128> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let v = if let Some(h) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(h, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') {
        i128::from_str_radix(&digits[1..], 8).ok()?
    } else {
        digits.parse().ok()?
    };
    Some(if neg { -v } else { v })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Argument {
    pub name: String,
    pub ty: Type,
    pub optional: bool,
    pub variadic: bool,
    pub default: Option<Literal>,
    pub ext: Vec<ExtAttr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Special {
    Getter,
    Setter,
    Deleter,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Operation {
    /// `None` for an unnamed special operation (`getter any (DOMString name)`) or an unnamed stringifier.
    pub name: Option<String>,
    pub special: Option<Special>,
    pub is_static: bool,
    pub stringifier: bool,
    pub ret: Type,
    pub args: Vec<Argument>,
    pub ext: Vec<ExtAttr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub ty: Type,
    pub readonly: bool,
    pub is_static: bool,
    pub stringifier: bool,
    pub inherit: bool,
    pub ext: Vec<ExtAttr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Const {
    pub name: String,
    pub ty: Type,
    pub value: Literal,
    pub ext: Vec<ExtAttr>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Member {
    Const(Const),
    Attribute(Attribute),
    Operation(Operation),
    Constructor(Vec<Argument>, Vec<ExtAttr>),
    /// `stringifier;`
    Stringifier(Vec<ExtAttr>),
    /// `iterable<V>` / `iterable<K, V>`; `async_iterable<...>(args)` when `is_async`.
    Iterable {
        key: Option<Type>,
        value: Type,
        is_async: bool,
        args: Vec<Argument>,
        ext: Vec<ExtAttr>,
    },
    Maplike {
        key: Type,
        value: Type,
        readonly: bool,
        ext: Vec<ExtAttr>,
    },
    Setlike {
        value: Type,
        readonly: bool,
        ext: Vec<ExtAttr>,
    },
}

impl Member {
    pub fn ext(&self) -> &[ExtAttr] {
        match self {
            Member::Const(c) => &c.ext,
            Member::Attribute(a) => &a.ext,
            Member::Operation(o) => &o.ext,
            Member::Constructor(_, e) | Member::Stringifier(e) => e,
            Member::Iterable { ext, .. }
            | Member::Maplike { ext, .. }
            | Member::Setlike { ext, .. } => ext,
        }
    }

    pub fn ext_mut(&mut self) -> &mut Vec<ExtAttr> {
        match self {
            Member::Const(c) => &mut c.ext,
            Member::Attribute(a) => &mut a.ext,
            Member::Operation(o) => &mut o.ext,
            Member::Constructor(_, e) | Member::Stringifier(e) => e,
            Member::Iterable { ext, .. }
            | Member::Maplike { ext, .. }
            | Member::Setlike { ext, .. } => ext,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DictMember {
    pub name: String,
    pub ty: Type,
    pub required: bool,
    pub default: Option<Literal>,
    pub ext: Vec<ExtAttr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceKind {
    Interface,
    Mixin,
    CallbackInterface,
    Namespace,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Definition {
    Interface {
        kind: InterfaceKind,
        name: String,
        partial: bool,
        inherits: Option<String>,
        members: Vec<Member>,
        ext: Vec<ExtAttr>,
        span: Span,
    },
    Dictionary {
        name: String,
        partial: bool,
        inherits: Option<String>,
        members: Vec<DictMember>,
        ext: Vec<ExtAttr>,
        span: Span,
    },
    Enum {
        name: String,
        values: Vec<String>,
        ext: Vec<ExtAttr>,
        span: Span,
    },
    Typedef {
        name: String,
        ty: Type,
        ext: Vec<ExtAttr>,
        span: Span,
    },
    Callback {
        name: String,
        ret: Type,
        args: Vec<Argument>,
        ext: Vec<ExtAttr>,
        span: Span,
    },
    Includes {
        target: String,
        mixin: String,
        span: Span,
    },
}
