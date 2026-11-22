// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XPath 1.0 over the DOM (DOM Standard §8, XPath 1.0 REC 1999-11-16), with HTML's rules for HTML
//! documents (HTML §"Interactions with XPath and XSLT": unprefixed name tests match HTML elements
//! by their lowercase name, in the HTML namespace). The namespace axis selects nothing.
//!
//! [`parse`] compiles an expression, resolving its prefixes through a callback; [`Expression::
//! evaluate`] runs it against a context node. Attribute nodes are materialized as the attribute axis
//! visits them, so evaluation takes the store mutably.

mod eval;
mod parse;

use crate::{Dom, NodeId};

pub use eval::{string_value, Value};

/// Why an expression could not be compiled or evaluated.
#[derive(Debug)]
pub enum XPathError<E> {
    /// The text is not an expression (a DOM `SyntaxError`).
    Syntax(&'static str),
    /// A prefix the resolver did not resolve (a DOM `NamespaceError`).
    Namespace(String),
    /// A value used where a node-set is required, or a variable (XPath 1.0 has no bindings in the
    /// DOM): Web browsers throw a `SyntaxError`.
    Type(&'static str),
    /// The resolver failed.
    Resolver(E),
}

/// A compiled expression (`XPathExpression`).
#[derive(Debug)]
pub struct Expression {
    root: Expr,
}

/// Compiles `text`, calling `resolve` for each prefix (`lookupNamespaceURI`).
pub fn parse<E>(
    text: &str,
    resolve: &mut dyn FnMut(&str) -> Result<Option<String>, E>,
) -> Result<Expression, XPathError<E>> {
    Ok(Expression {
        root: parse::Parser::parse(text, resolve)?,
    })
}

impl Expression {
    /// Evaluates the expression with `context` as the context node (position 1 of 1).
    pub fn evaluate(&self, dom: &mut Dom, context: NodeId) -> Result<Value, XPathError<()>> {
        eval::Evaluator { dom }.eval(
            &self.root,
            &eval::Context {
                node: context,
                position: 1,
                size: 1,
            },
        )
    }
}

/// Whether `node` can be a context node (an Attr, Text, CDATASection, Comment, Document, Element or
/// ProcessingInstruction).
pub fn is_valid_context(dom: &Dom, node: NodeId) -> bool {
    !matches!(
        dom.node_type(node),
        crate::NodeType::DocumentType | crate::NodeType::DocumentFragment
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    /// `self::`.
    Itself,
}

impl Axis {
    fn from_name(name: &str) -> Option<Axis> {
        Some(match name {
            "ancestor" => Axis::Ancestor,
            "ancestor-or-self" => Axis::AncestorOrSelf,
            "attribute" => Axis::Attribute,
            "child" => Axis::Child,
            "descendant" => Axis::Descendant,
            "descendant-or-self" => Axis::DescendantOrSelf,
            "following" => Axis::Following,
            "following-sibling" => Axis::FollowingSibling,
            "namespace" => Axis::Namespace,
            "parent" => Axis::Parent,
            "preceding" => Axis::Preceding,
            "preceding-sibling" => Axis::PrecedingSibling,
            "self" => Axis::Itself,
            _ => return None,
        })
    }

    /// A reverse axis lists its nodes in reverse document order (§2.4: proximity positions).
    fn is_reverse(self) -> bool {
        matches!(
            self,
            Axis::Ancestor | Axis::AncestorOrSelf | Axis::Preceding | Axis::PrecedingSibling
        )
    }
}

#[derive(Clone, Debug)]
enum NodeTest {
    /// A name test: the resolved namespace of its prefix (none when unprefixed), and its local
    /// name (none for `*`).
    Name {
        ns: Option<String>,
        local: Option<String>,
    },
    Text,
    Comment,
    ProcessingInstruction(Option<String>),
    Node,
}

#[derive(Clone, Debug)]
struct Step {
    axis: Axis,
    test: NodeTest,
    predicates: Vec<Expr>,
}

impl Step {
    /// `//`'s `/descendant-or-self::node()/`.
    fn descendant_or_self() -> Step {
        Step {
            axis: Axis::DescendantOrSelf,
            test: NodeTest::Node,
            predicates: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Union,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Func {
    Last,
    Position,
    Count,
    Id,
    LocalName,
    NamespaceUri,
    Name,
    String,
    Concat,
    StartsWith,
    Contains,
    SubstringBefore,
    SubstringAfter,
    Substring,
    StringLength,
    NormalizeSpace,
    Translate,
    Boolean,
    Not,
    True,
    False,
    Lang,
    Number,
    Sum,
    Floor,
    Ceiling,
    Round,
}

impl Func {
    fn from_name(name: &str) -> Option<Func> {
        Some(match name {
            "last" => Func::Last,
            "position" => Func::Position,
            "count" => Func::Count,
            "id" => Func::Id,
            "local-name" => Func::LocalName,
            "namespace-uri" => Func::NamespaceUri,
            "name" => Func::Name,
            "string" => Func::String,
            "concat" => Func::Concat,
            "starts-with" => Func::StartsWith,
            "contains" => Func::Contains,
            "substring-before" => Func::SubstringBefore,
            "substring-after" => Func::SubstringAfter,
            "substring" => Func::Substring,
            "string-length" => Func::StringLength,
            "normalize-space" => Func::NormalizeSpace,
            "translate" => Func::Translate,
            "boolean" => Func::Boolean,
            "not" => Func::Not,
            "true" => Func::True,
            "false" => Func::False,
            "lang" => Func::Lang,
            "number" => Func::Number,
            "sum" => Func::Sum,
            "floor" => Func::Floor,
            "ceiling" => Func::Ceiling,
            "round" => Func::Round,
            _ => return None,
        })
    }

    /// The function library's signatures (§4).
    fn arity_ok(self, n: usize) -> bool {
        use Func::*;
        match self {
            Last | Position | True | False => n == 0,
            Count | Id | Boolean | Not | Lang | Sum | Floor | Ceiling | Round => n == 1,
            LocalName | NamespaceUri | Name | String | StringLength | NormalizeSpace | Number => {
                n <= 1
            }
            StartsWith | Contains | SubstringBefore | SubstringAfter => n == 2,
            Substring => n == 2 || n == 3,
            Translate => n == 3,
            Concat => n >= 2,
        }
    }
}

#[derive(Clone, Debug)]
enum Expr {
    Number(f64),
    Literal(String),
    /// A variable reference (no bindings exist).
    Variable,
    Negate(Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(Func, Vec<Expr>),
    /// A primary expression with predicates.
    Filter(Box<Expr>, Vec<Expr>),
    /// A location path.
    Path {
        absolute: bool,
        steps: Vec<Step>,
    },
    /// A filter expression followed by a relative location path.
    PathFrom(Box<Expr>, Vec<Step>),
}

/// What an expression's value is known to be before it runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Number,
    Other,
    Unknown,
}

fn kind(e: &Expr) -> Kind {
    use Func::*;
    match e {
        Expr::Number(_) | Expr::Negate(_) => Kind::Number,
        Expr::Binary(op, ..) => match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => Kind::Number,
            _ => Kind::Other,
        },
        Expr::Call(f, _) => match f {
            Last | Position | Count | StringLength | Number | Sum | Floor | Ceiling | Round => {
                Kind::Number
            }
            _ => Kind::Other,
        },
        Expr::Literal(_) | Expr::Path { .. } | Expr::PathFrom(..) => Kind::Other,
        Expr::Filter(primary, _) => match kind(primary) {
            Kind::Other => Kind::Other,
            _ => Kind::Unknown,
        },
        Expr::Variable => Kind::Unknown,
    }
}

/// Whether `e` reads the context position or size (outside the predicates and steps of nested
/// paths, which have their own).
fn uses_position(e: &Expr) -> bool {
    match e {
        Expr::Call(Func::Last | Func::Position, _) => true,
        Expr::Call(_, args) => args.iter().any(uses_position),
        Expr::Negate(a) => uses_position(a),
        Expr::Binary(_, a, b) => uses_position(a) || uses_position(b),
        Expr::Filter(p, _) | Expr::PathFrom(p, _) => uses_position(p),
        Expr::Number(_) | Expr::Literal(_) | Expr::Variable | Expr::Path { .. } => false,
    }
}

/// Rewrites `descendant-or-self::node()/child::t[p]` as `descendant::t[p]` when no predicate
/// depends on the position or size (both select the same nodes then), so `//x` does not first
/// collect every node of the document.
fn optimize(steps: Vec<Step>) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::with_capacity(steps.len());
    for step in steps {
        let fuse = step.axis == Axis::Child
            && step
                .predicates
                .iter()
                .all(|p| kind(p) == Kind::Other && !uses_position(p))
            && out.last().is_some_and(|prev| {
                prev.axis == Axis::DescendantOrSelf
                    && matches!(prev.test, NodeTest::Node)
                    && prev.predicates.is_empty()
            });
        if fuse {
            out.pop();
            out.push(Step {
                axis: Axis::Descendant,
                ..step
            });
        } else {
            out.push(step);
        }
    }
    out
}
