// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Cosmetic filtering: the engine's element hiding and procedural filters for a document, in the
//! form the page's [`snowlark_page::ContentFilter`] hook takes them.
//!
//! adblock-rust gives a document's site-specific hiding selectors (with the generic ones that name
//! neither a class nor an id), the generic class and id selectors on demand, and procedural or
//! action filters as JSON; the procedural operators it parses are `:has-text()`,
//! `:matches-attr()`, `:matches-css()` (and `-before`/`-after`), `:matches-path()`,
//! `:min-text-length()`, `:upward()` and `:xpath()`, the actions `:remove()`, `:style()`,
//! `:remove-attr()` and `:remove-class()`; `:has()` is CSS. `:xpath()` filters are dropped (no
//! XPath evaluator here), as are regular expressions the `regex` crate can't compile.

use adblock::cosmetic_filter_cache::ProceduralOrActionFilter;
use adblock::filters::cosmetic::{CosmeticFilterAction as Act, CosmeticFilterOperator as Op};
use snowlark_page::content_filter::{
    DocumentFilters, ProceduralAction, ProceduralFilter, ProceduralStep, TextPattern,
};

use crate::engine::FilterEngine;

/// What one procedural-or-action filter becomes.
#[derive(Debug)]
pub enum Converted {
    /// A plain CSS rule (selector, declarations).
    Css(String, String),
    Procedural(ProceduralFilter),
}

/// `name="value"`, `name`, `/re/="value"`...: the attribute name and value patterns of
/// `:matches-attr()`.
fn split_attr(arg: &str) -> Option<(TextPattern, Option<TextPattern>)> {
    let arg = arg.trim();
    let name_end = if let Some(body) = arg.strip_prefix('/') {
        // The regex runs to its closing slash and flags.
        let close = body.find('/')? + 1;
        arg[close + 1..]
            .find(|c: char| !c.is_ascii_alphabetic())
            .map_or(arg.len(), |i| close + 1 + i)
    } else {
        arg.find('=').unwrap_or(arg.len())
    };
    let unquote = |s: &str| {
        let s = s.trim();
        s.strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(s)
            .to_owned()
    };
    let name = TextPattern::parse(&unquote(&arg[..name_end]))?;
    let rest = arg[name_end..].trim();
    let value = match rest.strip_prefix('=') {
        Some(v) => Some(TextPattern::parse(&unquote(v))?),
        None => None,
    };
    Some((name, value))
}

fn matches_css(arg: &str, before: Option<bool>) -> Option<ProceduralStep> {
    let (property, value) = arg.split_once(':')?;
    Some(ProceduralStep::MatchesCss {
        before,
        property: property.trim().to_ascii_lowercase(),
        value: TextPattern::parse(value.trim())?,
    })
}

/// Converts one of adblock-rust's procedural-or-action filters (`None`: unsupported).
pub fn convert(filter: &ProceduralOrActionFilter) -> Option<Converted> {
    if let Some((selector, style)) = filter.as_css() {
        return Some(Converted::Css(selector, style));
    }
    let mut steps = Vec::with_capacity(filter.selector.len());
    for op in &filter.selector {
        steps.push(match op {
            Op::CssSelector(s) => ProceduralStep::Css(s.clone()),
            Op::HasText(a) => ProceduralStep::HasText(TextPattern::parse(a)?),
            Op::MatchesAttr(a) => {
                let (name, value) = split_attr(a)?;
                ProceduralStep::MatchesAttr { name, value }
            }
            Op::MatchesCss(a) => matches_css(a, None)?,
            Op::MatchesCssBefore(a) => matches_css(a, Some(true))?,
            Op::MatchesCssAfter(a) => matches_css(a, Some(false))?,
            Op::MatchesPath(a) => ProceduralStep::MatchesPath(TextPattern::parse(a)?),
            Op::MinTextLength(a) => ProceduralStep::MinTextLength(a.trim().parse().ok()?),
            Op::Upward(a) => match a.trim().parse::<usize>() {
                Ok(n) if n >= 1 => ProceduralStep::UpwardCount(n),
                Ok(_) => return None,
                Err(_) => ProceduralStep::UpwardSelector(a.trim().to_owned()),
            },
            Op::Xpath(_) => return None,
        });
    }
    let action = match &filter.action {
        None => ProceduralAction::Hide,
        Some(Act::Remove) => ProceduralAction::Remove,
        Some(Act::Style(s)) => ProceduralAction::Style(s.clone()),
        Some(Act::RemoveAttr(a)) => ProceduralAction::RemoveAttr(TextPattern::parse(a)?),
        Some(Act::RemoveClass(a)) => ProceduralAction::RemoveClass(TextPattern::parse(a)?),
    };
    Some(Converted::Procedural(ProceduralFilter { steps, action }))
}

/// The cosmetic filters and scriptlets of a document at `url` (on `host`).
pub fn document_filters(engine: &FilterEngine, url: &str, host: &str) -> DocumentFilters {
    let r = engine.cosmetic(url);
    let specific = !engine.specific_hide_off(host);
    let mut out = DocumentFilters {
        generic: !r.generichide,
        exceptions: r.exceptions,
        script: crate::scriptlets::wrap_injection(&r.injected_script),
        ..Default::default()
    };
    if specific {
        out.hide = r.hide_selectors.into_iter().collect();
        out.hide.sort();
        let mut actions: Vec<String> = r.procedural_actions.into_iter().collect();
        actions.sort();
        for json in actions {
            let Ok(f) = serde_json::from_str::<ProceduralOrActionFilter>(&json) else {
                continue;
            };
            match convert(&f) {
                Some(Converted::Css(sel, style)) if style == "display: none !important" => {
                    out.hide.push(sel)
                }
                Some(Converted::Css(sel, style)) => out.styles.push((sel, style)),
                Some(Converted::Procedural(p)) => out.procedural.push(p),
                None => {}
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ListFormat, ListText};

    fn engine(list: &str) -> FilterEngine {
        FilterEngine::compile(
            &[ListText {
                text: list,
                format: ListFormat::Filters,
                trusted: false,
            }],
            Vec::new(),
        )
    }

    #[test]
    fn site_generic_and_exception_rules() {
        let e = engine(
            "news.example##.promo\n\
             ##.ad-banner\n\
             ##div[data-ad]\n\
             news.example#@#.ad-banner\n\
             other.example##.promo",
        );
        let f = document_filters(&e, "https://www.news.example/a", "www.news.example");
        assert!(f.hide.contains(&".promo".to_owned()));
        assert!(f.hide.contains(&"div[data-ad]".to_owned()), "{:?}", f.hide);
        assert!(f.generic);
        // Class and id rules come on demand, minus the site's exceptions.
        let sel = e.class_id_selectors(&["ad-banner".into()], &[], &f.exceptions);
        assert!(sel.is_empty(), "{sel:?}");
        let g = document_filters(&e, "https://elsewhere.example/", "elsewhere.example");
        assert!(!g.hide.contains(&".promo".to_owned()));
        let sel = e.class_id_selectors(&["ad-banner".into()], &[], &g.exceptions);
        assert_eq!(sel, [".ad-banner"]);
    }

    #[test]
    fn generichide_and_specifichide() {
        let e = engine(
            "##.ad-banner\nquiet.example##.promo\n@@||quiet.example^$specifichide\n\
             calm.example##.promo\n@@||calm.example^$generichide",
        );
        let q = document_filters(&e, "https://quiet.example/", "quiet.example");
        assert!(q.hide.is_empty() && q.generic);
        let c = document_filters(&e, "https://calm.example/", "calm.example");
        assert!(!c.generic);
        assert!(c.hide.contains(&".promo".to_owned()));
    }

    #[test]
    fn procedural_filters_convert() {
        let e = engine(
            "site.example##.post:has-text(/sponsored/i)\n\
             site.example##.card:has-text(Promoted):upward(2)\n\
             site.example##.box:upward(.wrapper)\n\
             site.example##div:matches-css(position: fixed)\n\
             site.example##a:matches-attr(href=\"/^https:\\/\\/ads/\")\n\
             site.example##.x:matches-path(/shop)\n\
             site.example##.y:min-text-length(20)\n\
             site.example##.overlay:remove()\n\
             site.example##.page:style(overflow: auto !important)\n\
             site.example##.btn:remove-attr(onclick)\n\
             site.example##.z:xpath(//div)\n\
             site.example##.list:has(> .ad)",
        );
        let f = document_filters(&e, "https://site.example/", "site.example");
        assert_eq!(f.procedural.len(), 9, "{:#?}", f.procedural);
        assert!(f
            .styles
            .contains(&(".page".to_owned(), "overflow: auto !important".to_owned())));
        assert!(
            f.hide.contains(&".list:has(> .ad)".to_owned()),
            "{:?}",
            f.hide
        );
        assert!(f
            .procedural
            .iter()
            .any(|p| matches!(p.action, ProceduralAction::Remove)));
        assert!(f.procedural.iter().any(|p| p
            .steps
            .iter()
            .any(|s| matches!(s, ProceduralStep::UpwardCount(2)))));
    }

    #[test]
    fn matches_attr_arguments() {
        let (n, v) = split_attr("href=\"/^https/\"").unwrap();
        assert!(n.matches_whole("href"));
        assert!(v.unwrap().matches_whole("https://x"));
        let (n, v) = split_attr("/^data-ad/").unwrap();
        assert!(n.matches_whole("data-ad-slot"));
        assert!(v.is_none());
        let (n, v) = split_attr("\"aria-label\"=\"Ad\"").unwrap();
        assert!(n.matches_whole("aria-label"));
        assert!(v.unwrap().matches_whole("Ad"));
    }
}
