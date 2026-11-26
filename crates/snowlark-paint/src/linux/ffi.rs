// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The parts of fontconfig and HarfBuzz the Linux text engine calls, declared from their public
//! headers (`fontconfig/fontconfig.h`, `hb.h`, `hb-ot.h`). Both are system libraries, linked
//! dynamically: fontconfig 2.13 or later, HarfBuzz 7.0 or later (`hb_font_draw_glyph`).

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

// ---- fontconfig ----

pub type FcBool = c_int;
pub type FcChar8 = u8;
pub type FcResult = c_int;
pub const FC_RESULT_MATCH: FcResult = 0;
pub const FC_MATCH_PATTERN: c_int = 0;
pub const FC_VALUE_BINDING_WEAK: c_int = 0;
pub const FC_TYPE_STRING: c_int = 3;

pub const FC_SLANT_ROMAN: c_int = 0;
pub const FC_SLANT_ITALIC: c_int = 100;
pub const FC_SLANT_OBLIQUE: c_int = 110;

#[repr(C)]
pub struct FcPattern {
    _private: [u8; 0],
}
#[repr(C)]
pub struct FcConfig {
    _private: [u8; 0],
}
#[repr(C)]
pub struct FcCharSet {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FcFontSet {
    pub nfont: c_int,
    pub sfont: c_int,
    pub fonts: *mut *mut FcPattern,
}

#[repr(C)]
pub struct FcMatrix {
    pub xx: f64,
    pub xy: f64,
    pub yx: f64,
    pub yy: f64,
}

/// `FcValue`: a type tag and a union whose largest member is a pointer or a double.
#[repr(C)]
pub struct FcValue {
    pub kind: c_int,
    pub u: FcValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FcValueUnion {
    pub s: *const FcChar8,
    pub i: c_int,
    pub d: f64,
}

pub const FC_FAMILY: &[u8] = b"family\0";
pub const FC_SLANT: &[u8] = b"slant\0";
pub const FC_WEIGHT: &[u8] = b"weight\0";
pub const FC_WIDTH: &[u8] = b"width\0";
pub const FC_FILE: &[u8] = b"file\0";
pub const FC_INDEX: &[u8] = b"index\0";
pub const FC_CHARSET: &[u8] = b"charset\0";
pub const FC_LANG: &[u8] = b"lang\0";
pub const FC_SCALABLE: &[u8] = b"scalable\0";
pub const FC_EMBOLDEN: &[u8] = b"embolden\0";
pub const FC_MATRIX: &[u8] = b"matrix\0";

#[link(name = "fontconfig")]
extern "C" {
    pub fn FcInitLoadConfigAndFonts() -> *mut FcConfig;
    pub fn FcConfigDestroy(config: *mut FcConfig);
    pub fn FcPatternCreate() -> *mut FcPattern;
    pub fn FcPatternDuplicate(p: *const FcPattern) -> *mut FcPattern;
    pub fn FcPatternDestroy(p: *mut FcPattern);
    pub fn FcPatternDel(p: *mut FcPattern, object: *const c_char) -> FcBool;
    pub fn FcPatternAddString(
        p: *mut FcPattern,
        object: *const c_char,
        s: *const FcChar8,
    ) -> FcBool;
    pub fn FcPatternAddInteger(p: *mut FcPattern, object: *const c_char, i: c_int) -> FcBool;
    pub fn FcPatternAddDouble(p: *mut FcPattern, object: *const c_char, d: f64) -> FcBool;
    pub fn FcPatternAddBool(p: *mut FcPattern, object: *const c_char, b: FcBool) -> FcBool;
    pub fn FcPatternGetString(
        p: *const FcPattern,
        object: *const c_char,
        n: c_int,
        s: *mut *mut FcChar8,
    ) -> FcResult;
    pub fn FcPatternGetInteger(
        p: *const FcPattern,
        object: *const c_char,
        n: c_int,
        i: *mut c_int,
    ) -> FcResult;
    pub fn FcPatternGetDouble(
        p: *const FcPattern,
        object: *const c_char,
        n: c_int,
        d: *mut f64,
    ) -> FcResult;
    pub fn FcPatternGetBool(
        p: *const FcPattern,
        object: *const c_char,
        n: c_int,
        b: *mut FcBool,
    ) -> FcResult;
    pub fn FcPatternGetCharSet(
        p: *const FcPattern,
        object: *const c_char,
        n: c_int,
        c: *mut *mut FcCharSet,
    ) -> FcResult;
    pub fn FcPatternGetMatrix(
        p: *const FcPattern,
        object: *const c_char,
        n: c_int,
        m: *mut *mut FcMatrix,
    ) -> FcResult;
    pub fn FcPatternGetWithBinding(
        p: *const FcPattern,
        object: *const c_char,
        id: c_int,
        v: *mut FcValue,
        b: *mut c_int,
    ) -> FcResult;
    pub fn FcConfigSubstitute(config: *mut FcConfig, p: *mut FcPattern, kind: c_int) -> FcBool;
    pub fn FcDefaultSubstitute(p: *mut FcPattern);
    pub fn FcFontMatch(
        config: *mut FcConfig,
        p: *mut FcPattern,
        result: *mut FcResult,
    ) -> *mut FcPattern;
    pub fn FcFontSort(
        config: *mut FcConfig,
        p: *mut FcPattern,
        trim: FcBool,
        csp: *mut *mut FcCharSet,
        result: *mut FcResult,
    ) -> *mut FcFontSet;
    pub fn FcFontSetDestroy(s: *mut FcFontSet);
    pub fn FcCharSetHasChar(c: *const FcCharSet, ucs4: u32) -> FcBool;
    pub fn FcWeightFromOpenTypeDouble(ot: f64) -> f64;
}

// ---- HarfBuzz ----

pub type hb_bool_t = c_int;
pub type hb_codepoint_t = u32;
pub type hb_position_t = i32;
pub type hb_tag_t = u32;
pub type hb_script_t = u32;
pub type hb_direction_t = c_uint;
pub type hb_language_t = *const c_void;

pub const HB_DIRECTION_LTR: hb_direction_t = 4;
pub const HB_DIRECTION_RTL: hb_direction_t = 5;
pub const HB_DIRECTION_TTB: hb_direction_t = 6;
pub const HB_BUFFER_FLAG_BOT: c_uint = 1;
pub const HB_BUFFER_FLAG_EOT: c_uint = 2;
pub const HB_MEMORY_MODE_READONLY: c_int = 1;

/// `HB_TAG('a','b','c','d')`.
pub const fn tag(t: &[u8; 4]) -> hb_tag_t {
    u32::from_be_bytes(*t)
}

#[repr(C)]
pub struct hb_blob_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct hb_face_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct hb_font_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct hb_buffer_t {
    _private: [u8; 0],
}
#[repr(C)]
pub struct hb_draw_funcs_t {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hb_feature_t {
    pub tag: hb_tag_t,
    pub value: u32,
    pub start: c_uint,
    pub end: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hb_variation_t {
    pub tag: hb_tag_t,
    pub value: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hb_glyph_info_t {
    pub codepoint: hb_codepoint_t,
    pub mask: u32,
    pub cluster: u32,
    pub var1: u32,
    pub var2: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hb_glyph_position_t {
    pub x_advance: hb_position_t,
    pub y_advance: hb_position_t,
    pub x_offset: hb_position_t,
    pub y_offset: hb_position_t,
    pub var: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct hb_glyph_extents_t {
    pub x_bearing: hb_position_t,
    pub y_bearing: hb_position_t,
    pub width: hb_position_t,
    pub height: hb_position_t,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct hb_ot_var_axis_info_t {
    pub axis_index: c_uint,
    pub tag: hb_tag_t,
    pub name_id: c_uint,
    pub flags: c_uint,
    pub min_value: f32,
    pub default_value: f32,
    pub max_value: f32,
    pub reserved: c_uint,
}

/// `hb_draw_state_t`: only the current point is read.
#[repr(C)]
pub struct hb_draw_state_t {
    pub path_open: hb_bool_t,
    pub path_start_x: f32,
    pub path_start_y: f32,
    pub current_x: f32,
    pub current_y: f32,
}

pub type hb_destroy_func_t = Option<unsafe extern "C" fn(*mut c_void)>;
pub type hb_draw_move_to_func_t = unsafe extern "C" fn(
    *mut hb_draw_funcs_t,
    *mut c_void,
    *mut hb_draw_state_t,
    f32,
    f32,
    *mut c_void,
);
pub type hb_draw_quadratic_to_func_t = unsafe extern "C" fn(
    *mut hb_draw_funcs_t,
    *mut c_void,
    *mut hb_draw_state_t,
    f32,
    f32,
    f32,
    f32,
    *mut c_void,
);
pub type hb_draw_cubic_to_func_t = unsafe extern "C" fn(
    *mut hb_draw_funcs_t,
    *mut c_void,
    *mut hb_draw_state_t,
    f32,
    f32,
    f32,
    f32,
    f32,
    f32,
    *mut c_void,
);
pub type hb_draw_close_path_func_t =
    unsafe extern "C" fn(*mut hb_draw_funcs_t, *mut c_void, *mut hb_draw_state_t, *mut c_void);

pub const HB_OT_METRICS_TAG_HORIZONTAL_ASCENDER: hb_tag_t = tag(b"hasc");
pub const HB_OT_METRICS_TAG_HORIZONTAL_DESCENDER: hb_tag_t = tag(b"hdsc");
pub const HB_OT_METRICS_TAG_HORIZONTAL_LINE_GAP: hb_tag_t = tag(b"hlgp");
pub const HB_OT_METRICS_TAG_X_HEIGHT: hb_tag_t = tag(b"xhgt");
pub const HB_OT_METRICS_TAG_CAP_HEIGHT: hb_tag_t = tag(b"cpht");
pub const HB_OT_METRICS_TAG_UNDERLINE_SIZE: hb_tag_t = tag(b"unds");
pub const HB_OT_METRICS_TAG_UNDERLINE_OFFSET: hb_tag_t = tag(b"undo");
pub const HB_OT_METRICS_TAG_STRIKEOUT_SIZE: hb_tag_t = tag(b"strs");
pub const HB_OT_METRICS_TAG_STRIKEOUT_OFFSET: hb_tag_t = tag(b"stro");
pub const HB_STYLE_TAG_WEIGHT: hb_tag_t = tag(b"wght");

#[link(name = "harfbuzz")]
extern "C" {
    pub fn hb_blob_create_from_file_or_fail(file_name: *const c_char) -> *mut hb_blob_t;
    pub fn hb_blob_create_or_fail(
        data: *const c_char,
        length: c_uint,
        mode: c_int,
        user_data: *mut c_void,
        destroy: hb_destroy_func_t,
    ) -> *mut hb_blob_t;
    pub fn hb_blob_get_data(blob: *mut hb_blob_t, length: *mut c_uint) -> *const c_char;
    pub fn hb_blob_reference(blob: *mut hb_blob_t) -> *mut hb_blob_t;
    pub fn hb_blob_destroy(blob: *mut hb_blob_t);

    pub fn hb_face_create(blob: *mut hb_blob_t, index: c_uint) -> *mut hb_face_t;
    pub fn hb_face_destroy(face: *mut hb_face_t);
    pub fn hb_face_get_upem(face: *const hb_face_t) -> c_uint;
    pub fn hb_face_get_glyph_count(face: *const hb_face_t) -> c_uint;

    pub fn hb_font_create(face: *mut hb_face_t) -> *mut hb_font_t;
    pub fn hb_font_destroy(font: *mut hb_font_t);
    pub fn hb_font_make_immutable(font: *mut hb_font_t);
    pub fn hb_font_set_var_named_instance(font: *mut hb_font_t, instance_index: c_uint);
    pub fn hb_font_set_variations(
        font: *mut hb_font_t,
        variations: *const hb_variation_t,
        variations_length: c_uint,
    );
    pub fn hb_font_get_var_coords_design(font: *mut hb_font_t, length: *mut c_uint) -> *const f32;
    pub fn hb_font_get_nominal_glyph(
        font: *mut hb_font_t,
        unicode: hb_codepoint_t,
        glyph: *mut hb_codepoint_t,
    ) -> hb_bool_t;
    pub fn hb_font_get_glyph_h_advance(
        font: *mut hb_font_t,
        glyph: hb_codepoint_t,
    ) -> hb_position_t;
    pub fn hb_font_get_glyph_extents(
        font: *mut hb_font_t,
        glyph: hb_codepoint_t,
        extents: *mut hb_glyph_extents_t,
    ) -> hb_bool_t;
    pub fn hb_font_draw_glyph(
        font: *mut hb_font_t,
        glyph: hb_codepoint_t,
        dfuncs: *mut hb_draw_funcs_t,
        draw_data: *mut c_void,
    );

    pub fn hb_ot_metrics_get_position(
        font: *mut hb_font_t,
        metrics_tag: hb_tag_t,
        position: *mut hb_position_t,
    ) -> hb_bool_t;
    pub fn hb_style_get_value(font: *mut hb_font_t, style_tag: hb_tag_t) -> f32;
    pub fn hb_ot_color_has_layers(face: *mut hb_face_t) -> hb_bool_t;
    pub fn hb_ot_color_has_paint(face: *mut hb_face_t) -> hb_bool_t;
    pub fn hb_ot_color_has_png(face: *mut hb_face_t) -> hb_bool_t;
    pub fn hb_ot_color_has_svg(face: *mut hb_face_t) -> hb_bool_t;
    pub fn hb_ot_name_get_utf8(
        face: *mut hb_face_t,
        name_id: c_uint,
        language: hb_language_t,
        text_size: *mut c_uint,
        text: *mut c_char,
    ) -> c_uint;
    pub fn hb_ot_layout_table_get_feature_tags(
        face: *mut hb_face_t,
        table_tag: hb_tag_t,
        start_offset: c_uint,
        feature_count: *mut c_uint,
        feature_tags: *mut hb_tag_t,
    ) -> c_uint;
    pub fn hb_ot_var_get_axis_infos(
        face: *mut hb_face_t,
        start_offset: c_uint,
        axes_count: *mut c_uint,
        axes_array: *mut hb_ot_var_axis_info_t,
    ) -> c_uint;

    pub fn hb_language_from_string(s: *const c_char, len: c_int) -> hb_language_t;
    pub fn hb_script_from_iso15924_tag(tag: hb_tag_t) -> hb_script_t;

    pub fn hb_buffer_create() -> *mut hb_buffer_t;
    pub fn hb_buffer_destroy(buffer: *mut hb_buffer_t);
    pub fn hb_buffer_reset(buffer: *mut hb_buffer_t);
    pub fn hb_buffer_add_utf8(
        buffer: *mut hb_buffer_t,
        text: *const c_char,
        text_length: c_int,
        item_offset: c_uint,
        item_length: c_int,
    );
    pub fn hb_buffer_set_direction(buffer: *mut hb_buffer_t, direction: hb_direction_t);
    pub fn hb_buffer_set_script(buffer: *mut hb_buffer_t, script: hb_script_t);
    pub fn hb_buffer_set_language(buffer: *mut hb_buffer_t, language: hb_language_t);
    pub fn hb_buffer_set_flags(buffer: *mut hb_buffer_t, flags: c_uint);
    pub fn hb_buffer_get_glyph_infos(
        buffer: *mut hb_buffer_t,
        length: *mut c_uint,
    ) -> *mut hb_glyph_info_t;
    pub fn hb_buffer_get_glyph_positions(
        buffer: *mut hb_buffer_t,
        length: *mut c_uint,
    ) -> *mut hb_glyph_position_t;
    pub fn hb_shape(
        font: *mut hb_font_t,
        buffer: *mut hb_buffer_t,
        features: *const hb_feature_t,
        num_features: c_uint,
    );

    pub fn hb_draw_funcs_create() -> *mut hb_draw_funcs_t;
    pub fn hb_draw_funcs_destroy(funcs: *mut hb_draw_funcs_t);
    pub fn hb_draw_funcs_make_immutable(funcs: *mut hb_draw_funcs_t);
    pub fn hb_draw_funcs_set_move_to_func(
        funcs: *mut hb_draw_funcs_t,
        func: hb_draw_move_to_func_t,
        user_data: *mut c_void,
        destroy: hb_destroy_func_t,
    );
    pub fn hb_draw_funcs_set_line_to_func(
        funcs: *mut hb_draw_funcs_t,
        func: hb_draw_move_to_func_t,
        user_data: *mut c_void,
        destroy: hb_destroy_func_t,
    );
    pub fn hb_draw_funcs_set_quadratic_to_func(
        funcs: *mut hb_draw_funcs_t,
        func: hb_draw_quadratic_to_func_t,
        user_data: *mut c_void,
        destroy: hb_destroy_func_t,
    );
    pub fn hb_draw_funcs_set_cubic_to_func(
        funcs: *mut hb_draw_funcs_t,
        func: hb_draw_cubic_to_func_t,
        user_data: *mut c_void,
        destroy: hb_destroy_func_t,
    );
    pub fn hb_draw_funcs_set_close_path_func(
        funcs: *mut hb_draw_funcs_t,
        func: hb_draw_close_path_func_t,
        user_data: *mut c_void,
        destroy: hb_destroy_func_t,
    );
}
