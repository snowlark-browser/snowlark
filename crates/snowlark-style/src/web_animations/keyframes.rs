// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Animations 1 §5.3.1 keyframes and §6.6.3 "process a keyframes argument" from the point where
//! script's values have been read: property names (§6.6.2), the property-indexed form's expansion,
//! offset checks, value and easing parsing, and "compute missing keyframe offsets" (§5.3.3).

use std::sync::OnceLock;

use style::properties::{NonCustomPropertyId, PropertyDeclarationBlock, PropertyId};
use url::Url;

use super::easing::Easing;
use crate::declarations::DeclarationBlock;

/// `CompositeOperation` (§6.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositeOperation {
    Replace,
    Add,
    Accumulate,
}

/// A keyframe after processing: offsets, its keyframe-specific easing and composite operation, and
/// its property values (as `getKeyframes()` serializes them, and parsed).
#[derive(Clone, Debug)]
pub struct Keyframe {
    pub offset: Option<f64>,
    pub computed_offset: f64,
    pub easing: Easing,
    /// `None` is `auto`: the effect's composite operation.
    pub composite: Option<CompositeOperation>,
    /// (animation property name, serialized value), sorted by IDL name.
    pub values: Vec<(String, String)>,
    /// The values as declarations (shorthands expanded; logical properties as written).
    pub(crate) block: PropertyDeclarationBlock,
}

impl Keyframe {
    /// Whether the keyframe declares a value for some property.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// A keyframe as script gave it, before value and easing parsing.
#[derive(Clone, Debug, Default)]
pub struct RawKeyframe {
    pub offset: Option<f64>,
    pub easing: String,
    pub composite: Option<CompositeOperation>,
    /// (animation property name, value), in the order the properties were read.
    pub values: Vec<(String, String)>,
}

/// The property-indexed form after reading (§6.6.3 `BasePropertyIndexedKeyframe` plus the property
/// value lists), in the order the properties were read (sorted by IDL name).
#[derive(Clone, Debug, Default)]
pub struct PropertyIndexed {
    pub values: Vec<(String, Vec<String>)>,
    pub offsets: Vec<Option<f64>>,
    pub easings: Vec<String>,
    pub composites: Vec<Option<CompositeOperation>>,
}

/// The CSS properties an animation can name (Web Animations 1 §6.6.3 "animatable properties"):
/// longhands that are animatable and shorthands with an animatable longhand, as stylo enables them.
fn is_animatable(id: &PropertyId) -> bool {
    match id.as_shorthand() {
        Ok(shorthand) => {
            shorthand != style::properties::ShorthandId::All
                && shorthand.longhands().any(|l| l.is_animatable())
        }
        Err(style::properties::PropertyDeclarationId::Longhand(l)) => l.is_animatable(),
        Err(style::properties::PropertyDeclarationId::Custom(_)) => true,
    }
}

/// CSSOM "IDL attribute to CSS property": an ASCII upper alpha becomes `-` and its lowercase.
fn idl_to_css(attr: &str) -> String {
    let mut out = String::with_capacity(attr.len() + 4);
    for c in attr.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `name` follows the `<custom-property-name>` production (`--` and an identifier's rest).
fn is_custom_property_name(name: &str) -> bool {
    name.len() > 2 && name.starts_with("--")
}

/// §6.6.2 "IDL attribute name to animation property name", for the attributes that name an
/// animatable property (others are not animation properties and are ignored): `None` otherwise.
pub fn animation_property_from_idl(attr: &str) -> Option<String> {
    crate::prefs::init();
    if is_custom_property_name(attr) {
        return Some(attr.to_owned());
    }
    let css = match attr {
        "cssFloat" => "float".to_owned(),
        "cssOffset" => "offset".to_owned(),
        // (the CSS names of these two are not IDL names: `float` is cssFloat, and `offset` is the
        // keyframe offset)
        "float" | "offset" => return None,
        _ if attr.contains('-') => return None,
        _ => idl_to_css(attr),
    };
    if css.starts_with('-') {
        // (`WebkitFoo` style attributes are not animation property names)
        return None;
    }
    let id = PropertyId::parse_enabled_for_all_content(&css).ok()?;
    if !is_animatable(&id) {
        return None;
    }
    Some(css)
}

/// §6.6.2 "animation property name to IDL attribute name".
pub fn idl_from_animation_property(property: &str) -> String {
    if is_custom_property_name(property) {
        return property.to_owned();
    }
    match property {
        "float" => return "cssFloat".to_owned(),
        "offset" => return "cssOffset".to_owned(),
        _ => {}
    }
    let mut out = String::with_capacity(property.len());
    let mut upper = false;
    for c in property.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.push(c.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// The animation properties' IDL names, sorted (for tests and diagnostics).
pub fn animatable_idl_names() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        crate::prefs::init();
        let mut names: Vec<String> = NonCustomPropertyId::iter()
            .map(|id| id.name())
            .filter(|n| !n.starts_with('-'))
            .map(idl_from_animation_property)
            .filter(|n| animation_property_from_idl(n).is_some())
            .collect();
        names.sort();
        names.dedup();
        names
    })
}

/// §5.3.3 "compute missing keyframe offsets" over (offset, computed offset) pairs.
pub fn compute_missing_offsets(offsets: &[Option<f64>]) -> Vec<f64> {
    let n = offsets.len();
    let mut computed: Vec<Option<f64>> = offsets.to_vec();
    if n == 0 {
        return Vec::new();
    }
    if n > 1 && computed[0].is_none() {
        computed[0] = Some(0.0);
    }
    if computed[n - 1].is_none() {
        computed[n - 1] = Some(1.0);
    }
    let mut a = 0;
    while a < n {
        let Some(start) = computed[a] else {
            a += 1;
            continue;
        };
        let mut b = a + 1;
        while b < n && computed[b].is_none() {
            b += 1;
        }
        if b >= n {
            break;
        }
        let end = computed[b].expect("resolved");
        let span = (b - a) as f64;
        for (i, slot) in computed.iter_mut().enumerate().take(b).skip(a + 1) {
            *slot = Some(start + (end - start) * (i - a) as f64 / span);
        }
        a = b;
    }
    computed.into_iter().map(|c| c.unwrap_or(0.0)).collect()
}

/// The order in which a keyframe's values go into its declaration block so that the §5.3.3
/// conflict rules hold (a later write wins): shorthands with more longhands first, then those with
/// fewer, longhands last; among equals, the later IDL name first.
fn override_rank(property: &str) -> (usize, std::cmp::Reverse<String>) {
    let longhands = match PropertyId::parse_enabled_for_all_content(property)
        .ok()
        .and_then(|id| id.as_shorthand().ok())
    {
        Some(shorthand) => shorthand.longhands().count(),
        None => 0,
    };
    // More longhands sort earlier: invert so that a larger count ranks first.
    (
        usize::MAX - longhands,
        std::cmp::Reverse(idl_from_animation_property(property)),
    )
}

/// Parses one keyframe's values (invalid values are dropped) and its easing.
fn parse_keyframe(raw: RawKeyframe, base: &Url) -> Result<Keyframe, String> {
    let mut values: Vec<(String, String)> = Vec::with_capacity(raw.values.len());
    for (property, value) in raw.values {
        let mut single = DeclarationBlock::new(base);
        if !single.set_property(&property, &value, "") {
            continue;
        }
        let serialized = single.property_value(&property);
        values.push((property, serialized));
    }
    values.sort_by_key(|(p, _)| idl_from_animation_property(p));
    let mut order: Vec<&(String, String)> = values.iter().collect();
    order.sort_by_key(|(p, _)| override_rank(p));
    let mut block = DeclarationBlock::new(base);
    for (property, value) in order {
        block.set_property(property, value, "");
    }
    let easing = Easing::parse(&raw.easing).ok_or_else(|| invalid_easing(&raw.easing))?;
    Ok(Keyframe {
        offset: raw.offset,
        computed_offset: 0.0,
        easing,
        composite: raw.composite,
        values,
        block: block.into_block(),
    })
}

fn invalid_easing(easing: &str) -> String {
    format!("'{easing}' is not a valid value for easing")
}

/// Checks offsets (§6.6.3: loosely sorted, within [0, 1]), parses values and easings, and computes
/// the missing offsets.
fn finish(
    raws: Vec<RawKeyframe>,
    unused_easings: &[String],
    base: &Url,
) -> Result<Vec<Keyframe>, String> {
    let mut previous: Option<f64> = None;
    for k in &raws {
        if let Some(o) = k.offset {
            if previous.is_some_and(|p| o < p) {
                return Err("Offsets must be monotonically non-decreasing.".into());
            }
            previous = Some(o);
        }
    }
    if raws
        .iter()
        .any(|k| k.offset.is_some_and(|o| !(0.0..=1.0).contains(&o)))
    {
        return Err("Offsets must be null or in the range [0,1].".into());
    }
    let mut frames = raws
        .into_iter()
        .map(|r| parse_keyframe(r, base))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(bad) = unused_easings.iter().find(|e| Easing::parse(e).is_none()) {
        return Err(invalid_easing(bad));
    }
    let offsets: Vec<Option<f64>> = frames.iter().map(|k| k.offset).collect();
    for (k, c) in frames.iter_mut().zip(compute_missing_offsets(&offsets)) {
        k.computed_offset = c;
    }
    Ok(frames)
}

/// §6.6.3 for the array form: the keyframes in the order given. `Err` is a TypeError's message.
pub fn process_sequence(raws: Vec<RawKeyframe>, base: &Url) -> Result<Vec<Keyframe>, String> {
    crate::prefs::init();
    finish(raws, &[], base)
}

/// §6.6.3 for the property-indexed form. `Err` is a TypeError's message.
pub fn process_property_indexed(
    input: PropertyIndexed,
    base: &Url,
) -> Result<Vec<Keyframe>, String> {
    crate::prefs::init();
    // Each property's values become keyframes spaced evenly over [0, 1].
    let mut processed: Vec<(f64, RawKeyframe)> = Vec::new();
    for (property, values) in input.values {
        let offsets = compute_missing_offsets(&vec![None; values.len()]);
        for (value, offset) in values.into_iter().zip(offsets) {
            processed.push((
                offset,
                RawKeyframe {
                    offset: None,
                    easing: String::new(),
                    composite: None,
                    values: vec![(property.clone(), value)],
                },
            ));
        }
    }
    // Sort by computed offset (stably) and merge keyframes with equal computed offsets.
    processed.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, RawKeyframe)> = Vec::new();
    for (offset, k) in processed {
        match merged.last_mut() {
            Some((o, m)) if *o == offset => m.values.extend(k.values),
            _ => merged.push((offset, k)),
        }
    }
    let mut raws: Vec<RawKeyframe> = merged.into_iter().map(|(_, k)| k).collect();
    for (k, o) in raws.iter_mut().zip(input.offsets.iter()) {
        k.offset = *o;
    }
    let mut easings = input.easings;
    if easings.is_empty() {
        easings.push("linear".into());
    }
    let unused: Vec<String> = if easings.len() > raws.len() {
        easings.split_off(raws.len())
    } else {
        Vec::new()
    };
    for (i, k) in raws.iter_mut().enumerate() {
        k.easing = easings[i % easings.len()].clone();
    }
    if !input.composites.is_empty() {
        for (i, k) in raws.iter_mut().enumerate() {
            k.composite = input.composites[i % input.composites.len()];
        }
    }
    finish(raws, &unused, base)
}

impl Keyframe {
    /// The longhands (and custom properties) the keyframe sets, by CSS name.
    pub fn longhand_names(&self) -> Vec<String> {
        use style_traits::ToCss;
        self.block
            .declarations()
            .iter()
            .map(|d| {
                let mut out = String::new();
                let _ = d.id().to_css(&mut style_traits::CssWriter::new(&mut out));
                out
            })
            .collect()
    }

    /// The declarations of the keyframe's values, for computing them.
    pub(crate) fn declarations(&self) -> &PropertyDeclarationBlock {
        &self.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("http://example.test/").unwrap()
    }

    #[test]
    fn property_names() {
        assert_eq!(
            animation_property_from_idl("marginLeft").as_deref(),
            Some("margin-left")
        );
        assert_eq!(
            animation_property_from_idl("cssFloat").as_deref(),
            Some("float")
        );
        assert_eq!(
            animation_property_from_idl("--my-var").as_deref(),
            Some("--my-var")
        );
        assert_eq!(
            animation_property_from_idl("margin").as_deref(),
            Some("margin")
        );
        assert_eq!(animation_property_from_idl("margin-left"), None);
        assert_eq!(animation_property_from_idl("float"), None);
        assert_eq!(animation_property_from_idl("offset"), None);
        assert_eq!(animation_property_from_idl("easing"), None);
        assert_eq!(animation_property_from_idl("bogus"), None);
        assert_eq!(animation_property_from_idl("all"), None);
        // Not animatable: animation properties themselves.
        assert_eq!(animation_property_from_idl("animationName"), None);
        assert_eq!(idl_from_animation_property("margin-left"), "marginLeft");
        assert_eq!(idl_from_animation_property("float"), "cssFloat");
        assert_eq!(idl_from_animation_property("--x"), "--x");
        assert!(animatable_idl_names().iter().any(|n| n == "opacity"));
        assert!(animatable_idl_names().iter().any(|n| n == "cssFloat"));
    }

    #[test]
    fn missing_offsets() {
        assert_eq!(compute_missing_offsets(&[None]), vec![1.0]);
        assert_eq!(compute_missing_offsets(&[None, None]), vec![0.0, 1.0]);
        assert_eq!(
            compute_missing_offsets(&[None, Some(0.5), None, Some(0.8), None]),
            vec![0.0, 0.5, 0.65, 0.8, 1.0]
        );
        let c = compute_missing_offsets(&[None, None, None, None]);
        assert!((c[1] - 1.0 / 3.0).abs() < 1e-12 && (c[2] - 2.0 / 3.0).abs() < 1e-12);
    }

    fn raw(offset: Option<f64>, values: &[(&str, &str)]) -> RawKeyframe {
        RawKeyframe {
            offset,
            easing: "linear".into(),
            composite: None,
            values: values
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }

    #[test]
    fn sequence_form() {
        let k = process_sequence(
            vec![
                raw(None, &[("left", "10PX"), ("opacity", "bogus")]),
                raw(Some(0.25), &[("left", "20px")]),
                raw(None, &[("margin", "1px 2px")]),
            ],
            &base(),
        )
        .unwrap();
        assert_eq!(k.len(), 3);
        assert_eq!(k[0].values, vec![("left".to_string(), "10px".to_string())]);
        assert_eq!(k[0].computed_offset, 0.0);
        assert_eq!(k[1].computed_offset, 0.25);
        assert_eq!(k[2].computed_offset, 1.0);
        assert_eq!(k[2].values[0].1, "1px 2px");
        assert_eq!(k[2].declarations().declarations().len(), 4);
        // Offsets out of order or out of range, and bad easings, are TypeErrors.
        assert!(process_sequence(vec![raw(Some(0.5), &[]), raw(Some(0.2), &[])], &base()).is_err());
        assert!(process_sequence(vec![raw(Some(1.5), &[])], &base()).is_err());
        let mut bad = raw(None, &[]);
        bad.easing = "nope".into();
        assert!(process_sequence(vec![bad], &base()).is_err());
    }

    #[test]
    fn longhands_override_shorthands() {
        let k = process_sequence(
            vec![raw(None, &[("margin", "1px"), ("margin-left", "5px")])],
            &base(),
        )
        .unwrap();
        let left = k[0]
            .declarations()
            .declarations()
            .iter()
            .filter(|d| {
                d.id()
                    == style::properties::PropertyDeclarationId::Longhand(
                        style::properties::LonghandId::MarginLeft,
                    )
            })
            .count();
        assert_eq!(left, 1);
        let mut out = String::new();
        let _ = k[0].declarations().property_value_to_css(
            &PropertyId::parse_enabled_for_all_content("margin-left").unwrap(),
            &mut out,
        );
        assert_eq!(out, "5px");
    }

    #[test]
    fn property_indexed_form() {
        let k = process_property_indexed(
            PropertyIndexed {
                values: vec![
                    (
                        "left".into(),
                        vec!["0px".into(), "10px".into(), "20px".into()],
                    ),
                    ("opacity".into(), vec!["0".into(), "1".into()]),
                ],
                offsets: vec![],
                easings: vec!["ease-in".into(), "ease-out".into()],
                composites: vec![Some(CompositeOperation::Add)],
            },
            &base(),
        )
        .unwrap();
        assert_eq!(k.len(), 3);
        assert_eq!(
            k.iter().map(|k| k.computed_offset).collect::<Vec<_>>(),
            vec![0.0, 0.5, 1.0]
        );
        assert_eq!(k[0].values.len(), 2);
        assert_eq!(k[1].values.len(), 1);
        assert_eq!(k[2].easing.css(), "ease-in");
        assert!(k
            .iter()
            .all(|k| k.composite == Some(CompositeOperation::Add)));
        // A single value is at offset 1.
        let k = process_property_indexed(
            PropertyIndexed {
                values: vec![("opacity".into(), vec!["0".into()])],
                ..Default::default()
            },
            &base(),
        )
        .unwrap();
        assert_eq!(k.len(), 1);
        assert_eq!(k[0].computed_offset, 1.0);
        // Unused easings are still parsed.
        assert!(process_property_indexed(
            PropertyIndexed {
                values: vec![],
                easings: vec!["bad".into()],
                ..Default::default()
            },
            &base(),
        )
        .is_err());
    }
}
