// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Responsive images (HTML §4.8.4.3): parsing `srcset` and `sizes`, media and type checks for
//! `<picture>` `<source>` elements, and selecting an image source for the device pixel ratio.

use cssparser::{Delimiter, Parser};
use style::context::QuirksMode;
use style::device::Device;
use style::dom::AttributeTracker;
use style::media_queries::MediaList;
use style::parser::{Parse, ParserContext};
use style::queries::{FeatureType, QueryCondition};
use style::stylesheets::{CssRuleType, CustomMediaEvaluator, Origin, UrlExtraData};
use style::values::computed::{Context, ToComputedValue};
use style::values::specified::Length;
use style_traits::ParsingMode;

/// One image candidate string.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub url: String,
    /// The `x` descriptor.
    pub density: Option<f32>,
    /// The `w` descriptor.
    pub width: Option<u32>,
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{c}' | '\r')
}

/// HTML §4.8.4.3.10 "parse a srcset attribute".
pub fn parse_srcset(input: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0;
    loop {
        // 1. Skip white space and commas.
        while pos < chars.len() && (is_ws(chars[pos]) || chars[pos] == ',') {
            pos += 1;
        }
        if pos >= chars.len() {
            return out;
        }
        // 2. The URL: up to white space.
        let start = pos;
        while pos < chars.len() && !is_ws(chars[pos]) {
            pos += 1;
        }
        let mut url: String = chars[start..pos].iter().collect();
        let mut descriptors = Vec::new();
        if url.ends_with(',') {
            // Trailing commas end the candidate, with no descriptors.
            while url.ends_with(',') {
                url.pop();
            }
        } else {
            // 3. Tokenize descriptors.
            while pos < chars.len() && is_ws(chars[pos]) {
                pos += 1;
            }
            let mut current = String::new();
            let mut in_parens = false;
            while pos < chars.len() {
                let c = chars[pos];
                pos += 1;
                if in_parens {
                    current.push(c);
                    if c == ')' {
                        in_parens = false;
                    }
                    continue;
                }
                if is_ws(c) {
                    if !current.is_empty() {
                        descriptors.push(std::mem::take(&mut current));
                    }
                } else if c == ',' {
                    break;
                } else {
                    if c == '(' {
                        in_parens = true;
                    }
                    current.push(c);
                }
            }
            if !current.is_empty() {
                descriptors.push(current);
            }
        }
        // 4. Parse descriptors; an error drops the candidate.
        let mut candidate = Candidate {
            url,
            density: None,
            width: None,
        };
        let mut error = false;
        let mut height = false;
        for d in &descriptors {
            let (num, unit) = d.split_at(d.len().saturating_sub(1));
            match unit {
                "w" if candidate.width.is_none() && candidate.density.is_none() => {
                    match num.parse::<u32>() {
                        Ok(w) if w > 0 && num.bytes().all(|b| b.is_ascii_digit()) => {
                            candidate.width = Some(w)
                        }
                        _ => error = true,
                    }
                }
                "x" if candidate.width.is_none() && candidate.density.is_none() && !height => {
                    match valid_float(num).then(|| num.parse::<f32>()) {
                        Some(Ok(x)) if x >= 0.0 && x.is_finite() => candidate.density = Some(x),
                        _ => error = true,
                    }
                }
                "h" if !height && candidate.density.is_none() => {
                    // (a valid non-negative integer greater than zero)
                    match num.parse::<u32>() {
                        Ok(h) if h > 0 && num.bytes().all(|b| b.is_ascii_digit()) => height = true,
                        _ => error = true,
                    }
                }
                _ => error = true,
            }
        }
        if height && candidate.width.is_none() {
            error = true;
        }
        if !error && !candidate.url.is_empty() {
            out.push(candidate);
        }
    }
}

/// HTML §2.3.4.3 "valid floating-point number": an optional `-`, digits, optionally `.` and
/// digits, optionally `e`/`E`, an optional sign and digits.
fn valid_float(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > start
    };
    if b.first() == Some(&b'-') {
        i += 1;
    }
    let int = digits(&mut i);
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    } else if !int {
        return false;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == b.len()
}

fn parser_context(url: &UrlExtraData, quirks: QuirksMode) -> ParserContext<'_> {
    ParserContext::new(
        Origin::Author,
        url,
        Some(CssRuleType::Media),
        ParsingMode::DEFAULT,
        quirks,
        Default::default(),
        None,
        None,
        Default::default(),
    )
}

/// CSSOM "serialize a media query list" of `media` as parsed (`MediaQueryList.media`).
pub fn serialize_media(url: &UrlExtraData, quirks: QuirksMode, media: &str) -> String {
    let mut parser = Parser::new(media);
    let mut context = parser_context(url, quirks);
    let list = MediaList::parse(&mut context, &mut parser);
    let mut out = String::new();
    let _ = style_traits::ToCss::to_css(&list, &mut style_traits::CssWriter::new(&mut out));
    out
}

/// Whether a media query list (`<source media>`) matches the device. An empty list matches.
pub fn media_matches(device: &Device, quirks: QuirksMode, url: &UrlExtraData, media: &str) -> bool {
    if media.trim().is_empty() {
        return true;
    }
    let mut parser = Parser::new(media);
    let mut context = parser_context(url, quirks);
    let list = MediaList::parse(&mut context, &mut parser);
    list.evaluate(device, quirks, &mut CustomMediaEvaluator::none())
}

/// What "parse a sizes attribute" found in a `sizes` value.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SourceSizes {
    /// An entry was `auto` alone (the size then comes from the image's box when the image allows
    /// auto-sizes).
    pub auto: bool,
    /// The size of the first entry whose media condition matches, in CSS px.
    pub size: Option<f32>,
}

/// HTML §4.8.4.3.11 "parse a sizes attribute": the source size in CSS px (100vw when nothing
/// matches). `auto` is the size an `auto` entry stands for (see [`parse_sizes`]).
pub fn source_size(
    device: &Device,
    quirks: QuirksMode,
    url: &UrlExtraData,
    sizes: &str,
    auto: Option<f32>,
) -> f32 {
    parse_sizes(device, quirks, url, sizes, auto)
        .size
        .unwrap_or_else(|| device.au_viewport_size().width.to_f32_px())
}

/// HTML §4.8.4.3.11 "parse a sizes attribute": the value is a comma-separated list of component
/// values; in each entry the last component value is the size (a non-negative length, math
/// functions allowed, or `auto`) and what precedes it a `<media-condition>`. Entries that do not
/// parse are skipped. `auto` is the size an `auto` entry stands for when the `img` allows
/// auto-sizes: its concrete object size width while it is being rendered, else its last
/// auto-sizes width; with `None` an `auto` entry is ignored.
pub fn parse_sizes(
    device: &Device,
    quirks: QuirksMode,
    url: &UrlExtraData,
    sizes: &str,
    auto: Option<f32>,
) -> SourceSizes {
    let mut out = SourceSizes::default();
    let mut parser = Parser::new(sizes);
    loop {
        // The entry's start and the start of its last component value.
        let entry = parser.parse_until_before(Delimiter::Comma, |p| {
            let start = p.position();
            let mut last = None;
            loop {
                p.skip_whitespace();
                let at = p.position();
                if p.next().is_err() {
                    break;
                }
                last = Some(at);
            }
            Ok::<_, cssparser::ParseError<()>>((start, last, p.position()))
        });
        if let Ok((start, Some(last), end)) = entry {
            let condition = parser.slice(start..last);
            let size = parser.slice(last..end);
            let px = if size.trim().eq_ignore_ascii_case("auto") {
                out.auto |= is_blank(condition);
                auto
            } else {
                parse_size(device, quirks, url, size)
            };
            if let Some(px) = px {
                if is_blank(condition) || condition_matches(device, quirks, url, condition) {
                    out.size = Some(px);
                    return out;
                }
            }
        }
        if parser.next().is_err() {
            return out;
        }
    }
}

/// Whether a run of CSS text holds only white space and comments.
fn is_blank(text: &str) -> bool {
    Parser::new(text).is_exhausted()
}

/// Whether `text` parses as a `<media-condition>` (not a media query: no media type) that matches.
fn condition_matches(device: &Device, quirks: QuirksMode, url: &UrlExtraData, text: &str) -> bool {
    let context = parser_context(url, quirks);
    let mut parser = Parser::new(text);
    let Ok(condition) =
        parser.parse_entirely(|p| QueryCondition::parse(&context, p, FeatureType::Media))
    else {
        return false;
    };
    Context::for_media_query_evaluation(device, quirks, |cx| {
        condition
            .matches(
                cx,
                &mut CustomMediaEvaluator::none(),
                &mut AttributeTracker::new_dummy(),
            )
            .to_bool(false)
    })
}

/// A `<source-size-value>` length in CSS px: a non-negative length, or a math function, whose
/// negative result is clamped to 0 and which is invalid when it is not finite (a division by
/// zero).
fn parse_size(device: &Device, quirks: QuirksMode, url: &UrlExtraData, text: &str) -> Option<f32> {
    let context = parser_context(url, quirks);
    let length = Parser::new(text)
        .parse_entirely(|p| Length::parse(&context, p))
        .ok()?;
    let px =
        Context::for_media_query_evaluation(device, quirks, |cx| length.to_computed_value(cx).px());
    if !px.is_finite() {
        None
    } else if length.is_calc() {
        // Computing censors NaN and infinities; the parsed value still shows them (a division
        // by zero is folded into the value as it is parsed).
        let text = style_traits::ToCss::to_css_string(&length);
        if text.contains("NaN") || text.contains("infinity") {
            return None;
        }
        Some(px.max(0.0))
    } else if px < 0.0 {
        None
    } else {
        Some(px)
    }
}

/// HTML §4.8.4.3.6 "select an image source" from a source set: the candidate whose density is
/// closest above the device pixel ratio (the smallest above it, or the largest below it). `w`
/// descriptors become densities with the source size. Returns the URL and its density.
pub fn select(candidates: &[Candidate], source_size: f32, dpr: f32) -> Option<(String, f32)> {
    let mut best: Option<(String, f32)> = None;
    for c in candidates {
        let density = match (c.density, c.width) {
            (Some(x), _) => x,
            // (a source size of 0 makes the density infinite: the image shows at 0x0)
            (None, Some(w)) if source_size > 0.0 => w as f32 / source_size,
            (None, Some(_)) => f32::INFINITY,
            (None, None) => 1.0,
        };
        if density < 0.0 || density.is_nan() {
            continue;
        }
        best = match best {
            None => Some((c.url.clone(), density)),
            Some((url, d)) => {
                let better = if d >= dpr {
                    density >= dpr && density < d
                } else {
                    density > d
                };
                if better {
                    Some((c.url.clone(), density))
                } else {
                    Some((url, d))
                }
            }
        };
    }
    best
}

/// Whether a `<source type>` names an image format this engine decodes (HTML §4.8.4.3 "update the
/// source set": a `type` that is not a supported image type skips the source). AVIF counts when
/// the build decodes it itself ([`snowlark_paint::image::AVIF`]).
pub fn supported_type(mime: &str) -> bool {
    let mime = mime.trim();
    if mime.is_empty() {
        return true;
    }
    let essence = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if essence == "image/avif" {
        return snowlark_paint::image::AVIF;
    }
    matches!(
        essence.as_str(),
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/bmp"
            | "image/webp"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/tiff"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srcset_parsing() {
        let c = parse_srcset("a.png 1x, b.png 2x,c.png 640w , d.png");
        assert_eq!(c.len(), 4);
        assert_eq!(c[0].density, Some(1.0));
        assert_eq!(c[1].url, "b.png");
        assert_eq!(c[2].width, Some(640));
        assert_eq!(c[3].density, None);
        // URLs may contain commas; descriptors with errors drop the candidate.
        let c = parse_srcset("data:image/png;base64,AAA 1x, x.png 2q");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].url, "data:image/png;base64,AAA");
        // Only trailing commas end a URL: "a.png,b.png" is one URL (HTML §4.8.4.3.10 step 4).
        let c = parse_srcset("a.png,b.png 2x, c.png, d.png 3x");
        assert_eq!(c[0].url, "a.png,b.png");
        assert_eq!(c[0].density, Some(2.0));
        assert_eq!(c[1].url, "c.png");
        assert_eq!(c[2].density, Some(3.0));
    }

    #[test]
    fn selection_prefers_the_smallest_density_at_or_above_the_ratio() {
        let c = parse_srcset("a 1x, b 2x, c 3x");
        assert_eq!(select(&c, 0.0, 1.0).unwrap().0, "a");
        assert_eq!(select(&c, 0.0, 1.5).unwrap().0, "b");
        assert_eq!(select(&c, 0.0, 4.0).unwrap().0, "c");
        let w = parse_srcset("s 400w, m 800w, l 1600w");
        assert_eq!(select(&w, 800.0, 1.0).unwrap().0, "m");
        assert_eq!(select(&w, 500.0, 1.0).unwrap().0, "m");
        assert_eq!(select(&w, 300.0, 1.0).unwrap().0, "s");
    }

    #[test]
    fn sizes_parse_as_component_values() {
        let mut dom = snowlark_dom::Dom::new();
        let document = dom.parse_html_str("", snowlark_dom::ParseOptions::default());
        let engine = snowlark_style::StyleEngine::new(
            &dom,
            document,
            snowlark_style::DeviceConfig::default(),
        );
        let device = engine.stylist().device();
        let vw = device.au_viewport_size().width.to_f32_px();
        let url = UrlExtraData::from(url::Url::parse("about:blank").unwrap());
        let size = |s: &str| source_size(device, QuirksMode::NoQuirks, &url, s, None);
        assert_eq!(size("(min-width: 0) 50px, 100vw"), 50.0);
        assert_eq!(size("(max-width: 1px) calc(100vw - 2px), 7px"), 7.0);
        // Comments are white space; commas inside a function or block do not end an entry.
        assert_eq!(size("/* */1px/* */"), 1.0);
        assert_eq!(size("clamp(1px, -50px, 100px)"), 1.0);
        assert_eq!(size("(min-width:0) clamp(1px, -50px, 200vw"), 1.0);
        assert_eq!(size("((),1px"), vw);
        assert_eq!(size("[[],1px"), vw);
        // A media type is not a <media-condition>.
        assert_eq!(size("all 100vw, 1px"), 1.0);
        assert_eq!(size("not print 100vw, 1px"), 1.0);
        assert_eq!(size("all 1px"), vw);
        // Negative math results clamp to 0; a division by zero is invalid; a negative length is.
        assert_eq!(size("min(-100px, 1px)"), 0.0);
        assert_eq!(size("calc(0px / 0)"), vw);
        assert_eq!(size("calc(-1px / 0)"), vw);
        assert_eq!(size("-1px"), vw);
        assert_eq!(size("0"), 0.0);
        let sizes = |s: &str| parse_sizes(device, QuirksMode::NoQuirks, &url, s, None);
        assert_eq!(
            sizes("auto, 30px"),
            SourceSizes {
                auto: true,
                size: Some(30.0)
            }
        );
        assert!(!sizes("(min-width: 0) auto").auto);
    }

    /// HTML "parse a sizes attribute": an `auto` entry is the image's width when it allows
    /// auto-sizes, in its place in the list; else it is skipped.
    #[test]
    fn auto_sizes_take_the_width_given() {
        let mut dom = snowlark_dom::Dom::new();
        let document = dom.parse_html_str("", snowlark_dom::ParseOptions::default());
        let engine = snowlark_style::StyleEngine::new(
            &dom,
            document,
            snowlark_style::DeviceConfig::default(),
        );
        let device = engine.stylist().device();
        let vw = device.au_viewport_size().width.to_f32_px();
        let url = UrlExtraData::from(url::Url::parse("about:blank").unwrap());
        let size = |s: &str, a| source_size(device, QuirksMode::NoQuirks, &url, s, a);
        assert_eq!(size("auto", Some(10.0)), 10.0);
        assert_eq!(size("AUTO", Some(0.0)), 0.0);
        assert_eq!(size("auto", None), vw);
        assert_eq!(size("auto, 30px", Some(10.0)), 10.0);
        assert_eq!(size("auto, 30px", None), 30.0);
        assert_eq!(size("100vw, auto", Some(10.0)), vw);
        assert_eq!(size("(min-width: 0px) auto", Some(10.0)), 10.0);
        assert_eq!(size("(max-width: 0px) auto, 5px", Some(10.0)), 5.0);
    }
}
