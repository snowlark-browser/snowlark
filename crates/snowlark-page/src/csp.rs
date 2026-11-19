// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Content Security Policy in the page: each document's (and worker's) policy container, the request
//! policy's one entry point, and violation reporting.
//!
//! - **The policy container** (HTML §7.1.4.4): a document's CSP list comes from its response's headers
//!   (`Content-Security-Policy`, `-Report-Only`), from `<meta http-equiv>` policies the parser or a
//!   script inserts (each governing only what comes after it), or, for documents of local schemes
//!   (`about:blank`, `about:srcdoc`, `blob:`, `data:`), a copy of their creator's. The CSP `sandbox`
//!   directive gives the document sandboxing flags: no scripts without `allow-scripts`, an opaque origin
//!   without `allow-same-origin`, no form submission without `allow-forms`.
//! - **The request policy's entry point** ([`Engine::request_client`], [`Engine::policy_fetch`],
//!   [`Engine::fetch_policy`]): every request a document makes (scripts, modules, sheets, `@import`,
//!   images, fonts, frames, `fetch()`, XHR, beacons, workers, form submissions, reports) gets the
//!   document's environment as its Fetch client: origin, CSP list, referrer, top-level origin,
//!   cross-site ancestry and whether it prohibits mixed content. `snowlark-net` runs the CSP, mixed
//!   content, upgrade-insecure-requests and bad-port checks on it, for the request and each redirect,
//!   and sends the violations as fetch events, which an observer on the network thread queues here
//!   for the page thread to report. A document without a policy gets a client without a CSP list: the
//!   network layer's check is one null test, and its fetches have no observer.
//! - **Reporting** (CSP §5.3 "report a violation"): a Chrome-style console message, a
//!   `securitypolicyviolation` event (a task) at the element the violation is about when it is still
//!   connected to the document, else at the document (or at a worker's global), and a report POSTed
//!   to `report-uri` or to the `report-to` endpoint the document's `Reporting-Endpoints` (or `Report-To`)
//!   named, each distinct report once.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

use snowlark_bindings::generated::ids;
use snowlark_bindings::generated::{
    SecurityPolicyViolationEventDisposition, SecurityPolicyViolationEventImpl,
    SecurityPolicyViolationEventInit,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};
use snowlark_bindings::InterfaceId;
use snowlark_dom::{local_name, ns, NodeId};
use snowlark_net::csp::{
    CspList, Disposition, InlineType, Policy, PolicySource, RequestInfo, Verdict, Violation,
};
use snowlark_net::report::ViolationReport;
use snowlark_net::{Client, FetchEvent, Observer, Request, ResponseHead, ResponseStream};

use crate::engine::events::EventExt;
use crate::engine::{node_handle, Engine};
use crate::event_loop::TaskSource;

/// A document's policy container beyond the defaults, and its request client.
#[derive(Default)]
pub(crate) struct DocPolicy {
    /// The CSP list; `None` when the document has no policy.
    pub csp: Option<Arc<CspList>>,
    /// The CSP `sandbox` directive's flags.
    pub sandbox: Option<Sandbox>,
    /// The opaque origin of a sandboxed document (without `allow-same-origin`) or of the document a
    /// blocked frame navigation leaves.
    pub opaque_origin: Option<url::Origin>,
    /// The flags the document got from its frame ([`Engine::apply_frame_sandbox`]), which a policy of
    /// its own can only add to.
    frame_sandbox: Option<Sandbox>,
    /// Reporting API endpoints: (name, URL).
    pub endpoints: Vec<(String, url::Url)>,
    /// The status of the response the document came from (`statusCode` of its reports).
    pub status: u16,
    /// The policy container's referrer policy (`None`: the empty string, the default): its
    /// response's `Referrer-Policy` header, or a `<meta name=referrer>`'s.
    pub referrer_policy: Option<snowlark_net::ReferrerPolicy>,
    /// The request client, made at the first request (and again when the policy or the document's
    /// URL changes), with the document URL it was made at.
    pub(crate) client: Option<(Arc<Client>, String)>,
}

/// HTML's sandboxing flags a CSP `sandbox` directive or an iframe's `sandbox` attribute sets (the
/// ones this engine acts on), as what they still allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sandbox {
    pub scripts: bool,
    pub same_origin: bool,
    pub forms: bool,
    pub top_navigation: bool,
    /// `allow-popups`: not the "sandboxed auxiliary navigation browsing context flag".
    pub popups: bool,
    /// `allow-popups-to-escape-sandbox`: not the "sandbox propagates to auxiliary browsing contexts
    /// flag".
    pub escape_popups: bool,
}

impl Sandbox {
    /// HTML §7.1.5 "parse a sandboxing directive" for the tokens this engine acts on.
    fn parse(tokens: &[String]) -> Sandbox {
        let has = |t: &str| tokens.iter().any(|x| x.eq_ignore_ascii_case(t));
        Sandbox {
            scripts: has("allow-scripts"),
            same_origin: has("allow-same-origin"),
            forms: has("allow-forms"),
            top_navigation: has("allow-top-navigation"),
            popups: has("allow-popups"),
            escape_popups: has("allow-popups-to-escape-sandbox"),
        }
    }

    /// What two sets of flags together allow (a set of flags is the union of what sets it).
    fn and(self, other: Option<Sandbox>) -> Sandbox {
        match other {
            Some(o) => Sandbox {
                scripts: self.scripts && o.scripts,
                same_origin: self.same_origin && o.same_origin,
                forms: self.forms && o.forms,
                top_navigation: self.top_navigation && o.top_navigation,
                popups: self.popups && o.popups,
                escape_popups: self.escape_popups && o.escape_popups,
            },
            None => self,
        }
    }
}

/// The referrer policy a `meta name=referrer` with this `content` sets (HTML §4.2.5.1, steps 3-6):
/// its value in ASCII lowercase, a legacy keyword standing for its policy; `None` for an empty or
/// unknown value, which sets nothing.
pub(crate) fn meta_referrer_policy(content: &str) -> Option<snowlark_net::ReferrerPolicy> {
    let value = content.to_ascii_lowercase();
    let value = match value.as_str() {
        "never" => "no-referrer",
        "default" => "strict-origin-when-cross-origin",
        "always" => "unsafe-url",
        "origin-when-crossorigin" => "origin-when-cross-origin",
        v => v,
    };
    snowlark_net::ReferrerPolicy::parse(value).filter(|&p| p != snowlark_net::ReferrerPolicy::Empty)
}

/// The engine's Content Security Policy state.
#[derive(Default)]
pub(crate) struct PolicyState {
    /// Some document of this engine has (or had) a policy container that isn't the default: until
    /// then nothing here is looked at beyond this flag.
    pub any: bool,
    pub docs: rustc_hash::FxHashMap<NodeId, DocPolicy>,
    /// Request clients a document had before its policy or URL changed, while fetches made with
    /// them may still be in flight: their results are still that document's (Resource Timing).
    pub retired_clients: Vec<(std::sync::Weak<snowlark_net::Client>, NodeId)>,
    /// Violations the network thread saw: (the document's id bits, the violation).
    inbox: Arc<Mutex<Vec<(u64, Violation)>>>,
    /// Hashes of the reports already sent.
    sent: rustc_hash::FxHashSet<u64>,
    /// (element, kind, source hash) of inline checks already reported, so that checking the same
    /// thing again (a `<style>` whose `media` changed) reports once.
    inline_seen: HashSet<(NodeId, u8, u64)>,
    /// The style engine's `style` attribute checks' violations: (element id bits, value, violations).
    style_pending: Arc<Mutex<PendingStyleAttributes>>,
    /// `style` attribute values the CSSOM wrote ((element id bits, value hash)): `style-src-attr`
    /// governs markup and `setAttribute`, not `element.style`.
    cssom_styles: Arc<Mutex<HashSet<(u64, u64)>>>,
}

impl PolicyState {
    /// Keeps `doc`'s replaced client known while something still holds it (the fetches it made).
    fn retire_client(&mut self, old: Option<(Arc<snowlark_net::Client>, String)>, doc: NodeId) {
        self.retired_clients.retain(|(c, _)| c.strong_count() > 0);
        if let Some((c, _)) = old {
            self.retired_clients.push((Arc::downgrade(&c), doc));
        }
    }
}

fn hash_of(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

impl Engine {
    // ----- the policy container ----------------------------------------------------------------------

    /// The document's CSP list (`None` without a policy: one flag test for documents of an engine
    /// that never saw a policy).
    pub(crate) fn doc_csp(&self, doc: NodeId) -> Option<Arc<CspList>> {
        let p = self.policy.borrow();
        if !p.any {
            return None;
        }
        p.docs.get(&doc).and_then(|d| d.csp.clone())
    }

    /// Whether the document's CSP `sandbox` turns its scripts off.
    pub(crate) fn scripts_sandboxed(&self, doc: NodeId) -> bool {
        let p = self.policy.borrow();
        p.any
            && p.docs
                .get(&doc)
                .and_then(|d| d.sandbox)
                .is_some_and(|s| !s.scripts)
    }

    /// Whether the document's active sandboxing flag set has the sandboxed plugins browsing context
    /// flag: any sandbox sets it, and no keyword lifts it (HTML §7.1.5). Its `object` and `embed`
    /// elements show no PDF (HTML §4.8.6, §4.8.7).
    pub(crate) fn plugins_sandboxed(&self, doc: NodeId) -> bool {
        let p = self.policy.borrow();
        p.any && p.docs.get(&doc).is_some_and(|d| d.sandbox.is_some())
    }

    /// Whether the document's active sandboxing flag set has the sandboxed automatic features
    /// browsing context flag: a sandbox without `allow-scripts` (HTML §7.1.5).
    pub(crate) fn automatic_features_sandboxed(&self, doc: NodeId) -> bool {
        self.scripts_sandboxed(doc)
    }

    /// Whether the document's CSP `sandbox` forbids form submission.
    pub(crate) fn forms_sandboxed(&self, doc: NodeId) -> bool {
        let p = self.policy.borrow();
        p.any
            && p.docs
                .get(&doc)
                .and_then(|d| d.sandbox)
                .is_some_and(|s| !s.forms)
    }

    /// The opaque origin a sandbox (or a blocked frame navigation) gave the document.
    pub(crate) fn sandboxed_origin(&self, doc: NodeId) -> Option<url::Origin> {
        let p = self.policy.borrow();
        if !p.any {
            return None;
        }
        p.docs.get(&doc).and_then(|d| d.opaque_origin.clone())
    }

    fn doc_policy_mut<R>(&self, doc: NodeId, f: impl FnOnce(&mut DocPolicy) -> R) -> R {
        let mut p = self.policy.borrow_mut();
        p.any = true;
        let old = p.docs.entry(doc).or_default().client.take();
        p.retire_client(old, doc);
        f(p.docs.get_mut(&doc).expect("entry"))
    }

    /// The policy container of a document created from a response (HTML "create and initialize a
    /// `Document` object" with the navigation params' policy container): the response's CSP list, its
    /// Reporting API endpoints and its status. A document of a local scheme (`about:`, `blob:`,
    /// `data:`) takes a copy of `creator`'s instead.
    pub(crate) fn document_response_policy(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        url: &url::Url,
        head: &ResponseHead,
        creator: Option<NodeId>,
    ) {
        if matches!(url.scheme(), "about" | "blob" | "data") {
            if let Some(c) = creator {
                self.inherit_policy(cx, doc, c);
            }
            return;
        }
        // HTML "create a policy container from a fetch response": the referrer policy is the
        // response's `Referrer-Policy` header's.
        let referrer_policy = snowlark_net::referrer::parse_referrer_policy_header(&head.headers);
        if referrer_policy != snowlark_net::ReferrerPolicy::Empty {
            self.doc_policy_mut(doc, |d| d.referrer_policy = Some(referrer_policy));
            self.render_policy_changed(doc);
        }
        let origin = snowlark_net::origin::origin_of(url, None);
        let (csp, warnings) = CspList::from_headers(&head.headers, &origin);
        for w in warnings {
            self.console_print("warn", &w);
        }
        let endpoints = snowlark_net::report::reporting_endpoints(&head.headers, url);
        if csp.is_empty() && endpoints.is_empty() {
            if self.policy.borrow().any {
                // (a realm kept from the initial about:blank document drops its policy's eval check)
                self.update_codegen(cx, doc);
            }
            return;
        }
        let status = head.status;
        self.doc_policy_mut(doc, |d| {
            d.endpoints = endpoints;
            d.status = status;
        });
        self.set_csp_list(cx, doc, (!csp.is_empty()).then(|| Arc::new(csp)));
    }

    /// `doc` gets a copy of `from`'s policy container (local-scheme documents, a frame's initial
    /// `about:blank` document, a `blob:` or `data:` worker).
    pub(crate) fn inherit_policy(&self, cx: &mut Cx, doc: NodeId, from: NodeId) {
        let (csp, endpoints, referrer_policy) = {
            let p = self.policy.borrow();
            if !p.any {
                return;
            }
            match p.docs.get(&from) {
                Some(d) => (d.csp.clone(), d.endpoints.clone(), d.referrer_policy),
                None => return,
            }
        };
        self.doc_policy_mut(doc, |d| {
            d.endpoints = endpoints;
            d.referrer_policy = referrer_policy;
        });
        // (`set_csp_list` below hands the renderer the new client)
        self.set_csp_list(cx, doc, csp);
    }

    /// Sets the document's CSP list and applies what it changes: the sandbox, the realm's string
    /// compilation, the renderer's fetches.
    pub(crate) fn set_csp_list(&self, cx: &mut Cx, doc: NodeId, csp: Option<Arc<CspList>>) {
        let sandbox = csp
            .as_ref()
            .and_then(|c| c.sandbox_tokens())
            .map(|t| Sandbox::parse(&t));
        let sandbox = self.doc_policy_mut(doc, |d| {
            let sandbox = match (sandbox, d.frame_sandbox) {
                (Some(s), frame) => Some(s.and(frame)),
                (None, frame) => frame,
            };
            d.csp = csp;
            d.sandbox = sandbox;
            if sandbox.is_some_and(|s| !s.same_origin) && d.opaque_origin.is_none() {
                d.opaque_origin = Some(url::Origin::new_opaque());
            }
            sandbox
        });
        if sandbox.is_some_and(|s| !s.same_origin) {
            // (the realm's security token follows the document's new origin)
            self.reset_security_token(cx, doc);
        }
        self.update_codegen(cx, doc);
        self.render_policy_changed(doc);
    }

    /// HTML §4.2.5.3 the `Content-Security-Policy` state of `<meta http-equiv>`: a `meta` inserted
    /// into the document's `head` adds its policy (without `report-uri`, `frame-ancestors` and
    /// `sandbox`) to the document's CSP list.
    pub(crate) fn meta_inserted(&self, cx: &mut Cx, el: NodeId) {
        self.meta_referrer(el);
        let (doc, text) = {
            let dom = self.dom();
            let is_csp = dom
                .attr(el, &ns!(), &local_name!("http-equiv"))
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("content-security-policy"));
            if !is_csp {
                return;
            }
            let in_head = dom
                .parent(el)
                .is_some_and(|p| dom.is_html_element_named(p, &local_name!("head")));
            let Some(content) = dom.attr(el, &ns!(), &local_name!("content")) else {
                return;
            };
            if !in_head || content.trim().is_empty() {
                return;
            }
            (dom.node_document(el), content.to_owned())
        };
        if !self.is_active_document(doc) && self.worker_scope.borrow().is_none() {
            return;
        }
        let origin = self.net_origin_of_document(doc);
        let (policy, warnings) =
            Policy::parse(&text, PolicySource::Meta, Disposition::Enforce, origin);
        for w in warnings {
            self.console_print("warn", &w);
        }
        if policy.directives.is_empty() {
            return;
        }
        let mut list = self.doc_csp(doc).map(|l| (*l).clone()).unwrap_or_default();
        list.push(policy);
        self.set_csp_list(cx, doc, Some(Arc::new(list)));
    }

    /// HTML §4.2.5.1 the `referrer` metadata name: a `meta name=referrer` inserted into a document,
    /// or whose `name` or `content` changed, sets the document's referrer policy to its content's
    /// (a legacy keyword stands for its policy; an unknown or empty value changes nothing).
    pub(crate) fn meta_referrer(&self, el: NodeId) {
        let (doc, value) = {
            let dom = self.dom();
            // 1-3.
            if !dom.is_connected(el)
                || dom.is_in_shadow_tree(el)
                || !dom
                    .attr(el, &ns!(), &local_name!("name"))
                    .is_some_and(|n| n.eq_ignore_ascii_case("referrer"))
            {
                return;
            }
            let Some(content) = dom.attr(el, &ns!(), &local_name!("content")) else {
                return;
            };
            (dom.node_document(el), content.to_owned())
        };
        if let Some(policy) = meta_referrer_policy(&value) {
            self.doc_policy_mut(doc, |d| d.referrer_policy = Some(policy));
            // (the renderer's fetches, images and style sheets, take the new client)
            self.render_policy_changed(doc);
        }
    }

    /// V8 compiles strings (`eval`, `new Function`) in a realm without asking only while its
    /// document's policy has nothing to say about them.
    fn update_codegen(&self, cx: &mut Cx, doc: NodeId) {
        // (Trusted Types' `require-trusted-types-for 'script'` checks string compilation too)
        let checked = self.doc_csp(doc).is_some_and(|c| {
            !c.should_block_eval("").violations.is_empty() || {
                let (enforced, report_only) = c.requires_trusted_types_for_script();
                enforced || report_only
            }
        });
        let Some(realm) = self
            .context_of_document(doc)
            .and_then(|id| self.realm_of_context(id))
        else {
            return;
        };
        let scope = cx.scope();
        let ctx = v8::Local::new(scope, &realm);
        ctx.set_allow_generation_from_strings(!checked);
    }

    fn reset_security_token(&self, cx: &mut Cx, doc: NodeId) {
        self.document_origin_changed(cx, doc);
    }

    /// HTML §4.8.5 the `sandbox` attribute, and §7.1.5 the flags a nested document inherits: what
    /// the value of the attribute and the document of the container (`parent`) make of `doc`, a
    /// document of the frame.
    pub(crate) fn apply_frame_sandbox(&self, doc: NodeId, attribute: Option<&str>, parent: NodeId) {
        let inherited = {
            let p = self.policy.borrow();
            if p.any {
                p.docs.get(&parent).and_then(|d| d.sandbox)
            } else {
                None
            }
        };
        let tokens = attribute.map(|a| {
            a.split_ascii_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        let sandbox = match (tokens, inherited) {
            (Some(t), inherited) => Sandbox::parse(&t).and(inherited),
            (None, Some(inherited)) => inherited,
            (None, None) => return,
        };
        self.doc_policy_mut(doc, |d| {
            d.frame_sandbox = Some(sandbox);
            d.sandbox = Some(sandbox);
            if !sandbox.same_origin && d.opaque_origin.is_none() {
                d.opaque_origin = Some(url::Origin::new_opaque());
            }
        });
    }

    /// HTML §7.4.2 "allowed by sandboxing to navigate": a sandboxed document navigates its own browsing
    /// context and those below it, and the top-level one with `allow-top-navigation`.
    pub(crate) fn allowed_by_sandboxing_to_navigate(&self, source: NodeId, target: usize) -> bool {
        let sandbox = {
            let p = self.policy.borrow();
            if !p.any {
                return true;
            }
            p.docs.get(&source).and_then(|d| d.sandbox)
        };
        let Some(sandbox) = sandbox else {
            return true;
        };
        // (its browsing context also when a navigation replaced the document: code of that realm can
        // still run, under the flags it had)
        let f = self.frames.borrow();
        let Some(&own) = f.by_document.get(&source) else {
            return false;
        };
        let mut c = Some(target);
        while let Some(id) = c {
            if id == own {
                return true;
            }
            c = f.list[id].parent;
        }
        // (its own top-level traversable, not another this page hosts)
        sandbox.top_navigation && crate::engine::frames::top_of(&f, own) == target
    }

    /// HTML "sandboxed auxiliary navigation browsing context flag": a sandbox without `allow-popups`.
    pub(crate) fn auxiliary_navigation_sandboxed(&self, doc: NodeId) -> bool {
        let p = self.policy.borrow();
        p.any
            && p.docs
                .get(&doc)
                .and_then(|d| d.sandbox)
                .is_some_and(|s| !s.popups)
    }

    /// HTML "the rules for choosing a navigable", the step for a new auxiliary browsing context: the
    /// flags of `doc`'s active sandboxing flag set its popup's sandboxing flag set takes, when the set
    /// has the "sandbox propagates to auxiliary browsing contexts flag" (no
    /// `allow-popups-to-escape-sandbox`).
    pub(crate) fn sandbox_propagated_to_popups(&self, doc: NodeId) -> Option<Sandbox> {
        let p = self.policy.borrow();
        if !p.any {
            return None;
        }
        p.docs
            .get(&doc)
            .and_then(|d| d.sandbox)
            .filter(|s| !s.escape_popups)
    }

    /// HTML "determine the creation sandboxing flags" of a top-level browsing context: its popup
    /// sandboxing flag set, which each of its documents starts with (and a policy of its own can
    /// only add to).
    pub(crate) fn apply_popup_sandbox(&self, doc: NodeId, sandbox: Sandbox) {
        self.doc_policy_mut(doc, |d| {
            d.frame_sandbox = Some(sandbox);
            d.sandbox = Some(sandbox);
            if !sandbox.same_origin && d.opaque_origin.is_none() {
                d.opaque_origin = Some(url::Origin::new_opaque());
            }
        });
    }

    /// HTML "sandboxed document.domain browsing context flag": any `sandbox` sets it.
    pub(crate) fn document_domain_sandboxed(&self, doc: NodeId) -> bool {
        let p = self.policy.borrow();
        p.any && p.docs.get(&doc).is_some_and(|d| d.sandbox.is_some())
    }

    // ----- the request policy's entry point ----------------------------------------------------------

    /// The Fetch client of the document's requests (its environment settings object, as `snowlark-net`
    /// reads it): made once per document and kept until its policy or URL changes.
    pub(crate) fn request_client(&self, doc: NodeId) -> Arc<Client> {
        {
            let p = self.policy.borrow();
            if let Some((c, at)) = p.docs.get(&doc).and_then(|d| d.client.as_ref()) {
                if self.dom().document_url(doc) == at {
                    return c.clone();
                }
            }
        }
        let url = self.effective_document_url(doc);
        let origin = self.net_origin_of_document(doc);
        let mut c = Client::new(url, origin);
        c.csp = self.doc_csp(doc);
        c.referrer_policy = self
            .policy
            .borrow()
            .docs
            .get(&doc)
            .and_then(|d| d.referrer_policy)
            .unwrap_or(snowlark_net::ReferrerPolicy::Empty);
        let mut ancestor = self.parent_document(doc);
        let mut top = None;
        while let Some(a) = ancestor {
            let o = self.net_origin_of_document(a);
            if !snowlark_net::origin::same_site(&o, &c.origin) {
                c.has_cross_site_ancestor = true;
            }
            if snowlark_net::origin::is_origin_potentially_trustworthy(&o) {
                c.prohibits_mixed_security_contexts = true;
            }
            top = Some(o);
            ancestor = self.parent_document(a);
        }
        c.top_level_origin = top;
        let c = Arc::new(c);
        let at = self.dom().document_url(doc).to_owned();
        let mut p = self.policy.borrow_mut();
        let old = p
            .docs
            .entry(doc)
            .or_default()
            .client
            .replace((c.clone(), at));
        p.retire_client(old, doc);
        c
    }

    /// An observer of a fetch's events, on the network thread, that queues its CSP violations for
    /// [`Engine::report_network_violations`] (as `doc`'s).
    pub(crate) fn violation_observer(&self, doc: NodeId) -> Observer {
        let inbox = self.policy.borrow().inbox.clone();
        let waker = self.waker();
        let bits = doc.to_bits();
        Arc::new(move |e: &FetchEvent| {
            if let FetchEvent::Violation(v) = e {
                inbox.lock().unwrap().push((bits, v.clone()));
                waker.wake();
            }
        })
    }

    /// The request client and violation observer for the document's renderer's fetches.
    pub(crate) fn fetch_policy(&self, doc: NodeId) -> snowlark_render::FetchPolicy {
        let client = self.request_client(doc);
        let observer = client.csp.is_some().then(|| self.violation_observer(doc));
        snowlark_render::FetchPolicy { client, observer }
    }

    /// Starts `request` for `doc` through the request policy: it gets `doc`'s client unless it has
    /// one (a navigation's is its initiator's), and its CSP violations are reported. `None` without a
    /// network.
    pub(crate) fn policy_fetch(
        &self,
        doc: NodeId,
        mut request: Request,
        notify: Option<Arc<dyn Fn() + Send + Sync>>,
        observer: Option<Observer>,
    ) -> Option<ResponseStream> {
        let net = self.hooks.borrow().net.clone()?;
        if request.client.is_none() {
            request.client = Some(self.request_client(doc));
        }
        let reports = request
            .client
            .as_ref()
            .is_some_and(|c| c.csp.is_some())
            .then(|| self.violation_observer(doc));
        let observer: Option<Observer> = match (observer, reports) {
            (Some(a), Some(b)) => Some(Arc::new(move |e: &FetchEvent| {
                a(e);
                b(e);
            })),
            (a, b) => a.or(b),
        };
        let notify = notify.unwrap_or_else(|| Arc::new(|| {}));
        Some(match observer {
            Some(o) => net.fetch_observed(request, notify, o),
            None => net.fetch_with_notify(request, notify),
        })
    }

    /// Reports the violations the network thread queued. Returns whether there were any.
    pub(crate) fn report_network_violations(&self, cx: &mut Cx) -> bool {
        let items = {
            let p = self.policy.borrow();
            if !p.any {
                return false;
            }
            let mut inbox = p.inbox.lock().unwrap();
            if inbox.is_empty() {
                return false;
            }
            std::mem::take(&mut *inbox)
        };
        for (bits, v) in items {
            if let Some(doc) = NodeId::from_bits(bits) {
                if self.dom().is_alive(doc) {
                    self.report_violation(cx, doc, None, &v, None);
                }
            }
        }
        true
    }

    /// The renderer of `doc` takes its request client, violation observer and `<style>` check.
    pub(crate) fn render_policy_changed(&self, doc: NodeId) {
        let has_renderer = {
            let st = self.render.borrow();
            st.document == Some(doc) && st.renderer.is_some()
        } || self.frame_renders.borrow().contains_key(&doc);
        if !has_renderer {
            return;
        }
        let policy = self.fetch_policy(doc);
        let check = self.style_block_check(doc);
        let attribute_check = self.style_attribute_check(doc);
        let apply = |r: &mut snowlark_render::Renderer| {
            r.set_fetch_policy(policy.clone());
            r.set_style_block_check(check.clone());
            r.set_style_attribute_check(attribute_check.clone());
        };
        {
            let mut st = self.render.borrow_mut();
            if st.document == Some(doc) {
                if let Some(r) = st.renderer.as_mut() {
                    apply(r);
                }
            }
        }
        if let Some(st) = self.frame_renders.borrow_mut().get_mut(&doc) {
            if let Some(r) = st.renderer.as_mut() {
                apply(r);
            }
        }
    }

    /// The renderer's check of `<style>` elements for a document with a policy that restricts
    /// styles (none otherwise). It reports violations as it finds them.
    pub(crate) fn style_block_check(
        &self,
        doc: NodeId,
    ) -> Option<snowlark_render::StyleBlockCheck> {
        let csp = self.doc_csp(doc)?;
        if csp
            .should_block_inline(InlineType::Style, "", false, "")
            .violations
            .is_empty()
        {
            return None;
        }
        let pending = self.policy_pending.clone();
        let hidden = self.hidden_nonces.clone();
        Some(std::rc::Rc::new(
            move |dom: &snowlark_dom::Dom, el: NodeId| {
                let hidden = hidden.borrow();
                let nonce = match hidden.get(&el) {
                    Some(n) => n.as_str(),
                    None => dom.attr(el, &ns!(), &local_name!("nonce")).unwrap_or(""),
                };
                let nonceable = snowlark_net::csp::is_nonceable(
                    dom.attrs(el).iter().map(|a| (&**a.local_name(), a.value())),
                    false,
                );
                let text = dom.child_text_content(el);
                let verdict = csp.should_block_inline(InlineType::Style, nonce, nonceable, &text);
                let blocked = verdict.blocked;
                if !verdict.violations.is_empty() {
                    pending
                        .borrow_mut()
                        .push((el, InlineType::Style, text, verdict.violations));
                }
                !blocked
            },
        ))
    }

    /// The style engine's check of `style` attributes for a document with a policy that restricts
    /// them (none otherwise). Violations are queued for the page thread to report.
    fn style_attribute_check(&self, doc: NodeId) -> Option<snowlark_style::StyleAttributeCheck> {
        let csp = self.doc_csp(doc)?;
        if csp
            .should_block_inline(InlineType::StyleAttribute, "", false, "")
            .violations
            .is_empty()
        {
            return None;
        }
        let (pending, cssom) = {
            let p = self.policy.borrow();
            (p.style_pending.clone(), p.cssom_styles.clone())
        };
        Some(Arc::new(move |el: NodeId, css: &str| {
            if cssom
                .lock()
                .unwrap()
                .contains(&(el.to_bits(), hash_of(css)))
            {
                return true;
            }
            let verdict = csp.should_block_inline(InlineType::StyleAttribute, "", false, css);
            if !verdict.violations.is_empty() {
                pending
                    .lock()
                    .unwrap()
                    .push((el.to_bits(), css.to_owned(), verdict.violations));
            }
            !verdict.blocked
        }))
    }

    /// Cloning steps: a copy keeps the original's `[[CryptographicNonce]]` (HTML), and a `style`
    /// attribute the CSSOM wrote stays exempt from `style-src-attr` in the copy.
    pub(crate) fn csp_cloning_steps(&self, node: NodeId, copy: NodeId) {
        let hidden = self.hidden_nonces.borrow().get(&node).cloned();
        if let Some(n) = hidden {
            self.hidden_nonces.borrow_mut().insert(copy, n);
        }
        let css = self
            .dom()
            .attr(node, &ns!(), &local_name!("style"))
            .map(str::to_owned);
        if let Some(css) = css {
            let set = self.policy.borrow().cssom_styles.clone();
            let mut set = set.lock().unwrap();
            let h = hash_of(&css);
            if set.contains(&(node.to_bits(), h)) {
                set.insert((copy.to_bits(), h));
            }
        }
    }

    /// The CSSOM wrote `css` to `el`'s `style` attribute (exempt from `style-src-attr`).
    pub(crate) fn cssom_style_written(&self, el: NodeId, css: &str) {
        let set = self.policy.borrow().cssom_styles.clone();
        set.lock().unwrap().insert((el.to_bits(), hash_of(css)));
    }

    /// Reports the violations of checks that ran where no engine borrow was possible (the renderer's
    /// `<style>` and `style` attribute checks), and of the network thread.
    pub(crate) fn report_pending_violations(&self, cx: &mut Cx) -> bool {
        let mut any = self.report_network_violations(cx);
        let styles = {
            let p = self.policy.borrow();
            if p.any {
                std::mem::take(&mut *p.style_pending.lock().unwrap())
            } else {
                Vec::new()
            }
        };
        for (bits, css, violations) in styles {
            if let Some(el) = NodeId::from_bits(bits) {
                self.policy_pending.borrow_mut().push((
                    el,
                    InlineType::StyleAttribute,
                    css,
                    violations,
                ));
            }
        }
        let pending = std::mem::take(&mut *self.policy_pending.borrow_mut());
        for (el, kind, text, violations) in pending {
            if !self.dom().is_alive(el) {
                continue;
            }
            if !self.first_inline_report(el, kind, &text) {
                continue;
            }
            let doc = self.dom().node_document(el);
            for v in violations {
                self.report_violation(cx, doc, Some(el), &v, None);
            }
            any = true;
        }
        any
    }

    fn first_inline_report(&self, el: NodeId, kind: InlineType, text: &str) -> bool {
        let k = match kind {
            InlineType::Script => 0,
            InlineType::ScriptAttribute => 1,
            InlineType::Style => 2,
            InlineType::StyleAttribute => 3,
            InlineType::Navigation => 4,
        };
        self.policy
            .borrow_mut()
            .inline_seen
            .insert((el, k, hash_of(text)))
    }

    // ----- nonces and `<base>` -----------------------------------------------------------------------

    /// HTML's nonce hiding: an element with a `nonce` that becomes connected to a document whose CSP
    /// list has a header-delivered policy keeps the nonce in its `[[CryptographicNonce]]` slot and
    /// gets an empty `nonce` attribute (so that selectors and attribute reads can't exfiltrate it).
    pub(crate) fn hide_nonce(&self, el: NodeId) {
        let (doc, nonce) = {
            let dom = self.dom();
            let ns = dom.namespace(el);
            if *ns != ns!(html) && *ns != ns!(svg) && *ns != ns!(mathml) {
                return;
            }
            let Some(n) = dom.attr(el, &ns!(), &local_name!("nonce")) else {
                return;
            };
            if n.is_empty() {
                return;
            }
            (dom.node_document(el), n.to_owned())
        };
        if !self.doc_csp(doc).is_some_and(|c| c.has_header_policy()) {
            return;
        }
        self.hidden_nonces.borrow_mut().insert(el, nonce);
        self.dom_mut().set_attribute_value(
            el,
            snowlark_dom::LocalName::from("nonce"),
            ns!(),
            None,
            "",
        );
    }

    /// CSP §6.3.1 `base-uri` for a `<base href>` that is or becomes connected: the violation is
    /// reported (at the element, once per URL); [`Engine::base_uri_blocked`] keeps the document's base
    /// URL at its fallback.
    pub(crate) fn base_inserted(&self, cx: &mut Cx, el: NodeId) {
        let (doc, href) = {
            let dom = self.dom();
            let Some(href) = dom.attr(el, &ns!(), &local_name!("href")) else {
                return;
            };
            (dom.node_document(el), href.to_owned())
        };
        let Some(csp) = self.doc_csp(doc) else {
            return;
        };
        let Some(url) = url::Url::parse(self.dom().document_url(doc))
            .ok()
            .and_then(|f| f.join(&href).ok())
        else {
            return;
        };
        let verdict = csp.should_block_base_uri(&url);
        if !verdict.violations.is_empty()
            && self.first_inline_report(el, InlineType::Navigation, url.as_str())
        {
            for v in &verdict.violations {
                self.report_violation(cx, doc, Some(el), v, None);
            }
        }
    }

    /// Whether `doc`'s policy keeps `<base>` from making `url` its base URL (HTML "set the frozen
    /// base URL" step 2).
    pub(crate) fn base_uri_blocked(&self, doc: NodeId, url: &url::Url) -> bool {
        self.doc_csp(doc)
            .is_some_and(|c| c.should_block_base_uri(url).blocked)
    }

    // ----- inline checks -----------------------------------------------------------------------------

    /// The element's `[[CryptographicNonce]]` (the `nonce` attribute, unless the slot was kept apart
    /// from it: hidden, or set through the IDL attribute).
    pub(crate) fn nonce_of(&self, el: NodeId) -> String {
        if let Some(n) = self.hidden_nonces.borrow().get(&el) {
            return n.clone();
        }
        self.dom()
            .attr(el, &ns!(), &local_name!("nonce"))
            .unwrap_or("")
            .to_owned()
    }

    /// CSP §4.2.3 "should element's inline type behavior be blocked by Content Security Policy?",
    /// reporting the violations at the element. `el` is the element (for a `javascript:` URL, the
    /// hyperlink; its document's policy applies).
    pub(crate) fn inline_blocked(
        &self,
        cx: &mut Cx,
        el: NodeId,
        kind: InlineType,
        source: &str,
    ) -> bool {
        let doc = self.dom().node_document(el);
        let Some(csp) = self.doc_csp(doc) else {
            return false;
        };
        let (nonce, nonceable) = match kind {
            InlineType::Script | InlineType::Style => {
                let dom = self.dom();
                let nonceable = snowlark_net::csp::is_nonceable(
                    dom.attrs(el).iter().map(|a| (&**a.local_name(), a.value())),
                    false,
                );
                drop(dom);
                (self.nonce_of(el), nonceable)
            }
            _ => (String::new(), false),
        };
        let verdict = if kind == InlineType::Script {
            let parser_inserted = self.script_parser_inserted(el);
            csp.should_block_inline_script(&nonce, nonceable, source, parser_inserted)
        } else {
            csp.should_block_inline(kind, &nonce, nonceable, source)
        };
        if !verdict.violations.is_empty() && self.first_inline_report(el, kind, source) {
            let location = self.script_location(cx);
            for v in &verdict.violations {
                self.report_violation(cx, doc, Some(el), v, location.clone());
            }
        }
        verdict.blocked
    }

    /// CSP §4.4 EnsureCSPDoesNotBlockStringCompilation for `doc`'s realm: whether `source` may be
    /// compiled (reporting the violations).
    pub(crate) fn eval_blocked(&self, cx: &mut Cx, doc: NodeId, source: &str) -> bool {
        let Some(csp) = self.doc_csp(doc) else {
            return false;
        };
        let verdict = csp.should_block_eval(source);
        self.report_verdict(cx, doc, &verdict);
        verdict.blocked
    }

    /// CSP §6.4.1 `form-action` for a submission to `url` that is not fetched (a `javascript:` URL).
    pub(crate) fn form_action_blocked(&self, cx: &mut Cx, doc: NodeId, url: &url::Url) -> bool {
        let Some(csp) = self.doc_csp(doc) else {
            return false;
        };
        let verdict = csp.should_block_form_action(url, 0);
        self.report_verdict(cx, doc, &verdict);
        verdict.blocked
    }

    pub(crate) fn report_verdict(&self, cx: &mut Cx, doc: NodeId, verdict: &Verdict) {
        if verdict.violations.is_empty() {
            return;
        }
        let location = self.script_location(cx);
        for v in &verdict.violations {
            self.report_violation(cx, doc, None, v, location.clone());
        }
    }

    /// Checks a request's URL against `doc`'s policy without fetching (a worker's script before its
    /// thread starts, a plugin's URL): reports the violations, returns whether an enforced policy
    /// blocks it.
    pub(crate) fn request_blocked(&self, cx: &mut Cx, doc: NodeId, request: &Request) -> bool {
        let Some(csp) = self.doc_csp(doc) else {
            return false;
        };
        let verdict = csp.check_request(&RequestInfo::from_request(request));
        for v in &verdict.violations {
            self.report_violation(cx, doc, None, v, None);
        }
        verdict.blocked
    }

    /// The document of the current realm (a worker's for a worker engine).
    pub(crate) fn current_global_document(&self, cx: &mut Cx) -> Option<NodeId> {
        self.current_window(cx)
            .and_then(|w| self.associated_document(w))
    }

    /// The script running now: its URL, line and column (1-based), for violation reports.
    fn script_location(&self, cx: &mut Cx) -> Option<(String, u32, u32)> {
        let scope = cx.scope();
        let trace = v8::StackTrace::current_stack_trace(scope, 1)?;
        if trace.get_frame_count() == 0 {
            return None;
        }
        let frame = trace.get_frame(scope, 0)?;
        let file = frame.get_script_name_or_source_url(scope)?;
        let file = file.to_rust_string_lossy(scope);
        if file.is_empty() {
            return None;
        }
        Some((
            file,
            frame.get_line_number() as u32,
            frame.get_column() as u32,
        ))
    }

    // ----- frames --------------------------------------------------------------------------------------

    /// The origins of the active documents of child navigable `id`'s ancestors, nearest first
    /// (`Request::frame_ancestors`: the network layer checks the response's `X-Frame-Options`
    /// and `frame-ancestors` against them); `None` for a top-level navigable.
    pub(crate) fn frame_ancestor_origins(&self, id: usize) -> Option<Vec<url::Origin>> {
        let parent = {
            let f = self.frames.borrow();
            f.list
                .get(id)
                .and_then(|b| b.parent)
                .map(|p| f.list[p].document)
        }?;
        let mut ancestors = Vec::new();
        let mut d = Some(parent);
        while let Some(doc) = d {
            ancestors.push(self.net_origin_of_document(doc));
            d = self.parent_document(doc);
        }
        Some(ancestors)
    }

    /// The document a blocked frame navigation leaves: empty, with an opaque origin and no scripts.
    pub(crate) fn mark_blocked_document(&self, cx: &mut Cx, doc: NodeId) {
        self.doc_policy_mut(doc, |d| {
            d.opaque_origin = Some(url::Origin::new_opaque());
            d.sandbox = Some(Sandbox {
                scripts: false,
                same_origin: false,
                forms: false,
                top_navigation: false,
                popups: false,
                escape_popups: false,
            });
        });
        self.reset_security_token(cx, doc);
    }

    // ----- reporting -----------------------------------------------------------------------------------

    /// CSP §5.3 "report a violation" of `doc`'s policy: the console, the `securitypolicyviolation`
    /// event (at `element` when it is still connected to `doc`, else at the document or the worker's
    /// global), and the reports.
    pub(crate) fn report_violation(
        &self,
        cx: &mut Cx,
        doc: NodeId,
        element: Option<NodeId>,
        v: &Violation,
        source: Option<(String, u32, u32)>,
    ) {
        let _ = cx;
        self.devtools_note_realm(doc);
        self.console_print("error", &v.console_message());
        let url = self.effective_document_url(doc);
        let doc_url = url::Url::parse(self.dom().document_url(doc)).unwrap_or_else(|_| url.clone());
        let referrer = self.doc_state(doc, |s| s.referrer.clone());
        let status = {
            let p = self.policy.borrow();
            p.docs.get(&doc).map_or(0, |d| d.status)
        };
        let status = if status == 0 && matches!(doc_url.scheme(), "http" | "https") {
            200
        } else {
            status
        };
        let report = ViolationReport::new(
            v,
            &doc_url,
            &referrer,
            status,
            source.as_ref().map(|(f, l, c)| (f.as_str(), *l, *c)),
        );
        // The event, as a task.
        let worker = self.worker_scope.borrow().is_some();
        let target = if worker {
            self.window()
        } else {
            let el = element.filter(|&el| {
                let dom = self.dom();
                dom.is_alive(el) && dom.is_connected(el) && dom.node_document(el) == doc
            });
            Some(node_handle(el.unwrap_or(doc)))
        };
        if let Some(target) = target {
            let fields = Box::new(report.clone());
            self.queue_task(TaskSource::DomManipulation, move |e, cx| {
                if !e.dom().is_alive(doc) {
                    return;
                }
                e.in_document_realm(cx, doc, |cx| {
                    let ev = e.trusted_event(
                        InterfaceId::SecurityPolicyViolationEvent,
                        "securitypolicyviolation",
                        true,
                        false,
                        EventExt::Csp(fields),
                    );
                    e.ev_mut(ev, |d| d.composed = true);
                    e.dispatch(cx, ev, target, false);
                });
            });
        }
        // The reports.
        let (list, endpoints) = {
            let p = self.policy.borrow();
            match p.docs.get(&doc) {
                Some(d) => (d.csp.clone(), d.endpoints.clone()),
                None => (None, Vec::new()),
            }
        };
        if let Some(list) = list {
            self.send_reports(doc, &list, v, &report, &endpoints, &url);
        }
    }

    /// Sends `report` to where `list`'s violated policy says, from `doc` (each distinct report once).
    fn send_reports(
        &self,
        doc: NodeId,
        list: &CspList,
        v: &Violation,
        report: &ViolationReport,
        endpoints: &[(String, url::Url)],
        base: &url::Url,
    ) {
        let destinations = snowlark_net::report::report_destinations(list, v, endpoints, base);
        if destinations.is_empty() {
            return;
        }
        for (url, reporting_api) in destinations {
            let (body, content_type) = if reporting_api {
                (
                    report.reports_json(&self.user_agent_string()),
                    "application/reports+json",
                )
            } else {
                (report.csp_report_json(), "application/csp-report")
            };
            let key = hash_of(&format!("{url} {body}"));
            if !self.policy.borrow_mut().sent.insert(key) {
                continue;
            }
            let request = snowlark_net::report::report_request(
                url,
                body,
                content_type,
                self.request_client(doc),
            );
            self.start_api_fetch(doc, request, crate::engine::fetch::JobOwner::Beacon);
        }
    }
}

// ----- SecurityPolicyViolationEvent --------------------------------------------------------------------

impl Engine {
    fn csp_field<R>(&self, this: Handle, f: impl FnOnce(&ViolationReport) -> R, or: R) -> R {
        self.ev(this, |e| match &e.ext {
            EventExt::Csp(r) => f(r),
            _ => or,
        })
    }

    fn csp_str(&self, this: Handle, f: impl FnOnce(&ViolationReport) -> &str) -> DomStr<'_> {
        DomStr::Owned(self.csp_field(this, |r| f(r).to_owned(), String::new()))
    }
}

impl SecurityPolicyViolationEventImpl for Engine {
    fn document_uri(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.document_url)
    }
    fn referrer(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.referrer)
    }
    fn blocked_uri(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.blocked_url)
    }
    fn effective_directive(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.effective_directive)
    }
    fn violated_directive(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.effective_directive)
    }
    fn original_policy(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.original_policy)
    }
    fn source_file(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.source_file)
    }
    fn sample(&self, this: Handle) -> DomStr<'_> {
        self.csp_str(this, |r| &r.sample)
    }
    fn disposition(&self, this: Handle) -> SecurityPolicyViolationEventDisposition {
        self.csp_field(
            this,
            |r| match r.disposition {
                Disposition::Enforce => SecurityPolicyViolationEventDisposition::Enforce,
                Disposition::Report => SecurityPolicyViolationEventDisposition::Report,
            },
            SecurityPolicyViolationEventDisposition::Enforce,
        )
    }
    fn status_code(&self, this: Handle) -> u16 {
        self.csp_field(this, |r| r.status_code, 0)
    }
    fn line_number(&self, this: Handle) -> u32 {
        self.csp_field(this, |r| r.line_number, 0)
    }
    fn column_number(&self, this: Handle) -> u32 {
        self.csp_field(this, |r| r.column_number, 0)
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        type_: &str,
        init: SecurityPolicyViolationEventInit,
    ) -> rt::Result<Handle> {
        let report = ViolationReport {
            document_url: init.document_uri,
            referrer: init.referrer,
            blocked_url: init.blocked_uri,
            effective_directive: init.effective_directive,
            original_policy: init.original_policy,
            source_file: init.source_file,
            sample: init.sample,
            disposition: match init.disposition {
                SecurityPolicyViolationEventDisposition::Enforce => Disposition::Enforce,
                SecurityPolicyViolationEventDisposition::Report => Disposition::Report,
            },
            status_code: init.status_code,
            line_number: init.line_number,
            column_number: init.column_number,
        };
        Ok(self.new_event(
            InterfaceId::SecurityPolicyViolationEvent,
            type_,
            init.bubbles,
            init.cancelable,
            init.composed,
            EventExt::Csp(Box::new(report)),
        ))
    }
}

// ----- the V8 hooks --------------------------------------------------------------------------------------

/// `eval()` and `new Function()` in a realm whose document's policy restricts them (installed with
/// `snowlark_js::hooks::set_codegen_hook`).
pub fn codegen_hook<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<'s, v8::Value>,
    is_code_like: bool,
) -> snowlark_js::hooks::Codegen<'s> {
    use snowlark_js::hooks::Codegen;
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    // A TrustedScript is "code like": its data is compiled, as a string would be. V8 hands each one
    // over as it is (`eval`, and each `Function` constructor argument), then a `Function`'s source
    // made only of them as a string marked code like: HTML's EnsureCSPDoesNotBlockStringCompilation
    // counts both as trusted (every parameter and the body a TrustedScript).
    let trusted = v8::Local::<v8::Object>::try_from(source)
        .ok()
        .and_then(|o| rt::unwrap(&o, ids::TRUSTED_SCRIPT, ids::TRUSTED_SCRIPT_END))
        .and_then(|h| engine.trusted_script_data(h));
    let text = match &trusted {
        Some(data) => data.clone(),
        None if source.is_string() => source.to_rust_string_lossy(scope),
        None => return Codegen::Allow,
    };
    let compiled = match &trusted {
        Some(data) => match v8::String::new(scope, data) {
            Some(s) => Some(s),
            None => return Codegen::Block,
        },
        None => None,
    };
    let mut cx = Cx::new(scope);
    let allowed = match engine.current_global_document(&mut cx) {
        // Trusted Types first (HTML EnsureCSPDoesNotBlockStringCompilation), then script-src.
        Some(doc) => {
            (trusted.is_some()
                || (is_code_like && source.is_string())
                || engine.tt_eval_allowed(&mut cx, doc, &text))
                && !engine.eval_blocked(&mut cx, doc, &text)
        }
        // (a realm with no document has no policy to ask: nothing compiles)
        None => false,
    };
    match (allowed, compiled) {
        (false, _) => Codegen::Block,
        (true, Some(s)) => Codegen::Compile(s),
        (true, None) => Codegen::Allow,
    }
}

/// WebAssembly compilation (installed with `snowlark_js::hooks::set_wasm_codegen_hook`).
pub fn wasm_hook(scope: &mut v8::PinScope<'_, '_>) -> bool {
    let st = rt::state(scope);
    let engine = st.engine::<Engine>();
    let mut cx = Cx::new(scope);
    let Some(doc) = engine.current_global_document(&mut cx) else {
        return false;
    };
    let Some(csp) = engine.doc_csp(doc) else {
        return true;
    };
    let verdict = csp.should_block_wasm();
    engine.report_verdict(&mut cx, doc, &verdict);
    !verdict.blocked
}

/// The style engine's `style` attribute violations: (element id bits, value, violations).
type PendingStyleAttributes = Vec<(u64, String, Vec<Violation>)>;

/// The (element, kind, source, violations) of inline checks waiting to be reported.
pub(crate) type PendingInline = Vec<(NodeId, InlineType, String, Vec<Violation>)>;
