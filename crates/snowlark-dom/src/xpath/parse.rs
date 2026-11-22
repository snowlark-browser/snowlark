// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The XPath 1.0 expression lexer (§3.7, with its rules for telling operators from names) and a
//! recursive-descent parser over the grammar of §2 and §3. Prefixes are resolved while parsing, as
//! the DOM's `createExpression` requires (DOM §8.3: the resolver is called during parsing).

use super::{Axis, BinOp, Expr, Func, NodeTest, Step, XPathError};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DotDot,
    At,
    Comma,
    ColonColon,
    Slash,
    SlashSlash,
    Pipe,
    Plus,
    Minus,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `*`, `and`, `or`, `mod`, `div` as operators.
    Mul,
    And,
    Or,
    Mod,
    Div,
    /// `*`, `prefix:*` or a QName in a name test: (prefix, local; `None` for `*`).
    NameTest(Option<String>, Option<String>),
    NodeType(String),
    /// A function name (a QName).
    Function(String),
    Axis(Axis),
    Literal(String),
    Number(f64),
    Variable(String),
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// XML 1.0 (Fifth Edition) NameStartChar, without ':'.
fn is_name_start(c: char) -> bool {
    matches!(c,
        'A'..='Z' | '_' | 'a'..='z'
        | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}' | '\u{F8}'..='\u{2FF}'
        | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}' | '\u{200C}'..='\u{200D}'
        | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}' | '\u{3001}'..='\u{D7FF}'
        | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}' | '\u{10000}'..='\u{EFFFF}')
}

/// XML 1.0 NameChar, without ':'.
fn is_name_char(c: char) -> bool {
    is_name_start(c)
        || matches!(c,
            '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

struct Lexer<'a> {
    s: &'a str,
    pos: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }
    fn peek2(&self) -> Option<char> {
        self.s[self.pos..].chars().nth(1)
    }
    fn skip_space(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.pos += 1;
        }
    }
    fn ncname(&mut self) -> Option<&str> {
        let start = self.pos;
        let mut it = self.s[start..].char_indices();
        match it.next() {
            Some((_, c)) if is_name_start(c) => {}
            _ => return None,
        }
        let mut end = self.s.len();
        for (i, c) in it {
            if !is_name_char(c) {
                end = start + i;
                break;
            }
        }
        self.pos = end;
        Some(&self.s[start..end])
    }
    /// The next non-space character after the current position, without consuming anything.
    fn lookahead(&self) -> (Option<char>, Option<char>) {
        let rest = self.s[self.pos..].trim_start_matches(is_space);
        let mut it = rest.chars();
        (it.next(), it.next())
    }
}

/// Splits `s` into tokens (XPath 1.0 §3.7).
fn tokenize(s: &str) -> Result<Vec<Tok>, &'static str> {
    let mut lx = Lexer { s, pos: 0 };
    let mut out: Vec<Tok> = Vec::new();
    loop {
        lx.skip_space();
        let Some(c) = lx.peek() else { break };
        // "If there is a preceding token and the preceding token is not one of @, ::, (, [, , or an
        // Operator, then a * must be recognized as a MultiplyOperator and an NCName must be
        // recognized as an OperatorName."
        let operator_expected = match out.last() {
            None => false,
            Some(t) => !matches!(
                t,
                Tok::At
                    | Tok::ColonColon
                    | Tok::LParen
                    | Tok::LBracket
                    | Tok::Comma
                    | Tok::Slash
                    | Tok::SlashSlash
                    | Tok::Pipe
                    | Tok::Plus
                    | Tok::Minus
                    | Tok::Eq
                    | Tok::Ne
                    | Tok::Lt
                    | Tok::Le
                    | Tok::Gt
                    | Tok::Ge
                    | Tok::Mul
                    | Tok::And
                    | Tok::Or
                    | Tok::Mod
                    | Tok::Div
            ),
        };
        let one = |t: Tok, lx: &mut Lexer| {
            lx.pos += 1;
            t
        };
        let tok = match c {
            '(' => one(Tok::LParen, &mut lx),
            ')' => one(Tok::RParen, &mut lx),
            '[' => one(Tok::LBracket, &mut lx),
            ']' => one(Tok::RBracket, &mut lx),
            '@' => one(Tok::At, &mut lx),
            ',' => one(Tok::Comma, &mut lx),
            '|' => one(Tok::Pipe, &mut lx),
            '+' => one(Tok::Plus, &mut lx),
            '-' => one(Tok::Minus, &mut lx),
            '=' => one(Tok::Eq, &mut lx),
            ':' if lx.peek2() == Some(':') => {
                lx.pos += 2;
                Tok::ColonColon
            }
            '!' if lx.peek2() == Some('=') => {
                lx.pos += 2;
                Tok::Ne
            }
            '<' | '>' => {
                let eq = lx.peek2() == Some('=');
                lx.pos += 1 + eq as usize;
                match (c, eq) {
                    ('<', false) => Tok::Lt,
                    ('<', true) => Tok::Le,
                    ('>', false) => Tok::Gt,
                    _ => Tok::Ge,
                }
            }
            '/' => {
                if lx.peek2() == Some('/') {
                    lx.pos += 2;
                    Tok::SlashSlash
                } else {
                    lx.pos += 1;
                    Tok::Slash
                }
            }
            '.' => match lx.peek2() {
                Some('.') => {
                    lx.pos += 2;
                    Tok::DotDot
                }
                Some('0'..='9') => number(&mut lx),
                _ => one(Tok::Dot, &mut lx),
            },
            '0'..='9' => number(&mut lx),
            '"' | '\'' => {
                let start = lx.pos + 1;
                let Some(len) = s[start..].find(c) else {
                    return Err("An unterminated string literal");
                };
                lx.pos = start + len + 1;
                Tok::Literal(s[start..start + len].to_owned())
            }
            '$' => {
                lx.pos += 1;
                let name = qname(&mut lx).ok_or("A variable reference without a name")?;
                Tok::Variable(name)
            }
            '*' => {
                lx.pos += 1;
                if operator_expected {
                    Tok::Mul
                } else {
                    Tok::NameTest(None, None)
                }
            }
            _ => {
                let start = lx.pos;
                let Some(name) = lx.ncname().map(str::to_owned) else {
                    return Err("An unexpected character");
                };
                if operator_expected {
                    match name.as_str() {
                        "and" => Tok::And,
                        "or" => Tok::Or,
                        "mod" => Tok::Mod,
                        "div" => Tok::Div,
                        _ => return Err("An operator was expected"),
                    }
                } else if lx.peek() == Some(':') && lx.peek2() != Some(':') {
                    // A QName or `prefix:*`.
                    lx.pos += 1;
                    if lx.peek() == Some('*') {
                        lx.pos += 1;
                        Tok::NameTest(Some(name), None)
                    } else {
                        let Some(local) = lx.ncname().map(str::to_owned) else {
                            return Err("A name was expected after the prefix");
                        };
                        if lx.lookahead().0 == Some('(') {
                            Tok::Function(s[start..lx.pos].to_owned())
                        } else {
                            Tok::NameTest(Some(name), Some(local))
                        }
                    }
                } else {
                    match lx.lookahead() {
                        (Some('('), _) => match name.as_str() {
                            "comment" | "text" | "processing-instruction" | "node" => {
                                Tok::NodeType(name)
                            }
                            _ => Tok::Function(name),
                        },
                        (Some(':'), Some(':')) => {
                            Tok::Axis(Axis::from_name(&name).ok_or("An unknown axis")?)
                        }
                        _ => Tok::NameTest(None, Some(name)),
                    }
                }
            }
        };
        out.push(tok);
    }
    Ok(out)
}

fn number(lx: &mut Lexer) -> Tok {
    let start = lx.pos;
    while lx.peek().is_some_and(|c| c.is_ascii_digit()) {
        lx.pos += 1;
    }
    if lx.peek() == Some('.') {
        lx.pos += 1;
        while lx.peek().is_some_and(|c| c.is_ascii_digit()) {
            lx.pos += 1;
        }
    }
    Tok::Number(lx.s[start..lx.pos].parse().unwrap_or(f64::NAN))
}

fn qname(lx: &mut Lexer) -> Option<String> {
    let start = lx.pos;
    lx.ncname()?;
    if lx.peek() == Some(':') && lx.peek2() != Some(':') {
        lx.pos += 1;
        lx.ncname()?;
    }
    Some(lx.s[start..lx.pos].to_owned())
}

pub(super) struct Parser<'r, E> {
    toks: Vec<Tok>,
    pos: usize,
    resolve: &'r mut dyn FnMut(&str) -> Result<Option<String>, E>,
}

type PResult<T, E> = Result<T, XPathError<E>>;

const SYNTAX: &str = "The string is not a valid XPath expression.";

impl<'r, E> Parser<'r, E> {
    pub(super) fn parse(
        text: &str,
        resolve: &'r mut dyn FnMut(&str) -> Result<Option<String>, E>,
    ) -> PResult<Expr, E> {
        let toks = tokenize(text).map_err(XPathError::Syntax)?;
        let mut p = Parser {
            toks,
            pos: 0,
            resolve,
        };
        let e = p.or_expr()?;
        if p.pos != p.toks.len() {
            return Err(XPathError::Syntax(SYNTAX));
        }
        Ok(e)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &Tok) -> PResult<(), E> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(XPathError::Syntax(SYNTAX))
        }
    }

    fn binary(
        &mut self,
        next: fn(&mut Self) -> PResult<Expr, E>,
        ops: &[(Tok, BinOp)],
    ) -> PResult<Expr, E> {
        let mut left = next(self)?;
        'outer: loop {
            for (t, op) in ops {
                if self.eat(t) {
                    let right = next(self)?;
                    left = Expr::Binary(*op, Box::new(left), Box::new(right));
                    continue 'outer;
                }
            }
            return Ok(left);
        }
    }

    fn or_expr(&mut self) -> PResult<Expr, E> {
        self.binary(Self::and_expr, &[(Tok::Or, BinOp::Or)])
    }
    fn and_expr(&mut self) -> PResult<Expr, E> {
        self.binary(Self::equality, &[(Tok::And, BinOp::And)])
    }
    fn equality(&mut self) -> PResult<Expr, E> {
        self.binary(
            Self::relational,
            &[(Tok::Eq, BinOp::Eq), (Tok::Ne, BinOp::Ne)],
        )
    }
    fn relational(&mut self) -> PResult<Expr, E> {
        self.binary(
            Self::additive,
            &[
                (Tok::Lt, BinOp::Lt),
                (Tok::Le, BinOp::Le),
                (Tok::Gt, BinOp::Gt),
                (Tok::Ge, BinOp::Ge),
            ],
        )
    }
    fn additive(&mut self) -> PResult<Expr, E> {
        self.binary(
            Self::multiplicative,
            &[(Tok::Plus, BinOp::Add), (Tok::Minus, BinOp::Sub)],
        )
    }
    fn multiplicative(&mut self) -> PResult<Expr, E> {
        self.binary(
            Self::unary,
            &[
                (Tok::Mul, BinOp::Mul),
                (Tok::Div, BinOp::Div),
                (Tok::Mod, BinOp::Mod),
            ],
        )
    }
    fn unary(&mut self) -> PResult<Expr, E> {
        if self.eat(&Tok::Minus) {
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        self.union()
    }
    fn union(&mut self) -> PResult<Expr, E> {
        self.binary(Self::path_expr, &[(Tok::Pipe, BinOp::Union)])
    }

    /// PathExpr: a location path, or a filter expression optionally followed by `/` or `//` and a
    /// relative location path.
    fn path_expr(&mut self) -> PResult<Expr, E> {
        let starts_filter = matches!(
            self.peek(),
            Some(
                Tok::Variable(_)
                    | Tok::LParen
                    | Tok::Literal(_)
                    | Tok::Number(_)
                    | Tok::Function(_)
            )
        );
        if !starts_filter {
            return self.location_path();
        }
        let primary = self.primary()?;
        let mut predicates = Vec::new();
        while self.peek() == Some(&Tok::LBracket) {
            predicates.push(self.predicate()?);
        }
        let mut e = if predicates.is_empty() {
            primary
        } else {
            Expr::Filter(Box::new(primary), predicates)
        };
        let mut steps = Vec::new();
        if self.eat(&Tok::Slash) {
            self.relative_path(&mut steps)?;
        } else if self.eat(&Tok::SlashSlash) {
            steps.push(Step::descendant_or_self());
            self.relative_path(&mut steps)?;
        }
        if !steps.is_empty() {
            e = Expr::PathFrom(Box::new(e), super::optimize(steps));
        }
        Ok(e)
    }

    fn location_path(&mut self) -> PResult<Expr, E> {
        let mut steps = Vec::new();
        let absolute = match self.peek() {
            Some(Tok::Slash) => {
                self.pos += 1;
                if self.starts_step() {
                    self.relative_path(&mut steps)?;
                }
                true
            }
            Some(Tok::SlashSlash) => {
                self.pos += 1;
                steps.push(Step::descendant_or_self());
                self.relative_path(&mut steps)?;
                true
            }
            _ => {
                self.relative_path(&mut steps)?;
                false
            }
        };
        Ok(Expr::Path {
            absolute,
            steps: super::optimize(steps),
        })
    }

    fn starts_step(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Tok::Dot
                    | Tok::DotDot
                    | Tok::At
                    | Tok::Axis(_)
                    | Tok::NameTest(..)
                    | Tok::NodeType(_)
            )
        )
    }

    fn relative_path(&mut self, steps: &mut Vec<Step>) -> PResult<(), E> {
        steps.push(self.step()?);
        loop {
            if self.eat(&Tok::Slash) {
                steps.push(self.step()?);
            } else if self.eat(&Tok::SlashSlash) {
                steps.push(Step::descendant_or_self());
                steps.push(self.step()?);
            } else {
                return Ok(());
            }
        }
    }

    fn step(&mut self) -> PResult<Step, E> {
        if self.eat(&Tok::Dot) {
            return Ok(Step {
                axis: Axis::Itself,
                test: NodeTest::Node,
                predicates: Vec::new(),
            });
        }
        if self.eat(&Tok::DotDot) {
            return Ok(Step {
                axis: Axis::Parent,
                test: NodeTest::Node,
                predicates: Vec::new(),
            });
        }
        let axis = match self.peek() {
            Some(Tok::At) => {
                self.pos += 1;
                Axis::Attribute
            }
            Some(&Tok::Axis(a)) => {
                self.pos += 1;
                self.expect(&Tok::ColonColon)?;
                a
            }
            _ => Axis::Child,
        };
        let test = match self.toks.get(self.pos).cloned() {
            Some(Tok::NameTest(prefix, local)) => {
                self.pos += 1;
                let ns = match prefix {
                    None => None,
                    Some(p) => Some(self.namespace(&p)?),
                };
                NodeTest::Name { ns, local }
            }
            Some(Tok::NodeType(t)) => {
                self.pos += 1;
                self.expect(&Tok::LParen)?;
                let test = match t.as_str() {
                    "comment" => NodeTest::Comment,
                    "text" => NodeTest::Text,
                    "node" => NodeTest::Node,
                    _ => match self.toks.get(self.pos).cloned() {
                        Some(Tok::Literal(target)) => {
                            self.pos += 1;
                            NodeTest::ProcessingInstruction(Some(target))
                        }
                        _ => NodeTest::ProcessingInstruction(None),
                    },
                };
                self.expect(&Tok::RParen)?;
                test
            }
            _ => return Err(XPathError::Syntax(SYNTAX)),
        };
        let mut predicates = Vec::new();
        while self.peek() == Some(&Tok::LBracket) {
            predicates.push(self.predicate()?);
        }
        Ok(Step {
            axis,
            test,
            predicates,
        })
    }

    fn predicate(&mut self) -> PResult<Expr, E> {
        self.expect(&Tok::LBracket)?;
        let e = self.or_expr()?;
        self.expect(&Tok::RBracket)?;
        Ok(e)
    }

    fn namespace(&mut self, prefix: &str) -> PResult<String, E> {
        match (self.resolve)(prefix) {
            // (an empty string resolves, to no namespace: only null leaves a prefix unresolved)
            Ok(Some(uri)) => Ok(uri),
            Ok(None) => Err(XPathError::Namespace(prefix.to_owned())),
            Err(e) => Err(XPathError::Resolver(e)),
        }
    }

    fn primary(&mut self) -> PResult<Expr, E> {
        let Some(t) = self.toks.get(self.pos).cloned() else {
            return Err(XPathError::Syntax(SYNTAX));
        };
        self.pos += 1;
        match t {
            Tok::Variable(_) => Ok(Expr::Variable),
            Tok::LParen => {
                let e = self.or_expr()?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::Literal(s) => Ok(Expr::Literal(s)),
            Tok::Number(n) => Ok(Expr::Number(n)),
            Tok::Function(name) => {
                self.expect(&Tok::LParen)?;
                let mut args = Vec::new();
                if !self.eat(&Tok::RParen) {
                    loop {
                        args.push(self.or_expr()?);
                        if self.eat(&Tok::RParen) {
                            break;
                        }
                        self.expect(&Tok::Comma)?;
                    }
                }
                let f = Func::from_name(&name).ok_or(XPathError::Syntax(
                    "The expression calls an unknown function.",
                ))?;
                if !f.arity_ok(args.len()) {
                    return Err(XPathError::Syntax(
                        "A function is called with the wrong number of arguments.",
                    ));
                }
                Ok(Expr::Call(f, args))
            }
            _ => Err(XPathError::Syntax(SYNTAX)),
        }
    }
}
