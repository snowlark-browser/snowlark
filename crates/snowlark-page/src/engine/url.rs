// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `URL` and `URLSearchParams` (URL Standard §6) over the `url` crate, whose `quirks` module implements
//! the API's getters and setters, and `application/x-www-form-urlencoded` (§5).

use snowlark_bindings::generated::{
    StringSequenceSequenceOrStringRecordOrString, URLImpl, URLSearchParamsImpl,
};
use snowlark_bindings::rt::{self, Cx, DomStr, Handle};

use super::{Engine, Object};

pub struct UrlData {
    pub url: url::Url,
    pub params: Handle,
}

pub struct ParamsData {
    pub list: Vec<(String, String)>,
    pub url: Option<Handle>,
}

pub fn set_protocol(u: &mut url::Url, v: &str) {
    let _ = url::quirks::set_protocol(u, v);
}

pub fn set_host(u: &mut url::Url, v: &str) {
    let _ = url::quirks::set_host(u, v);
}

pub fn set_hostname(u: &mut url::Url, v: &str) {
    let _ = url::quirks::set_hostname(u, v);
}

pub fn set_port(u: &mut url::Url, v: &str) {
    let _ = url::quirks::set_port(u, v);
}

pub fn set_pathname(u: &mut url::Url, v: &str) {
    url::quirks::set_pathname(u, v);
}

pub fn set_search(u: &mut url::Url, v: &str) {
    url::quirks::set_search(u, v);
}

pub fn set_hash(u: &mut url::Url, v: &str) {
    url::quirks::set_hash(u, v);
}

pub fn host_with_port(u: &url::Url) -> String {
    url::quirks::host(u).to_owned()
}

/// The URL Standard's "API URL parser": `url` against an optional `base`.
fn api_parse(url: &str, base: Option<&str>) -> Option<url::Url> {
    match base {
        Some(b) => {
            let base = url::Url::parse(b).ok()?;
            base.join(url).ok()
        }
        None => url::Url::parse(url).ok(),
    }
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(q.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn serialize_pairs(list: &[(String, String)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in list {
        s.append_pair(k, v);
    }
    s.finish()
}

impl Engine {
    fn with_url<R>(&self, h: Handle, f: impl FnOnce(&url::Url) -> R) -> R {
        match &*self.obj(h) {
            Object::Url(u) => f(&u.url),
            _ => panic!("not a URL"),
        }
    }

    fn url_str(&self, h: Handle, f: impl FnOnce(&url::Url) -> &str) -> DomStr<'_> {
        DomStr::Guard(std::cell::Ref::map(self.objects.borrow(), |o| {
            match &o[h.get() as usize - 1] {
                Object::Url(u) => f(&u.url),
                _ => "",
            }
        }))
    }

    /// Changes the URL and, when its query may have changed, re-parses its search params.
    fn update_url(&self, h: Handle, f: impl FnOnce(&mut url::Url), query_changed: bool) {
        let params = {
            let mut o = self.obj_mut(h);
            let Object::Url(u) = &mut *o else {
                return;
            };
            f(&mut u.url);
            u.params
        };
        if query_changed {
            let q = self.with_url(h, |u| u.query().unwrap_or("").to_owned());
            if let Object::UrlSearchParams(p) = &mut *self.obj_mut(params) {
                p.list = parse_query(&q);
            }
        }
    }

    fn new_url_object(&self, url: url::Url) -> Handle {
        let list = parse_query(url.query().unwrap_or(""));
        let params = self.alloc(Object::UrlSearchParams(ParamsData { list, url: None }));
        let h = self.alloc(Object::Url(UrlData { url, params }));
        if let Object::UrlSearchParams(p) = &mut *self.obj_mut(params) {
            p.url = Some(h);
        }
        h
    }

    fn params<R>(&self, h: Handle, f: impl FnOnce(&mut ParamsData) -> R) -> R {
        match &mut *self.obj_mut(h) {
            Object::UrlSearchParams(p) => f(p),
            _ => panic!("not URLSearchParams"),
        }
    }

    /// URLSearchParams "update": the associated URL's query becomes the serialization (null if empty).
    fn params_updated(&self, h: Handle) {
        let (url, query) = self.params(h, |p| (p.url, serialize_pairs(&p.list)));
        if let Some(u) = url {
            if let Object::Url(d) = &mut *self.obj_mut(u) {
                if query.is_empty() {
                    d.url.set_query(None);
                } else {
                    d.url.set_query(Some(&query));
                }
            }
        }
    }
}

impl URLImpl for Engine {
    fn href(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, |u| u.as_str())
    }
    fn set_href(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        let Ok(new) = url::Url::parse(value) else {
            return Err(rt::Error::Type(format!("Invalid URL: {value}").into()));
        };
        self.update_url(this, |u| *u = new, true);
        Ok(())
    }
    fn origin(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.with_url(this, url::quirks::origin))
    }
    fn protocol(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::protocol)
    }
    fn set_protocol(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_protocol(u, value), false);
        Ok(())
    }
    fn username(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::username)
    }
    fn set_username(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(
            this,
            |u| {
                let _ = url::quirks::set_username(u, value);
            },
            false,
        );
        Ok(())
    }
    fn password(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::password)
    }
    fn set_password(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(
            this,
            |u| {
                let _ = url::quirks::set_password(u, value);
            },
            false,
        );
        Ok(())
    }
    fn host(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::host)
    }
    fn set_host(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_host(u, value), false);
        Ok(())
    }
    fn hostname(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::hostname)
    }
    fn set_hostname(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_hostname(u, value), false);
        Ok(())
    }
    fn port(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::port)
    }
    fn set_port(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_port(u, value), false);
        Ok(())
    }
    fn pathname(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::pathname)
    }
    fn set_pathname(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_pathname(u, value), false);
        Ok(())
    }
    fn search(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::search)
    }
    fn set_search(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_search(u, value), true);
        Ok(())
    }
    fn search_params(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::Url(u) => u.params,
            _ => panic!("not a URL"),
        }
    }
    fn hash(&self, this: Handle) -> DomStr<'_> {
        self.url_str(this, url::quirks::hash)
    }
    fn set_hash(&self, _cx: &mut Cx, this: Handle, value: &str) -> rt::Result<()> {
        self.update_url(this, |u| set_hash(u, value), false);
        Ok(())
    }
    fn static_parse(
        &self,
        _cx: &mut Cx,
        url: &str,
        base: Option<&str>,
    ) -> rt::Result<Option<Handle>> {
        Ok(api_parse(url, base).map(|u| self.new_url_object(u)))
    }
    fn static_can_parse(&self, _cx: &mut Cx, url: &str, base: Option<&str>) -> rt::Result<bool> {
        Ok(api_parse(url, base).is_some())
    }
    fn to_json(&self, _cx: &mut Cx, this: Handle) -> rt::Result<DomStr<'_>> {
        Ok(self.url_str(this, |u| u.as_str()))
    }
    fn static_revoke_object_url(&self, cx: &mut Cx, url: &str) -> rt::Result<()> {
        if !self.revoke_media_source_url(url) {
            self.revoke_object_url(cx, url);
        }
        Ok(())
    }
    fn static_create_object_url(
        &self,
        cx: &mut Cx,
        obj: snowlark_bindings::generated::BlobOrMediaSource,
    ) -> rt::Result<DomStr<'_>> {
        use snowlark_bindings::generated::BlobOrMediaSource as O;
        match obj {
            // MSE §2.2: a MediaSource gets a URL that names it.
            O::MediaSource(ms) => Ok(DomStr::Owned(self.media_source_object_url(cx, ms)?)),
            O::Blob(b) => Ok(DomStr::Owned(self.create_object_url(cx, b)?)),
        }
    }
    fn constructor(&self, _cx: &mut Cx, url: &str, base: Option<&str>) -> rt::Result<Handle> {
        match api_parse(url, base) {
            Some(u) => Ok(self.new_url_object(u)),
            None => Err(rt::Error::Type(format!("Invalid URL: {url}").into())),
        }
    }
}

impl URLSearchParamsImpl for Engine {
    fn size(&self, this: Handle) -> u32 {
        self.params(this, |p| p.list.len() as u32)
    }
    fn pair_at(&self, this: Handle, index: u32) -> Option<(String, String)> {
        self.params(this, |p| p.list.get(index as usize).cloned())
    }
    fn stringify(&self, this: Handle) -> DomStr<'_> {
        DomStr::Owned(self.params(this, |p| serialize_pairs(&p.list)))
    }
    fn append(&self, _cx: &mut Cx, this: Handle, name: &str, value: &str) -> rt::Result<()> {
        self.params(this, |p| p.list.push((name.to_owned(), value.to_owned())));
        self.params_updated(this);
        Ok(())
    }
    fn delete(
        &self,
        _cx: &mut Cx,
        this: Handle,
        name: &str,
        value: Option<&str>,
    ) -> rt::Result<()> {
        self.params(this, |p| {
            p.list
                .retain(|(k, v)| !(k == name && value.is_none_or(|val| v == val)))
        });
        self.params_updated(this);
        Ok(())
    }
    fn get(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Option<DomStr<'_>>> {
        Ok(self
            .params(this, |p| {
                p.list
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
            .map(DomStr::Owned))
    }
    fn get_all(&self, _cx: &mut Cx, this: Handle, name: &str) -> rt::Result<Vec<String>> {
        Ok(self.params(this, |p| {
            p.list
                .iter()
                .filter(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
                .collect()
        }))
    }
    fn has(&self, _cx: &mut Cx, this: Handle, name: &str, value: Option<&str>) -> rt::Result<bool> {
        Ok(self.params(this, |p| {
            p.list
                .iter()
                .any(|(k, v)| k == name && value.is_none_or(|val| v == val))
        }))
    }
    fn set(&self, _cx: &mut Cx, this: Handle, name: &str, value: &str) -> rt::Result<()> {
        self.params(this, |p| match p.list.iter().position(|(k, _)| k == name) {
            Some(i) => {
                p.list[i].1 = value.to_owned();
                let mut j = i + 1;
                while j < p.list.len() {
                    if p.list[j].0 == name {
                        p.list.remove(j);
                    } else {
                        j += 1;
                    }
                }
            }
            None => p.list.push((name.to_owned(), value.to_owned())),
        });
        self.params_updated(this);
        Ok(())
    }
    /// `sort()`: a stable sort by name in code unit order.
    fn sort(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        self.params(this, |p| {
            p.list
                .sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()))
        });
        self.params_updated(this);
        Ok(())
    }
    fn constructor(
        &self,
        _cx: &mut Cx,
        init: StringSequenceSequenceOrStringRecordOrString,
    ) -> rt::Result<Handle> {
        let list = match init {
            StringSequenceSequenceOrStringRecordOrString::String(s) => {
                parse_query(s.strip_prefix('?').unwrap_or(&s))
            }
            StringSequenceSequenceOrStringRecordOrString::StringRecord(r) => r.0,
            StringSequenceSequenceOrStringRecordOrString::StringSequenceSequence(seq) => {
                let mut out = Vec::with_capacity(seq.len());
                for pair in seq {
                    if pair.len() != 2 {
                        return Err(rt::Error::Type(
                            "Each pair of the sequence must have exactly two items".into(),
                        ));
                    }
                    let mut it = pair.into_iter();
                    out.push((it.next().unwrap(), it.next().unwrap()));
                }
                out
            }
        };
        Ok(self.alloc(Object::UrlSearchParams(ParamsData { list, url: None })))
    }
}
