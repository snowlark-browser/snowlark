// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DOM §8 XPath: `XPathEvaluator` and the `XPathEvaluatorBase` mixin (on `Document` too),
//! `XPathExpression` and `XPathResult`. Expressions are compiled and evaluated by
//! `snowlark_dom::xpath`; the resolver is called while compiling, and an exception it throws is
//! reported and the prefix treated as unresolved.

use std::rc::Rc;

use snowlark_bindings::generated::{
    XPathEvaluatorBaseImpl, XPathEvaluatorImpl, XPathExpressionImpl, XPathNSResolver,
    XPathResultImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Error, ExceptionName, Handle};
use snowlark_dom::xpath::{self, Expression, Value, XPathError};
use snowlark_dom::NodeId;

use super::{exception, node_handle, Engine, Object};

const ANY_TYPE: u16 = 0;
const NUMBER_TYPE: u16 = 1;
const STRING_TYPE: u16 = 2;
const BOOLEAN_TYPE: u16 = 3;
const UNORDERED_NODE_ITERATOR_TYPE: u16 = 4;
const ORDERED_NODE_ITERATOR_TYPE: u16 = 5;
const UNORDERED_NODE_SNAPSHOT_TYPE: u16 = 6;
const ORDERED_NODE_SNAPSHOT_TYPE: u16 = 7;
const ANY_UNORDERED_NODE_TYPE: u16 = 8;
const FIRST_ORDERED_NODE_TYPE: u16 = 9;

/// An `XPathResult`: its type, its value (node-sets in document order, which serves the unordered
/// kinds too), the iterator's position and the document version an iterator is valid for.
pub struct ResultData {
    kind: u16,
    value: Value,
    position: usize,
    document: NodeId,
    version: u64,
}

impl ResultData {
    /// The nodes and document a result keeps alive.
    pub(crate) fn gc_nodes(&self, out: &mut Vec<NodeId>) {
        out.push(self.document);
        if let Value::Nodes(n) = &self.value {
            out.extend(n);
        }
    }
}

fn wrong_type(message: &'static str) -> Error {
    Error::Type(message.into())
}

impl Engine {
    /// Compiles `text`, resolving its prefixes with `resolver`.
    fn xpath_compile(
        &self,
        cx: &mut Cx,
        text: &str,
        resolver: Option<XPathNSResolver>,
    ) -> rt::Result<Rc<Expression>> {
        let mut thrown: Vec<v8::Global<v8::Value>> = Vec::new();
        let parsed = {
            let mut resolve = |prefix: &str| -> Result<Option<String>, ()> {
                let Some(r) = &resolver else {
                    return Ok(None);
                };
                let scope = cx.scope();
                v8::tc_scope!(let tc, scope);
                let outcome = {
                    let mut inner = Cx::new(tc);
                    r.lookup_namespace_uri(&mut inner, None, Some(prefix))
                };
                match outcome {
                    Ok(uri) => Ok(uri),
                    Err(e) => {
                        let value = match e {
                            Error::Pending => tc.exception(),
                            other => rt::error_value(tc, other),
                        };
                        if let Some(v) = value {
                            thrown.push(v8::Global::new(tc, v));
                        }
                        if !tc.has_terminated() {
                            tc.reset();
                        }
                        Err(())
                    }
                }
            };
            xpath::parse(text, &mut resolve)
        };
        // "Report the exception" for what the resolver threw; the prefix stays unresolved.
        for g in thrown {
            self.report_exception(cx, g, None);
        }
        match parsed {
            Ok(e) => Ok(Rc::new(e)),
            Err(XPathError::Syntax(m)) => Err(exception(ExceptionName::SyntaxError, m)),
            Err(XPathError::Namespace(_) | XPathError::Resolver(())) => Err(exception(
                ExceptionName::NamespaceError,
                "The string contains an unresolvable namespace prefix.",
            )),
            Err(XPathError::Type(m)) => Err(exception(ExceptionName::SyntaxError, m)),
        }
    }

    /// Evaluates `expr` at `context` and makes a result of `kind` (0 for the value's own type).
    fn xpath_evaluate(&self, expr: &Expression, context: Handle, kind: u16) -> rt::Result<Handle> {
        let node = self.n(context);
        if !xpath::is_valid_context(&self.dom(), node) {
            let name = self.dom().node_name(node).into_owned();
            return Err(Error::Dom(
                ExceptionName::NotSupportedError,
                format!("The node provided is '{name}', which is not a valid context node type.")
                    .into(),
            ));
        }
        let value = expr
            .evaluate(&mut self.dom_mut(), node)
            .map_err(|e| match e {
                XPathError::Type(m) | XPathError::Syntax(m) => {
                    exception(ExceptionName::SyntaxError, m)
                }
                _ => exception(ExceptionName::SyntaxError, "The expression failed."),
            })?;
        let is_nodes = matches!(value, Value::Nodes(_));
        let (kind, value) = {
            let dom = self.dom();
            match kind {
                ANY_TYPE => (
                    match &value {
                        Value::Nodes(_) => UNORDERED_NODE_ITERATOR_TYPE,
                        Value::Number(_) => NUMBER_TYPE,
                        Value::String(_) => STRING_TYPE,
                        Value::Boolean(_) => BOOLEAN_TYPE,
                    },
                    value,
                ),
                NUMBER_TYPE => (kind, Value::Number(value.number(&dom))),
                STRING_TYPE => (kind, Value::String(value.string(&dom))),
                BOOLEAN_TYPE => (kind, Value::Boolean(value.boolean())),
                UNORDERED_NODE_ITERATOR_TYPE..=FIRST_ORDERED_NODE_TYPE if !is_nodes => {
                    return Err(wrong_type(
                        "The result is not a node set, and therefore cannot be converted to the \
                         desired type.",
                    ));
                }
                UNORDERED_NODE_ITERATOR_TYPE..=FIRST_ORDERED_NODE_TYPE => (kind, value),
                // (an unknown type keeps the value's own, as `ANY_TYPE` does)
                _ => (
                    match &value {
                        Value::Nodes(_) => UNORDERED_NODE_ITERATOR_TYPE,
                        Value::Number(_) => NUMBER_TYPE,
                        Value::String(_) => STRING_TYPE,
                        Value::Boolean(_) => BOOLEAN_TYPE,
                    },
                    value,
                ),
            }
        };
        let (document, version) = {
            let dom = self.dom();
            let document = dom.node_document(node);
            (document, dom.document_version(document))
        };
        Ok(self.alloc(Object::XPathResult(ResultData {
            kind,
            value,
            position: 0,
            document,
            version,
        })))
    }

    fn with_result<T>(&self, h: Handle, f: impl FnOnce(&ResultData) -> T) -> T {
        match &*self.obj(h) {
            Object::XPathResult(r) => f(r),
            _ => unreachable!("not an XPathResult"),
        }
    }

    fn result_invalid(&self, r: &ResultData) -> bool {
        matches!(
            r.kind,
            UNORDERED_NODE_ITERATOR_TYPE | ORDERED_NODE_ITERATOR_TYPE
        ) && self.dom().document_version(r.document) != r.version
    }
}

impl XPathEvaluatorBaseImpl for Engine {
    fn create_expression(
        &self,
        cx: &mut Cx,
        _this: Handle,
        expression: &str,
        resolver: Option<XPathNSResolver>,
    ) -> rt::Result<Handle> {
        let expr = self.xpath_compile(cx, expression, resolver)?;
        Ok(self.alloc(Object::XPathExpression(expr)))
    }

    /// `createNSResolver(nodeResolver)` returns its argument (DOM §8.3).
    fn create_ns_resolver(
        &self,
        _cx: &mut Cx,
        _this: Handle,
        node_resolver: Handle,
    ) -> rt::Result<Handle> {
        Ok(node_resolver)
    }

    fn evaluate(
        &self,
        cx: &mut Cx,
        _this: Handle,
        expression: &str,
        context_node: Handle,
        resolver: Option<XPathNSResolver>,
        type_: u16,
        _result: Option<Handle>,
    ) -> rt::Result<Handle> {
        let expr = self.xpath_compile(cx, expression, resolver)?;
        self.xpath_evaluate(&expr, context_node, type_)
    }
}

impl XPathEvaluatorImpl for Engine {
    fn constructor(&self, _cx: &mut Cx) -> rt::Result<Handle> {
        Ok(self.alloc(Object::XPathEvaluator))
    }
}

impl XPathExpressionImpl for Engine {
    fn evaluate(
        &self,
        _cx: &mut Cx,
        this: Handle,
        context_node: Handle,
        type_: u16,
        _result: Option<Handle>,
    ) -> rt::Result<Handle> {
        let expr = match &*self.obj(this) {
            Object::XPathExpression(e) => e.clone(),
            _ => unreachable!("not an XPathExpression"),
        };
        self.xpath_evaluate(&expr, context_node, type_)
    }
}

impl XPathResultImpl for Engine {
    fn result_type(&self, this: Handle) -> u16 {
        self.with_result(this, |r| r.kind)
    }

    fn number_value(&self, _cx: &mut Cx, this: Handle) -> rt::Result<f64> {
        self.with_result(this, |r| match (&r.value, r.kind) {
            (Value::Number(n), NUMBER_TYPE) => Ok(*n),
            _ => Err(wrong_type("The result type is not a number.")),
        })
    }

    fn string_value(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        self.with_result(this, |r| match (&r.value, r.kind) {
            (Value::String(s), STRING_TYPE) => Ok(DomStr::Owned(s.clone())),
            _ => Err(wrong_type("The result type is not a string.")),
        })
    }

    fn boolean_value(&self, _cx: &mut Cx, this: Handle) -> rt::Result<bool> {
        self.with_result(this, |r| match (&r.value, r.kind) {
            (Value::Boolean(b), BOOLEAN_TYPE) => Ok(*b),
            _ => Err(wrong_type("The result type is not a boolean.")),
        })
    }

    fn single_node_value(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        self.with_result(this, |r| match (&r.value, r.kind) {
            (Value::Nodes(n), ANY_UNORDERED_NODE_TYPE | FIRST_ORDERED_NODE_TYPE) => {
                Ok(n.first().map(|&n| node_handle(n)))
            }
            _ => Err(wrong_type("The result type is not a single node.")),
        })
    }

    fn invalid_iterator_state(&self, this: Handle) -> bool {
        self.with_result(this, |r| self.result_invalid(r))
    }

    fn snapshot_length(&self, _cx: &mut Cx, this: Handle) -> rt::Result<u32> {
        self.with_result(this, |r| match (&r.value, r.kind) {
            (Value::Nodes(n), UNORDERED_NODE_SNAPSHOT_TYPE | ORDERED_NODE_SNAPSHOT_TYPE) => {
                Ok(n.len() as u32)
            }
            _ => Err(wrong_type("The result type is not a snapshot.")),
        })
    }

    fn iterate_next(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Option<Handle>> {
        let next = self.with_result(this, |r| {
            let Value::Nodes(n) = &r.value else {
                return Err(wrong_type("The result type is not an iterator."));
            };
            if !matches!(
                r.kind,
                UNORDERED_NODE_ITERATOR_TYPE | ORDERED_NODE_ITERATOR_TYPE
            ) {
                return Err(wrong_type("The result type is not an iterator."));
            }
            if self.result_invalid(r) {
                return Err(exception(
                    ExceptionName::InvalidStateError,
                    "The document has mutated since the result was returned.",
                ));
            }
            Ok(n.get(r.position).copied())
        })?;
        if next.is_some() {
            if let Object::XPathResult(r) = &mut *self.obj_mut(this) {
                r.position += 1;
            }
        }
        Ok(next.map(node_handle))
    }

    fn snapshot_item(&self, _cx: &mut Cx, this: Handle, index: u32) -> rt::Result<Option<Handle>> {
        self.with_result(this, |r| match (&r.value, r.kind) {
            (Value::Nodes(n), UNORDERED_NODE_SNAPSHOT_TYPE | ORDERED_NODE_SNAPSHOT_TYPE) => {
                Ok(n.get(index as usize).map(|&n| node_handle(n)))
            }
            _ => Err(wrong_type("The result type is not a snapshot.")),
        })
    }
}
