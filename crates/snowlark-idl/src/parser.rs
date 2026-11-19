// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A recursive-descent parser for the Web IDL grammar (https://webidl.spec.whatwg.org/#idl-grammar).
//!
//! Hand-written rather than `weedle2`: weedle2 5.0 rejects 22 of the 346 webref 3.86 files, among them
//! `dom.idl` and `html.idl` (`[Exposed=*]`, `[ReflectDefault=1.0]`, `required [EnforceRange] T` in
//! dictionaries, `async_iterable`, `async_sequence`), and its nom errors carry no position. This parser
//! follows the current grammar and reports file and line on error.

use crate::ast::*;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Integer(String),
    Decimal(String),
    Str(String),
    Punct(char),
    Ellipsis,
    Eof,
}

struct Token {
    tok: Tok,
    line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub file: String,
    pub line: u32,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.message)
    }
}

fn lex(src: &str, file: &str) -> Result<Vec<Token>, ParseError> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut line = 1u32;
    let mut out = Vec::new();
    let err = |line, message: String| ParseError {
        file: file.to_string(),
        line,
        message,
    };
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
        } else if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                if b[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            i += 2;
        } else if c == b'"' {
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            if i >= b.len() {
                return Err(err(line, "unterminated string".into()));
            }
            out.push(Token {
                tok: Tok::Str(src[start..i].to_string()),
                line,
            });
            i += 1;
        } else if c == b'.' && b.get(i + 1) == Some(&b'.') && b.get(i + 2) == Some(&b'.') {
            out.push(Token {
                tok: Tok::Ellipsis,
                line,
            });
            i += 3;
        } else if c.is_ascii_digit()
            || (c == b'-'
                && b.get(i + 1)
                    .is_some_and(|d| d.is_ascii_digit() || *d == b'.'))
            || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let start = i;
            if c == b'-' {
                i += 1;
            }
            let mut decimal = false;
            if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X')) {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < b.len() && b[i] == b'.' {
                    decimal = true;
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    decimal = true;
                    i += 1;
                    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                        i += 1;
                    }
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text = src[start..i].to_string();
            out.push(Token {
                tok: if decimal {
                    Tok::Decimal(text)
                } else {
                    Tok::Integer(text)
                },
                line,
            });
        } else if c.is_ascii_alphabetic()
            || c == b'_'
            // Web IDL identifiers may start with a dash before a letter (`-webkit-transform`); `-Infinity`
            // stays the punctuator and the keyword.
            || (c == b'-'
                && b.get(i + 1).is_some_and(u8::is_ascii_alphabetic)
                && !src[i + 1..].starts_with("Infinity"))
        {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'-') {
                i += 1;
            }
            out.push(Token {
                tok: Tok::Ident(src[start..i].to_string()),
                line,
            });
        } else if b"(){}[]<>,;:=?*-".contains(&c) {
            out.push(Token {
                tok: Tok::Punct(c as char),
                line,
            });
            i += 1;
        } else {
            return Err(err(line, format!("unexpected character {:?}", c as char)));
        }
    }
    out.push(Token {
        tok: Tok::Eof,
        line,
    });
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    file: &'a str,
}

type R<T> = Result<T, ParseError>;

/// Parses one IDL file.
pub fn parse(src: &str, file: &str) -> Result<Vec<Definition>, ParseError> {
    let toks = lex(src, file)?;
    let mut p = Parser { toks, pos: 0, file };
    let mut defs = Vec::new();
    while p.peek() != &Tok::Eof {
        defs.push(p.definition()?);
    }
    Ok(defs)
}

/// Strips the leading underscore Web IDL uses to escape identifiers that collide with keywords.
fn unescape(id: String) -> String {
    match id.strip_prefix('_') {
        Some(rest) => rest.to_string(),
        None => id,
    }
}

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn line(&self) -> u32 {
        self.toks[self.pos].line
    }

    fn span(&self) -> Span {
        Span {
            file: self.file.to_string(),
            line: self.line(),
        }
    }

    fn error<T>(&self, message: impl Into<String>) -> R<T> {
        Err(ParseError {
            file: self.file.to_string(),
            line: self.line(),
            message: format!("{} (at {:?})", message.into(), self.peek()),
        })
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn is_punct(&self, c: char) -> bool {
        self.peek() == &Tok::Punct(c)
    }

    fn eat_punct(&mut self, c: char) -> bool {
        if self.is_punct(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_punct(&mut self, c: char) -> R<()> {
        if self.eat_punct(c) {
            Ok(())
        } else {
            self.error(format!("expected '{c}'"))
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == kw)
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Tok::Ident(s) if s == kw)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> R<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            self.error(format!("expected '{kw}'"))
        }
    }

    fn ident(&mut self) -> R<String> {
        match self.peek().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(unescape(s))
            }
            _ => self.error("expected identifier"),
        }
    }

    fn definition(&mut self) -> R<Definition> {
        let ext = self.ext_attrs()?;
        let span = self.span();
        if self.eat_kw("callback") {
            if self.eat_kw("interface") {
                return self.interface_body(InterfaceKind::CallbackInterface, false, ext, span);
            }
            let name = self.ident()?;
            self.expect_punct('=')?;
            let ret = self.ty()?;
            let args = self.arg_list()?;
            self.expect_punct(';')?;
            return Ok(Definition::Callback {
                name,
                ret,
                args,
                ext,
                span,
            });
        }
        let partial = self.eat_kw("partial");
        if self.eat_kw("interface") {
            if self.eat_kw("mixin") {
                return self.interface_body(InterfaceKind::Mixin, partial, ext, span);
            }
            return self.interface_body(InterfaceKind::Interface, partial, ext, span);
        }
        if self.eat_kw("namespace") {
            return self.interface_body(InterfaceKind::Namespace, partial, ext, span);
        }
        if self.eat_kw("dictionary") {
            return self.dictionary(partial, ext, span);
        }
        if partial {
            return self.error("expected interface, namespace or dictionary after 'partial'");
        }
        if self.eat_kw("enum") {
            let name = self.ident()?;
            self.expect_punct('{')?;
            let mut values = Vec::new();
            while let Tok::Str(s) = self.peek().clone() {
                self.bump();
                values.push(s);
                if !self.eat_punct(',') {
                    break;
                }
            }
            self.expect_punct('}')?;
            self.expect_punct(';')?;
            return Ok(Definition::Enum {
                name,
                values,
                ext,
                span,
            });
        }
        if self.eat_kw("typedef") {
            let ty = self.ty_with_ext()?;
            let name = self.ident()?;
            self.expect_punct(';')?;
            return Ok(Definition::Typedef {
                name,
                ty,
                ext,
                span,
            });
        }
        if matches!(self.peek(), Tok::Ident(_)) && self.is_kw_at(1, "includes") {
            let target = self.ident()?;
            self.expect_kw("includes")?;
            let mixin = self.ident()?;
            self.expect_punct(';')?;
            return Ok(Definition::Includes {
                target,
                mixin,
                span,
            });
        }
        self.error("expected a definition")
    }

    fn interface_body(
        &mut self,
        kind: InterfaceKind,
        partial: bool,
        ext: Vec<ExtAttr>,
        span: Span,
    ) -> R<Definition> {
        let name = self.ident()?;
        let inherits = if self.eat_punct(':') {
            Some(self.ident()?)
        } else {
            None
        };
        self.expect_punct('{')?;
        let mut members = Vec::new();
        while !self.eat_punct('}') {
            members.push(self.member()?);
        }
        self.expect_punct(';')?;
        Ok(Definition::Interface {
            kind,
            name,
            partial,
            inherits,
            members,
            ext,
            span,
        })
    }

    fn dictionary(&mut self, partial: bool, ext: Vec<ExtAttr>, span: Span) -> R<Definition> {
        let name = self.ident()?;
        let inherits = if self.eat_punct(':') {
            Some(self.ident()?)
        } else {
            None
        };
        self.expect_punct('{')?;
        let mut members = Vec::new();
        while !self.eat_punct('}') {
            let mut mext = self.ext_attrs()?;
            let required = self.eat_kw("required");
            let mut ty = if required {
                self.ty_with_ext()?
            } else {
                self.ty()?
            };
            move_type_ext(&mut mext, &mut ty);
            let name = self.ident()?;
            let default = if self.eat_punct('=') {
                Some(self.literal()?)
            } else {
                None
            };
            self.expect_punct(';')?;
            members.push(DictMember {
                name,
                ty,
                required,
                default,
                ext: mext,
            });
        }
        self.expect_punct(';')?;
        Ok(Definition::Dictionary {
            name,
            partial,
            inherits,
            members,
            ext,
            span,
        })
    }

    fn member(&mut self) -> R<Member> {
        let mut ext = self.ext_attrs()?;
        if self.eat_kw("const") {
            let ty = self.ty()?;
            let name = self.ident()?;
            self.expect_punct('=')?;
            let value = self.literal()?;
            self.expect_punct(';')?;
            return Ok(Member::Const(Const {
                name,
                ty,
                value,
                ext,
            }));
        }
        if self.is_kw("constructor") && self.peek_at(1) == &Tok::Punct('(') {
            self.bump();
            let args = self.arg_list()?;
            self.expect_punct(';')?;
            return Ok(Member::Constructor(args, ext));
        }
        let is_async = if self.is_kw("async") && self.is_kw_at(1, "iterable") {
            self.bump();
            true
        } else {
            false
        };
        if self.is_kw("iterable") || self.is_kw("async_iterable") {
            let is_async = is_async || self.is_kw("async_iterable");
            self.bump();
            self.expect_punct('<')?;
            let first = self.ty_with_ext()?;
            let (key, value) = if self.eat_punct(',') {
                (Some(first), self.ty_with_ext()?)
            } else {
                (None, first)
            };
            self.expect_punct('>')?;
            let args = if self.is_punct('(') {
                self.arg_list()?
            } else {
                Vec::new()
            };
            self.expect_punct(';')?;
            return Ok(Member::Iterable {
                key,
                value,
                is_async,
                args,
                ext,
            });
        }
        let readonly_like =
            self.is_kw("readonly") && (self.is_kw_at(1, "maplike") || self.is_kw_at(1, "setlike"));
        if readonly_like || self.is_kw("maplike") || self.is_kw("setlike") {
            let readonly = self.eat_kw("readonly");
            let map = self.eat_kw("maplike");
            if !map {
                self.expect_kw("setlike")?;
            }
            self.expect_punct('<')?;
            let first = self.ty_with_ext()?;
            let member = if map {
                self.expect_punct(',')?;
                let value = self.ty_with_ext()?;
                Member::Maplike {
                    key: first,
                    value,
                    readonly,
                    ext,
                }
            } else {
                Member::Setlike {
                    value: first,
                    readonly,
                    ext,
                }
            };
            self.expect_punct('>')?;
            self.expect_punct(';')?;
            return Ok(member);
        }
        let is_static = self.eat_kw("static");
        let stringifier = self.eat_kw("stringifier");
        if stringifier && self.eat_punct(';') {
            return Ok(Member::Stringifier(ext));
        }
        let inherit = self.eat_kw("inherit");
        let readonly = self.eat_kw("readonly");
        if self.eat_kw("attribute") {
            let mut ty = self.ty_with_ext()?;
            move_type_ext(&mut ext, &mut ty);
            let name = self.ident()?;
            self.expect_punct(';')?;
            return Ok(Member::Attribute(Attribute {
                name,
                ty,
                readonly,
                is_static,
                stringifier,
                inherit,
                ext,
            }));
        }
        if readonly || inherit {
            return self.error("expected 'attribute'");
        }
        let special = if self.eat_kw("getter") {
            Some(Special::Getter)
        } else if self.eat_kw("setter") {
            Some(Special::Setter)
        } else if self.eat_kw("deleter") {
            Some(Special::Deleter)
        } else {
            None
        };
        let ret = self.ty()?;
        let name = if let Tok::Ident(_) = self.peek() {
            Some(self.ident()?)
        } else {
            None
        };
        let args = self.arg_list()?;
        self.expect_punct(';')?;
        Ok(Member::Operation(Operation {
            name,
            special,
            is_static,
            stringifier,
            ret,
            args,
            ext,
        }))
    }

    fn arg_list(&mut self) -> R<Vec<Argument>> {
        self.expect_punct('(')?;
        let mut args = Vec::new();
        if self.eat_punct(')') {
            return Ok(args);
        }
        loop {
            let mut ext = self.ext_attrs()?;
            let optional = self.eat_kw("optional");
            let mut ty = if optional {
                self.ty_with_ext()?
            } else {
                self.ty()?
            };
            move_type_ext(&mut ext, &mut ty);
            let variadic = matches!(self.peek(), Tok::Ellipsis);
            if variadic {
                self.bump();
            }
            let name = self.ident()?;
            let default = if optional && self.eat_punct('=') {
                Some(self.literal()?)
            } else {
                None
            };
            args.push(Argument {
                name,
                ty,
                optional,
                variadic,
                default,
                ext,
            });
            if !self.eat_punct(',') {
                break;
            }
        }
        self.expect_punct(')')?;
        Ok(args)
    }

    fn literal(&mut self) -> R<Literal> {
        let lit = match self.peek().clone() {
            Tok::Integer(s) => Literal::Integer(s),
            Tok::Decimal(s) => Literal::Decimal(s),
            Tok::Str(s) => Literal::String(s),
            Tok::Ident(s) => match s.as_str() {
                "true" => Literal::Bool(true),
                "false" => Literal::Bool(false),
                "null" => Literal::Null,
                "undefined" => Literal::Undefined,
                "Infinity" => Literal::Infinity,
                "NaN" => Literal::NaN,
                _ => return self.error("expected a literal"),
            },
            Tok::Punct('-') if self.is_kw_at(1, "Infinity") => {
                self.bump();
                Literal::NegInfinity
            }
            Tok::Punct('[') => {
                self.bump();
                self.expect_punct(']')?;
                return Ok(Literal::EmptySequence);
            }
            Tok::Punct('{') => {
                self.bump();
                self.expect_punct('}')?;
                return Ok(Literal::EmptyDictionary);
            }
            _ => return self.error("expected a literal"),
        };
        self.bump();
        Ok(lit)
    }

    fn ext_attrs(&mut self) -> R<Vec<ExtAttr>> {
        let mut out = Vec::new();
        if !self.eat_punct('[') {
            return Ok(out);
        }
        loop {
            let name = self.ident()?;
            let value = if self.is_punct('(') {
                ExtValue::Args(self.arg_list()?)
            } else if self.eat_punct('=') {
                if self.eat_punct('*') {
                    ExtValue::Wildcard
                } else if self.eat_punct('(') {
                    let mut list = Vec::new();
                    loop {
                        match self.bump() {
                            Tok::Ident(s) | Tok::Str(s) | Tok::Integer(s) | Tok::Decimal(s) => {
                                list.push(s)
                            }
                            _ => return self.error("expected a list item"),
                        }
                        if !self.eat_punct(',') {
                            break;
                        }
                    }
                    self.expect_punct(')')?;
                    ExtValue::List(list)
                } else {
                    match self.bump() {
                        Tok::Ident(s) => {
                            if self.is_punct('(') {
                                ExtValue::NamedArgs(s, self.arg_list()?)
                            } else {
                                ExtValue::Token(s)
                            }
                        }
                        Tok::Str(s) | Tok::Integer(s) | Tok::Decimal(s) => ExtValue::Token(s),
                        _ => return self.error("expected an extended attribute value"),
                    }
                }
            } else {
                ExtValue::None
            };
            out.push(ExtAttr { name, value });
            if !self.eat_punct(',') {
                break;
            }
        }
        self.expect_punct(']')?;
        Ok(out)
    }

    fn ty_with_ext(&mut self) -> R<Type> {
        let ext = self.ext_attrs()?;
        let mut ty = self.ty()?;
        ty.ext.extend(ext);
        Ok(ty)
    }

    fn ty(&mut self) -> R<Type> {
        let kind = if self.eat_punct('(') {
            let mut members = vec![self.ty_with_ext()?];
            while self.eat_kw("or") {
                members.push(self.ty_with_ext()?);
            }
            self.expect_punct(')')?;
            TypeKind::Union(members)
        } else {
            self.non_union()?
        };
        let nullable = self.eat_punct('?');
        Ok(Type {
            kind,
            nullable,
            ext: Vec::new(),
        })
    }

    fn generic_arg(&mut self) -> R<Box<Type>> {
        self.expect_punct('<')?;
        let t = self.ty_with_ext()?;
        self.expect_punct('>')?;
        Ok(Box::new(t))
    }

    fn non_union(&mut self) -> R<TypeKind> {
        let Tok::Ident(word) = self.peek().clone() else {
            return self.error("expected a type");
        };
        self.bump();
        let prim = |p| Ok(TypeKind::Prim(p));
        match word.as_str() {
            "any" => prim(Prim::Any),
            "undefined" => prim(Prim::Undefined),
            "boolean" => prim(Prim::Boolean),
            "byte" => prim(Prim::Byte),
            "octet" => prim(Prim::Octet),
            "bigint" => prim(Prim::BigInt),
            "DOMString" => prim(Prim::DOMString),
            "ByteString" => prim(Prim::ByteString),
            "USVString" => prim(Prim::USVString),
            "object" => prim(Prim::Object),
            "symbol" => prim(Prim::Symbol),
            "float" => prim(Prim::Float),
            "double" => prim(Prim::Double),
            "short" => prim(Prim::Short),
            "long" => {
                if self.eat_kw("long") {
                    prim(Prim::LongLong)
                } else {
                    prim(Prim::Long)
                }
            }
            "unsigned" => {
                if self.eat_kw("short") {
                    prim(Prim::UnsignedShort)
                } else {
                    self.expect_kw("long")?;
                    if self.eat_kw("long") {
                        prim(Prim::UnsignedLongLong)
                    } else {
                        prim(Prim::UnsignedLong)
                    }
                }
            }
            "unrestricted" => {
                if self.eat_kw("float") {
                    prim(Prim::UnrestrictedFloat)
                } else {
                    self.expect_kw("double")?;
                    prim(Prim::UnrestrictedDouble)
                }
            }
            "sequence" => Ok(TypeKind::Sequence(self.generic_arg()?)),
            "FrozenArray" => Ok(TypeKind::FrozenArray(self.generic_arg()?)),
            "ObservableArray" => Ok(TypeKind::ObservableArray(self.generic_arg()?)),
            "async_sequence" => Ok(TypeKind::AsyncSequence(self.generic_arg()?)),
            "Promise" => {
                self.expect_punct('<')?;
                let t = self.ty()?;
                self.expect_punct('>')?;
                Ok(TypeKind::Promise(Box::new(t)))
            }
            "record" => {
                self.expect_punct('<')?;
                let key = match self.ty()?.as_prim() {
                    Some(p) if p.is_string() => p,
                    _ => return self.error("record keys must be a string type"),
                };
                self.expect_punct(',')?;
                let value = self.ty_with_ext()?;
                self.expect_punct('>')?;
                Ok(TypeKind::Record(key, Box::new(value)))
            }
            _ => Ok(TypeKind::Named(unescape(word))),
        }
    }
}

/// Moves the type-applicable extended attributes written on an argument, attribute or dictionary member onto
/// its type.
fn move_type_ext(ext: &mut Vec<ExtAttr>, ty: &mut Type) {
    let (to_type, keep): (Vec<_>, Vec<_>) = ext
        .drain(..)
        .partition(|a| TYPE_EXT_ATTRS.contains(&a.name.as_str()));
    *ext = keep;
    ty.ext.extend(to_type);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(src: &str) -> Definition {
        let mut defs = parse(src, "test.idl").unwrap();
        assert_eq!(defs.len(), 1);
        defs.remove(0)
    }

    #[test]
    fn parses_interface_members() {
        let d = one(r#"
            [Exposed=*]
            interface Event : Base {
              constructor(DOMString type, optional EventInit eventInitDict = {});
              const unsigned short AT_TARGET = 2;
              readonly attribute EventTarget? target;
              [CEReactions, Reflect="for"] attribute DOMString htmlFor;
              sequence<EventTarget> composedPath();
              getter Node? item(unsigned long index);
              stringifier;
              static Response json(any data, optional ResponseInit init = {});
              undefined f([Clamp] long x, (Node or DOMString)... nodes);
              iterable<Node>;
              async_iterable<any>(optional ReadableStreamIteratorOptions options = {});
              readonly maplike<DOMString, long>;
            };
        "#);
        let Definition::Interface {
            name,
            inherits,
            members,
            ext,
            ..
        } = d
        else {
            panic!()
        };
        assert_eq!(name, "Event");
        assert_eq!(inherits.as_deref(), Some("Base"));
        assert_eq!(ext[0].value, ExtValue::Wildcard);
        assert_eq!(members.len(), 12);
        let Member::Operation(op) = &members[8] else {
            panic!()
        };
        assert_eq!(op.args[0].ty.ext[0].name, "Clamp");
        assert!(op.args[0].ext.is_empty());
        assert!(op.args[1].variadic);
        let Member::Attribute(a) = &members[3] else {
            panic!()
        };
        assert_eq!(a.ext[1].token(), Some("for"));
    }

    #[test]
    fn parses_dictionary_and_ext_values() {
        let d = one(r#"
            dictionary SerialOptions : Base {
              required [EnforceRange] unsigned long baudRate;
              [EnforceRange] octet dataBits = 8;
              double x = -Infinity;
              sequence<long> list = [];
              [ReflectDefault=1.0, ReflectRange=(1, 1000)] DOMString s = "a";
            };
        "#);
        let Definition::Dictionary { members, .. } = d else {
            panic!()
        };
        assert!(members[0].required);
        assert_eq!(members[0].ty.ext[0].name, "EnforceRange");
        assert_eq!(members[1].ty.ext[0].name, "EnforceRange");
        assert_eq!(members[2].default, Some(Literal::NegInfinity));
        assert_eq!(members[3].default, Some(Literal::EmptySequence));
        assert_eq!(members[4].ext[0].token(), Some("1.0"));
        assert_eq!(
            members[4].ext[1].value,
            ExtValue::List(vec!["1".into(), "1000".into()])
        );
    }

    #[test]
    fn reports_line_of_error() {
        let e = parse("interface A {\n  attribute ;\n};", "x.idl").unwrap_err();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn integer_literals() {
        assert_eq!(parse_integer("0x10"), Some(16));
        assert_eq!(parse_integer("010"), Some(8));
        assert_eq!(parse_integer("-5"), Some(-5));
        assert_eq!(parse_integer("0"), Some(0));
    }
}
