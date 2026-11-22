// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! XPath 1.0 evaluation: location steps along the axes (§2.2), node tests (§2.3, with HTML's rules
//! for HTML documents), predicates (§2.4), the operators of §3 and the core function library (§4).
//! Every node-set value is kept in document order without duplicates.

use std::collections::{HashMap, HashSet};

use crate::{local_name, ns, Dom, NodeData, NodeId, NodeType};

use super::{Axis, BinOp, Expr, Func, NodeTest, Step, XPathError};

type R<T> = Result<T, XPathError<()>>;

const NOT_A_NODE_SET: XPathError<()> =
    XPathError::Type("Type conversion failed while evaluating the expression.");

/// An expression's value (§1: node-set, boolean, number, string).
#[derive(Clone, Debug)]
pub enum Value {
    /// In document order, without duplicates.
    Nodes(Vec<NodeId>),
    Number(f64),
    String(String),
    Boolean(bool),
}

impl Value {
    /// The `number()` function's conversion.
    pub fn number(&self, dom: &Dom) -> f64 {
        match self {
            Value::Number(n) => *n,
            Value::Boolean(b) => *b as u8 as f64,
            Value::String(s) => string_to_number(s),
            Value::Nodes(_) => string_to_number(&self.string(dom)),
        }
    }

    /// The `string()` function's conversion.
    pub fn string(&self, dom: &Dom) -> String {
        match self {
            Value::Nodes(n) => n.first().map(|&n| string_value(dom, n)).unwrap_or_default(),
            Value::Number(n) => number_to_string(*n),
            Value::String(s) => s.clone(),
            Value::Boolean(b) => if *b { "true" } else { "false" }.to_owned(),
        }
    }

    /// The `boolean()` function's conversion.
    pub fn boolean(&self) -> bool {
        match self {
            Value::Nodes(n) => !n.is_empty(),
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
            Value::Boolean(b) => *b,
        }
    }
}

/// A node's string-value (§5): the text of an element's or a root's Text descendants, an
/// attribute's value, a character data node's data.
pub fn string_value(dom: &Dom, n: NodeId) -> String {
    match dom.data(n) {
        NodeData::Attr(_) => dom.attr_value(n).to_owned(),
        NodeData::Text(_)
        | NodeData::CDataSection(_)
        | NodeData::Comment(_)
        | NodeData::ProcessingInstruction { .. } => {
            dom.character_data(n).unwrap_or_default().to_owned()
        }
        NodeData::DocumentType { .. } => String::new(),
        _ => dom.descendant_text_content(n),
    }
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// §4.4 `number()` of a string: optional whitespace, an optional minus, a Number, optional
/// whitespace; anything else is NaN.
fn string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(is_space);
    if t.is_empty()
        || !t
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'.' || b == b'-')
    {
        return f64::NAN;
    }
    // (Rust's parser takes exactly XPath's forms once exponents and signs other than a leading
    // minus are ruled out; "-" and "." alone fail.)
    t.parse().unwrap_or(f64::NAN)
}

/// §4.2 `string()` of a number: no exponent, no trailing zeros, integers without a point.
fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n == 0.0 {
        "0".into()
    } else if n.is_infinite() {
        if n < 0.0 { "-Infinity" } else { "Infinity" }.into()
    } else {
        format!("{n}")
    }
}

/// §4.4 `round()`: the closest integer, halves toward positive infinity; -0 for (-0.5, -0].
fn round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    if r == 0.0 && x < 0.0 {
        -0.0
    } else {
        r
    }
}

pub(super) struct Context {
    pub node: NodeId,
    pub position: usize,
    pub size: usize,
}

pub(super) struct Evaluator<'d> {
    pub dom: &'d mut Dom,
}

/// Where a node sits in document order: an attribute right after its element, in list order.
fn order_key(dom: &Dom, n: NodeId) -> (NodeId, usize) {
    if let Some(owner) = dom.attr_owner_element(n) {
        let i = dom
            .attrs(owner)
            .iter()
            .position(|a| a.node() == Some(n))
            .unwrap_or(0);
        return (owner, i + 1);
    }
    (n, 0)
}

impl Evaluator<'_> {
    /// A node's parent in the XPath data model: an attribute's parent is its element.
    fn parent(&self, n: NodeId) -> Option<NodeId> {
        match self.dom.data(n) {
            NodeData::Attr(_) => self.dom.attr_owner_element(n),
            _ => self.dom.parent(n),
        }
    }

    fn is_attr(&self, n: NodeId) -> bool {
        matches!(self.dom.data(n), NodeData::Attr(_))
    }

    /// Sorts `v` into document order and removes duplicates.
    fn sort_unique(&self, v: &mut Vec<NodeId>) {
        if v.len() < 2 {
            return;
        }
        let dom = &*self.dom;
        if v.len() <= 32 {
            v.sort_by(|&a, &b| {
                let (ba, sa) = order_key(dom, a);
                let (bb, sb) = order_key(dom, b);
                if ba == bb {
                    sa.cmp(&sb)
                } else {
                    dom.tree_order(ba, bb)
                }
            });
            v.dedup();
            return;
        }
        // Number every node of the trees involved once: a comparison sort over tree positions costs
        // a walk to the root (and along siblings) per comparison.
        let mut seen = HashSet::with_capacity(v.len());
        v.retain(|n| seen.insert(*n));
        let mut roots: Vec<NodeId> = Vec::new();
        for &n in v.iter() {
            let r = dom.root(order_key(dom, n).0);
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        let mut index: HashMap<NodeId, usize> = HashMap::new();
        for (ri, &r) in roots.iter().enumerate() {
            for (i, d) in dom.inclusive_descendants(r).enumerate() {
                index.insert(d, (ri << 40) | i);
            }
        }
        v.sort_by_cached_key(|&n| {
            let (base, sub) = order_key(dom, n);
            (index.get(&base).copied().unwrap_or(usize::MAX), sub)
        });
    }

    pub(super) fn eval(&mut self, e: &Expr, cx: &Context) -> R<Value> {
        Ok(match e {
            Expr::Number(n) => Value::Number(*n),
            Expr::Literal(s) => Value::String(s.clone()),
            Expr::Variable => {
                return Err(XPathError::Type("XPath variables have no bindings."));
            }
            Expr::Negate(a) => Value::Number(-self.eval(a, cx)?.number(self.dom)),
            Expr::Binary(op, a, b) => self.binary(*op, a, b, cx)?,
            Expr::Call(f, args) => self.call(*f, args, cx)?,
            Expr::Filter(primary, predicates) => {
                let Value::Nodes(mut nodes) = self.eval(primary, cx)? else {
                    return Err(NOT_A_NODE_SET);
                };
                for p in predicates {
                    nodes = self.filter(nodes, p)?;
                }
                Value::Nodes(nodes)
            }
            Expr::Path { absolute, steps } => {
                let start = if *absolute {
                    let mut top = cx.node;
                    while let Some(p) = self.parent(top) {
                        top = p;
                    }
                    top
                } else {
                    cx.node
                };
                Value::Nodes(self.steps(vec![start], steps)?)
            }
            Expr::PathFrom(primary, steps) => {
                let Value::Nodes(nodes) = self.eval(primary, cx)? else {
                    return Err(NOT_A_NODE_SET);
                };
                Value::Nodes(self.steps(nodes, steps)?)
            }
        })
    }

    /// Keeps the nodes of `nodes` (in the order their positions count) for which `p` holds.
    fn filter(&mut self, nodes: Vec<NodeId>, p: &Expr) -> R<Vec<NodeId>> {
        let size = nodes.len();
        let mut out = Vec::with_capacity(size);
        for (i, n) in nodes.into_iter().enumerate() {
            let cx = Context {
                node: n,
                position: i + 1,
                size,
            };
            let keep = match self.eval(p, &cx)? {
                Value::Number(x) => x == (i + 1) as f64,
                v => v.boolean(),
            };
            if keep {
                out.push(n);
            }
        }
        Ok(out)
    }

    fn steps(&mut self, mut nodes: Vec<NodeId>, steps: &[Step]) -> R<Vec<NodeId>> {
        for step in steps {
            let single = nodes.len() == 1;
            let mut next = Vec::new();
            for &n in &nodes {
                let mut v = self.axis_nodes(n, step)?;
                for p in &step.predicates {
                    v = self.filter(v, p)?;
                }
                if step.axis.is_reverse() {
                    v.reverse();
                }
                next.extend(v);
            }
            if !single {
                self.sort_unique(&mut next);
            }
            nodes = next;
        }
        Ok(nodes)
    }

    /// The nodes of `step`'s axis from `n` that pass its node test, in the axis' order.
    fn axis_nodes(&mut self, n: NodeId, step: &Step) -> R<Vec<NodeId>> {
        let mut out = Vec::new();
        let axis = step.axis;
        let attr = self.is_attr(n);
        match axis {
            Axis::Attribute => {
                if !self.dom.is_element(n) {
                    return Ok(out);
                }
                let count = self.dom.attrs(n).len();
                for i in 0..count {
                    let a = &self.dom.attrs(n)[i];
                    if self.attr_matches(n, a, &step.test) {
                        out.push(self.dom.attribute_node(n, i));
                    }
                }
                return Ok(out);
            }
            Axis::Namespace => return Ok(out),
            Axis::Itself => out.push(n),
            Axis::Child if !attr => out.extend(self.dom.children(n)),
            Axis::Descendant if !attr => out.extend(self.dom.descendants(n)),
            Axis::DescendantOrSelf => {
                out.push(n);
                if !attr {
                    out.extend(self.dom.descendants(n));
                }
            }
            Axis::Child | Axis::Descendant => {}
            Axis::Parent => out.extend(self.parent(n)),
            Axis::Ancestor | Axis::AncestorOrSelf => {
                if axis == Axis::AncestorOrSelf {
                    out.push(n);
                }
                let mut cur = self.parent(n);
                while let Some(p) = cur {
                    out.push(p);
                    cur = self.parent(p);
                }
            }
            Axis::FollowingSibling | Axis::PrecedingSibling if attr => {}
            Axis::FollowingSibling => {
                let mut cur = self.dom.next_sibling(n);
                while let Some(s) = cur {
                    out.push(s);
                    cur = self.dom.next_sibling(s);
                }
            }
            Axis::PrecedingSibling => {
                let mut cur = self.dom.previous_sibling(n);
                while let Some(s) = cur {
                    out.push(s);
                    cur = self.dom.previous_sibling(s);
                }
            }
            Axis::Following => {
                // After the node and outside its descendants; an attribute's are its element's
                // descendants and what follows them.
                let (from, skip_children) = match self.dom.attr_owner_element(n) {
                    Some(owner) => (owner, false),
                    None if attr => return Ok(out),
                    None => (n, true),
                };
                let root = self.dom.root(from);
                let mut cur = if skip_children {
                    self.dom.next_skipping_children(from, Some(root))
                } else {
                    self.dom.next_in_tree_order(from, Some(root))
                };
                while let Some(c) = cur {
                    out.push(c);
                    cur = self.dom.next_in_tree_order(c, Some(root));
                }
            }
            Axis::Preceding => {
                // Before the node, excluding its ancestors (an attribute's element is one).
                let from = match self.dom.attr_owner_element(n) {
                    Some(owner) => owner,
                    None if attr => return Ok(out),
                    None => n,
                };
                let mut ancestor = self.dom.parent(from);
                let mut cur = self.dom.previous_in_tree_order(from, None);
                while let Some(c) = cur {
                    if Some(c) == ancestor {
                        ancestor = self.dom.parent(c);
                    } else {
                        out.push(c);
                    }
                    cur = self.dom.previous_in_tree_order(c, None);
                }
            }
        }
        out.retain(|&c| self.node_matches(c, &step.test));
        Ok(out)
    }

    /// A name test on the attribute axis (whose principal node type is attribute). Namespace
    /// declarations are not attributes in XPath.
    fn attr_matches(&self, element: NodeId, a: &crate::Attr, test: &NodeTest) -> bool {
        if *a.namespace() == ns!(xmlns) {
            return false;
        }
        match test {
            NodeTest::Node => true,
            NodeTest::Name { ns, local } => {
                let attr_ns: &str = a.namespace();
                let Some(local) = local else {
                    return ns.as_deref().is_none_or(|u| u == attr_ns);
                };
                if ns.is_none() && attr_ns.is_empty() && self.html_element_in_html_document(element)
                {
                    let attr_local: &str = a.local_name();
                    return attr_local.eq_ignore_ascii_case(local);
                }
                **a.local_name() == **local && ns.as_deref().unwrap_or("") == attr_ns
            }
            _ => false,
        }
    }

    fn html_element_in_html_document(&self, element: NodeId) -> bool {
        *self.dom.namespace(element) == ns!(html)
            && self.dom.is_html_document(self.dom.node_document(element))
    }

    /// A node test on an axis whose principal node type is element.
    fn node_matches(&self, n: NodeId, test: &NodeTest) -> bool {
        let dom = &*self.dom;
        match test {
            NodeTest::Node => true,
            NodeTest::Text => matches!(dom.node_type(n), NodeType::Text | NodeType::CDataSection),
            NodeTest::Comment => dom.node_type(n) == NodeType::Comment,
            NodeTest::ProcessingInstruction(target) => {
                dom.node_type(n) == NodeType::ProcessingInstruction
                    && target
                        .as_deref()
                        .is_none_or(|t| dom.pi_target(n) == Some(t))
            }
            NodeTest::Name { ns, local } => {
                if !dom.is_element(n) {
                    return false;
                }
                let el_ns: &str = dom.namespace(n);
                let Some(local) = local else {
                    return ns.as_deref().is_none_or(|u| u == el_ns);
                };
                let el_local: &str = dom.local_name(n);
                if dom.is_html_document(dom.node_document(n)) {
                    if *dom.namespace(n) == ns!(html) {
                        // Unprefixed names match HTML elements whatever their case.
                        return el_local.eq_ignore_ascii_case(local)
                            && ns.as_deref().is_none_or(|u| u == el_ns);
                    }
                    // An unprefixed name matches no element outside a namespace in an HTML
                    // document (it means the HTML namespace there).
                    return el_local == local && ns.as_deref().is_some_and(|u| u == el_ns);
                }
                el_local == local && ns.as_deref().unwrap_or("") == el_ns
            }
        }
    }

    fn binary(&mut self, op: BinOp, a: &Expr, b: &Expr, cx: &Context) -> R<Value> {
        Ok(match op {
            BinOp::Or => Value::Boolean(self.eval(a, cx)?.boolean() || self.eval(b, cx)?.boolean()),
            BinOp::And => {
                Value::Boolean(self.eval(a, cx)?.boolean() && self.eval(b, cx)?.boolean())
            }
            BinOp::Union => {
                let (Value::Nodes(mut l), Value::Nodes(r)) = (self.eval(a, cx)?, self.eval(b, cx)?)
                else {
                    return Err(NOT_A_NODE_SET);
                };
                l.extend(r);
                self.sort_unique(&mut l);
                Value::Nodes(l)
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.eval(a, cx)?;
                let r = self.eval(b, cx)?;
                Value::Boolean(self.compare(op, &l, &r))
            }
            _ => {
                let x = self.eval(a, cx)?.number(self.dom);
                let y = self.eval(b, cx)?.number(self.dom);
                Value::Number(match op {
                    BinOp::Add => x + y,
                    BinOp::Sub => x - y,
                    BinOp::Mul => x * y,
                    BinOp::Div => x / y,
                    // (truncating, as Rust's % is: XPath's mod is ECMAScript's %)
                    _ => x % y,
                })
            }
        })
    }

    /// §3.4 comparisons, with a node-set's existential rules.
    fn compare(&self, op: BinOp, l: &Value, r: &Value) -> bool {
        let dom = &*self.dom;
        match (l, r) {
            (Value::Nodes(a), Value::Nodes(b)) => {
                let bs: Vec<String> = b.iter().map(|&n| string_value(dom, n)).collect();
                a.iter().any(|&n| {
                    let s = Value::String(string_value(dom, n));
                    bs.iter()
                        .any(|t| atom_compare(dom, op, &s, &Value::String(t.clone())))
                })
            }
            (Value::Nodes(a), Value::Boolean(_)) => {
                atom_compare(dom, op, &Value::Boolean(!a.is_empty()), r)
            }
            (Value::Boolean(_), Value::Nodes(b)) => {
                atom_compare(dom, op, l, &Value::Boolean(!b.is_empty()))
            }
            (Value::Nodes(a), _) => a
                .iter()
                .any(|&n| atom_compare(dom, op, &Value::String(string_value(dom, n)), r)),
            (_, Value::Nodes(b)) => b
                .iter()
                .any(|&n| atom_compare(dom, op, l, &Value::String(string_value(dom, n)))),
            _ => atom_compare(dom, op, l, r),
        }
    }

    fn arg(&mut self, args: &[Expr], i: usize, cx: &Context) -> R<Value> {
        self.eval(&args[i], cx)
    }
    fn arg_string(&mut self, args: &[Expr], i: usize, cx: &Context) -> R<String> {
        Ok(self.arg(args, i, cx)?.string(self.dom))
    }
    fn arg_number(&mut self, args: &[Expr], i: usize, cx: &Context) -> R<f64> {
        Ok(self.arg(args, i, cx)?.number(self.dom))
    }
    /// An optional string argument, the context node when absent.
    fn string_or_context(&mut self, args: &[Expr], cx: &Context) -> R<String> {
        if args.is_empty() {
            Ok(string_value(self.dom, cx.node))
        } else {
            self.arg_string(args, 0, cx)
        }
    }
    /// An optional node-set argument's first node, the context node when absent.
    fn node_or_context(&mut self, args: &[Expr], cx: &Context) -> R<Option<NodeId>> {
        if args.is_empty() {
            return Ok(Some(cx.node));
        }
        match self.arg(args, 0, cx)? {
            Value::Nodes(n) => Ok(n.first().copied()),
            _ => Err(NOT_A_NODE_SET),
        }
    }

    fn call(&mut self, f: Func, args: &[Expr], cx: &Context) -> R<Value> {
        use Func::*;
        Ok(match f {
            Last => Value::Number(cx.size as f64),
            Position => Value::Number(cx.position as f64),
            Count => match self.arg(args, 0, cx)? {
                Value::Nodes(n) => Value::Number(n.len() as f64),
                _ => return Err(NOT_A_NODE_SET),
            },
            Id => {
                let tokens: Vec<std::string::String> = match self.arg(args, 0, cx)? {
                    Value::Nodes(n) => n.iter().map(|&n| string_value(self.dom, n)).collect(),
                    v => vec![v.string(self.dom)],
                };
                let mut root = cx.node;
                while let Some(p) = self.parent(root) {
                    root = p;
                }
                let mut out = Vec::new();
                for t in &tokens {
                    for id in t.split(is_space).filter(|s| !s.is_empty()) {
                        out.extend(self.element_by_id(root, id));
                    }
                }
                self.sort_unique(&mut out);
                Value::Nodes(out)
            }
            LocalName | NamespaceUri | Name => {
                let Some(n) = self.node_or_context(args, cx)? else {
                    return Ok(Value::String(std::string::String::new()));
                };
                let dom = &*self.dom;
                let qual = match dom.data(n) {
                    NodeData::Element(_) => Some(dom.element_name(n)),
                    NodeData::Attr(_) => Some(dom.attr_node_attr(n).qual_name()),
                    _ => None,
                };
                Value::String(match (f, qual) {
                    (LocalName, Some(q)) => q.local.to_string(),
                    (NamespaceUri, Some(q)) => q.ns.to_string(),
                    (Name, Some(q)) => match &q.prefix {
                        Some(p) if !p.is_empty() => format!("{p}:{}", q.local),
                        _ => q.local.to_string(),
                    },
                    (LocalName | Name, None) => dom.pi_target(n).unwrap_or_default().to_owned(),
                    _ => std::string::String::new(),
                })
            }
            String => Value::String(self.string_or_context(args, cx)?),
            Concat => {
                let mut s = std::string::String::new();
                for i in 0..args.len() {
                    s.push_str(&self.arg_string(args, i, cx)?);
                }
                Value::String(s)
            }
            StartsWith | Contains | SubstringBefore | SubstringAfter => {
                let s = self.arg_string(args, 0, cx)?;
                let t = self.arg_string(args, 1, cx)?;
                match f {
                    StartsWith => Value::Boolean(s.starts_with(&t)),
                    Contains => Value::Boolean(s.contains(&t)),
                    SubstringBefore => {
                        Value::String(s.find(&t).map(|i| s[..i].to_owned()).unwrap_or_default())
                    }
                    _ => Value::String(
                        s.find(&t)
                            .map(|i| s[i + t.len()..].to_owned())
                            .unwrap_or_default(),
                    ),
                }
            }
            Substring => {
                let s = self.arg_string(args, 0, cx)?;
                let start = round(self.arg_number(args, 1, cx)?);
                let end = if args.len() == 3 {
                    start + round(self.arg_number(args, 2, cx)?)
                } else {
                    f64::INFINITY
                };
                // Characters at positions p (from 1) with start <= p < start + length; NaN anywhere
                // selects none.
                Value::String(
                    s.chars()
                        .enumerate()
                        .filter(|&(i, _)| {
                            let p = (i + 1) as f64;
                            p >= start && p < end
                        })
                        .map(|(_, c)| c)
                        .collect(),
                )
            }
            StringLength => Value::Number(self.string_or_context(args, cx)?.chars().count() as f64),
            NormalizeSpace => {
                let s = self.string_or_context(args, cx)?;
                Value::String(
                    s.split(is_space)
                        .filter(|w| !w.is_empty())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
            Translate => {
                let s = self.arg_string(args, 0, cx)?;
                let from: Vec<char> = self.arg_string(args, 1, cx)?.chars().collect();
                let to: Vec<char> = self.arg_string(args, 2, cx)?.chars().collect();
                Value::String(
                    s.chars()
                        .filter_map(|c| match from.iter().position(|&x| x == c) {
                            Some(i) => to.get(i).copied(),
                            None => Some(c),
                        })
                        .collect(),
                )
            }
            Boolean => Value::Boolean(self.arg(args, 0, cx)?.boolean()),
            Not => Value::Boolean(!self.arg(args, 0, cx)?.boolean()),
            True => Value::Boolean(true),
            False => Value::Boolean(false),
            Lang => {
                let want = self.arg_string(args, 0, cx)?;
                Value::Boolean(self.lang(cx.node, &want))
            }
            Number => Value::Number(if args.is_empty() {
                string_to_number(&string_value(self.dom, cx.node))
            } else {
                self.arg_number(args, 0, cx)?
            }),
            Sum => match self.arg(args, 0, cx)? {
                Value::Nodes(n) => Value::Number(
                    n.iter()
                        .map(|&n| string_to_number(&string_value(self.dom, n)))
                        .sum(),
                ),
                _ => return Err(NOT_A_NODE_SET),
            },
            Floor => Value::Number(self.arg_number(args, 0, cx)?.floor()),
            Ceiling => Value::Number(self.arg_number(args, 0, cx)?.ceil()),
            Round => Value::Number(round(self.arg_number(args, 0, cx)?)),
        })
    }

    /// §4.3 `lang()`: the nearest `xml:lang` on the node or an ancestor, compared ignoring ASCII
    /// case, equal to `want` or starting with it and a hyphen.
    fn lang(&self, n: NodeId, want: &str) -> bool {
        let mut cur = Some(n);
        while let Some(c) = cur {
            if self.dom.is_element(c) {
                if let Some(v) = self.dom.attr(c, &ns!(xml), &local_name!("lang")) {
                    let Some(head) = v.get(..want.len()) else {
                        return false;
                    };
                    return head.eq_ignore_ascii_case(want)
                        && (v.len() == want.len() || v.as_bytes()[want.len()] == b'-');
                }
            }
            cur = self.parent(c);
        }
        false
    }

    /// The first element in tree order under `root` (inclusive) whose ID is `id`.
    fn element_by_id(&self, root: NodeId, id: &str) -> Option<NodeId> {
        if let Some(found) = self.dom.indexed_element_by_id(root, id) {
            return found;
        }
        self.dom.inclusive_descendants(root).find(|&d| {
            self.dom.is_element(d) && self.dom.attr(d, &ns!(), &local_name!("id")) == Some(id)
        })
    }
}

/// §3.4 comparison of two values that are not node-sets.
fn atom_compare(dom: &Dom, op: BinOp, l: &Value, r: &Value) -> bool {
    let num = |op: BinOp, x: f64, y: f64| match op {
        BinOp::Eq => x == y,
        BinOp::Ne => x != y,
        BinOp::Lt => x < y,
        BinOp::Le => x <= y,
        BinOp::Gt => x > y,
        _ => x >= y,
    };
    match op {
        BinOp::Eq | BinOp::Ne => {
            let eq = if matches!(l, Value::Boolean(_)) || matches!(r, Value::Boolean(_)) {
                l.boolean() == r.boolean()
            } else if matches!(l, Value::Number(_)) || matches!(r, Value::Number(_)) {
                return num(op, l.number(dom), r.number(dom));
            } else {
                l.string(dom) == r.string(dom)
            };
            eq == (op == BinOp::Eq)
        }
        _ => num(op, l.number(dom), r.number(dom)),
    }
}
