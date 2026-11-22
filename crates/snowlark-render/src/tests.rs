// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The style adapter property by property: small documents styled by the real style engine, read back
//! through [`snowlark_layout::Style`] and [`snowlark_layout::StyleSource`].

use std::collections::HashMap;
use std::sync::Arc;

use snowlark_dom::{Dom, NodeId, ParseOptions};
use snowlark_layout::style::*;
use snowlark_layout::{Style, StyleSource};
use snowlark_paint::{BorderStyle, Color, ImageId, Point};
use snowlark_style::{DeviceConfig, StyleData, StyleEngine};

use crate::env::{FontAliases, ImageTable, LoadedImage, StyleEnv, WebFace, WebFamily};
use crate::source::{DocumentStyles, ImageState};
use crate::RenderStyle;

struct Doc {
    dom: Dom,
    engine: StyleEngine,
    data: StyleData,
    env: Arc<StyleEnv>,
    images: HashMap<NodeId, ImageState>,
}

fn doc_with(html: &str, env: StyleEnv) -> Doc {
    let mut dom = Dom::new();
    let document = dom.parse_html_str(html, ParseOptions::default());
    dom.set_document_url(document, "https://example.test/dir/page.html");
    let mut data = StyleData::new();
    let mut engine = StyleEngine::new(&dom, document, DeviceConfig::default());
    engine.update_all_style_blocks(&dom, &data);
    engine.restyle(&dom, &mut data);
    Doc {
        dom,
        engine,
        data,
        env: Arc::new(env),
        images: HashMap::new(),
    }
}

fn doc(html: &str) -> Doc {
    doc_with(html, StyleEnv::default())
}

impl Doc {
    fn id(&self, id: &str) -> NodeId {
        let root = self.dom.document_element(self.engine.document()).unwrap();
        let all = || {
            std::iter::once(root)
                .chain(self.dom.descendants(root))
                .filter(|&n| self.dom.is_element(n))
        };
        all()
            .find(|&n| self.dom.get_attribute(n, "id") == Some(id))
            .or_else(|| all().find(|&n| &*self.dom.element(n).unwrap().name().local == id))
            .unwrap_or_else(|| panic!("no #{id}"))
    }

    fn source(&self) -> DocumentStyles<'_> {
        DocumentStyles::new(
            &self.dom,
            &self.engine,
            &self.data,
            self.env.clone(),
            &self.images,
        )
    }

    fn s(&self, id: &str) -> RenderStyle {
        self.source().element_style(self.id(id)).expect("styled")
    }
}

fn approx(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

#[test]
fn display_position_float_and_insets() {
    let d = doc("<!DOCTYPE html><style>
        #ib { display: inline-block } #f { display: inline-flex } #g { display: grid }
        #tr { display: table-row } #li { display: list-item } #c { display: contents }
        #cap { display: table-caption } #fr { display: flow-root }
        #abs { position: absolute; top: 10px; left: 50%; right: calc(10% + 5px); }
        #fl { float: right; clear: both; position: relative; z-index: 3 }
        #st { position: sticky } #fx { position: fixed }
      </style><body><div id=ib></div><div id=f></div><div id=g></div><div id=tr></div>
      <div id=li></div><div id=c></div><div id=cap></div><div id=fr></div>
      <span id=abs></span><div id=fl></div><div id=st></div><div id=fx></div><span id=sp></span>
      <div id=n style='display:none'><p id=np></p></div></body>");
    assert_eq!(d.s("ib").display(), Display::INLINE_BLOCK);
    assert_eq!(d.s("f").display(), Display::INLINE_FLEX);
    assert_eq!(d.s("g").display(), Display::GRID);
    assert_eq!(d.s("tr").display(), Display::TABLE_ROW);
    assert_eq!(d.s("li").display(), Display::LIST_ITEM);
    assert_eq!(d.s("c").display(), Display::Contents);
    assert_eq!(d.s("cap").display(), Display::TABLE_CAPTION);
    assert_eq!(d.s("fr").display(), Display::FLOW_ROOT);
    assert_eq!(d.s("sp").display(), Display::INLINE);
    assert_eq!(d.s("body").display(), Display::BLOCK);
    // An absolutely positioned span is blockified; its original display stays inline.
    let abs = d.s("abs");
    assert_eq!(abs.display(), Display::BLOCK);
    assert_eq!(abs.original_display(), Display::INLINE);
    assert_eq!(abs.position(), Position::Absolute);
    assert_eq!(abs.inset(Side::Top), LengthPercentageAuto::px(10.0));
    assert_eq!(abs.inset(Side::Left), LengthPercentageAuto::percent(50.0));
    match abs.inset(Side::Right) {
        LengthPercentageAuto::LengthPercentage(v) => {
            assert!(approx(v.length, 5.0) && approx(v.percent.unwrap(), 0.1));
        }
        other => panic!("{other:?}"),
    }
    assert!(abs.inset(Side::Bottom).is_auto());
    assert_eq!(abs.z_index(), None);
    let fl = d.s("fl");
    assert_eq!(fl.float(), Float::Right);
    assert_eq!(fl.clear(), Clear::Both);
    assert_eq!(fl.position(), Position::Relative);
    assert_eq!(fl.z_index(), Some(3));
    assert_eq!(d.s("st").position(), Position::Sticky);
    assert_eq!(d.s("fx").position(), Position::Fixed);
    // A display: none element's own style says so; its subtree has none.
    assert_eq!(d.s("n").display(), Display::None);
    assert!(d.source().element_style(d.id("np")).is_none());
}

#[test]
fn sizes_margins_padding_and_borders() {
    let d = doc("<!DOCTYPE html><style>
        #a { width: 120px; height: 50%; min-width: min-content; max-width: none;
             min-height: 10px; max-height: fit-content; box-sizing: border-box;
             margin: auto 5% 3px; padding: 1px 2% 3px 4px; aspect-ratio: 16 / 9;
             border-top: 2px solid red; border-right: 3px dashed; border-bottom: 4px none blue;
             border-left: thin double rgb(0 0 255 / 50%);
             border-radius: 10px 5% / 20px; color: rgb(1, 2, 3); overflow: hidden auto; }
        #b { width: calc(50% + 10px); height: max-content; max-width: fit-content(100px);
             min-width: -webkit-fill-available; aspect-ratio: auto 2 / 1; overflow: clip scroll }
        #c { width: min(50%, 100px); }
      </style><div id=a></div><div id=b></div><div id=c></div>");
    let a = d.s("a");
    assert_eq!(a.width(), SizeValue::px(120.0));
    assert_eq!(a.height(), SizeValue::percent(50.0));
    assert_eq!(a.min_width(), SizeValue::MinContent);
    assert_eq!(a.max_width(), MaxSizeValue::None);
    assert_eq!(a.min_height(), SizeValue::px(10.0));
    assert_eq!(a.max_height(), MaxSizeValue::FitContent(None));
    assert_eq!(a.box_sizing(), BoxSizing::BorderBox);
    assert!(a.margin(Side::Top).is_auto());
    assert_eq!(a.margin(Side::Right), LengthPercentageAuto::percent(5.0));
    assert_eq!(a.margin(Side::Bottom), LengthPercentageAuto::px(3.0));
    assert_eq!(a.padding(Side::Left), LengthPercentage::px(4.0));
    assert_eq!(a.padding(Side::Right), LengthPercentage::percent(2.0));
    let ratio = a.aspect_ratio();
    assert!(!ratio.auto && approx(ratio.ratio.unwrap(), 16.0 / 9.0));
    assert_eq!(a.border_width(Side::Top), 2.0);
    assert_eq!(a.border_style(Side::Top), BorderStyle::Solid);
    assert_eq!(a.border_color(Side::Top), Color::rgb(255, 0, 0));
    // currentcolor.
    assert_eq!(a.border_color(Side::Right), Color::rgb(1, 2, 3));
    assert_eq!(a.border_style(Side::Right), BorderStyle::Dashed);
    // A `none` side has no width.
    assert_eq!(a.border_width(Side::Bottom), 0.0);
    assert_eq!(a.border_width(Side::Left), 1.0);
    assert_eq!(a.border_style(Side::Left), BorderStyle::Double);
    assert_eq!(a.border_color(Side::Left), Color::rgba(0, 0, 255, 128));
    assert_eq!(
        a.border_radius(Corner::TopLeft),
        (LengthPercentage::px(10.0), LengthPercentage::px(20.0))
    );
    assert_eq!(
        a.border_radius(Corner::TopRight),
        (LengthPercentage::percent(5.0), LengthPercentage::px(20.0))
    );
    assert_eq!(a.overflow_x(), Overflow::Hidden);
    assert_eq!(a.overflow_y(), Overflow::Auto);
    let b = d.s("b");
    match b.width() {
        SizeValue::LengthPercentage(v) => {
            assert!(approx(v.length, 10.0) && approx(v.percent.unwrap(), 0.5))
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(b.height(), SizeValue::MaxContent);
    assert_eq!(
        b.max_width(),
        MaxSizeValue::FitContent(Some(LengthPercentage::px(100.0)))
    );
    assert_eq!(b.min_width(), SizeValue::Stretch);
    let ratio = b.aspect_ratio();
    assert!(ratio.auto && approx(ratio.ratio.unwrap(), 2.0));
    // CSS Overflow 3 §3.1 (as css/css-overflow/parsing/overflow-computed.html tests it): `clip`
    // beside a scrolling axis stays `clip`; only `visible` computes to `auto`.
    assert_eq!(b.overflow_x(), Overflow::Clip);
    assert_eq!(b.overflow_y(), Overflow::Scroll);
    // A non-linear calc() (min() of a length and a percentage) resolves at every basis.
    match d.s("c").width() {
        SizeValue::LengthPercentage(v) => {
            assert_eq!(v.resolve(1000.0), 100.0);
            assert_eq!(v.resolve(100.0), 50.0);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn min_max_clamp_mixing_lengths_and_percentages() {
    // CSS Values 4 §10.2: min()/max()/clamp() of lengths and percentages resolve exactly at every
    // basis (not as a line through two bases).
    let d = doc("<!DOCTYPE html><style>
        #a { width: min(100%, calc(540px * 720 / 1280)); }
        #b { width: max(50%, 100px); }
        #c { width: clamp(100px, 50%, 400px); }
        #d { width: calc(100% - min(10%, 20px)); }
        #e { width: calc(min(100%, 300px) + 10px); }
        #f { width: min(100%, 300px, 400px); }
      </style><p id=a><p id=b><p id=c><p id=d><p id=e><p id=f>");
    let at = |id: &str, basis: f32| match d.s(id).width() {
        SizeValue::LengthPercentage(v) => v.resolve(basis),
        other => panic!("{other:?}"),
    };
    assert!(approx(at("a", 648.0), 303.75) && approx(at("a", 200.0), 200.0));
    assert!(approx(at("b", 100.0), 100.0) && approx(at("b", 1000.0), 500.0));
    assert!(approx(at("c", 100.0), 100.0) && approx(at("c", 400.0), 200.0));
    assert!(approx(at("c", 1000.0), 400.0));
    assert!(approx(at("d", 100.0), 90.0) && approx(at("d", 1000.0), 980.0));
    assert!(approx(at("e", 100.0), 110.0) && approx(at("e", 1000.0), 310.0));
    assert!(approx(at("f", 100.0), 100.0) && approx(at("f", 1000.0), 300.0));
}

#[test]
fn flex_and_alignment() {
    let d = doc("<!DOCTYPE html><style>
        #f { display: flex; flex-flow: column-reverse wrap-reverse; justify-content: space-between;
             align-items: center; align-content: flex-end; gap: 4px 10%; justify-items: legacy center }
        #i { flex: 2 3 30%; order: -1; align-self: baseline; justify-self: end }
        #j { flex-basis: content } #k { flex: auto }
      </style><div id=f><div id=i></div><div id=j></div><div id=k></div></div><div id=n></div>");
    let f = d.s("f");
    assert_eq!(f.flex_direction(), FlexDirection::ColumnReverse);
    assert_eq!(f.flex_wrap(), FlexWrap::WrapReverse);
    assert_eq!(f.justify_content(), Align::SpaceBetween);
    assert_eq!(f.align_items(), Align::Center);
    assert_eq!(f.align_content(), Align::FlexEnd);
    assert_eq!(f.row_gap(), Some(LengthPercentage::px(4.0)));
    assert_eq!(f.column_gap(), Some(LengthPercentage::percent(10.0)));
    assert_eq!(f.justify_items(), Align::Center);
    let i = d.s("i");
    assert_eq!(i.flex_grow(), 2.0);
    assert_eq!(i.flex_shrink(), 3.0);
    assert_eq!(
        i.flex_basis(),
        FlexBasis::LengthPercentage(LengthPercentage::percent(30.0))
    );
    assert_eq!(i.order(), -1);
    assert_eq!(i.align_self(), Align::Baseline);
    assert_eq!(i.justify_self(), Align::End);
    assert_eq!(d.s("j").flex_basis(), FlexBasis::Content);
    assert_eq!(d.s("k").flex_basis(), FlexBasis::Auto);
    let n = d.s("n");
    assert_eq!(n.row_gap(), None);
    assert_eq!(n.align_self(), Align::Auto);
    assert_eq!(n.justify_content(), Align::Normal);
    assert_eq!(n.flex_shrink(), 1.0);
}

#[test]
fn grid_templates_and_placement() {
    let d = doc("<!DOCTYPE html><style>
        #g { display: grid;
             grid-template-columns: [a] 100px [b] 1fr repeat(2, [c] minmax(10px, auto)) fit-content(50px);
             grid-template-rows: repeat(auto-fill, 20%);
             grid-template-areas: 'h h' 'n m';
             grid-auto-columns: min-content max-content; grid-auto-flow: column dense }
        #i { grid-row: 2 / span 3; grid-column: a / b 2 }
        #j { grid-area: n }
      </style><div id=g><div id=i></div><div id=j></div></div>");
    let g = d.s("g");
    let cols = g.grid_template_columns().into_owned();
    assert_eq!(cols.items.len(), 4);
    assert_eq!(
        cols.items[0],
        TrackListItem::Track(TrackSize::Breadth(TrackBreadth::LengthPercentage(
            LengthPercentage::px(100.0)
        )))
    );
    assert_eq!(
        cols.items[1],
        TrackListItem::Track(TrackSize::Breadth(TrackBreadth::Fr(1.0)))
    );
    match &cols.items[2] {
        TrackListItem::Repeat {
            count,
            tracks,
            line_names,
        } => {
            assert_eq!(*count, RepeatCount::Count(2));
            assert_eq!(
                tracks[0],
                TrackSize::MinMax(
                    TrackBreadth::LengthPercentage(LengthPercentage::px(10.0)),
                    TrackBreadth::Auto
                )
            );
            assert_eq!(&*line_names[0][0], "c");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        cols.items[3],
        TrackListItem::Track(TrackSize::FitContent(LengthPercentage::px(50.0)))
    );
    assert_eq!(&*cols.line_names[0][0], "a");
    assert_eq!(&*cols.line_names[1][0], "b");
    let rows = g.grid_template_rows().into_owned();
    match &rows.items[0] {
        TrackListItem::Repeat { count, .. } => assert_eq!(*count, RepeatCount::AutoFill),
        other => panic!("{other:?}"),
    }
    let areas = g.grid_template_areas().unwrap().into_owned();
    assert_eq!((areas.rows, areas.columns), (2, 2));
    let h = areas.areas.iter().find(|a| &*a.name == "h").unwrap();
    assert_eq!(
        (h.row_start, h.row_end, h.column_start, h.column_end),
        (1, 2, 1, 3)
    );
    assert_eq!(
        g.grid_auto_columns().into_owned(),
        vec![
            TrackSize::Breadth(TrackBreadth::MinContent),
            TrackSize::Breadth(TrackBreadth::MaxContent)
        ]
    );
    assert_eq!(
        g.grid_auto_flow(),
        GridAutoFlow {
            column: true,
            dense: true
        }
    );
    let i = d.s("i");
    assert_eq!(i.grid_row_start(), GridLine::Line(2));
    assert_eq!(i.grid_row_end(), GridLine::Span(3));
    assert_eq!(i.grid_column_start(), GridLine::Named("a".into(), 0));
    assert_eq!(i.grid_column_end(), GridLine::Named("b".into(), 2));
    assert_eq!(d.s("j").grid_row_start(), GridLine::Named("n".into(), 0));
    assert_eq!(d.s("j").grid_column_end(), GridLine::Named("n".into(), 0));
    assert!(d.s("i").grid_template_areas().is_none());
}

#[test]
fn table_properties() {
    let d = doc("<!DOCTYPE html><style>
        #t { table-layout: fixed; border-collapse: collapse; border-spacing: 3px 5px;
             caption-side: bottom; empty-cells: hide }
      </style><table id=t><tr><td id=c>x</td></tr></table><table id=u></table>");
    let t = d.s("t");
    assert_eq!(t.table_layout(), TableLayoutMode::Fixed);
    assert_eq!(t.border_collapse(), BorderCollapse::Collapse);
    assert_eq!(t.border_spacing(), (3.0, 5.0));
    assert_eq!(t.caption_side(), CaptionSide::Bottom);
    // Inherited.
    assert_eq!(d.s("c").empty_cells(), EmptyCells::Hide);
    // The UA sheet's table defaults.
    assert_eq!(d.s("u").border_spacing(), (2.0, 2.0));
    assert_eq!(d.s("u").border_collapse(), BorderCollapse::Separate);
}

#[test]
fn fonts_and_web_font_substitution() {
    let mut fonts = FontAliases::default();
    fonts.families.push(WebFamily {
        name: "Brand".into(),
        faces: vec![
            WebFace {
                order: 0,
                ..WebFace::described("__web_regular".into(), "400", "", "")
            },
            WebFace {
                order: 1,
                ..WebFace::described("__web_bold".into(), "700", "", "")
            },
        ],
    });
    fonts.families.push(WebFamily {
        name: "Loading".into(),
        faces: Vec::new(),
    });
    let d = doc_with(
        "<!DOCTYPE html><style>
          #a { font: italic bold 20px/1.5 Foo, 'Brand', Arial, sans-serif; font-stretch: condensed }
          #b { font-family: brand, Loading, monospace; font-weight: 300; font-style: oblique 10deg }
          #c { font-family: Foo; font-feature-settings: 'liga' 0, 'ss01';
               font-kerning: none; font-variant-ligatures: no-common-ligatures discretionary-ligatures }
          #d { font-variant-ligatures: none } #e { font-size: 2em; font-weight: bolder }
        </style><div id=a></div><div id=b lang=de-CH></div><div id=c></div>
        <div id=d></div><div id=e></div><div id=n></div>",
        StyleEnv {
            fonts,
            images: Arc::new(ImageTable::default()),
            motion_paths: Default::default(),
        },
    );
    let a = d.s("a");
    // Bold asks for the bold face first; the system family after; no default appended after a
    // generic.
    assert_eq!(
        a.font_family().collect::<Vec<_>>(),
        ["Foo", "__web_bold", "__web_regular", "Arial", "sans-serif"]
    );
    assert_eq!(a.font_size(), 20.0);
    assert_eq!(a.font_weight(), 700);
    assert_eq!(a.font_style(), snowlark_paint::FontStyle::Italic);
    assert_eq!(a.font_stretch(), snowlark_paint::FontStretch::CONDENSED);
    assert_eq!(a.line_height(), LineHeight::Number(1.5));
    let b = d.s("b");
    // Case-insensitive family match; a family still loading is skipped (it hides the system font of
    // that name).
    assert_eq!(
        b.font_family().collect::<Vec<_>>(),
        ["__web_regular", "__web_bold", "monospace"]
    );
    assert_eq!(b.font_weight(), 300);
    assert_eq!(b.font_style(), snowlark_paint::FontStyle::Oblique);
    // No generic: the default (serif) comes last.
    let c = d.s("c");
    assert_eq!(c.font_family().collect::<Vec<_>>(), ["Foo", "serif"]);
    let features = c.font_features().into_owned();
    assert!(features.contains(&snowlark_paint::FontFeature::new(b"kern", 0)));
    assert!(features.contains(&snowlark_paint::FontFeature::new(b"liga", 0)));
    assert!(features.contains(&snowlark_paint::FontFeature::new(b"dlig", 1)));
    assert!(features.contains(&snowlark_paint::FontFeature::new(b"ss01", 1)));
    let d_ = d.s("d").font_features().into_owned();
    assert!(d_.contains(&snowlark_paint::FontFeature::new(b"calt", 0)));
    assert_eq!(d.s("e").font_size(), 32.0);
    assert_eq!(d.s("e").font_weight(), 700);
    let n = d.s("n");
    assert!(n.font_features().is_empty());
    assert_eq!(n.font_size(), 16.0);
    assert_eq!(n.line_height(), LineHeight::Normal);
    assert_eq!(n.locale(), "en-us");
}

#[test]
fn system_font_keywords_compute_to_a_family_and_size() {
    let d = doc("<!DOCTYPE html><style>
          div { font: italic bold 30px/2 Foo; font-variant-numeric: tabular-nums }
          #m { font: menu } #c { font: caption } #s { font: status-bar; font-size: 20px }
        </style><div><div id=m></div><div id=c></div><div id=s></div></div>");
    let m = d.s("m");
    assert_eq!(m.font_family().collect::<Vec<_>>(), ["Segoe UI", "serif"]);
    assert_eq!(m.font_size(), 12.0);
    assert_eq!(m.font_weight(), 400);
    assert_eq!(m.font_style(), snowlark_paint::FontStyle::Normal);
    assert_eq!(m.line_height(), LineHeight::Normal);
    assert!(m.font_features().is_empty());
    let c = d.s("c");
    assert_eq!(c.font_family().collect::<Vec<_>>(), ["Arial", "serif"]);
    assert_eq!(c.font_size(), 16.0);
    assert_eq!(d.s("s").font_size(), 20.0);
}

#[test]
fn font_variant_longhands_enable_their_opentype_features() {
    let d = doc("<!DOCTYPE html><style>
          #a { font-variant: all-small-caps tabular-nums slashed-zero super historical-forms }
          #b { font-variant-caps: all-petite-caps; font-variant-numeric: oldstyle-nums diagonal-fractions;
               font-variant-east-asian: jis04 full-width ruby; font-variant-position: sub }
          #c { font-variant-numeric: tabular-nums; font-feature-settings: 'tnum' 0 }
          #t { font-variant-caps: titling-caps } #u { font-variant-caps: unicase }
          #p { font-variant-caps: petite-caps } #s { font: small-caps 16px serif }
        </style><div id=a></div><div id=b></div><div id=c></div><div id=t></div><div id=u></div>
        <div id=p></div><div id=s></div><div id=n></div>");
    let on = |id: &str| -> Vec<[u8; 4]> {
        let f = d.s(id).font_features().into_owned();
        f.iter().filter(|f| f.value == 1).map(|f| f.tag).collect()
    };
    assert_eq!(
        on("a"),
        [*b"c2sc", *b"smcp", *b"tnum", *b"zero", *b"sups", *b"hist"]
    );
    assert_eq!(
        on("b"),
        [*b"c2pc", *b"pcap", *b"onum", *b"frac", *b"jp04", *b"fwid", *b"ruby", *b"subs"]
    );
    // font-feature-settings comes after, so it wins.
    let c = d.s("c").font_features().into_owned();
    assert_eq!(
        c,
        [
            snowlark_paint::FontFeature::new(b"tnum", 1),
            snowlark_paint::FontFeature::new(b"tnum", 0)
        ]
    );
    assert_eq!(on("t"), [*b"titl"]);
    assert_eq!(on("u"), [*b"unic"]);
    assert_eq!(on("p"), [*b"pcap"]);
    assert_eq!(on("s"), [*b"smcp"]);
    assert!(d.s("n").font_features().is_empty());
}

/// CSS Text 3 §5.2: `word-break: break-word` is kept for compatibility; it computes as itself and
/// breaks as `word-break: normal` with `overflow-wrap: anywhere`.
#[test]
fn word_break_break_word() {
    let d = doc(
        "<!DOCTYPE html><style>#a { word-break: break-word } #b { word-break: break-word;
        word-break: nonsense }</style><div id=a></div><div id=b></div>",
    );
    assert_eq!(d.s("a").word_break(), WordBreak::BreakWord);
    assert_eq!(d.s("a").overflow_wrap(), OverflowWrap::Normal);
    // (a later invalid declaration is dropped: break-word stays)
    assert_eq!(d.s("b").word_break(), WordBreak::BreakWord);
}

/// CSS Text 4 §5.2: `word-break: auto-phrase` is a value of its own (a later one does not drop
/// it), and breaks as `normal` without phrase analysis for the language.
#[test]
fn word_break_auto_phrase_breaks_as_normal() {
    let d = doc(
        "<!DOCTYPE html><style>#a { word-break: break-all; word-break: auto-phrase }</style>\
         <div id=a lang=ja></div>",
    );
    assert_eq!(d.s("a").word_break(), WordBreak::Normal);
}

#[test]
fn text_properties() {
    let d = doc("<!DOCTYPE html><style>
        #a { color: hsl(120 100% 25%); text-align: center; text-align-last: justify;
             text-indent: 2em; white-space: pre; word-break: break-all; overflow-wrap: anywhere;
             text-overflow: ellipsis; text-transform: uppercase; letter-spacing: 0.5em;
             word-spacing: 3px; tab-size: 4; text-decoration: underline overline wavy red;
             direction: rtl; unicode-bidi: isolate; line-height: 20px; vertical-align: middle }
        #b { white-space: nowrap; line-height: 1.25; vertical-align: 5px; tab-size: 20px;
             text-align: -webkit-center; text-decoration-line: line-through; writing-mode: vertical-rl }
        #c { white-space: pre-line; vertical-align: super } #d { white-space: break-spaces;
             vertical-align: text-top } #e { vertical-align: 50% } #f { vertical-align: bottom }
      </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div><span id=e></span>
      <span id=f></span><div id=n></div>");
    let a = d.s("a");
    assert_eq!(a.color(), Color::rgb(0, 128, 0));
    assert_eq!(a.text_align(), TextAlign::Center);
    assert_eq!(a.legacy_align(), None);
    assert_eq!(a.text_align_last(), TextAlignLast::Justify);
    assert_eq!(a.text_indent(), LengthPercentage::px(32.0));
    assert_eq!(a.white_space_collapse(), WhiteSpaceCollapse::Preserve);
    assert_eq!(a.text_wrap_mode(), TextWrapMode::Nowrap);
    assert_eq!(a.word_break(), WordBreak::BreakAll);
    assert_eq!(a.overflow_wrap(), OverflowWrap::Anywhere);
    assert_eq!(a.text_overflow(), TextOverflow::Ellipsis);
    assert_eq!(a.text_transform(), TextTransform::Uppercase);
    assert_eq!(a.letter_spacing(), 8.0);
    assert_eq!(a.word_spacing(), 3.0);
    assert_eq!(a.tab_size(), TabSize::Spaces(4.0));
    assert!(a
        .text_decoration_line()
        .contains(TextDecorationLine::UNDERLINE));
    assert!(a
        .text_decoration_line()
        .contains(TextDecorationLine::OVERLINE));
    assert_eq!(a.text_decoration_style(), TextDecorationStyle::Wavy);
    assert_eq!(a.text_decoration_color(), Color::rgb(255, 0, 0));
    assert_eq!(a.direction(), snowlark_paint::Direction::Rtl);
    assert_eq!(a.unicode_bidi(), UnicodeBidi::Isolate);
    assert_eq!(a.line_height(), LineHeight::Length(20.0));
    assert_eq!(a.vertical_align(), VerticalAlign::Middle);
    let b = d.s("b");
    assert_eq!(b.white_space_collapse(), WhiteSpaceCollapse::Collapse);
    assert_eq!(b.text_wrap_mode(), TextWrapMode::Nowrap);
    assert_eq!(b.line_height(), LineHeight::Number(1.25));
    assert_eq!(
        b.vertical_align(),
        VerticalAlign::LengthPercentage(LengthPercentage::px(5.0))
    );
    assert_eq!(b.tab_size(), TabSize::Length(20.0));
    assert_eq!(b.text_align(), TextAlign::Center);
    assert_eq!(b.legacy_align(), Some(LegacyAlign::Center));
    assert_eq!(b.text_decoration_line(), TextDecorationLine::LINE_THROUGH);
    // currentcolor by default.
    assert_eq!(b.text_decoration_color(), b.color());
    assert_eq!(b.writing_mode(), WritingMode::VerticalRl);
    assert_eq!(
        d.s("c").white_space_collapse(),
        WhiteSpaceCollapse::PreserveBreaks
    );
    assert_eq!(d.s("c").vertical_align(), VerticalAlign::Super);
    assert_eq!(
        d.s("d").white_space_collapse(),
        WhiteSpaceCollapse::BreakSpaces
    );
    assert_eq!(d.s("d").vertical_align(), VerticalAlign::TextTop);
    assert_eq!(
        d.s("e").vertical_align(),
        VerticalAlign::LengthPercentage(LengthPercentage::percent(50.0))
    );
    assert_eq!(d.s("f").vertical_align(), VerticalAlign::Bottom);
    let n = d.s("n");
    assert_eq!(n.color(), Color::BLACK);
    assert_eq!(n.text_align(), TextAlign::Start);
    assert_eq!(n.vertical_align(), VerticalAlign::Baseline);
    assert_eq!(n.letter_spacing(), 0.0);
    assert_eq!(n.text_decoration_line(), TextDecorationLine::NONE);
    assert_eq!(n.writing_mode(), WritingMode::HorizontalTb);
}

fn table_with(url: &str, w: f32, h: f32) -> Arc<ImageTable> {
    let t = ImageTable::default();
    t.insert(url.to_owned(), LoadedImage::bitmap(ImageId(7), w, h));
    Arc::new(t)
}

#[test]
fn lists_and_generated_content() {
    let d = doc_with(
        "<!DOCTYPE html><style>
          #a { list-style: square inside url(dot.png) }
          #b { list-style-type: upper-roman } #c { list-style-type: '\u{2605} ' }
          #d { list-style-type: none } #e { list-style-type: no-such-style }
          .t::before { content: 'pre ' url(dot.png) ' post'; color: red }
          .u::before { content: '[' attr(data-x) ']' }
          .t::after { content: none }
          ol { counter-reset: sec 4 }
          ol > li { counter-increment: sec 2 }
          ol > li::before { content: counter(sec) '.' counter(list-item, lower-alpha) ' ' counters(sec, '-') }
          q::before { content: open-quote } q::after { content: close-quote }
          #m::marker { content: '>' }
        </style><ul><li id=a>a</li><li id=b>b</li><li id=c>c</li><li id=d>d</li><li id=e>e</li></ul>
        <p class=t id=t>t</p><p class=u id=u data-x=XY>u</p><ol><li id=o1>1</li><li id=o2>2</li></ol>
        <p><q id=q1>a<q id=q2>b</q></q></p><ul><li id=m>m</li></ul><p id=p>p</p>",
        StyleEnv {
            fonts: FontAliases::default(),
            images: table_with("https://example.test/dir/dot.png", 8.0, 6.0),
            motion_paths: Default::default(),
        },
    );
    let a = d.s("a");
    assert_eq!(a.list_style_type(), ListStyleType::Square);
    assert_eq!(a.list_style_position(), ListStylePosition::Inside);
    assert_eq!(a.list_style_image(), Image::raster(ImageId(7), 8.0, 6.0));
    assert_eq!(d.s("b").list_style_type(), ListStyleType::UpperRoman);
    assert_eq!(
        d.s("c").list_style_type(),
        ListStyleType::String("\u{2605} ".into())
    );
    assert_eq!(d.s("d").list_style_type(), ListStyleType::None);
    assert_eq!(d.s("e").list_style_type(), ListStyleType::Decimal);
    let src = d.source();
    // ::before with strings and an image; attr() is substituted by the style engine.
    let before = src.pseudo_style(d.id("t"), PseudoElement::Before).unwrap();
    assert_eq!(before.color(), Color::rgb(255, 0, 0));
    assert_eq!(
        before.content(),
        Content::Items(vec![
            ContentItem::Text("pre ".into()),
            ContentItem::Image(Image::raster(ImageId(7), 8.0, 6.0)),
            ContentItem::Text(" post".into()),
        ])
    );
    let u = src.pseudo_style(d.id("u"), PseudoElement::Before).unwrap();
    assert_eq!(
        u.content(),
        Content::Items(vec![ContentItem::Text("[XY]".into())])
    );
    // content: none (or normal) on ::before/::after: no pseudo-element.
    assert!(src.pseudo_style(d.id("t"), PseudoElement::After).is_none());
    assert!(src.pseudo_style(d.id("p"), PseudoElement::Before).is_none());
    // Counters in document order. The `ol` rule replaces the UA's `counter-reset: list-item`, so the
    // list-item counter the `ul` reset (scoped to the ul and its following siblings) goes on: 6, 7.
    let o2 = src.pseudo_style(d.id("o2"), PseudoElement::Before).unwrap();
    assert_eq!(
        o2.content(),
        Content::Items(vec![ContentItem::Text("8.g 8".into())])
    );
    let o1 = src.pseudo_style(d.id("o1"), PseudoElement::Before).unwrap();
    assert_eq!(
        o1.content(),
        Content::Items(vec![ContentItem::Text("6.f 6".into())])
    );
    // Nested quotes alternate.
    let q1 = src.pseudo_style(d.id("q1"), PseudoElement::Before).unwrap();
    let q2 = src.pseudo_style(d.id("q2"), PseudoElement::Before).unwrap();
    let q2a = src.pseudo_style(d.id("q2"), PseudoElement::After).unwrap();
    let q1a = src.pseudo_style(d.id("q1"), PseudoElement::After).unwrap();
    let text = |s: &RenderStyle| match s.content() {
        Content::Items(i) => match &i[..] {
            [ContentItem::Text(t)] => t.to_string(),
            other => format!("{other:?}"),
        },
        other => format!("{other:?}"),
    };
    assert_eq!(
        [text(&q1), text(&q2), text(&q2a), text(&q1a)],
        ["\u{201C}", "\u{2018}", "\u{2019}", "\u{201D}"]
    );
    // ::marker exists for list items only; `content` on it is honoured.
    let m = src.pseudo_style(d.id("m"), PseudoElement::Marker).unwrap();
    assert_eq!(
        m.content(),
        Content::Items(vec![ContentItem::Text(">".into())])
    );
    let am = src.pseudo_style(d.id("a"), PseudoElement::Marker).unwrap();
    assert_eq!(am.content(), Content::Normal);
    assert!(src.pseudo_style(d.id("p"), PseudoElement::Marker).is_none());
}

#[test]
fn backgrounds_shadows_and_outlines() {
    let d = doc_with(
        "<!DOCTYPE html><style>
          #a { background: url(bg.png) no-repeat 50% 10px / cover content-box padding-box,
                           linear-gradient(to right, red, blue 30%, green) rgb(1 2 3);
               box-shadow: 1px 2px 3px 4px red, inset 0 0 5px blue;
               outline: 3px dashed green; outline-offset: 2px; opacity: 0.5 }
          #b { background-image: radial-gradient(circle closest-side at 10px 20%, red, blue),
                                 repeating-linear-gradient(45deg, red 0 10px, blue 10px 20px),
                                 url(missing.png);
               background-size: 10px auto; background-repeat: space round;
               outline: 4px none; outline-style: auto }
          #c { outline: 2px none red }
        </style><div id=a></div><div id=b></div><div id=c></div><div id=n></div>",
        StyleEnv {
            fonts: FontAliases::default(),
            images: table_with("https://example.test/dir/bg.png", 30.0, 20.0),
            motion_paths: Default::default(),
        },
    );
    let a = d.s("a");
    assert_eq!(a.background_color(), Color::rgb(1, 2, 3));
    assert_eq!(a.background_layer_count(), 2);
    let top = a.background_layer(0);
    assert_eq!(top.image, Image::raster(ImageId(7), 30.0, 20.0));
    assert_eq!(
        top.position,
        (LengthPercentage::percent(50.0), LengthPercentage::px(10.0))
    );
    assert_eq!(top.size, BackgroundSize::Cover);
    assert_eq!(
        top.repeat,
        (BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat)
    );
    assert_eq!(top.origin, BoxArea::ContentBox);
    assert_eq!(top.clip, BoxArea::PaddingBox);
    let bottom = a.background_layer(1);
    match &bottom.image {
        Image::LinearGradient(g) => {
            assert_eq!(g.angle, 90.0);
            assert!(!g.repeating);
            assert_eq!(g.stops.len(), 3);
            assert_eq!(g.stops[0].color, Color::rgb(255, 0, 0));
            assert_eq!(g.stops[0].position, None);
            assert_eq!(g.stops[1].position, Some(LengthPercentage::percent(30.0)));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(bottom.origin, BoxArea::PaddingBox);
    assert_eq!(bottom.clip, BoxArea::BorderBox);
    assert_eq!(a.box_shadow_count(), 2);
    assert_eq!(
        a.box_shadow(0),
        Shadow {
            offset_x: 1.0,
            offset_y: 2.0,
            blur: 3.0,
            spread: 4.0,
            color: Color::rgb(255, 0, 0),
            inset: false
        }
    );
    assert!(a.box_shadow(1).inset);
    assert_eq!(a.outline_width(), 3.0);
    assert_eq!(a.outline_style(), BorderStyle::Dashed);
    assert_eq!(a.outline_color(), Color::rgb(0, 128, 0));
    assert_eq!(a.outline_offset(), 2.0);
    assert_eq!(a.opacity(), 0.5);
    let b = d.s("b");
    assert_eq!(b.background_layer_count(), 3);
    match &b.background_layer(0).image {
        Image::RadialGradient(g) => {
            assert!(g.circle);
            assert_eq!(g.size, RadialSize::ClosestSide);
            assert_eq!(
                g.center,
                (LengthPercentage::px(10.0), LengthPercentage::percent(20.0))
            );
        }
        other => panic!("{other:?}"),
    }
    match &b.background_layer(1).image {
        Image::LinearGradient(g) => {
            assert_eq!(g.angle, 45.0);
            assert!(g.repeating);
            // Two-position stops become two stops each.
            assert_eq!(g.stops.len(), 4);
        }
        other => panic!("{other:?}"),
    }
    // An image that hasn't loaded paints nothing.
    assert_eq!(b.background_layer(2).image, Image::None);
    // The other lists repeat to the number of images.
    assert_eq!(
        b.background_layer(2).size,
        BackgroundSize::Explicit(Some(LengthPercentage::px(10.0)), None)
    );
    assert_eq!(
        b.background_layer(1).repeat,
        (BackgroundRepeat::Space, BackgroundRepeat::Round)
    );
    // outline-style: auto paints solid.
    assert_eq!(b.outline_style(), BorderStyle::Solid);
    assert_eq!(b.outline_width(), 4.0);
    assert_eq!(d.s("c").outline_width(), 0.0);
    let n = d.s("n");
    assert_eq!(n.background_layer_count(), 0);
    assert_eq!(n.background_color(), Color::TRANSPARENT);
    assert_eq!(n.box_shadow_count(), 0);
    assert_eq!(n.outline_style(), BorderStyle::None);
}

#[test]
fn transforms_and_stacking() {
    let d = doc("<!DOCTYPE html><style>
        #a { transform: translate(10px, 50%) }
        #b { transform: rotate(90deg); transform-origin: 0 0 }
        #c { rotate: 90deg }
        #d { translate: 5px 10%; scale: 2 }
        #e { filter: blur(2px) } #f { isolation: isolate } #g { mix-blend-mode: multiply }
        #h { will-change: transform } #i { contain: paint } #j { pointer-events: none }
        #pa { pointer-events: all } #pv { pointer-events: visiblePainted } #pb { pointer-events: bounding-box }
        #m { mask-image: url(icon.svg) } #mg { mask-image: linear-gradient(black, transparent) }
        #w { -webkit-mask-image: linear-gradient(black, black); -webkit-mask-size: 10px 4px;
             mask-position: right bottom; mask-repeat: no-repeat; mask-mode: luminance }
      </style><div id=a></div><div id=b></div><div id=c></div><div id=d></div><div id=e></div>
      <div id=f></div><div id=g></div><div id=h></div><div id=i></div><div id=j><div id=pa></div><div id=pv></div><div id=pb></div></div><div id=n></div>
      <div id=m></div><div id=mg></div><div id=w></div>");
    let reference = snowlark_paint::Rect::new(0.0, 0.0, 100.0, 40.0);
    let a = d.s("a");
    assert!(a.has_transform());
    let m = a.transform(reference).unwrap();
    assert_eq!(m.apply(Point::new(0.0, 0.0)), Point::new(10.0, 20.0));
    // A quarter turn about the top-left corner: (10, 0) goes to (0, 10).
    let m = d.s("b").transform(reference).unwrap();
    let p = m.apply(Point::new(10.0, 0.0));
    assert!(approx(p.x, 0.0) && approx(p.y, 10.0));
    // The `rotate` property, about the default origin (the center, 50, 20): the center stays put.
    let m = d.s("c").transform(reference).unwrap();
    let p = m.apply(Point::new(50.0, 20.0));
    assert!(approx(p.x, 50.0) && approx(p.y, 20.0));
    let p = m.apply(Point::new(60.0, 20.0));
    assert!(approx(p.x, 50.0) && approx(p.y, 30.0));
    // translate then scale about the center: the center moves by the translation.
    let m = d.s("d").transform(reference).unwrap();
    let p = m.apply(Point::new(50.0, 20.0));
    assert!(approx(p.x, 55.0) && approx(p.y, 24.0));
    let p = m.apply(Point::new(60.0, 20.0));
    assert!(approx(p.x, 75.0));
    // Mask layers (CSS Masking 1 §7): an image not loaded yet is no image (it masks everything out
    // until it arrives); the `-webkit-` names are aliases.
    let m = d.s("m");
    assert_eq!((m.opacity(), m.mask_layer_count()), (1.0, 1));
    assert_eq!(m.mask_layer(0).layer.image, Image::None);
    assert!(matches!(
        d.s("mg").mask_layer(0).layer.image,
        Image::LinearGradient(_)
    ));
    let w = d.s("w").mask_layer(0);
    assert!(matches!(w.layer.image, Image::LinearGradient(_)));
    assert!(w.luminance);
    assert_eq!(
        w.layer.repeat,
        (BackgroundRepeat::NoRepeat, BackgroundRepeat::NoRepeat)
    );
    assert_eq!(w.layer.origin, BoxArea::BorderBox);
    assert!(matches!(
        w.layer.size,
        BackgroundSize::Explicit(Some(_), Some(_))
    ));
    assert_eq!(d.s("n").mask_layer_count(), 0);
    for id in ["e", "f", "g", "h", "i", "m"] {
        assert!(d.s(id).forces_stacking_context(), "#{id}");
    }
    assert!(!d.s("j").pointer_events());
    // SVG 2 §16.6's values are valid and act as `auto` on HTML elements (not the inherited `none`).
    for id in ["pa", "pv", "pb"] {
        assert!(d.s(id).pointer_events(), "#{id}");
    }
    let n = d.s("n");
    assert!(!n.has_transform());
    assert!(n.transform(reference).is_none());
    assert!(!n.forces_stacking_context());
    assert!(n.pointer_events());
}

#[test]
fn replaced_properties_and_content() {
    let mut d = doc("<!DOCTYPE html><style>
        #i { object-fit: cover; object-position: right 10px top 0; image-rendering: pixelated }
      </style><img id=i src=a.png><img id=p src=b.png><canvas id=c width=50></canvas>
      <svg id=s viewBox='0 0 200 100' width=40></svg><iframe id=f></iframe><div id=n></div>");
    let i = d.id("i");
    d.images.insert(
        i,
        ImageState::Loaded(LoadedImage::bitmap(ImageId(3), 64.0, 32.0)),
    );
    d.images.insert(d.id("p"), ImageState::Pending);
    let s = d.s("i");
    assert_eq!(s.object_fit(), ObjectFit::Cover);
    assert_eq!(
        s.object_position(),
        (
            LengthPercentage::linear(-10.0, Some(1.0)),
            LengthPercentage::px(0.0)
        )
    );
    assert_eq!(
        s.image_rendering(),
        snowlark_paint::ImageRendering::Pixelated
    );
    let n = d.s("n");
    assert_eq!(n.object_fit(), ObjectFit::Fill);
    assert_eq!(
        n.object_position(),
        (
            LengthPercentage::percent(50.0),
            LengthPercentage::percent(50.0)
        )
    );
    let src = d.source();
    assert_eq!(
        src.replaced_content(i),
        Some(ReplacedContent::image(ImageId(3), 64.0, 32.0))
    );
    // Not available yet: no natural size of its own.
    let p = src.replaced_content(d.id("p")).unwrap();
    assert_eq!((p.image, p.natural_width), (None, Some(0.0)));
    let c = src.replaced_content(d.id("c")).unwrap();
    assert_eq!(
        (c.natural_width, c.natural_height),
        (Some(50.0), Some(150.0))
    );
    let svg = src.replaced_content(d.id("s")).unwrap();
    assert_eq!(
        (svg.natural_width, svg.natural_height, svg.natural_ratio),
        (Some(40.0), Some(20.0), Some(2.0))
    );
    let f = src.replaced_content(d.id("f")).unwrap();
    assert_eq!(f.natural_width, None);
    assert!(src.replaced_content(d.id("n")).is_none());
}

/// HTML §15.4.4: an `img` whose image failed and whose `alt` is not empty represents that text,
/// a non-replaced element showing it; one still loading, with an empty `alt`, or with
/// dimensions of its own stays replaced.
#[test]
fn a_broken_image_shows_its_alt_text() {
    let mut d = doc(
        "<!DOCTYPE html><img id=a alt=testing><img id=p alt=testing>\
        <img id=e alt=''><img id=w width=20 alt=testing><img id=c style='height: 5px' alt=x>",
    );
    for id in ["a", "e", "w", "c"] {
        let n = d.id(id);
        d.images.insert(n, ImageState::Failed);
    }
    d.images.insert(d.id("p"), ImageState::Pending);
    let src = d.source();
    assert_eq!(src.alt_text(d.id("a")).as_deref(), Some("testing"));
    assert!(src.replaced_content(d.id("a")).is_none());
    for id in ["p", "e", "w", "c"] {
        assert_eq!(src.alt_text(d.id(id)), None, "#{id}");
        assert!(src.replaced_content(d.id(id)).is_some(), "#{id}");
    }
}

#[test]
fn anonymous_boxes_inherit_and_set_display() {
    let d = doc("<!DOCTYPE html><style>
        #a { color: red; font-size: 20px; background: blue; padding: 5px; text-align: right }
      </style><div id=a></div>");
    let src = d.source();
    let parent = d.s("a");
    for (kind, display) in [
        (AnonymousBox::Block, Display::BLOCK),
        (AnonymousBox::Table { inline: false }, Display::TABLE),
        (AnonymousBox::Table { inline: true }, Display::INLINE_TABLE),
        (AnonymousBox::TableRow, Display::TABLE_ROW),
        (AnonymousBox::TableCell, Display::TABLE_CELL),
    ] {
        let s = src.anonymous_style(&parent, kind);
        assert_eq!(s.display(), display);
        // Inherited properties come from the parent; the others are initial.
        assert_eq!(s.color(), Color::rgb(255, 0, 0));
        assert_eq!(s.font_size(), 20.0);
        assert_eq!(s.text_align(), TextAlign::Right);
        assert_eq!(s.background_color(), Color::TRANSPARENT);
        assert_eq!(s.padding(Side::Top), LengthPercentage::ZERO);
    }
    // Cached per parent and kind.
    let x = src.anonymous_style(&parent, AnonymousBox::Block);
    let y = src.anonymous_style(&parent, AnonymousBox::Block);
    assert!(servo_arc::Arc::ptr_eq(x.values(), y.values()));
}

#[test]
fn used_values_for_resolved_styles() {
    use snowlark_style::computed::UsedValues;
    use snowlark_style::LonghandId;
    let d = doc("<!DOCTYPE html><style>
        #a { width: 50%; padding: 2px 3px; margin: 0 auto; line-height: 1.5; font-size: 10px;
             box-sizing: border-box; position: relative; left: 10%; transform-origin: 25% 0 }
      </style><div id=a></div>");
    let values = d.data.styles_arc(d.id("a")).unwrap();
    let geometry = snowlark_layout::BoxRects {
        border_box: snowlark_paint::Rect::new(100.0, 0.0, 200.0, 24.0),
        padding_box: snowlark_paint::Rect::new(100.0, 0.0, 200.0, 24.0),
        content_box: snowlark_paint::Rect::new(103.0, 2.0, 194.0, 20.0),
    };
    let cb = snowlark_paint::Rect::new(0.0, 0.0, 400.0, 300.0);
    let used = crate::NodeUsedValues::new(values, geometry, cb);
    assert_eq!(used.used_value(LonghandId::Width).as_deref(), Some("200px"));
    assert_eq!(used.used_value(LonghandId::Height).as_deref(), Some("24px"));
    assert_eq!(
        used.used_value(LonghandId::PaddingLeft).as_deref(),
        Some("3px")
    );
    assert_eq!(
        used.used_value(LonghandId::MarginLeft).as_deref(),
        Some("100px")
    );
    assert_eq!(
        used.used_value(LonghandId::MarginRight).as_deref(),
        Some("100px")
    );
    assert_eq!(
        used.used_value(LonghandId::LineHeight).as_deref(),
        Some("15px")
    );
    assert_eq!(used.used_value(LonghandId::Left).as_deref(), Some("40px"));
    assert_eq!(used.used_value(LonghandId::Right).as_deref(), Some("-40px"));
    assert_eq!(
        used.used_value(LonghandId::TransformOrigin).as_deref(),
        Some("50px 0px")
    );
    assert_eq!(used.used_value(LonghandId::Color), None);
}

#[test]
fn linear_calc_with_a_negative_length_keeps_its_length() {
    // A computed calc() in a non-negative property clamps its result; taken apart into a length and
    // a percentage, `calc(50% - 10px)` is still 50% minus 10px (css-flexbox/gap-003-ltr.html).
    let d = doc("<!DOCTYPE html><style>
        #a { width: calc(50% - 10px); padding-left: calc(100% - 100px) }
      </style><div id=a></div>");
    let a = d.s("a");
    let w = match a.width() {
        SizeValue::LengthPercentage(l) => l,
        other => panic!("{other:?}"),
    };
    assert!(
        approx(w.length, -10.0) && approx(w.percent.unwrap(), 0.5),
        "{w:?}"
    );
    assert!(approx(w.resolve(784.0), 382.0));
    let p = a.padding(Side::Left);
    assert!(
        approx(p.length, -100.0) && approx(p.percent.unwrap(), 1.0),
        "{p:?}"
    );
}

#[test]
fn alignment_overflow_positions() {
    let d = doc("<!DOCTYPE html><style>
        #f { display: flex; justify-content: safe flex-start; align-content: unsafe center;
             align-items: safe end }
        #i { align-self: unsafe flex-end }
      </style><div id=f><div id=i></div></div>");
    let f = d.s("f").align_safety();
    assert_eq!(f.justify_content, OverflowPosition::Safe);
    assert_eq!(f.align_content, OverflowPosition::Unsafe);
    assert_eq!(f.align_items, OverflowPosition::Safe);
    assert_eq!(f.align_self, OverflowPosition::Default);
    assert_eq!(d.s("i").align_safety().align_self, OverflowPosition::Unsafe);
}
