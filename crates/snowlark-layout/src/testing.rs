// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A plain-struct [`Style`] for tests and demos ([`TestStyle`], used as `Arc<TestStyle>`), a
//! [`StyleSource`] over a map of them ([`TestStyles`]), and [`TestDocument`], which builds a DOM and its
//! styles together: each element's style starts from its parent's inherited values, the way a cascade
//! would compute them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use snowlark_dom::{Dom, LocalName, NodeId};

use crate::style::*;

/// Every property [`Style`] reads, as a public field, defaulting to its initial value (CSS initial values,
/// with `font-family: sans-serif`, `font-size: 16px` as a UA would give the root).
#[derive(Clone, Debug, PartialEq)]
pub struct TestStyle {
    pub display: Display,
    /// The specified `display` before blockification (`None`: same as `display`).
    pub original_display: Option<Display>,
    pub position: Position,
    pub float: Float,
    pub clear: Clear,
    /// top, right, bottom, left
    pub inset: [LengthPercentageAuto; 4],
    pub z_index: Option<i32>,
    pub box_sizing: BoxSizing,
    pub width: SizeValue,
    pub height: SizeValue,
    pub min_width: SizeValue,
    pub min_height: SizeValue,
    pub max_width: MaxSizeValue,
    pub max_height: MaxSizeValue,
    pub aspect_ratio: AspectRatio,
    pub size_containment: SizeContainment,
    pub margin: [LengthPercentageAuto; 4],
    pub padding: [LengthPercentage; 4],
    pub border_width: [f32; 4],
    pub border_style: [BorderStyle; 4],
    /// `None` is `currentcolor`.
    pub border_color: [Option<Color>; 4],
    /// top-left, top-right, bottom-right, bottom-left
    pub border_radius: [(LengthPercentage, LengthPercentage); 4],
    pub overflow_x: Overflow,
    pub overflow_y: Overflow,
    pub flex_direction: FlexDirection,
    pub flex_wrap: FlexWrap,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: FlexBasis,
    pub order: i32,
    pub justify_content: Align,
    pub align_content: Align,
    pub align_items: Align,
    pub align_self: Align,
    pub justify_items: Align,
    pub justify_self: Align,
    pub align_safety: AlignSafety,
    pub row_gap: Option<LengthPercentage>,
    pub column_gap: Option<LengthPercentage>,
    pub grid_template_columns: TrackList,
    pub grid_template_rows: TrackList,
    pub grid_template_areas: Option<GridTemplateAreas>,
    pub grid_auto_columns: Vec<TrackSize>,
    pub grid_auto_rows: Vec<TrackSize>,
    pub grid_auto_flow: GridAutoFlow,
    pub grid_row_start: GridLine,
    pub grid_row_end: GridLine,
    pub grid_column_start: GridLine,
    pub grid_column_end: GridLine,
    pub table_layout: TableLayoutMode,
    pub border_collapse: BorderCollapse,
    pub border_spacing: (f32, f32),
    pub caption_side: CaptionSide,
    pub empty_cells: EmptyCells,
    pub font_family: Vec<String>,
    pub font_size: f32,
    pub font_weight: u16,
    pub font_style: FontStyle,
    pub color: Color,
    pub line_height: LineHeight,
    pub vertical_align: VerticalAlign,
    pub text_box_trim: TextBoxTrim,
    pub text_box_edge: TextBoxEdge,
    pub text_align: TextAlign,
    pub legacy_align: Option<LegacyAlign>,
    pub text_align_last: TextAlignLast,
    pub text_indent: LengthPercentage,
    pub white_space_collapse: WhiteSpaceCollapse,
    pub text_wrap_mode: TextWrapMode,
    pub text_wrap_style: TextWrapStyle,
    pub word_break: WordBreak,
    pub line_break: LineBreak,
    pub text_indent_hanging: bool,
    pub text_indent_each_line: bool,
    pub hyphens: Hyphens,
    pub hyphenate_character: Option<String>,
    /// The content language (`Style::locale`).
    pub locale: &'static str,
    pub overflow_wrap: OverflowWrap,
    pub text_overflow: TextOverflow,
    pub text_transform: TextTransform,
    pub letter_spacing: f32,
    pub word_spacing: f32,
    pub tab_size: TabSize,
    pub text_decoration_line: TextDecorationLine,
    pub text_decoration_style: TextDecorationStyle,
    pub text_decoration_color: Option<Color>,
    pub direction: Direction,
    pub unicode_bidi: UnicodeBidi,
    pub writing_mode: WritingMode,
    pub text_orientation: TextOrientation,
    pub text_combine_upright: bool,
    /// The view layout reads the box properties through (see [`Style::view`]).
    pub view: Flip,
    pub list_style_type: ListStyleType,
    pub list_style_position: ListStylePosition,
    pub list_style_image: Image,
    pub content: Content,
    pub object_fit: ObjectFit,
    pub object_view_box: Option<[LengthPercentage; 4]>,
    pub visibility: Visibility,
    pub opacity: f32,
    pub background_color: Color,
    pub background_layers: Vec<BackgroundLayer>,
    pub mask_layers: Vec<MaskLayer>,
    pub clip_path: Option<ClipPath>,
    /// `shape-outside` and `shape-margin`.
    pub shape_outside: Option<ShapeOutside>,
    pub shape_margin: LengthPercentage,
    /// `filter`'s functions.
    pub filters: Vec<FilterOp>,
    /// `backdrop-filter`'s functions.
    pub backdrop_filters: Vec<FilterOp>,
    pub box_shadows: Vec<Shadow>,
    /// `appearance` is not `none`.
    pub native_appearance: bool,
    /// The page sets a border or background property.
    pub author_border_background: bool,
    pub text_shadows: Vec<Shadow>,
    pub outline_width: f32,
    pub outline_style: BorderStyle,
    pub outline_color: Option<Color>,
    pub outline_offset: f32,
    /// A transform applied about the border box's center (`transform-origin: 50% 50%`).
    pub transform: Option<Matrix>,
    /// Some property other than a transform that makes the box the containing block of its fixed
    /// and absolutely positioned descendants (a filter, `contain: layout`, ...).
    pub fixed_cb: bool,
    pub pointer_events: bool,
    /// The used color scheme (inherited).
    pub color_scheme: ColorScheme,
    pub scrollbar_color: Option<ScrollbarColors>,
    pub scrollbar_width: ScrollbarWidth,
    pub custom_scrollbar: Option<CustomScrollbar>,
    pub resize: crate::style::Resize,
    pub overflow_clip_margin: OverflowClipMargin,
    pub line_clamp: Option<LineClamp>,
    pub column_width: Option<f32>,
    pub column_count: Option<u32>,
    pub column_fill_auto: bool,
    pub column_span_all: bool,
    pub column_rule_width: f32,
    pub column_rule_style: BorderStyle,
    pub column_rule_color: Option<Color>,
    pub break_before: BreakBetween,
    pub break_after: BreakBetween,
    pub break_inside: BreakInside,
    pub orphans: u32,
    pub widows: u32,
    pub scroll_padding: [LengthPercentage; 4],
    pub scroll_margin: [f32; 4],
    pub paint_containment: bool,
}

impl Default for TestStyle {
    fn default() -> TestStyle {
        TestStyle {
            display: Display::INLINE,
            original_display: None,
            position: Position::Static,
            float: Float::None,
            clear: Clear::None,
            inset: [LengthPercentageAuto::Auto; 4],
            z_index: None,
            box_sizing: BoxSizing::ContentBox,
            width: SizeValue::Auto,
            height: SizeValue::Auto,
            min_width: SizeValue::Auto,
            min_height: SizeValue::Auto,
            max_width: MaxSizeValue::None,
            max_height: MaxSizeValue::None,
            aspect_ratio: AspectRatio {
                auto: true,
                ratio: None,
            },
            size_containment: SizeContainment::default(),
            margin: [LengthPercentageAuto::ZERO; 4],
            padding: [LengthPercentage::ZERO; 4],
            border_width: [0.0; 4],
            border_style: [BorderStyle::None; 4],
            border_color: [None; 4],
            border_radius: [(LengthPercentage::ZERO, LengthPercentage::ZERO); 4],
            overflow_x: Overflow::Visible,
            overflow_y: Overflow::Visible,
            flex_direction: FlexDirection::Row,
            flex_wrap: FlexWrap::Nowrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: FlexBasis::Auto,
            order: 0,
            justify_content: Align::Normal,
            align_content: Align::Normal,
            align_items: Align::Normal,
            align_self: Align::Auto,
            justify_items: Align::Normal,
            justify_self: Align::Auto,
            align_safety: AlignSafety::default(),
            row_gap: None,
            column_gap: None,
            grid_template_columns: TrackList::default(),
            grid_template_rows: TrackList::default(),
            grid_template_areas: None,
            grid_auto_columns: Vec::new(),
            grid_auto_rows: Vec::new(),
            grid_auto_flow: GridAutoFlow::default(),
            grid_row_start: GridLine::Auto,
            grid_row_end: GridLine::Auto,
            grid_column_start: GridLine::Auto,
            grid_column_end: GridLine::Auto,
            table_layout: TableLayoutMode::Auto,
            border_collapse: BorderCollapse::Separate,
            border_spacing: (0.0, 0.0),
            caption_side: CaptionSide::Top,
            empty_cells: EmptyCells::Show,
            font_family: vec!["sans-serif".to_string()],
            font_size: 16.0,
            font_weight: 400,
            font_style: FontStyle::Normal,
            color: Color::BLACK,
            line_height: LineHeight::Normal,
            vertical_align: VerticalAlign::Baseline,
            text_box_trim: TextBoxTrim::default(),
            text_box_edge: TextBoxEdge::default(),
            text_align: TextAlign::Start,
            legacy_align: None,
            text_align_last: TextAlignLast::Auto,
            text_indent: LengthPercentage::ZERO,
            white_space_collapse: WhiteSpaceCollapse::Collapse,
            text_wrap_mode: TextWrapMode::Wrap,
            text_wrap_style: TextWrapStyle::Auto,
            word_break: WordBreak::Normal,
            line_break: LineBreak::Auto,
            text_indent_hanging: false,
            text_indent_each_line: false,
            hyphens: Hyphens::Manual,
            hyphenate_character: None,
            locale: "en-us",
            overflow_wrap: OverflowWrap::Normal,
            text_overflow: TextOverflow::Clip,
            text_transform: TextTransform::None,
            letter_spacing: 0.0,
            word_spacing: 0.0,
            tab_size: TabSize::Spaces(8.0),
            text_decoration_line: TextDecorationLine::NONE,
            text_decoration_style: TextDecorationStyle::Solid,
            text_decoration_color: None,
            direction: Direction::Ltr,
            unicode_bidi: UnicodeBidi::Normal,
            writing_mode: WritingMode::HorizontalTb,
            text_orientation: TextOrientation::Mixed,
            text_combine_upright: false,
            view: Flip::IDENTITY,
            list_style_type: ListStyleType::Disc,
            list_style_position: ListStylePosition::Outside,
            list_style_image: Image::None,
            content: Content::Normal,
            object_fit: ObjectFit::Fill,
            object_view_box: None,
            visibility: Visibility::Visible,
            opacity: 1.0,
            background_color: Color::TRANSPARENT,
            background_layers: Vec::new(),
            mask_layers: Vec::new(),
            clip_path: None,
            shape_outside: None,
            shape_margin: LengthPercentage::ZERO,
            filters: Vec::new(),
            backdrop_filters: Vec::new(),
            box_shadows: Vec::new(),
            native_appearance: false,
            author_border_background: false,
            text_shadows: Vec::new(),
            outline_width: 0.0,
            outline_style: BorderStyle::None,
            outline_color: None,
            outline_offset: 0.0,
            transform: None,
            fixed_cb: false,
            pointer_events: true,
            color_scheme: ColorScheme::Light,
            scrollbar_color: None,
            scrollbar_width: ScrollbarWidth::Auto,
            custom_scrollbar: None,
            resize: crate::style::Resize::None,
            overflow_clip_margin: OverflowClipMargin::default(),
            line_clamp: None,
            column_width: None,
            column_count: None,
            column_fill_auto: false,
            column_span_all: false,
            column_rule_width: 3.0,
            column_rule_style: BorderStyle::None,
            column_rule_color: None,
            break_before: BreakBetween::Auto,
            break_after: BreakBetween::Auto,
            break_inside: BreakInside::Auto,
            orphans: 2,
            widows: 2,
            scroll_padding: [LengthPercentage::ZERO; 4],
            scroll_margin: [0.0; 4],
            paint_containment: false,
        }
    }
}

impl TestStyle {
    /// A style whose inherited properties come from `parent` and the rest are initial.
    pub fn inherit(parent: &TestStyle) -> TestStyle {
        TestStyle {
            font_family: parent.font_family.clone(),
            font_size: parent.font_size,
            font_weight: parent.font_weight,
            font_style: parent.font_style,
            color: parent.color,
            line_height: parent.line_height,
            text_box_edge: parent.text_box_edge,
            text_align: parent.text_align,
            legacy_align: parent.legacy_align,
            text_align_last: parent.text_align_last,
            text_indent: parent.text_indent,
            white_space_collapse: parent.white_space_collapse,
            text_wrap_mode: parent.text_wrap_mode,
            text_wrap_style: parent.text_wrap_style,
            word_break: parent.word_break,
            line_break: parent.line_break,
            text_indent_hanging: parent.text_indent_hanging,
            text_indent_each_line: parent.text_indent_each_line,
            hyphens: parent.hyphens,
            hyphenate_character: parent.hyphenate_character.clone(),
            locale: parent.locale,
            overflow_wrap: parent.overflow_wrap,
            text_transform: parent.text_transform,
            letter_spacing: parent.letter_spacing,
            word_spacing: parent.word_spacing,
            tab_size: parent.tab_size,
            direction: parent.direction,
            writing_mode: parent.writing_mode,
            text_orientation: parent.text_orientation,
            text_combine_upright: parent.text_combine_upright,
            list_style_type: parent.list_style_type.clone(),
            list_style_position: parent.list_style_position,
            list_style_image: parent.list_style_image.clone(),
            border_collapse: parent.border_collapse,
            border_spacing: parent.border_spacing,
            caption_side: parent.caption_side,
            empty_cells: parent.empty_cells,
            visibility: parent.visibility,
            pointer_events: parent.pointer_events,
            color_scheme: parent.color_scheme,
            scrollbar_color: parent.scrollbar_color,
            ..TestStyle::default()
        }
    }

    /// Sets all four margins to `px`.
    pub fn margin_px(&mut self, px: f32) {
        self.margin = [LengthPercentageAuto::px(px); 4];
    }

    /// Sets all four paddings to `px`.
    pub fn padding_px(&mut self, px: f32) {
        self.padding = [LengthPercentage::px(px); 4];
    }

    /// A solid border of `width` px and `color` on every side.
    pub fn border(&mut self, width: f32, color: Color) {
        self.border_width = [width; 4];
        self.border_style = [BorderStyle::Solid; 4];
        self.border_color = [Some(color); 4];
    }

    fn side(side: Side) -> usize {
        match side {
            Side::Top => 0,
            Side::Right => 1,
            Side::Bottom => 2,
            Side::Left => 3,
        }
    }
}

impl Style for Arc<TestStyle> {
    fn display(&self) -> Display {
        self.display
    }
    fn original_display(&self) -> Display {
        self.original_display.unwrap_or(self.display)
    }
    fn position(&self) -> Position {
        self.position
    }
    fn float(&self) -> Float {
        self.float
    }
    fn clear(&self) -> Clear {
        self.clear
    }
    fn phys_inset(&self, side: Side) -> LengthPercentageAuto {
        self.inset[TestStyle::side(side)]
    }
    fn z_index(&self) -> Option<i32> {
        self.z_index
    }
    fn box_sizing(&self) -> BoxSizing {
        self.box_sizing
    }
    fn phys_width(&self) -> SizeValue {
        self.width
    }
    fn phys_height(&self) -> SizeValue {
        self.height
    }
    fn phys_min_width(&self) -> SizeValue {
        self.min_width
    }
    fn phys_min_height(&self) -> SizeValue {
        self.min_height
    }
    fn phys_max_width(&self) -> MaxSizeValue {
        self.max_width
    }
    fn phys_max_height(&self) -> MaxSizeValue {
        self.max_height
    }
    fn phys_aspect_ratio(&self) -> AspectRatio {
        self.aspect_ratio
    }
    fn phys_size_containment(&self) -> SizeContainment {
        self.size_containment
    }
    fn phys_margin(&self, side: Side) -> LengthPercentageAuto {
        self.margin[TestStyle::side(side)]
    }
    fn phys_padding(&self, side: Side) -> LengthPercentage {
        self.padding[TestStyle::side(side)]
    }
    fn phys_border_width(&self, side: Side) -> f32 {
        let i = TestStyle::side(side);
        if self.border_style[i].is_invisible() {
            0.0
        } else {
            self.border_width[i]
        }
    }
    fn phys_border_style(&self, side: Side) -> BorderStyle {
        self.border_style[TestStyle::side(side)]
    }
    fn phys_border_color(&self, side: Side) -> Color {
        self.border_color[TestStyle::side(side)].unwrap_or(self.color)
    }
    fn phys_border_radius(&self, corner: Corner) -> (LengthPercentage, LengthPercentage) {
        self.border_radius[match corner {
            Corner::TopLeft => 0,
            Corner::TopRight => 1,
            Corner::BottomRight => 2,
            Corner::BottomLeft => 3,
        }]
    }
    fn phys_overflow_x(&self) -> Overflow {
        self.overflow_x
    }
    fn phys_overflow_y(&self) -> Overflow {
        self.overflow_y
    }
    fn overflow_clip_margin(&self) -> OverflowClipMargin {
        self.overflow_clip_margin
    }
    fn line_clamp(&self) -> Option<LineClamp> {
        self.line_clamp.clone()
    }
    fn column_width(&self) -> Option<f32> {
        self.column_width
    }
    fn column_count(&self) -> Option<u32> {
        self.column_count
    }
    fn column_fill_auto(&self) -> bool {
        self.column_fill_auto
    }
    fn column_span_all(&self) -> bool {
        self.column_span_all
    }
    fn column_rule_width(&self) -> f32 {
        if self.column_rule_style.is_invisible() {
            0.0
        } else {
            self.column_rule_width
        }
    }
    fn column_rule_style(&self) -> BorderStyle {
        self.column_rule_style
    }
    fn column_rule_color(&self) -> Color {
        self.column_rule_color.unwrap_or(self.color)
    }
    fn break_before(&self) -> BreakBetween {
        self.break_before
    }
    fn break_after(&self) -> BreakBetween {
        self.break_after
    }
    fn break_inside(&self) -> BreakInside {
        self.break_inside
    }
    fn orphans(&self) -> u32 {
        self.orphans
    }
    fn widows(&self) -> u32 {
        self.widows
    }
    fn scroll_padding(&self, side: Side) -> LengthPercentage {
        self.scroll_padding[side as usize]
    }
    fn scroll_margin(&self, side: Side) -> f32 {
        self.scroll_margin[side as usize]
    }
    fn paint_containment(&self) -> bool {
        self.paint_containment
    }
    fn flex_direction(&self) -> FlexDirection {
        self.flex_direction
    }
    fn flex_wrap(&self) -> FlexWrap {
        self.flex_wrap
    }
    fn flex_grow(&self) -> f32 {
        self.flex_grow
    }
    fn flex_shrink(&self) -> f32 {
        self.flex_shrink
    }
    fn flex_basis(&self) -> FlexBasis {
        self.flex_basis
    }
    fn order(&self) -> i32 {
        self.order
    }
    fn justify_content(&self) -> Align {
        self.justify_content
    }
    fn align_content(&self) -> Align {
        self.align_content
    }
    fn align_items(&self) -> Align {
        self.align_items
    }
    fn align_self(&self) -> Align {
        self.align_self
    }
    fn justify_items(&self) -> Align {
        self.justify_items
    }
    fn justify_self(&self) -> Align {
        self.justify_self
    }
    fn align_safety(&self) -> AlignSafety {
        self.align_safety
    }
    fn row_gap(&self) -> Option<LengthPercentage> {
        self.row_gap
    }
    fn column_gap(&self) -> Option<LengthPercentage> {
        self.column_gap
    }
    fn grid_template_columns(&self) -> Cow<'_, TrackList> {
        Cow::Borrowed(&self.grid_template_columns)
    }
    fn grid_template_rows(&self) -> Cow<'_, TrackList> {
        Cow::Borrowed(&self.grid_template_rows)
    }
    fn grid_template_areas(&self) -> Option<Cow<'_, GridTemplateAreas>> {
        self.grid_template_areas.as_ref().map(Cow::Borrowed)
    }
    fn grid_auto_columns(&self) -> Cow<'_, [TrackSize]> {
        Cow::Borrowed(&self.grid_auto_columns)
    }
    fn grid_auto_rows(&self) -> Cow<'_, [TrackSize]> {
        Cow::Borrowed(&self.grid_auto_rows)
    }
    fn grid_auto_flow(&self) -> GridAutoFlow {
        self.grid_auto_flow
    }
    fn grid_row_start(&self) -> GridLine {
        self.grid_row_start.clone()
    }
    fn grid_row_end(&self) -> GridLine {
        self.grid_row_end.clone()
    }
    fn grid_column_start(&self) -> GridLine {
        self.grid_column_start.clone()
    }
    fn grid_column_end(&self) -> GridLine {
        self.grid_column_end.clone()
    }
    fn table_layout(&self) -> TableLayoutMode {
        self.table_layout
    }
    fn border_collapse(&self) -> BorderCollapse {
        self.border_collapse
    }
    fn phys_border_spacing(&self) -> (f32, f32) {
        self.border_spacing
    }
    fn caption_side(&self) -> CaptionSide {
        self.caption_side
    }
    fn empty_cells(&self) -> EmptyCells {
        self.empty_cells
    }
    fn font_family(&self) -> impl Iterator<Item = &str> + '_ {
        self.font_family.iter().map(String::as_str)
    }
    fn font_size(&self) -> f32 {
        self.font_size
    }
    fn font_weight(&self) -> u16 {
        self.font_weight
    }
    fn font_style(&self) -> FontStyle {
        self.font_style
    }
    fn font_key(&self) -> Option<(usize, usize)> {
        // A test style is immutable behind its Arc: the Arc is the identity.
        Some((Arc::as_ptr(self) as usize, 0))
    }
    fn color(&self) -> Color {
        self.color
    }
    fn line_height(&self) -> LineHeight {
        self.line_height
    }
    fn vertical_align(&self) -> VerticalAlign {
        self.vertical_align
    }
    fn text_box_trim(&self) -> TextBoxTrim {
        self.text_box_trim
    }
    fn text_box_edge(&self) -> TextBoxEdge {
        self.text_box_edge
    }
    fn text_align(&self) -> TextAlign {
        self.text_align
    }
    fn legacy_align(&self) -> Option<LegacyAlign> {
        self.legacy_align
    }

    fn text_align_last(&self) -> TextAlignLast {
        self.text_align_last
    }
    fn text_indent(&self) -> LengthPercentage {
        self.text_indent
    }
    fn white_space_collapse(&self) -> WhiteSpaceCollapse {
        self.white_space_collapse
    }
    fn text_wrap_mode(&self) -> TextWrapMode {
        self.text_wrap_mode
    }
    fn text_wrap_style(&self) -> TextWrapStyle {
        self.text_wrap_style
    }
    fn word_break(&self) -> WordBreak {
        self.word_break
    }
    fn line_break(&self) -> LineBreak {
        self.line_break
    }
    fn hyphens(&self) -> Hyphens {
        self.hyphens
    }
    fn text_indent_hanging(&self) -> bool {
        self.text_indent_hanging
    }
    fn text_indent_each_line(&self) -> bool {
        self.text_indent_each_line
    }
    fn hyphenate_character(&self) -> Option<String> {
        self.hyphenate_character.clone()
    }
    fn locale(&self) -> &str {
        self.locale
    }
    fn overflow_wrap(&self) -> OverflowWrap {
        self.overflow_wrap
    }
    fn text_overflow(&self) -> TextOverflow {
        self.text_overflow
    }
    fn text_transform(&self) -> TextTransform {
        self.text_transform
    }
    fn letter_spacing(&self) -> f32 {
        self.letter_spacing
    }
    fn word_spacing(&self) -> f32 {
        self.word_spacing
    }
    fn tab_size(&self) -> TabSize {
        self.tab_size
    }
    fn text_decoration_line(&self) -> TextDecorationLine {
        self.text_decoration_line
    }
    fn text_decoration_style(&self) -> TextDecorationStyle {
        self.text_decoration_style
    }
    fn text_decoration_color(&self) -> Color {
        self.text_decoration_color.unwrap_or(self.color)
    }
    fn direction(&self) -> Direction {
        self.direction
    }
    fn unicode_bidi(&self) -> UnicodeBidi {
        self.unicode_bidi
    }
    fn writing_mode(&self) -> WritingMode {
        self.writing_mode
    }
    fn text_orientation(&self) -> TextOrientation {
        self.text_orientation
    }
    fn text_combine_upright(&self) -> bool {
        self.text_combine_upright
    }
    fn same_inline_preparation(&self, other: &Self) -> bool {
        let (a, b) = (&**self, &**other);
        a.font_family == b.font_family
            && a.font_size == b.font_size
            && a.font_weight == b.font_weight
            && a.font_style == b.font_style
            && a.white_space_collapse == b.white_space_collapse
            && a.text_wrap_mode == b.text_wrap_mode
            && a.word_break == b.word_break
            && a.line_break == b.line_break
            && a.hyphens == b.hyphens
            && a.locale == b.locale
            && a.overflow_wrap == b.overflow_wrap
            && a.text_transform == b.text_transform
            && a.letter_spacing == b.letter_spacing
            && a.word_spacing == b.word_spacing
            && a.direction == b.direction
            && a.unicode_bidi == b.unicode_bidi
            && a.writing_mode == b.writing_mode
            && a.text_orientation == b.text_orientation
            && a.text_combine_upright == b.text_combine_upright
            && a.view == b.view
    }
    fn view(&self) -> Flip {
        self.view
    }
    fn with_view(&self, view: Flip) -> Self {
        let mut s = TestStyle::clone(self);
        s.view = view;
        Arc::new(s)
    }
    fn list_style_type(&self) -> ListStyleType {
        self.list_style_type.clone()
    }
    fn list_style_position(&self) -> ListStylePosition {
        self.list_style_position
    }
    fn list_style_image(&self) -> Image {
        self.list_style_image.clone()
    }
    fn content(&self) -> Content {
        self.content.clone()
    }
    fn object_fit(&self) -> ObjectFit {
        self.object_fit
    }
    fn object_view_box(&self) -> Option<[LengthPercentage; 4]> {
        self.object_view_box
    }
    fn visibility(&self) -> Visibility {
        self.visibility
    }
    fn opacity(&self) -> f32 {
        self.opacity
    }
    fn background_color(&self) -> Color {
        self.background_color
    }
    fn background_layer_count(&self) -> usize {
        self.background_layers.len()
    }
    fn mask_layer_count(&self) -> usize {
        self.mask_layers.len()
    }
    fn forces_stacking_context(&self) -> bool {
        // (a mask, a clipping path or a filter makes one: CSS Masking 1 §7, §5, Filter Effects 1
        // §5)
        !self.mask_layers.is_empty()
            || self.clip_path.is_some()
            || !self.filters.is_empty()
            || !self.backdrop_filters.is_empty()
    }
    fn clip_path(&self) -> Option<ClipPath> {
        self.clip_path.clone()
    }
    fn shape_outside(&self) -> Option<ShapeOutside> {
        self.shape_outside.clone()
    }
    fn shape_margin(&self) -> LengthPercentage {
        self.shape_margin
    }
    fn filter_ops(&self, _reference_box: Rect) -> Vec<FilterOp> {
        self.filters.clone()
    }
    fn has_filter(&self) -> bool {
        !self.filters.is_empty()
    }
    fn backdrop_filter_ops(&self, _reference_box: Rect) -> Vec<FilterOp> {
        self.backdrop_filters.clone()
    }
    fn forms_backdrop_root(&self) -> bool {
        self.clip_path.is_some() || !self.backdrop_filters.is_empty()
    }
    fn mask_layer(&self, index: usize) -> MaskLayer {
        self.mask_layers[index].clone()
    }
    fn background_layer(&self, index: usize) -> BackgroundLayer {
        self.background_layers[index].clone()
    }
    fn native_appearance(&self) -> bool {
        self.native_appearance
    }
    fn author_border_background(&self) -> bool {
        self.author_border_background
    }
    fn box_shadow_count(&self) -> usize {
        self.box_shadows.len()
    }
    fn box_shadow(&self, index: usize) -> Shadow {
        self.box_shadows[index]
    }
    fn text_shadow_count(&self) -> usize {
        self.text_shadows.len()
    }
    fn text_shadow(&self, index: usize) -> Shadow {
        self.text_shadows[index]
    }
    fn outline_width(&self) -> f32 {
        if self.outline_style.is_invisible() {
            0.0
        } else {
            self.outline_width
        }
    }
    fn outline_style(&self) -> BorderStyle {
        self.outline_style
    }
    fn outline_color(&self) -> Color {
        self.outline_color.unwrap_or(self.color)
    }
    fn outline_offset(&self) -> f32 {
        self.outline_offset
    }
    fn has_transform(&self) -> bool {
        self.transform.is_some()
    }
    fn establishes_fixed_cb(&self) -> bool {
        self.transform.is_some() || self.fixed_cb || !self.filters.is_empty()
    }
    fn phys_transform(&self, reference: Rect) -> Option<Matrix> {
        let m = self.transform?;
        let (cx, cy) = (
            reference.x + reference.width / 2.0,
            reference.y + reference.height / 2.0,
        );
        Some(
            Matrix::translation(-cx, -cy)
                .then(&m)
                .then(&Matrix::translation(cx, cy)),
        )
    }
    fn pointer_events(&self) -> bool {
        self.pointer_events
    }
    fn color_scheme(&self) -> ColorScheme {
        self.color_scheme
    }
    fn scrollbar_color(&self) -> Option<ScrollbarColors> {
        self.scrollbar_color
    }
    fn scrollbar_width(&self) -> ScrollbarWidth {
        self.scrollbar_width
    }
    fn custom_scrollbar(&self) -> Option<CustomScrollbar> {
        self.custom_scrollbar
    }
    fn resize(&self) -> crate::style::Resize {
        self.resize
    }
}

/// A [`StyleSource`] over explicit maps.
#[derive(Default)]
pub struct TestStyles {
    pub elements: HashMap<NodeId, Arc<TestStyle>>,
    /// Flat tree children that are not the DOM children (a shadow host's shadow tree), as the test
    /// sets them.
    pub flat: HashMap<NodeId, Vec<NodeId>>,
    pub pseudos: HashMap<(NodeId, PseudoElement), Arc<TestStyle>>,
    pub replaced: HashMap<NodeId, ReplacedContent>,
}

impl StyleSource for TestStyles {
    type Style = Arc<TestStyle>;

    fn element_style(&self, element: NodeId) -> Option<Arc<TestStyle>> {
        self.elements.get(&element).cloned()
    }

    fn flat_children(&self, parent: NodeId) -> Option<Vec<NodeId>> {
        self.flat.get(&parent).cloned()
    }

    fn pseudo_style(&self, element: NodeId, pseudo: PseudoElement) -> Option<Arc<TestStyle>> {
        if let Some(s) = self.pseudos.get(&(element, pseudo)) {
            return Some(s.clone());
        }
        // ::marker exists on every list item (css-lists-3 §3.1); its UA style is inherited plus
        // `unicode-bidi: isolate` and tabular numbers, which don't matter here.
        if pseudo == PseudoElement::Marker {
            let style = self.elements.get(&element)?;
            if let Display::Box {
                list_item: true, ..
            } = style.display
            {
                // The UA sheet: `::marker { white-space: pre; text-transform: none }`.
                let mut m = TestStyle::inherit(style);
                m.white_space_collapse = WhiteSpaceCollapse::Preserve;
                m.text_wrap_mode = TextWrapMode::Nowrap;
                m.text_transform = TextTransform::None;
                return Some(Arc::new(m));
            }
        }
        None
    }

    fn anonymous_style(&self, parent: &Arc<TestStyle>, kind: AnonymousBox) -> Arc<TestStyle> {
        let mut s = TestStyle::inherit(parent);
        s.display = match kind {
            AnonymousBox::Block => Display::BLOCK,
            AnonymousBox::Table { inline: false } => Display::TABLE,
            AnonymousBox::Table { inline: true } => Display::INLINE_TABLE,
            AnonymousBox::TableRow => Display::TABLE_ROW,
            AnonymousBox::TableCell => Display::TABLE_CELL,
        };
        Arc::new(s)
    }

    fn replaced_content(&self, element: NodeId) -> Option<ReplacedContent> {
        self.replaced.get(&element).cloned()
    }
}

/// A DOM and its styles built together. `root` is the `<html>` element (display block, the root style).
pub struct TestDocument {
    pub dom: Dom,
    pub document: NodeId,
    pub root: NodeId,
    pub styles: TestStyles,
}

impl Default for TestDocument {
    fn default() -> TestDocument {
        TestDocument::new()
    }
}

impl TestDocument {
    /// A document with an `<html>` root styled `display: block`.
    pub fn new() -> TestDocument {
        TestDocument::with_root_style(|s| s.display = Display::BLOCK)
    }

    pub fn with_root_style(f: impl FnOnce(&mut TestStyle)) -> TestDocument {
        let mut dom = Dom::new();
        let document = dom.create_html_document(None);
        // create_html_document makes html/head/body when given a title; with None only the document.
        let root = match dom.document_element(document) {
            Some(r) => r,
            None => {
                let r = dom.create_html_element(document, LocalName::from("html"));
                dom.append_child(document, r).expect("append root");
                r
            }
        };
        // Start from an empty root.
        while let Some(c) = dom.first_child(root) {
            dom.remove_node(c);
        }
        let mut style = TestStyle::default();
        f(&mut style);
        let mut styles = TestStyles::default();
        styles.elements.insert(root, Arc::new(style));
        TestDocument {
            dom,
            document,
            root,
            styles,
        }
    }

    /// The style of `node`.
    pub fn style(&self, node: NodeId) -> &TestStyle {
        &self.styles.elements[&node]
    }

    /// Appends an HTML element `tag` to `parent`, styled by `f` on top of the parent's inherited values.
    pub fn element(&mut self, parent: NodeId, tag: &str, f: impl FnOnce(&mut TestStyle)) -> NodeId {
        let el = self
            .dom
            .create_html_element(self.document, LocalName::from(tag));
        self.dom.append_child(parent, el).expect("append element");
        // (in a shadow tree, the top elements inherit from the host)
        let from = match self.dom.host(parent) {
            Some(host) if self.dom.is_shadow_root(parent) => host,
            _ => parent,
        };
        let mut style = TestStyle::inherit(&self.styles.elements[&from]);
        f(&mut style);
        self.styles.elements.insert(el, Arc::new(style));
        el
    }

    /// Appends a `display: block` element.
    pub fn block(&mut self, parent: NodeId, f: impl FnOnce(&mut TestStyle)) -> NodeId {
        self.element(parent, "div", |s| {
            s.display = Display::BLOCK;
            f(s)
        })
    }

    /// Appends an inline `<span>`.
    pub fn span(&mut self, parent: NodeId, f: impl FnOnce(&mut TestStyle)) -> NodeId {
        self.element(parent, "span", f)
    }

    /// Appends a text node.
    pub fn text(&mut self, parent: NodeId, data: &str) -> NodeId {
        let t = self.dom.create_text_node(self.document, data);
        self.dom.append_child(parent, t).expect("append text");
        t
    }

    /// Restyles `node`: `f` edits a copy of its current style.
    pub fn restyle(&mut self, node: NodeId, f: impl FnOnce(&mut TestStyle)) {
        let mut style = (*self.styles.elements[&node]).clone();
        f(&mut style);
        self.styles.elements.insert(node, Arc::new(style));
    }

    /// Gives `element` a pseudo-element style (`::before`, `::after`, `::marker`), inherited from it.
    pub fn pseudo(
        &mut self,
        element: NodeId,
        pseudo: PseudoElement,
        f: impl FnOnce(&mut TestStyle),
    ) {
        let mut style = TestStyle::inherit(&self.styles.elements[&element]);
        f(&mut style);
        self.styles
            .pseudos
            .insert((element, pseudo), Arc::new(style));
    }

    /// Makes `element` replaced, showing `content`.
    pub fn replace(&mut self, element: NodeId, content: ReplacedContent) {
        self.styles.replaced.insert(element, content);
    }
}
