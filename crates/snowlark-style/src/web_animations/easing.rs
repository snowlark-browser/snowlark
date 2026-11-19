// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Easing functions (CSS Easing 1) as Web Animations uses them: parsed from the `<easing-function>`
//! production of an `easing` string, serialized for `getTiming()`/`getKeyframes()`, and evaluated with
//! the before flag.

use style::parser::{Parse, ParserContext};
use style::stylesheets::{CssRuleType, Origin, UrlExtraData};
use style::values::computed::easing::ComputedTimingFunction;
use style::values::generics::easing::BeforeFlag;
use style::values::specified::easing::TimingFunction as SpecifiedTimingFunction;
use style_traits::{ParsingMode, ToCss};

/// An easing function: its serialization and its computed function.
#[derive(Clone, Debug, PartialEq)]
pub struct Easing {
    css: String,
    function: ComputedTimingFunction,
    linear: bool,
}

impl Easing {
    /// The linear easing function.
    pub fn linear() -> Easing {
        Easing {
            css: "linear".to_owned(),
            function: ComputedTimingFunction::Keyword(
                style::values::generics::easing::TimingKeyword::Linear,
            ),
            linear: true,
        }
    }

    /// Parses `text` with the `<easing-function>` production (CSS comments and escapes allowed,
    /// nothing else after the function). `None` when it does not parse (the caller throws a
    /// TypeError); `var()` references do not parse here.
    pub fn parse(text: &str) -> Option<Easing> {
        crate::prefs::init();
        let url = UrlExtraData::from(url::Url::parse("about:blank").expect("valid URL"));
        let context = ParserContext::new(
            Origin::Author,
            &url,
            Some(CssRuleType::Style),
            ParsingMode::DEFAULT,
            style::context::QuirksMode::NoQuirks,
            Default::default(),
            None,
            None,
            Default::default(),
        );
        let mut parser = cssparser::Parser::new(text);
        let specified = parser
            .parse_entirely(|p| SpecifiedTimingFunction::parse(&context, p))
            .ok()?;
        let css = specified.to_css_string();
        let function = specified.to_computed_value_without_context();
        let linear = css == "linear";
        Some(Easing {
            css,
            function,
            linear,
        })
    }

    /// The serialization (CSSOM "serialize a CSS value").
    pub fn css(&self) -> &str {
        &self.css
    }

    pub fn is_linear(&self) -> bool {
        self.linear
    }

    /// The computed easing function.
    pub fn function(&self) -> &ComputedTimingFunction {
        &self.function
    }

    /// The output progress for input progress `x`, with the before flag.
    pub fn apply(&self, x: f64, before_flag: bool) -> f64 {
        if self.linear {
            return x;
        }
        let flag = if before_flag {
            BeforeFlag::Set
        } else {
            BeforeFlag::Unset
        };
        // (cubic Bézier precision: a millionth, well under what a frame can show)
        self.function.calculate_output(x, flag, 1e-6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes() {
        assert_eq!(Easing::parse("linear").unwrap().css(), "linear");
        assert_eq!(Easing::parse("ease-in-out").unwrap().css(), "ease-in-out");
        assert_eq!(
            Easing::parse("step-start").unwrap().css(),
            "steps(1, start)"
        );
        assert_eq!(Easing::parse("step-end").unwrap().css(), "steps(1)");
        assert_eq!(Easing::parse("steps(2, end)").unwrap().css(), "steps(2)");
        assert_eq!(
            Easing::parse("cubic-bezier(0.1, 5, 0.23, 0)")
                .unwrap()
                .css(),
            "cubic-bezier(0.1, 5, 0.23, 0)"
        );
        assert_eq!(Easing::parse("/* c */ ease").unwrap().css(), "ease");
        for bad in [
            "",
            "invalid",
            "ease ease",
            "initial",
            "var(--x)",
            "cubic-bezier(1.1, 0, 1, 1)",
            "steps(0)",
        ] {
            assert!(Easing::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn evaluates() {
        let e = Easing::parse("ease-in").unwrap();
        assert!((e.apply(0.5, false) - 0.3153).abs() < 1e-3);
        let s = Easing::parse("steps(4)").unwrap();
        assert_eq!(s.apply(0.3, false), 0.25);
        assert_eq!(s.apply(1.0, false), 1.0);
        let start = Easing::parse("steps(1, start)").unwrap();
        assert_eq!(start.apply(0.0, false), 1.0);
        assert_eq!(start.apply(0.0, true), 0.0);
        assert_eq!(Easing::linear().apply(0.3, true), 0.3);
    }

    /// css-values-4 "combination of integers": a half rounds towards positive infinity, also
    /// when the progress comes from an easing whose `f32` arguments put it just off the half.
    #[test]
    fn an_integer_halfway_through_an_eased_interpolation_rounds_up() {
        use style::values::animated::{Animate, Procedure};
        let e =
            Easing::parse("cubic-bezier(0, -0.8333333333333334, 1, -0.8333333333333334)").unwrap();
        let progress = e.apply(0.5, false);
        assert!((progress + 0.5).abs() < 1e-6, "{progress}");
        assert_eq!(
            10i32.animate(&1, Procedure::Interpolate { progress }),
            Ok(15)
        );
        // Values clearly below or above a half still round to the nearest integer.
        let p = |progress| Procedure::Interpolate { progress };
        assert_eq!(10i32.animate(&1, p(-0.4999)), Ok(14));
        assert_eq!(0i32.animate(&3, p(0.5)), Ok(2));
        assert_eq!(0i32.animate(&-3, p(0.5)), Ok(-1));
        assert_eq!(
            0i32.animate(&i32::MAX, p(1.0)),
            Ok(i32::MAX),
            "beyond f32's exact integers"
        );
    }
}
