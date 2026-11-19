// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SVG 2 §4.5.2 `SVGLength` values (`<length>`, `<percentage>` or `<number>`) and §4.5.3
//! `SVGAngle` values (`<angle>` or `<number>`): parsing with CSS syntax, the unit type constants,
//! serialization and conversion to user units and degrees.

use super::parse::{fmt_number, is_wsp, Scanner};

/// A length's unit. The ones with an `SVGLength` unit type constant, and the others CSS has
/// (`SVG_LENGTHTYPE_UNKNOWN`).
#[derive(Clone, Debug, PartialEq)]
pub enum LengthUnit {
    /// A unitless `<number>` (user units).
    Number,
    Percent,
    Em,
    Ex,
    Px,
    Cm,
    Mm,
    In,
    Pt,
    Pc,
    /// Another CSS length unit, lowercase (`rem`, `ch`, `vw`, `q`, `lh`...).
    Other(Box<str>),
    /// A non-scalar value (`calc()`): its serialization; the value resolves to 0.
    Calc(Box<str>),
}

impl LengthUnit {
    /// Whether a length in this unit resolves without a context (user units and absolute units).
    pub fn is_absolute(&self) -> bool {
        matches!(
            self,
            LengthUnit::Number
                | LengthUnit::Px
                | LengthUnit::Cm
                | LengthUnit::Mm
                | LengthUnit::In
                | LengthUnit::Pt
                | LengthUnit::Pc
        )
    }
}

/// A `<length>`, `<percentage>` or `<number>`: the numeric factor and the unit.
#[derive(Clone, Debug, PartialEq)]
pub struct Length {
    pub value: f32,
    pub unit: LengthUnit,
}

/// Which viewport dimension percentages of a length resolve against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LengthDir {
    Horizontal,
    Vertical,
    /// The normalized diagonal (SVG 2 §8.9: sqrt((w² + h²) / 2)).
    Other,
}

/// What lengths resolve against: the SVG viewport and the element's font. `None` fields fall back
/// to the detached defaults (a 100 percentage basis, the initial `font-size` of 16px).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LengthContext {
    /// The SVG viewport's width and height.
    pub viewport: Option<(f32, f32)>,
    pub font_size: Option<f32>,
    /// The first available font's x-height, cap height, `0` advance and `水` advance.
    pub x_height: Option<f32>,
    pub cap_height: Option<f32>,
    pub ch: Option<f32>,
    pub ic: Option<f32>,
    /// The computed `line-height` of the element and of the root element.
    pub line_height: Option<f32>,
    pub root_line_height: Option<f32>,
    pub root_font_size: Option<f32>,
    /// The initial containing block (the `vw`/`vh` basis).
    pub icb: Option<(f32, f32)>,
}

/// The initial value of `font-size` (`medium`).
pub const INITIAL_FONT_SIZE: f32 = 16.0;

impl LengthUnit {
    /// The `SVG_LENGTHTYPE_*` constant.
    pub fn unit_type(&self) -> u16 {
        match self {
            LengthUnit::Number => 1,
            LengthUnit::Percent => 2,
            LengthUnit::Em => 3,
            LengthUnit::Ex => 4,
            LengthUnit::Px => 5,
            LengthUnit::Cm => 6,
            LengthUnit::Mm => 7,
            LengthUnit::In => 8,
            LengthUnit::Pt => 9,
            LengthUnit::Pc => 10,
            LengthUnit::Other(_) | LengthUnit::Calc(_) => 0,
        }
    }

    /// The unit of an `SVG_LENGTHTYPE_*` constant (not UNKNOWN).
    pub fn from_type(t: u16) -> Option<LengthUnit> {
        Some(match t {
            1 => LengthUnit::Number,
            2 => LengthUnit::Percent,
            3 => LengthUnit::Em,
            4 => LengthUnit::Ex,
            5 => LengthUnit::Px,
            6 => LengthUnit::Cm,
            7 => LengthUnit::Mm,
            8 => LengthUnit::In,
            9 => LengthUnit::Pt,
            10 => LengthUnit::Pc,
            _ => return None,
        })
    }

    fn from_ident(u: &str) -> Option<LengthUnit> {
        let l = u.to_ascii_lowercase();
        Some(match l.as_str() {
            "em" => LengthUnit::Em,
            "ex" => LengthUnit::Ex,
            "px" => LengthUnit::Px,
            "cm" => LengthUnit::Cm,
            "mm" => LengthUnit::Mm,
            "in" => LengthUnit::In,
            "pt" => LengthUnit::Pt,
            "pc" => LengthUnit::Pc,
            "q" | "rem" | "ch" | "ic" | "cap" | "lh" | "rlh" | "rex" | "rch" | "ric" | "rcap"
            | "vw" | "vh" | "vmin" | "vmax" | "vi" | "vb" | "svw" | "svh" | "lvw" | "lvh"
            | "dvw" | "dvh" | "svmin" | "svmax" | "lvmin" | "lvmax" | "dvmin" | "dvmax" => {
                LengthUnit::Other(l.into())
            }
            _ => return None,
        })
    }

    /// The canonical spelling of the unit after a number.
    pub fn suffix(&self) -> &str {
        match self {
            LengthUnit::Number | LengthUnit::Calc(_) => "",
            LengthUnit::Percent => "%",
            LengthUnit::Em => "em",
            LengthUnit::Ex => "ex",
            LengthUnit::Px => "px",
            LengthUnit::Cm => "cm",
            LengthUnit::Mm => "mm",
            LengthUnit::In => "in",
            LengthUnit::Pt => "pt",
            LengthUnit::Pc => "pc",
            LengthUnit::Other(u) => u,
        }
    }
}

impl LengthContext {
    fn font(&self) -> f32 {
        self.font_size.unwrap_or(INITIAL_FONT_SIZE)
    }

    /// The percentage basis in a direction (100 without a viewport).
    pub fn basis(&self, dir: LengthDir) -> f32 {
        match self.viewport {
            None => 100.0,
            Some((w, h)) => match dir {
                LengthDir::Horizontal => w,
                LengthDir::Vertical => h,
                LengthDir::Other => ((w * w + h * h) / 2.0).sqrt(),
            },
        }
    }

    /// How many user units one of `unit` is (`None` for percentages and numbers, which have their
    /// own rules, and `calc()`).
    pub fn unit_size(&self, unit: &LengthUnit) -> Option<f32> {
        let font = self.font();
        Some(match unit {
            LengthUnit::Number | LengthUnit::Px => 1.0,
            LengthUnit::Percent | LengthUnit::Calc(_) => return None,
            LengthUnit::Em => font,
            LengthUnit::Ex => self.x_height.unwrap_or(font * 0.5),
            LengthUnit::Cm => 96.0 / 2.54,
            LengthUnit::Mm => 96.0 / 25.4,
            LengthUnit::In => 96.0,
            LengthUnit::Pt => 96.0 / 72.0,
            LengthUnit::Pc => 16.0,
            LengthUnit::Other(u) => {
                let root = self.root_font_size.unwrap_or(INITIAL_FONT_SIZE);
                let (vw, vh) = self.icb.unwrap_or((0.0, 0.0));
                match &**u {
                    "q" => 96.0 / 101.6,
                    "rem" => root,
                    "ch" => self.ch.unwrap_or(font * 0.5),
                    "ic" => self.ic.unwrap_or(font),
                    "cap" => self.cap_height.unwrap_or(font * 0.7),
                    "lh" => self.line_height.unwrap_or(font * 1.2),
                    "rlh" => self.root_line_height.unwrap_or(root * 1.2),
                    "rex" => root * 0.5,
                    "rch" => root * 0.5,
                    "ric" => root,
                    "rcap" => root * 0.7,
                    "vw" | "svw" | "lvw" | "dvw" | "vi" => vw / 100.0,
                    "vh" | "svh" | "lvh" | "dvh" | "vb" => vh / 100.0,
                    "vmin" | "svmin" | "lvmin" | "dvmin" => vw.min(vh) / 100.0,
                    "vmax" | "svmax" | "lvmax" | "dvmax" => vw.max(vh) / 100.0,
                    _ => return None,
                }
            }
        })
    }
}

impl Length {
    pub const ZERO: Length = Length {
        value: 0.0,
        unit: LengthUnit::Number,
    };

    pub fn number(v: f32) -> Length {
        Length {
            value: v,
            unit: LengthUnit::Number,
        }
    }

    pub fn percent(v: f32) -> Length {
        Length {
            value: v,
            unit: LengthUnit::Percent,
        }
    }

    /// Parses `[ <number> | <length> | <percentage> ]` with CSS syntax (white space around it
    /// allowed, units ASCII case-insensitive). A `calc()` is kept as a non-scalar value.
    pub fn parse(s: &str) -> Option<Length> {
        let t = s.trim_matches(|c: char| c.is_ascii() && is_wsp(c as u8));
        if t.len() > 5 && t[..5].eq_ignore_ascii_case("calc(") && t.ends_with(')') {
            return Some(Length {
                value: 0.0,
                unit: LengthUnit::Calc(t.to_ascii_lowercase().into()),
            });
        }
        let mut sc = Scanner::new(t);
        let v = sc.number()?;
        let rest = std::str::from_utf8(sc.rest()).ok()?;
        let unit = if rest.is_empty() {
            LengthUnit::Number
        } else if rest == "%" {
            LengthUnit::Percent
        } else if rest.bytes().all(|b| b.is_ascii_alphabetic()) {
            LengthUnit::from_ident(rest)?
        } else {
            return None;
        };
        let value = v as f32;
        value.is_finite().then_some(Length { value, unit })
    }

    /// `unitType`.
    pub fn unit_type(&self) -> u16 {
        self.unit.unit_type()
    }

    /// `valueInSpecifiedUnits`: the numeric factor (0 for non-scalar values).
    pub fn specified(&self) -> f32 {
        match self.unit {
            LengthUnit::Calc(_) => 0.0,
            _ => self.value,
        }
    }

    /// `valueAsString`.
    pub fn to_css(&self) -> String {
        match &self.unit {
            LengthUnit::Calc(s) => s.to_string(),
            u => {
                let mut s = fmt_number(self.value);
                s.push_str(u.suffix());
                s
            }
        }
    }

    /// The value in user units (SVG 2 §4.5.2 `value`).
    pub fn to_user(&self, dir: LengthDir, ctx: &LengthContext) -> f32 {
        match &self.unit {
            LengthUnit::Number => self.value,
            LengthUnit::Percent => self.value * ctx.basis(dir) / 100.0,
            LengthUnit::Calc(_) => 0.0,
            u => ctx.unit_size(u).map_or(0.0, |k| self.value * k),
        }
    }

    /// The value in user units, or `None` when it is a non-scalar the caller must resolve some
    /// other way.
    pub fn resolve(&self, dir: LengthDir, ctx: &LengthContext) -> f32 {
        self.to_user(dir, ctx)
    }

    /// `convertToSpecifiedUnits`: the same length in `unit` (`absolute` is its value in user
    /// units).
    pub fn converted(
        unit: LengthUnit,
        absolute: f32,
        dir: LengthDir,
        ctx: &LengthContext,
    ) -> Length {
        let value = match &unit {
            LengthUnit::Number => absolute,
            LengthUnit::Percent => {
                let b = ctx.basis(dir);
                if b == 0.0 {
                    0.0
                } else {
                    absolute * 100.0 / b
                }
            }
            u => match ctx.unit_size(u) {
                Some(k) if k != 0.0 => absolute / k,
                _ => 0.0,
            },
        };
        Length { value, unit }
    }
}

/// An angle's unit (SVG 2 §4.5.3 `SVG_ANGLETYPE_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AngleUnit {
    /// A unitless number of degrees.
    Unspecified,
    Deg,
    Rad,
    Grad,
    /// `turn` (unit type UNKNOWN).
    Turn,
}

/// An `<angle>` or `<number>`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Angle {
    pub value: f32,
    pub unit: AngleUnit,
}

impl AngleUnit {
    pub fn unit_type(self) -> u16 {
        match self {
            AngleUnit::Unspecified => 1,
            AngleUnit::Deg => 2,
            AngleUnit::Rad => 3,
            AngleUnit::Grad => 4,
            AngleUnit::Turn => 0,
        }
    }

    pub fn from_type(t: u16) -> Option<AngleUnit> {
        Some(match t {
            1 => AngleUnit::Unspecified,
            2 => AngleUnit::Deg,
            3 => AngleUnit::Rad,
            4 => AngleUnit::Grad,
            _ => return None,
        })
    }

    /// Degrees per unit.
    fn degrees(self) -> f64 {
        match self {
            AngleUnit::Unspecified | AngleUnit::Deg => 1.0,
            AngleUnit::Rad => 180.0 / std::f64::consts::PI,
            AngleUnit::Grad => 0.9,
            AngleUnit::Turn => 360.0,
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            AngleUnit::Unspecified => "",
            AngleUnit::Deg => "deg",
            AngleUnit::Rad => "rad",
            AngleUnit::Grad => "grad",
            AngleUnit::Turn => "turn",
        }
    }
}

impl Angle {
    pub const ZERO: Angle = Angle {
        value: 0.0,
        unit: AngleUnit::Unspecified,
    };

    /// Parses `[ <angle> | <number> ]`.
    pub fn parse(s: &str) -> Option<Angle> {
        let t = s.trim_matches(|c: char| c.is_ascii() && is_wsp(c as u8));
        let mut sc = Scanner::new(t);
        let v = sc.number()? as f32;
        let rest = std::str::from_utf8(sc.rest()).ok()?.to_ascii_lowercase();
        let unit = match rest.as_str() {
            "" => AngleUnit::Unspecified,
            "deg" => AngleUnit::Deg,
            "rad" => AngleUnit::Rad,
            "grad" => AngleUnit::Grad,
            "turn" => AngleUnit::Turn,
            _ => return None,
        };
        v.is_finite().then_some(Angle { value: v, unit })
    }

    /// `value`: degrees.
    pub fn degrees(&self) -> f32 {
        (self.value as f64 * self.unit.degrees()) as f32
    }

    pub fn to_css(&self) -> String {
        let mut s = fmt_number(self.value);
        s.push_str(self.unit.suffix());
        s
    }

    /// `convertToSpecifiedUnits`.
    pub fn converted(unit: AngleUnit, degrees: f32) -> Angle {
        Angle {
            value: (degrees as f64 / unit.degrees()) as f32,
            unit,
        }
    }
}
