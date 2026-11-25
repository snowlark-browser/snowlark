// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `URLPattern` (URL Pattern Standard): the `urlpattern` crate parses patterns and matches URLs, with
//! V8's regular expressions (ECMAScript syntax, the `v` flag) behind it.

use snowlark_bindings::generated::{
    StringOrURLPatternInit, StringOrUndefined, URLPatternComponentResult, URLPatternImpl,
    URLPatternInit, URLPatternOptions, URLPatternResult,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle, Record};
use urlpattern::component::Component;
use urlpattern::quirks::{self, StringOrInit};
use urlpattern::regexp::RegExp;
use urlpattern::{RegexSyntax, UrlPattern, UrlPatternOptions};

use super::{Engine, Object};

/// A regular expression run by V8: the pattern and the flags it is built from each time (V8 caches
/// compiled expressions by source and flags).
#[derive(Debug)]
pub struct JsRegexp {
    pattern: String,
    flags: String,
}

impl RegExp for JsRegexp {
    fn syntax() -> RegexSyntax {
        RegexSyntax::EcmaScript
    }

    fn parse(pattern: &str, flags: &str, force_eval: bool) -> Result<Self, ()> {
        // The URL Pattern Standard builds its expressions with the `v` flag (the library says `u`).
        let flags = flags.replace('u', "v");
        if force_eval && !snowlark_js::ecma_regexp::validate(pattern, &flags) {
            return Err(());
        }
        Ok(JsRegexp {
            pattern: pattern.to_owned(),
            flags,
        })
    }

    fn matches<'a>(&self, text: &'a str) -> Option<Vec<Option<&'a str>>> {
        snowlark_js::ecma_regexp::exec(&self.pattern, &self.flags, text)
    }

    fn pattern_string(&self) -> &str {
        &self.pattern
    }
}

pub type Pattern = UrlPattern<JsRegexp>;

fn type_error(e: impl std::fmt::Display) -> rt::Error {
    rt::Error::Type(e.to_string().into())
}

fn init_of(d: &URLPatternInit) -> quirks::UrlPatternInit {
    quirks::UrlPatternInit {
        protocol: d.protocol.clone(),
        username: d.username.clone(),
        password: d.password.clone(),
        hostname: d.hostname.clone(),
        port: d.port.clone(),
        pathname: d.pathname.clone(),
        search: d.search.clone(),
        hash: d.hash.clone(),
        base_url: d.base_url.clone(),
    }
}

fn string_or_init(input: &StringOrURLPatternInit) -> StringOrInit<'_> {
    match input {
        StringOrURLPatternInit::String(s) => StringOrInit::String(s.as_str().into()),
        StringOrURLPatternInit::URLPatternInit(d) => StringOrInit::Init(init_of(d)),
    }
}

fn component_result(
    c: &Component<JsRegexp>,
    r: urlpattern::UrlPatternComponentResult,
) -> URLPatternComponentResult {
    let mut groups = r.groups;
    // (the library keeps the groups in a map: the names list has their order)
    let groups = c
        .group_name_list
        .iter()
        .map(|name| {
            let v = match groups.remove(name).flatten() {
                Some(s) => StringOrUndefined::String(s),
                None => StringOrUndefined::Undefined(()),
            };
            (name.clone(), v)
        })
        .collect();
    URLPatternComponentResult {
        input: Some(r.input),
        groups: Some(Record(groups)),
    }
}

impl Engine {
    fn with_pattern<R>(&self, h: Handle, f: impl FnOnce(&Pattern) -> R) -> R {
        match &*self.obj(h) {
            Object::UrlPattern(p) => f(p),
            _ => panic!("not a URLPattern"),
        }
    }

    fn pattern_str(
        &self,
        h: Handle,
        f: impl FnOnce(&Pattern) -> &Component<JsRegexp>,
    ) -> DomStr<'_> {
        DomStr::Owned(self.with_pattern(h, |p| f(p).pattern_string.clone()))
    }

    fn construct_pattern(
        &self,
        cx: &mut Cx,
        input: &StringOrURLPatternInit,
        base_url: Option<&str>,
        options: &URLPatternOptions,
    ) -> rt::Result<Handle> {
        let init = quirks::process_construct_pattern_input(string_or_init(input), base_url)
            .map_err(type_error)?;
        let options = UrlPatternOptions {
            regex_syntax: RegexSyntax::EcmaScript,
            ignore_case: options.ignore_case,
        };
        let pattern =
            snowlark_js::ecma_regexp::with_scope(cx.scope(), || Pattern::parse(init, options))
                .map_err(type_error)?;
        Ok(self.alloc(Object::UrlPattern(Box::new(pattern))))
    }

    /// URL Pattern §"match": `None` when the input is not a URL or does not match.
    fn match_pattern(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: &StringOrURLPatternInit,
        base_url: Option<&str>,
    ) -> rt::Result<Option<urlpattern::UrlPatternResult>> {
        let Some((match_input, _)) =
            quirks::process_match_input(string_or_init(input), base_url).map_err(type_error)?
        else {
            return Ok(None);
        };
        snowlark_js::ecma_regexp::with_scope(cx.scope(), || {
            self.with_pattern(this, |p| p.exec(match_input))
        })
        .map_err(type_error)
    }
}

impl URLPatternImpl for Engine {
    fn protocol(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.protocol)
    }
    fn username(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.username)
    }
    fn password(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.password)
    }
    fn hostname(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.hostname)
    }
    fn port(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.port)
    }
    fn pathname(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.pathname)
    }
    fn search(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.search)
    }
    fn hash(&self, this: Handle) -> DomStr<'_> {
        self.pattern_str(this, |p| &p.hash)
    }
    fn has_reg_exp_groups(&self, this: Handle) -> bool {
        self.with_pattern(this, |p| p.has_regexp_groups())
    }

    fn test(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: StringOrURLPatternInit,
        base_url: Option<&str>,
    ) -> rt::Result<bool> {
        Ok(self.match_pattern(cx, this, &input, base_url)?.is_some())
    }

    fn exec(
        &self,
        cx: &mut Cx,
        this: Handle,
        input: StringOrURLPatternInit,
        base_url: Option<&str>,
    ) -> rt::Result<Option<URLPatternResult>> {
        let Some(r) = self.match_pattern(cx, this, &input, base_url)? else {
            return Ok(None);
        };
        let mut inputs = vec![input];
        if let Some(base) = base_url {
            inputs.push(StringOrURLPatternInit::String(base.to_owned()));
        }
        Ok(self.with_pattern(this, |p| {
            Some(URLPatternResult {
                inputs: Some(inputs),
                protocol: Some(component_result(&p.protocol, r.protocol)),
                username: Some(component_result(&p.username, r.username)),
                password: Some(component_result(&p.password, r.password)),
                hostname: Some(component_result(&p.hostname, r.hostname)),
                port: Some(component_result(&p.port, r.port)),
                pathname: Some(component_result(&p.pathname, r.pathname)),
                search: Some(component_result(&p.search, r.search)),
                hash: Some(component_result(&p.hash, r.hash)),
            })
        }))
    }

    fn constructor_input_base_url_options(
        &self,
        cx: &mut Cx,
        input: StringOrURLPatternInit,
        base_url: &str,
        options: URLPatternOptions,
    ) -> rt::Result<Handle> {
        self.construct_pattern(cx, &input, Some(base_url), &options)
    }

    fn constructor_input_options(
        &self,
        cx: &mut Cx,
        input: StringOrURLPatternInit,
        options: URLPatternOptions,
    ) -> rt::Result<Handle> {
        self.construct_pattern(cx, &input, None, &options)
    }
}
