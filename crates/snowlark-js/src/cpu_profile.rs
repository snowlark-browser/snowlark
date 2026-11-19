// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! V8's sampling CPU profiler, for `SNOWLARK_PROFILE=1`.
//!
//! The `v8` crate does not bind `v8::CpuProfiler` (only `CollectSample`), so the profiler is driven
//! the way DevTools drives it: through an inspector session on the isolate, with the protocol's
//! `Profiler.start` / `Profiler.stop`. The profile that comes back (a call tree with samples) is
//! summarized as self and total time per function and self time per script.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write;
use std::rc::Rc;

use v8::inspector::{
    Channel, ChannelImpl, StringBuffer, StringView, V8Inspector, V8InspectorClient,
    V8InspectorClientImpl, V8InspectorClientTrustLevel, V8InspectorSession,
};
use v8::UniquePtr;

struct Client;

impl V8InspectorClientImpl for Client {}

/// Collects the session's responses by call id.
struct Responses(Rc<RefCell<HashMap<i32, String>>>);

impl ChannelImpl for Responses {
    fn send_response(&self, call_id: i32, message: UniquePtr<StringBuffer>) {
        if let Some(m) = message.as_ref() {
            self.0.borrow_mut().insert(call_id, m.string().to_string());
        }
    }
    fn send_notification(&self, _message: UniquePtr<StringBuffer>) {}
    fn flush_protocol_notifications(&self) {}
}

/// A running profile of one isolate. Stop it (or drop it) before the isolate goes.
pub struct CpuProfiler {
    // The session is dropped before the inspector (field order).
    session: V8InspectorSession,
    _inspector: V8Inspector,
    responses: Rc<RefCell<HashMap<i32, String>>>,
    next_id: i32,
}

impl CpuProfiler {
    /// Starts sampling `isolate` every `interval_us` microseconds.
    pub fn start(isolate: &mut v8::Isolate, interval_us: u32) -> CpuProfiler {
        let inspector = V8Inspector::create(isolate, V8InspectorClient::new(Box::new(Client)));
        let responses = Rc::new(RefCell::new(HashMap::new()));
        let channel = Channel::new(Box::new(Responses(responses.clone())));
        let session = inspector.connect(
            1,
            channel,
            StringView::empty(),
            V8InspectorClientTrustLevel::FullyTrusted,
        );
        let mut p = CpuProfiler {
            session,
            _inspector: inspector,
            responses,
            next_id: 1,
        };
        p.call("Profiler.enable", "{}");
        p.call(
            "Profiler.setSamplingInterval",
            &format!("{{\"interval\":{interval_us}}}"),
        );
        p.call("Profiler.start", "{}");
        p
    }

    /// Sends a protocol command and returns its response (the inspector answers the profiler's
    /// commands synchronously).
    fn call(&mut self, method: &str, params: &str) -> Option<String> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = format!("{{\"id\":{id},\"method\":\"{method}\",\"params\":{params}}}");
        self.session
            .dispatch_protocol_message(StringView::from(msg.as_bytes()));
        self.responses.borrow_mut().remove(&id)
    }

    /// Stops sampling and returns the profile (the protocol's `Profiler.Profile` JSON).
    pub fn stop(mut self) -> Option<serde_json::Value> {
        let r = self.call("Profiler.stop", "{}")?;
        let mut v: serde_json::Value = serde_json::from_str(&r).ok()?;
        Some(v.get_mut("result")?.get_mut("profile")?.take())
    }
}

/// One function's share of a profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FunctionTime {
    /// `name (url:line)`.
    pub function: String,
    pub self_ms: f64,
    pub total_ms: f64,
}

/// What a profile adds up to.
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub total_ms: f64,
    /// By function, most self time first.
    pub functions: Vec<FunctionTime>,
    /// Self time by script URL, most first.
    pub scripts: Vec<(String, f64)>,
}

/// Self and total time per function out of a `Profiler.Profile`: each sample's time is the delta to
/// the next sample; a function's total counts each sample once even when it recurses.
pub fn summarize(profile: &serde_json::Value) -> Summary {
    let empty = Vec::new();
    let nodes = profile["nodes"].as_array().unwrap_or(&empty);
    let samples = profile["samples"].as_array().unwrap_or(&empty);
    let deltas = profile["timeDeltas"].as_array().unwrap_or(&empty);
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut parent: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut keys: Vec<String> = Vec::with_capacity(nodes.len());
    let mut urls: Vec<String> = Vec::with_capacity(nodes.len());
    for (i, n) in nodes.iter().enumerate() {
        index.insert(n["id"].as_i64().unwrap_or(-1), i);
        let cf = &n["callFrame"];
        let name = cf["functionName"].as_str().unwrap_or("");
        let url = cf["url"].as_str().unwrap_or("");
        let line = cf["lineNumber"].as_i64().unwrap_or(-1);
        let name = if name.is_empty() { "(anonymous)" } else { name };
        keys.push(if url.is_empty() {
            name.to_string()
        } else {
            let file = url.rsplit('/').next().unwrap_or(url);
            format!("{name} ({file}:{})", line + 1)
        });
        urls.push(if url.is_empty() {
            name.to_string()
        } else {
            url.to_string()
        });
    }
    for (i, n) in nodes.iter().enumerate() {
        for c in n["children"].as_array().unwrap_or(&empty) {
            if let Some(&ci) = c.as_i64().and_then(|c| index.get(&c)) {
                parent[ci] = Some(i);
            }
        }
    }
    let mut node_ms = vec![0.0f64; nodes.len()];
    let mut total = 0.0;
    for (k, s) in samples.iter().enumerate() {
        let Some(&i) = s.as_i64().and_then(|s| index.get(&s)) else {
            continue;
        };
        let ms = deltas
            .get(k + 1)
            .and_then(|d| d.as_f64())
            .unwrap_or(0.0)
            .max(0.0)
            / 1000.0;
        node_ms[i] += ms;
        total += ms;
    }
    let mut funcs: HashMap<&str, (f64, f64)> = HashMap::new();
    let mut scripts: HashMap<&str, f64> = HashMap::new();
    for (i, &ms) in node_ms.iter().enumerate() {
        if ms == 0.0 {
            continue;
        }
        funcs.entry(&keys[i]).or_default().0 += ms;
        *scripts.entry(&urls[i]).or_default() += ms;
        // Total: this node's time goes to each distinct function on its stack.
        let mut seen: Vec<&str> = Vec::new();
        let mut at = Some(i);
        while let Some(n) = at {
            let k = keys[n].as_str();
            if !seen.contains(&k) {
                seen.push(k);
                funcs.entry(k).or_default().1 += ms;
            }
            at = parent[n];
        }
    }
    let mut functions: Vec<FunctionTime> = funcs
        .into_iter()
        .map(|(f, (s, t))| FunctionTime {
            function: f.to_string(),
            self_ms: s,
            total_ms: t,
        })
        .collect();
    functions.sort_by(|a, b| b.self_ms.total_cmp(&a.self_ms));
    let mut scripts: Vec<(String, f64)> = scripts
        .into_iter()
        .map(|(u, t)| (u.to_string(), t))
        .collect();
    scripts.sort_by(|a, b| b.1.total_cmp(&a.1));
    Summary {
        total_ms: total,
        functions,
        scripts,
    }
}

impl Summary {
    /// The `top` functions by self time, the `top` by total time and the scripts, as `[profile]`
    /// lines.
    pub fn report(&self, top: usize) -> String {
        let mut o = String::new();
        let _ = writeln!(o, "[profile] js: {:.1} ms sampled", self.total_ms);
        let _ = writeln!(
            o,
            "[profile] {:>10} {:>10}  function (by self)",
            "self ms", "total ms"
        );
        for f in self.functions.iter().take(top) {
            let _ = writeln!(
                o,
                "[profile] {:>10.1} {:>10.1}  {}",
                f.self_ms, f.total_ms, f.function
            );
        }
        let mut by_total: Vec<&FunctionTime> = self.functions.iter().collect();
        by_total.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
        let _ = writeln!(
            o,
            "[profile] {:>10} {:>10}  function (by total)",
            "self ms", "total ms"
        );
        for f in by_total.iter().take(top) {
            let _ = writeln!(
                o,
                "[profile] {:>10.1} {:>10.1}  {}",
                f.self_ms, f.total_ms, f.function
            );
        }
        let _ = writeln!(o, "[profile] {:>10}  script (self)", "ms");
        for (u, t) in self.scripts.iter().take(top) {
            let _ = writeln!(o, "[profile] {t:>10.1}  {u}");
        }
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_self_and_total_time() {
        let p = serde_json::json!({
            "nodes": [
                {"id": 1, "callFrame": {"functionName": "(root)", "url": "", "lineNumber": -1}, "children": [2]},
                {"id": 2, "callFrame": {"functionName": "outer", "url": "http://x/a.js", "lineNumber": 0}, "children": [3]},
                {"id": 3, "callFrame": {"functionName": "outer", "url": "http://x/a.js", "lineNumber": 0}, "children": [4]},
                {"id": 4, "callFrame": {"functionName": "", "url": "http://x/b.js", "lineNumber": 9}, "children": []}
            ],
            "samples": [2, 4, 4, 3],
            "timeDeltas": [0, 1000, 2000, 3000]
        });
        let s = summarize(&p);
        // Sample k lasts until sample k + 1: 1 ms at node 2, 2 + 3 ms at node 4, 0 at node 3.
        assert_eq!(s.total_ms, 6.0);
        let f = |n: &str| {
            s.functions
                .iter()
                .find(|f| f.function == n)
                .unwrap()
                .clone()
        };
        assert_eq!(f("(anonymous) (b.js:10)").self_ms, 5.0);
        let outer = f("outer (a.js:1)");
        assert_eq!((outer.self_ms, outer.total_ms), (1.0, 6.0));
        assert_eq!(s.scripts[0], ("http://x/b.js".to_string(), 5.0));
        assert!(s.report(5).contains("outer (a.js:1)"));
    }

    #[test]
    fn profiles_a_running_isolate() {
        crate::init();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let profiler = CpuProfiler::start(&mut isolate, 100);
        {
            v8::scope!(let scope, &mut isolate);
            let context = v8::Context::new(scope, Default::default());
            let scope = &mut v8::ContextScope::new(scope, context);
            let code = v8::String::new(
                scope,
                "function spin(){let x=0;for(let i=0;i<3e7;i++)x+=i%7;return x}spin()",
            )
            .unwrap();
            v8::Script::compile(scope, code, None)
                .unwrap()
                .run(scope)
                .unwrap();
        }
        let profile = profiler.stop().expect("a profile");
        let s = summarize(&profile);
        assert!(s.total_ms > 0.0, "{profile}");
        assert!(s.functions.iter().any(|f| f.function.starts_with("spin")));
    }
}
