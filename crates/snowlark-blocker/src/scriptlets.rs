// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! uBlock Origin's resources, fetched from its repository at run time (GPL-3.0; never bundled):
//!
//! - its scriptlets: `src/js/resources/scriptlets.js` and the modules it imports. They are ES
//!   modules that register each scriptlet's function in a table (`builtinScriptlets`); the table is
//!   read by evaluating the modules in an isolate of their own with nothing but ECMAScript
//!   (`snowlark_js::module_graph`), each function's source being its `toString()`, as uBlock Origin
//!   itself serializes them for injection. Each becomes one of adblock-rust's `Resource`s (helpers
//!   named `*.fn` as `fn/javascript` dependencies, `requiresTrust` ones only for trusted lists).
//! - its redirect resources: `src/js/redirect-resources.js` (read the same way) names the files of
//!   `src/web_accessible_resources/` and their aliases.
//!
//! Every file is kept under `<blocker>/ubo/` and revalidated with its ETag; the set is checked weekly
//! (uBlock Origin changes them with its releases). The assembled resources are `resources.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use adblock::resources::{MimeType, PermissionMask, Resource, ResourceType};
use base64::Engine as _;
use snowlark_net::NetClient;

use crate::lists::{ListState, ListStore};
use crate::updater::{download, Download};
use crate::ContentBlocker;

/// Where uBlock Origin's sources are fetched from.
pub const REPOSITORY: &str = "https://raw.githubusercontent.com/gorhill/uBlock/master/";
const SCRIPTLETS: &str = "src/js/resources/scriptlets.js";
const REDIRECTS: &str = "src/js/redirect-resources.js";
const WAR_DIR: &str = "src/web_accessible_resources/";
/// The id of the resources in the list store's states.
pub const STATE_ID: &str = "ubo-resources";
/// How often the resources are checked.
const INTERVAL: u64 = 7 * 86_400;

/// The files' copies and ETags.
struct FileCache {
    base: String,
    dir: PathBuf,
    etags: BTreeMap<String, String>,
    fetched: usize,
    revalidated: usize,
}

impl FileCache {
    fn open(blocker_dir: &Path, base: String) -> FileCache {
        let dir = blocker_dir.join("ubo");
        let etags = std::fs::read(dir.join("etags.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        FileCache {
            base,
            dir,
            etags,
            fetched: 0,
            revalidated: 0,
        }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.join(file.replace('/', "_"))
    }

    /// The file's bytes: the cached copy if the server says it is current.
    fn get(&mut self, net: &NetClient, file: &str) -> Result<Vec<u8>, String> {
        let path = self.path(file);
        let cached = std::fs::read(&path).ok();
        let etag = cached
            .as_ref()
            .and(self.etags.get(file).map(String::as_str));
        match download(net, &format!("{}{file}", self.base), etag, None)? {
            Download::NotModified => {
                self.revalidated += 1;
                cached.ok_or_else(|| format!("{file}: 304 without a copy"))
            }
            Download::Body { bytes, etag, .. } => {
                self.fetched += 1;
                let _ = std::fs::create_dir_all(&self.dir);
                std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
                match etag {
                    Some(e) => self.etags.insert(file.to_owned(), e),
                    None => self.etags.remove(file),
                };
                Ok(bytes)
            }
        }
    }

    fn save(&self) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(
            self.dir.join("etags.json"),
            serde_json::to_vec_pretty(&self.etags).unwrap_or_default(),
        );
    }
}

/// The relative imports of a module's source (`import ... from './x.js'`, `import './y.js'`).
pub fn imports(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    for kw in ["from", "import"] {
        let mut at = 0;
        while let Some(i) = source[at..].find(kw) {
            let start = at + i + kw.len();
            at = start;
            let rest = source[start..].trim_start();
            let Some(q) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else {
                continue;
            };
            let body = &rest[1..];
            let Some(end) = body.find(q) else {
                continue;
            };
            let spec = &body[..end];
            let boundary = at - kw.len() == 0 || !bytes[at - kw.len() - 1].is_ascii_alphanumeric();
            if boundary
                && (spec.starts_with("./") || spec.starts_with("../"))
                && !out.contains(&spec.to_owned())
            {
                out.push(spec.to_owned());
            }
        }
    }
    out
}

/// Fetches a module and the modules it imports.
fn fetch_graph(
    net: &NetClient,
    cache: &mut FileCache,
    entry: &str,
) -> Result<HashMap<String, String>, String> {
    let mut sources = HashMap::new();
    let mut queue = vec![entry.to_owned()];
    while let Some(file) = queue.pop() {
        if sources.contains_key(&file) {
            continue;
        }
        if sources.len() > 200 {
            return Err("too many modules".into());
        }
        let text = String::from_utf8_lossy(&cache.get(net, &file)?).into_owned();
        for spec in imports(&text) {
            let dep = join(&file, &spec);
            if !sources.contains_key(&dep) {
                queue.push(dep);
            }
        }
        sources.insert(file, text);
    }
    Ok(sources)
}

fn join(referrer: &str, spec: &str) -> String {
    let mut parts: Vec<&str> = referrer.split('/').collect();
    parts.pop();
    for p in spec.split('/') {
        match p {
            "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

#[derive(serde::Deserialize)]
struct ScriptletEntry {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    source: String,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    trusted: bool,
}

/// The scriptlet table of uBlock Origin's evaluated modules, as resources.
pub fn scriptlet_resources(sources: HashMap<String, String>) -> Result<Vec<Resource>, String> {
    let json = snowlark_js::module_graph::eval_module_graph(
        sources,
        SCRIPTLETS,
        "ns => JSON.stringify(ns.builtinScriptlets.map(s => ({ name: s.name, \
         aliases: s.aliases || [], source: s.fn.toString(), \
         dependencies: (s.dependencies || []).filter(d => typeof d === 'string'), \
         trusted: !!s.requiresTrust })))",
        Duration::from_secs(10),
    )?;
    let entries: Vec<ScriptletEntry> = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    Ok(entries
        .into_iter()
        .map(|e| Resource {
            kind: ResourceType::Mime(if e.name.ends_with(".fn") {
                MimeType::FnJavascript
            } else {
                MimeType::ApplicationJavascript
            }),
            content: base64::engine::general_purpose::STANDARD.encode(e.source),
            name: e.name,
            aliases: e.aliases,
            dependencies: e.dependencies,
            permission: if e.trusted {
                PermissionMask::from_bits(crate::engine::TRUSTED)
            } else {
                PermissionMask::default()
            },
        })
        .collect())
}

#[derive(serde::Deserialize)]
struct RedirectEntry {
    name: String,
    aliases: Vec<String>,
    /// Takes parameters (`click2load.html`): not usable as a plain redirect.
    params: bool,
    trusted: bool,
}

/// The redirect resources `redirect-resources.js` names, with the files' bytes.
fn redirect_resources(net: &NetClient, cache: &mut FileCache) -> Result<Vec<Resource>, String> {
    let map = String::from_utf8_lossy(&cache.get(net, REDIRECTS)?).into_owned();
    let json = snowlark_js::module_graph::eval_module_graph(
        HashMap::from([(REDIRECTS.to_owned(), map)]),
        REDIRECTS,
        "ns => JSON.stringify([...ns.default].map(([name, d]) => ({ name, \
         aliases: d.alias ? [].concat(d.alias) : [], params: !!d.params, \
         trusted: !!d.requiresTrust })))",
        Duration::from_secs(5),
    )?;
    let entries: Vec<RedirectEntry> = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in entries.into_iter().filter(|e| !e.params && !e.trusted) {
        let Ok(bytes) = cache.get(net, &format!("{WAR_DIR}{}", e.name)) else {
            continue;
        };
        out.push(Resource {
            kind: ResourceType::Mime(MimeType::from_extension(&e.name)),
            content: base64::engine::general_purpose::STANDARD.encode(bytes),
            name: e.name,
            aliases: e.aliases,
            dependencies: Vec::new(),
            permission: PermissionMask::default(),
        });
    }
    Ok(out)
}

/// When the resources are due (Unix seconds).
pub fn resources_due(store: &ListStore) -> u64 {
    store.states.get(STATE_ID).map_or(0, |s| s.due(true))
}

/// Fetches uBlock Origin's resources when due (or `force`d) and, when they changed, saves
/// `resources.json` and reloads the engine with them.
pub fn update_resources(
    blocker: &ContentBlocker,
    net: &NetClient,
    store: &mut ListStore,
    force: bool,
    now: u64,
) {
    let (Some(dir), Some(base)) = (
        blocker.dir().map(Path::to_owned),
        blocker.resource_repository(),
    ) else {
        return;
    };
    if !force && resources_due(store) > now {
        return;
    }
    let mut cache = FileCache::open(&dir, base.clone());
    let result = fetch_graph(net, &mut cache, SCRIPTLETS)
        .and_then(scriptlet_resources)
        .and_then(|mut scriptlets| {
            scriptlets.extend(redirect_resources(net, &mut cache)?);
            Ok(scriptlets)
        });
    cache.save();
    let state = store.states.entry(STATE_ID.to_owned()).or_default();
    match result {
        Ok(resources) if !resources.is_empty() => {
            let changed = cache.fetched > 0 || !dir.join("resources.json").is_file();
            *state = ListState {
                url: base,
                checked: now,
                expires: INTERVAL,
                lines: resources.len(),
                ..Default::default()
            };
            if changed {
                let json = serde_json::to_vec(&resources).unwrap_or_default();
                state.bytes = json.len();
                let tmp = dir.join("resources.json.tmp");
                if std::fs::write(&tmp, &json).is_ok() {
                    let _ = std::fs::rename(&tmp, dir.join("resources.json"));
                }
                blocker.reload_resources();
            }
        }
        Ok(_) => {
            state.failures += 1;
            state.failed_at = now;
            state.error = Some("no resources".into());
        }
        Err(e) => {
            state.failures += 1;
            state.failed_at = now;
            state.error = Some(e);
        }
    }
}

/// Wraps adblock-rust's scriptlet code for injection: one function scope (nothing it defines is a
/// global) with the `scriptletGlobals` uBlock Origin's scriptlets read (empty: no logger).
pub fn wrap_injection(code: &str) -> String {
    if code.trim().is_empty() {
        return String::new();
    }
    format!("(function() {{\nconst scriptletGlobals = {{}};\n{code}\n}})();\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_imports() {
        let src = "import './a.js';\nimport {\n  x,\n  y,\n} from './utils.js';\nimport { z } from \"../b.js\";\nconst important = 1; // from 'nowhere'\nexport { q } from './c.js';";
        assert_eq!(imports(src), ["./utils.js", "../b.js", "./c.js", "./a.js"]);
        assert_eq!(
            join("src/js/resources/scriptlets.js", "./base.js"),
            "src/js/resources/base.js"
        );
        assert_eq!(join("src/js/resources/x.js", "../y.js"), "src/js/y.js");
    }

    /// A miniature of uBlock Origin's module layout, evaluated and turned into resources the
    /// engine injects.
    #[test]
    fn scriptlet_table_becomes_resources() {
        let sources = HashMap::from([
            (
                SCRIPTLETS.to_owned(),
                "import { registeredScriptlets } from './base.js';\n\
                 import { safeSelf } from './safe-self.js';\n\
                 import './set-constant.js';\n\
                 export const builtinScriptlets = registeredScriptlets;"
                    .to_owned(),
            ),
            (
                "src/js/resources/base.js".to_owned(),
                "export const registeredScriptlets = [];\n\
                 export const registerScriptlet = (fn, details) => { details.fn = fn; fn.details = details;\n\
                 if (Array.isArray(details.dependencies)) details.dependencies.forEach((d, i, a) => { if (typeof d === 'function') a[i] = d.details.name; });\n\
                 registeredScriptlets.push(details); };"
                    .to_owned(),
            ),
            (
                "src/js/resources/safe-self.js".to_owned(),
                "import { registerScriptlet } from './base.js';\n\
                 export function safeSelf() { return { log: () => {} }; }\n\
                 registerScriptlet(safeSelf, { name: 'safe-self.fn' });"
                    .to_owned(),
            ),
            (
                "src/js/resources/set-constant.js".to_owned(),
                "import { registerScriptlet } from './base.js';\n\
                 import { safeSelf } from './safe-self.js';\n\
                 function setConstant(chain = '', value = '') { safeSelf(); window[chain] = value; }\n\
                 registerScriptlet(setConstant, { name: 'set-constant.js', aliases: ['set.js'], dependencies: [safeSelf] });\n\
                 function trustedThing(x = '') { window.trusted = x; }\n\
                 registerScriptlet(trustedThing, { name: 'trusted-thing.js', requiresTrust: true });"
                    .to_owned(),
            ),
        ]);
        let resources = scriptlet_resources(sources).unwrap();
        let names: Vec<&str> = resources.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["safe-self.fn", "set-constant.js", "trusted-thing.js"]
        );
        let e = crate::engine::FilterEngine::compile(
            &[crate::engine::ListText {
                text:
                    "site.example##+js(set, adsEnabled, false)\nsite.example##+js(trusted-thing, a)",
                format: crate::engine::ListFormat::Filters,
                trusted: false,
            }],
            resources,
        );
        let script = e.cosmetic("https://site.example/").injected_script;
        assert!(script.contains("function safeSelf()"), "{script}");
        assert!(
            script.contains("setConstant(\"adsEnabled\", \"false\")"),
            "{script}"
        );
        // Not from a trusted list: the trusted scriptlet is not injected.
        assert!(!script.contains("trustedThing("), "{script}");
        let wrapped = wrap_injection(&script);
        assert!(wrapped.starts_with("(function() {"));
        assert_eq!(wrap_injection("  "), "");
    }
}
