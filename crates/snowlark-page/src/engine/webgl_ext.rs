// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL extensions (WebGL 1.0 §5.14.14, the Khronos WebGL extension registry): which ones a
//! context offers, the object `getExtension` returns for each (one per context and name), and the
//! functions of those that have any. An extension is offered only when ANGLE's context has, or
//! can turn on, the OpenGL ES extensions it is built on; `getExtension` turns them on, so a page
//! that never asks for an extension sees WebGL without it, as the registry requires.

use snowlark_bindings::generated::{self as g};
use snowlark_bindings::rt::{self, Cx, Handle, JsObject};
use snowlark_bindings::InterfaceId;
use snowlark_gl::{Command, Kind, Query, Reply};

use super::{c, ints, Engine, Object, WebGlObject};

/// A WebGL extension: its name, its interface, the context versions that have it, the OpenGL ES
/// extensions it needs (each entry one name or alternatives joined by `|`), and the ones its
/// object turns on (those of them the context offers).
pub struct ExtDef {
    pub name: &'static str,
    pub iface: InterfaceId,
    pub webgl1: bool,
    pub webgl2: bool,
    needs: &'static [&'static str],
    turns_on: &'static [&'static str],
}

const fn ext(
    name: &'static str,
    iface: InterfaceId,
    versions: u8,
    needs: &'static [&'static str],
    turns_on: &'static [&'static str],
) -> ExtDef {
    ExtDef {
        name,
        iface,
        webgl1: versions & 1 != 0,
        webgl2: versions & 2 != 0,
        needs,
        turns_on,
    }
}

const V1: u8 = 1;
const V2: u8 = 2;
const BOTH: u8 = 3;

use InterfaceId as I;

/// The extensions, in the order `getSupportedExtensions` lists them.
pub static EXTENSIONS: &[ExtDef] = &[
    ext(
        "ANGLE_instanced_arrays",
        I::ANGLE_instanced_arrays,
        V1,
        &["GL_ANGLE_instanced_arrays"],
        &["GL_ANGLE_instanced_arrays"],
    ),
    ext(
        "EXT_blend_minmax",
        I::EXT_blend_minmax,
        V1,
        &["GL_EXT_blend_minmax"],
        &["GL_EXT_blend_minmax"],
    ),
    ext(
        "EXT_clip_control",
        I::EXT_clip_control,
        BOTH,
        &["GL_EXT_clip_control"],
        &["GL_EXT_clip_control"],
    ),
    ext(
        "EXT_color_buffer_float",
        I::EXT_color_buffer_float,
        V2,
        &["GL_EXT_color_buffer_float"],
        &["GL_EXT_color_buffer_float"],
    ),
    ext(
        "EXT_color_buffer_half_float",
        I::EXT_color_buffer_half_float,
        BOTH,
        &["GL_EXT_color_buffer_half_float"],
        &["GL_EXT_color_buffer_half_float"],
    ),
    ext(
        "EXT_conservative_depth",
        I::EXT_conservative_depth,
        V2,
        &["GL_EXT_conservative_depth"],
        &["GL_EXT_conservative_depth"],
    ),
    ext(
        "EXT_depth_clamp",
        I::EXT_depth_clamp,
        BOTH,
        &["GL_EXT_depth_clamp"],
        &["GL_EXT_depth_clamp"],
    ),
    ext(
        "EXT_float_blend",
        I::EXT_float_blend,
        BOTH,
        &["GL_EXT_float_blend"],
        &["GL_EXT_float_blend"],
    ),
    ext(
        "EXT_frag_depth",
        I::EXT_frag_depth,
        V1,
        &["GL_EXT_frag_depth"],
        &["GL_EXT_frag_depth"],
    ),
    ext(
        "EXT_polygon_offset_clamp",
        I::EXT_polygon_offset_clamp,
        BOTH,
        &["GL_EXT_polygon_offset_clamp"],
        &["GL_EXT_polygon_offset_clamp"],
    ),
    ext(
        "EXT_render_snorm",
        I::EXT_render_snorm,
        V2,
        &["GL_EXT_render_snorm"],
        &["GL_EXT_render_snorm"],
    ),
    ext(
        "EXT_shader_texture_lod",
        I::EXT_shader_texture_lod,
        V1,
        &["GL_EXT_shader_texture_lod"],
        &["GL_EXT_shader_texture_lod"],
    ),
    ext(
        "EXT_sRGB",
        I::EXT_sRGB,
        V1,
        &["GL_EXT_sRGB"],
        &["GL_EXT_sRGB"],
    ),
    ext(
        "EXT_texture_compression_bptc",
        I::EXT_texture_compression_bptc,
        BOTH,
        &["GL_EXT_texture_compression_bptc"],
        &["GL_EXT_texture_compression_bptc"],
    ),
    ext(
        "EXT_texture_compression_rgtc",
        I::EXT_texture_compression_rgtc,
        BOTH,
        &["GL_EXT_texture_compression_rgtc"],
        &["GL_EXT_texture_compression_rgtc"],
    ),
    ext(
        "EXT_texture_filter_anisotropic",
        I::EXT_texture_filter_anisotropic,
        BOTH,
        &["GL_EXT_texture_filter_anisotropic"],
        &["GL_EXT_texture_filter_anisotropic"],
    ),
    ext(
        "EXT_texture_mirror_clamp_to_edge",
        I::EXT_texture_mirror_clamp_to_edge,
        BOTH,
        &["GL_EXT_texture_mirror_clamp_to_edge"],
        &["GL_EXT_texture_mirror_clamp_to_edge"],
    ),
    ext(
        "EXT_texture_norm16",
        I::EXT_texture_norm16,
        V2,
        &["GL_EXT_texture_norm16"],
        &["GL_EXT_texture_norm16"],
    ),
    ext(
        "KHR_parallel_shader_compile",
        I::KHR_parallel_shader_compile,
        BOTH,
        &["GL_KHR_parallel_shader_compile"],
        &["GL_KHR_parallel_shader_compile"],
    ),
    ext(
        "OES_draw_buffers_indexed",
        I::OES_draw_buffers_indexed,
        V2,
        &["GL_OES_draw_buffers_indexed"],
        &["GL_OES_draw_buffers_indexed"],
    ),
    ext(
        "OES_element_index_uint",
        I::OES_element_index_uint,
        V1,
        &["GL_OES_element_index_uint"],
        &["GL_OES_element_index_uint"],
    ),
    ext(
        "OES_fbo_render_mipmap",
        I::OES_fbo_render_mipmap,
        V1,
        &["GL_OES_fbo_render_mipmap"],
        &["GL_OES_fbo_render_mipmap"],
    ),
    ext(
        "OES_sample_variables",
        I::OES_sample_variables,
        V2,
        &["GL_OES_sample_variables"],
        &["GL_OES_sample_variables"],
    ),
    ext(
        "OES_shader_multisample_interpolation",
        I::OES_shader_multisample_interpolation,
        V2,
        &["GL_OES_shader_multisample_interpolation"],
        &["GL_OES_shader_multisample_interpolation"],
    ),
    ext(
        "OES_standard_derivatives",
        I::OES_standard_derivatives,
        V1,
        &["GL_OES_standard_derivatives"],
        &["GL_OES_standard_derivatives"],
    ),
    ext(
        "OES_texture_float",
        I::OES_texture_float,
        V1,
        &["GL_OES_texture_float"],
        &["GL_OES_texture_float"],
    ),
    ext(
        "OES_texture_float_linear",
        I::OES_texture_float_linear,
        BOTH,
        &["GL_OES_texture_float_linear"],
        &["GL_OES_texture_float_linear"],
    ),
    ext(
        "OES_texture_half_float",
        I::OES_texture_half_float,
        V1,
        &["GL_OES_texture_half_float"],
        &["GL_OES_texture_half_float"],
    ),
    ext(
        "OES_texture_half_float_linear",
        I::OES_texture_half_float_linear,
        V1,
        &["GL_OES_texture_half_float_linear"],
        &["GL_OES_texture_half_float_linear"],
    ),
    ext(
        "OES_vertex_array_object",
        I::OES_vertex_array_object,
        V1,
        &["GL_OES_vertex_array_object"],
        &["GL_OES_vertex_array_object"],
    ),
    ext(
        "OVR_multiview2",
        I::OVR_multiview2,
        V2,
        &["GL_OVR_multiview2"],
        &["GL_OVR_multiview2"],
    ),
    ext(
        "WEBGL_blend_func_extended",
        I::WEBGL_blend_func_extended,
        BOTH,
        &["GL_EXT_blend_func_extended"],
        &["GL_EXT_blend_func_extended"],
    ),
    ext(
        "WEBGL_clip_cull_distance",
        I::WEBGL_clip_cull_distance,
        V2,
        &["GL_ANGLE_clip_cull_distance"],
        &["GL_ANGLE_clip_cull_distance"],
    ),
    ext(
        "WEBGL_color_buffer_float",
        I::WEBGL_color_buffer_float,
        V1,
        &[
            "GL_OES_texture_float",
            "GL_CHROMIUM_color_buffer_float_rgba",
        ],
        &[
            "GL_CHROMIUM_color_buffer_float_rgba",
            "GL_CHROMIUM_color_buffer_float_rgb",
        ],
    ),
    ext(
        "WEBGL_compressed_texture_astc",
        I::WEBGL_compressed_texture_astc,
        BOTH,
        &["GL_KHR_texture_compression_astc_ldr"],
        &[
            "GL_KHR_texture_compression_astc_ldr",
            "GL_KHR_texture_compression_astc_hdr",
        ],
    ),
    ext(
        "WEBGL_compressed_texture_etc",
        I::WEBGL_compressed_texture_etc,
        BOTH,
        &["GL_ANGLE_compressed_texture_etc"],
        &["GL_ANGLE_compressed_texture_etc"],
    ),
    ext(
        "WEBGL_compressed_texture_etc1",
        I::WEBGL_compressed_texture_etc1,
        BOTH,
        &["GL_OES_compressed_ETC1_RGB8_texture"],
        &["GL_OES_compressed_ETC1_RGB8_texture"],
    ),
    ext(
        "WEBGL_compressed_texture_s3tc",
        I::WEBGL_compressed_texture_s3tc,
        BOTH,
        &[
            "GL_EXT_texture_compression_s3tc|GL_EXT_texture_compression_dxt1",
            "GL_EXT_texture_compression_s3tc|GL_ANGLE_texture_compression_dxt3",
            "GL_EXT_texture_compression_s3tc|GL_ANGLE_texture_compression_dxt5",
        ],
        &[
            "GL_EXT_texture_compression_s3tc",
            "GL_EXT_texture_compression_dxt1",
            "GL_ANGLE_texture_compression_dxt3",
            "GL_ANGLE_texture_compression_dxt5",
        ],
    ),
    ext(
        "WEBGL_compressed_texture_s3tc_srgb",
        I::WEBGL_compressed_texture_s3tc_srgb,
        BOTH,
        &["GL_EXT_texture_compression_s3tc_srgb"],
        &["GL_EXT_texture_compression_s3tc_srgb"],
    ),
    ext(
        "WEBGL_debug_renderer_info",
        I::WEBGL_debug_renderer_info,
        BOTH,
        &[],
        &[],
    ),
    ext(
        "WEBGL_depth_texture",
        I::WEBGL_depth_texture,
        V1,
        &["GL_ANGLE_depth_texture|GL_OES_depth_texture"],
        &[
            "GL_ANGLE_depth_texture",
            "GL_OES_depth_texture",
            "GL_OES_packed_depth_stencil",
        ],
    ),
    ext(
        "WEBGL_draw_buffers",
        I::WEBGL_draw_buffers,
        V1,
        &["GL_EXT_draw_buffers"],
        &["GL_EXT_draw_buffers"],
    ),
    // (needs nothing of the device: it loses and restores the context itself)
    ext("WEBGL_lose_context", I::WEBGL_lose_context, BOTH, &[], &[]),
    ext(
        "WEBGL_multi_draw",
        I::WEBGL_multi_draw,
        BOTH,
        &["GL_ANGLE_multi_draw"],
        &[
            "GL_ANGLE_instanced_arrays",
            "GL_ANGLE_multi_draw",
            "GL_WEBGL_multi_draw",
        ],
    ),
    ext(
        "WEBGL_polygon_mode",
        I::WEBGL_polygon_mode,
        BOTH,
        &["GL_ANGLE_polygon_mode"],
        &["GL_ANGLE_polygon_mode"],
    ),
    ext(
        "WEBGL_provoking_vertex",
        I::WEBGL_provoking_vertex,
        V2,
        &["GL_ANGLE_provoking_vertex"],
        &["GL_ANGLE_provoking_vertex"],
    ),
    ext(
        "WEBGL_render_shared_exponent",
        I::WEBGL_render_shared_exponent,
        V2,
        &["GL_QCOM_render_shared_exponent"],
        &["GL_QCOM_render_shared_exponent"],
    ),
    ext(
        "WEBGL_stencil_texturing",
        I::WEBGL_stencil_texturing,
        V2,
        &["GL_ANGLE_stencil_texturing"],
        &["GL_ANGLE_stencil_texturing"],
    ),
];

/// The index in [`EXTENSIONS`] of the extension named exactly `name`.
pub fn index_of(name: &str) -> usize {
    EXTENSIONS
        .iter()
        .position(|e| e.name == name)
        .expect("a known extension")
}

/// The OpenGL ES extensions of a context: those on and those it can turn on.
#[derive(Default)]
pub struct ExtState {
    on: Vec<String>,
    requestable: Vec<String>,
    /// The extension objects made so far: index in [`EXTENSIONS`], object.
    pub objects: Vec<(usize, Handle)>,
}

impl ExtState {
    pub fn new(on: Vec<String>, requestable: Vec<String>) -> ExtState {
        ExtState {
            on,
            requestable,
            objects: Vec::new(),
        }
    }

    fn offers(&self, gl: &str) -> bool {
        self.on.iter().any(|e| e == gl) || self.requestable.iter().any(|e| e == gl)
    }

    /// Whether `def` is offered in a context of this version.
    pub fn supports(&self, def: &ExtDef, webgl2: bool) -> bool {
        (if webgl2 { def.webgl2 } else { def.webgl1 })
            && def
                .needs
                .iter()
                .all(|n| n.split('|').any(|alt| self.offers(alt)))
    }

    /// The OpenGL ES extensions to request for `def`, marked on from now.
    fn turn_on(&mut self, def: &ExtDef) -> Vec<String> {
        let mut out = Vec::new();
        for name in def.turns_on {
            if let Some(i) = self.requestable.iter().position(|r| r == name) {
                let name = self.requestable.remove(i);
                self.on.push(name.clone());
                out.push(name);
            }
        }
        out
    }

    /// Whether the extension at `index` was asked for (`getExtension` returned its object).
    pub fn enabled(&self, index: usize) -> bool {
        self.objects.iter().any(|(i, _)| *i == index)
    }

    /// Whether the context offers an OpenGL ES extension.
    pub fn has(&self, gl: &str) -> bool {
        self.offers(gl)
    }
}

/// A marker in a vertex array object's `shader_type`: made by `OES_vertex_array_object`, so its
/// interface is `WebGLVertexArrayObjectOES`.
pub const OES_VERTEX_ARRAY: u32 = 1;

pub const UNMASKED_VENDOR_WEBGL: u32 = 0x9245;
pub const UNMASKED_RENDERER_WEBGL: u32 = 0x9246;
/// KHR_parallel_shader_compile's `getShaderParameter` / `getProgramParameter` name.
pub const COMPLETION_STATUS_KHR: u32 = 0x91B1;
/// OES_vertex_array_object's `getParameter` name.
pub const VERTEX_ARRAY_BINDING_OES: u32 = 0x85B5;

/// The graphics card's maker and model in ANGLE's renderer string, without the driver, Direct3D
/// and shader model details it adds: `ANGLE (Maker, Model (0x0000abcd) Direct3D11 vs_5_0 ps_5_0,
/// D3D11)` gives `("Maker", "Model")`, with `(R)` and `(TM)` marks dropped. A string not in that
/// form keeps its text up to the first parenthesis, and its maker is the one in `vendor`
/// (`Google Inc. (Maker)`).
pub fn trim_renderer(vendor: &str, renderer: &str) -> (String, String) {
    fn clean(s: &str) -> String {
        s.replace("(R)", "")
            .replace("(r)", "")
            .replace("(TM)", "")
            .replace("(tm)", "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
    if renderer.contains("SwiftShader") {
        return ("Google".into(), "SwiftShader".into());
    }
    let vendor_maker = match (vendor.find('('), vendor.rfind(')')) {
        (Some(a), Some(b)) if a < b => clean(&vendor[a + 1..b]),
        _ => clean(vendor),
    };
    let Some(inner) = renderer
        .strip_prefix("ANGLE (")
        .and_then(|s| s.strip_suffix(')'))
    else {
        let model = renderer.split(" (").next().unwrap_or(renderer);
        return (vendor_maker, clean(model));
    };
    let (maker, rest) = match inner.split_once(", ") {
        Some((m, r)) => (clean(m), r),
        None => (vendor_maker, inner),
    };
    // The model ends where ANGLE's additions start: the device id, the Direct3D version, the
    // shader models, or the backend after the last comma.
    let mut end = rest.rfind(", ").unwrap_or(rest.len());
    for marker in [" (0x", " Direct3D", " vs_", " ps_"] {
        if let Some(i) = rest.find(marker) {
            end = end.min(i);
        }
    }
    (maker, clean(&rest[..end]))
}

impl Engine {
    /// The context of an extension object.
    fn ext_ctx(&self, this: Handle) -> Handle {
        match &*self.obj(this) {
            Object::WebGl(o) => match &**o {
                WebGlObject::Extension { context, .. } => *context,
                _ => panic!("not a WebGL extension object"),
            },
            _ => panic!("not a WebGL extension object"),
        }
    }

    /// Whether the extension named `name` was asked for on the context `this`.
    pub(super) fn gl_ext_enabled(&self, this: Handle, name: &str) -> bool {
        let i = index_of(name);
        self.gl_ctx(this, |c| c.ext.enabled(i))
    }

    /// WebGL 1.0 §5.14.14 `getSupportedExtensions`.
    pub(super) fn gl_supported_extensions(&self, this: Handle) -> Vec<String> {
        self.gl_ctx(this, |c| {
            let v2 = c.attributes.webgl2;
            EXTENSIONS
                .iter()
                .filter(|d| c.ext.supports(d, v2))
                .map(|d| d.name.to_string())
                .collect()
        })
    }

    /// WebGL 1.0 §5.14.14 `getExtension`: the names are compared ASCII case-insensitively, and
    /// each call for one extension returns the same object.
    pub(super) fn gl_get_extension(
        &self,
        cx: &mut Cx,
        this: Handle,
        name: &str,
    ) -> rt::Result<Option<JsObject>> {
        if self.gl_lost(this) {
            return Ok(None);
        }
        let found = self.gl_ctx(this, |c| {
            let v2 = c.attributes.webgl2;
            let i = EXTENSIONS
                .iter()
                .position(|d| d.name.eq_ignore_ascii_case(name) && c.ext.supports(d, v2))?;
            let existing = c.ext.objects.iter().find(|(j, _)| *j == i).map(|x| x.1);
            Some((i, existing))
        });
        let Some((i, existing)) = found else {
            return Ok(None);
        };
        let h = match existing {
            Some(h) => h,
            None => {
                let requests = self.gl_ctx(this, |c| c.ext.turn_on(&EXTENSIONS[i]));
                for r in requests {
                    self.gl_send(this, Command::RequestExtension(r));
                }
                let generation = self.gl_ctx(this, |c| c.generation);
                let h = self.alloc(Object::WebGl(Box::new(WebGlObject::Extension {
                    context: this,
                    index: i,
                    generation,
                })));
                self.gl_ctx(this, |c| c.ext.objects.push((i, h)));
                h
            }
        };
        let scope = cx.scope();
        Ok(rt::wrap(scope, h).map(|w| JsObject::new(scope, w)))
    }

    /// WEBGL_debug_renderer_info's parameters: the graphics card's maker and model.
    pub(super) fn gl_unmasked(&self, this: Handle, pname: u32) -> Option<String> {
        let get = |p| match self.gl_query(this, Query::String(p)) {
            Some(Reply::String(s)) => s.unwrap_or_default(),
            _ => String::new(),
        };
        let (maker, model) = trim_renderer(&get(c::VENDOR), &get(c::RENDERER));
        Some(if pname == UNMASKED_VENDOR_WEBGL {
            maker
        } else {
            model
        })
    }

    fn multi_list(
        &self,
        cx: &mut Cx,
        ctx: Handle,
        list: &g::Int32ArrayOrLongSequence,
        offset: u64,
        drawcount: i32,
    ) -> Option<Vec<i32>> {
        let v = ints(cx, list);
        // WEBGL_multi_draw: the list must hold drawcount values from the offset.
        let end = offset.checked_add(drawcount as u64)?;
        if end > v.len() as u64 {
            self.gl_error(ctx, c::INVALID_OPERATION);
            return None;
        }
        Some(v[offset as usize..end as usize].to_vec())
    }

    #[allow(clippy::too_many_arguments)]
    fn multi_draw(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        first_or_counts: (&g::Int32ArrayOrLongSequence, u64),
        counts_or_offsets: (&g::Int32ArrayOrLongSequence, u64),
        elements_type: Option<u32>,
        instances: Option<(&g::Int32ArrayOrLongSequence, u64)>,
        drawcount: i32,
    ) {
        let ctx = self.ext_ctx(this);
        if self.gl_lost(ctx) {
            return;
        }
        if drawcount < 0 {
            return self.gl_error(ctx, c::INVALID_VALUE);
        }
        let Some(a) = self.multi_list(cx, ctx, first_or_counts.0, first_or_counts.1, drawcount)
        else {
            return;
        };
        let Some(b) = self.multi_list(cx, ctx, counts_or_offsets.0, counts_or_offsets.1, drawcount)
        else {
            return;
        };
        let inst = match instances {
            Some((l, o)) => match self.multi_list(cx, ctx, l, o, drawcount) {
                Some(v) => Some(v),
                None => return,
            },
            None => None,
        };
        let cmd = match elements_type {
            None => Command::MultiDrawArrays(mode, a, b, inst),
            Some(ty) => {
                Command::MultiDrawElements(mode, a, ty, b.iter().map(|o| *o as i64).collect(), inst)
            }
        };
        self.gl_send(ctx, cmd);
        self.gl_drew(ctx);
    }
}

impl g::ANGLE_instanced_arraysImpl for Engine {
    fn draw_arrays_instanced_angle(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        first: i32,
        count: i32,
        primcount: i32,
    ) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        g::WebGL2RenderingContextBaseImpl::draw_arrays_instanced(
            self, cx, ctx, mode, first, count, primcount,
        )
    }
    fn draw_elements_instanced_angle(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        count: i32,
        type_: u32,
        offset: i64,
        primcount: i32,
    ) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        g::WebGL2RenderingContextBaseImpl::draw_elements_instanced(
            self, cx, ctx, mode, count, type_, offset, primcount,
        )
    }
    fn vertex_attrib_divisor_angle(
        &self,
        cx: &mut Cx,
        this: Handle,
        index: u32,
        divisor: u32,
    ) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        g::WebGL2RenderingContextBaseImpl::vertex_attrib_divisor(self, cx, ctx, index, divisor)
    }
}

impl g::OES_vertex_array_objectImpl for Engine {
    fn create_vertex_array_oes(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Handle> {
        let ctx = self.ext_ctx(this);
        Ok(self.gl_create(ctx, Kind::VertexArray, OES_VERTEX_ARRAY))
    }
    fn delete_vertex_array_oes(
        &self,
        cx: &mut Cx,
        this: Handle,
        array_object: Option<Handle>,
    ) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        // Deleting the bound vertex array binds the default one first (OES_vertex_array_object).
        if array_object.is_some() && self.gl_vertex_array(ctx) == array_object {
            g::WebGL2RenderingContextBaseImpl::bind_vertex_array(self, cx, ctx, None)?;
        }
        g::WebGL2RenderingContextBaseImpl::delete_vertex_array(self, cx, ctx, array_object)
    }
    fn is_vertex_array_oes(
        &self,
        cx: &mut Cx,
        this: Handle,
        array_object: Option<Handle>,
    ) -> rt::Result<bool> {
        let ctx = self.ext_ctx(this);
        g::WebGL2RenderingContextBaseImpl::is_vertex_array(self, cx, ctx, array_object)
    }
    fn bind_vertex_array_oes(
        &self,
        cx: &mut Cx,
        this: Handle,
        array_object: Option<Handle>,
    ) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        g::WebGL2RenderingContextBaseImpl::bind_vertex_array(self, cx, ctx, array_object)
    }
}

impl g::WEBGL_draw_buffersImpl for Engine {
    fn draw_buffers_webgl(&self, cx: &mut Cx, this: Handle, buffers: Vec<u32>) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        g::WebGL2RenderingContextBaseImpl::draw_buffers(self, cx, ctx, buffers)
    }
}

impl g::WEBGL_multi_drawImpl for Engine {
    fn multi_draw_arrays_webgl(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        firsts_list: g::Int32ArrayOrLongSequence,
        firsts_offset: u64,
        counts_list: g::Int32ArrayOrLongSequence,
        counts_offset: u64,
        drawcount: i32,
    ) -> rt::Result<()> {
        self.multi_draw(
            cx,
            this,
            mode,
            (&firsts_list, firsts_offset),
            (&counts_list, counts_offset),
            None,
            None,
            drawcount,
        );
        Ok(())
    }
    fn multi_draw_elements_webgl(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        counts_list: g::Int32ArrayOrLongSequence,
        counts_offset: u64,
        type_: u32,
        offsets_list: g::Int32ArrayOrLongSequence,
        offsets_offset: u64,
        drawcount: i32,
    ) -> rt::Result<()> {
        self.multi_draw(
            cx,
            this,
            mode,
            (&counts_list, counts_offset),
            (&offsets_list, offsets_offset),
            Some(type_),
            None,
            drawcount,
        );
        Ok(())
    }
    fn multi_draw_arrays_instanced_webgl(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        firsts_list: g::Int32ArrayOrLongSequence,
        firsts_offset: u64,
        counts_list: g::Int32ArrayOrLongSequence,
        counts_offset: u64,
        instance_counts_list: g::Int32ArrayOrLongSequence,
        instance_counts_offset: u64,
        drawcount: i32,
    ) -> rt::Result<()> {
        self.multi_draw(
            cx,
            this,
            mode,
            (&firsts_list, firsts_offset),
            (&counts_list, counts_offset),
            None,
            Some((&instance_counts_list, instance_counts_offset)),
            drawcount,
        );
        Ok(())
    }
    fn multi_draw_elements_instanced_webgl(
        &self,
        cx: &mut Cx,
        this: Handle,
        mode: u32,
        counts_list: g::Int32ArrayOrLongSequence,
        counts_offset: u64,
        type_: u32,
        offsets_list: g::Int32ArrayOrLongSequence,
        offsets_offset: u64,
        instance_counts_list: g::Int32ArrayOrLongSequence,
        instance_counts_offset: u64,
        drawcount: i32,
    ) -> rt::Result<()> {
        self.multi_draw(
            cx,
            this,
            mode,
            (&counts_list, counts_offset),
            (&offsets_list, offsets_offset),
            Some(type_),
            Some((&instance_counts_list, instance_counts_offset)),
            drawcount,
        );
        Ok(())
    }
}

/// Extensions whose functions are one command each.
macro_rules! ext_command {
    ($this:ident, $self:ident, $cmd:expr) => {{
        let ctx = $self.ext_ctx($this);
        $self.gl_send(ctx, $cmd);
        Ok(())
    }};
}

impl g::EXT_clip_controlImpl for Engine {
    fn clip_control_ext(
        &self,
        _cx: &mut Cx,
        this: Handle,
        origin: u32,
        depth: u32,
    ) -> rt::Result<()> {
        ext_command!(this, self, Command::ClipControl(origin, depth))
    }
}

/// WEBGL_lose_context (the Khronos registry): the steps of a lost context, run on purpose.
impl g::WEBGL_lose_contextImpl for Engine {
    fn lose_context(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        self.webgl_lose_context(ctx);
        Ok(())
    }

    fn restore_context(&self, _cx: &mut Cx, this: Handle) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        self.webgl_restore_context(ctx);
        Ok(())
    }
}

impl g::WEBGL_polygon_modeImpl for Engine {
    fn polygon_mode_webgl(
        &self,
        _cx: &mut Cx,
        this: Handle,
        face: u32,
        mode: u32,
    ) -> rt::Result<()> {
        ext_command!(this, self, Command::PolygonMode(face, mode))
    }
}

impl g::WEBGL_provoking_vertexImpl for Engine {
    fn provoking_vertex_webgl(&self, _cx: &mut Cx, this: Handle, mode: u32) -> rt::Result<()> {
        ext_command!(this, self, Command::ProvokingVertex(mode))
    }
}

impl g::EXT_polygon_offset_clampImpl for Engine {
    fn polygon_offset_clamp_ext(
        &self,
        _cx: &mut Cx,
        this: Handle,
        factor: f32,
        units: f32,
        clamp: f32,
    ) -> rt::Result<()> {
        ext_command!(
            this,
            self,
            Command::PolygonOffsetClamp(factor, units, clamp)
        )
    }
}

impl g::OES_draw_buffers_indexedImpl for Engine {
    fn enablei_oes(&self, _cx: &mut Cx, this: Handle, target: u32, index: u32) -> rt::Result<()> {
        ext_command!(this, self, Command::EnableIndexed(target, index, true))
    }
    fn disablei_oes(&self, _cx: &mut Cx, this: Handle, target: u32, index: u32) -> rt::Result<()> {
        ext_command!(this, self, Command::EnableIndexed(target, index, false))
    }
    fn blend_equationi_oes(
        &self,
        _cx: &mut Cx,
        this: Handle,
        buf: u32,
        mode: u32,
    ) -> rt::Result<()> {
        ext_command!(this, self, Command::BlendEquationIndexed(buf, mode, mode))
    }
    fn blend_equation_separatei_oes(
        &self,
        _cx: &mut Cx,
        this: Handle,
        buf: u32,
        mode_rgb: u32,
        mode_alpha: u32,
    ) -> rt::Result<()> {
        ext_command!(
            this,
            self,
            Command::BlendEquationIndexed(buf, mode_rgb, mode_alpha)
        )
    }
    fn blend_funci_oes(
        &self,
        _cx: &mut Cx,
        this: Handle,
        buf: u32,
        src: u32,
        dst: u32,
    ) -> rt::Result<()> {
        ext_command!(
            this,
            self,
            Command::BlendFuncIndexed(buf, [src, dst, src, dst])
        )
    }
    fn blend_func_separatei_oes(
        &self,
        _cx: &mut Cx,
        this: Handle,
        buf: u32,
        src_rgb: u32,
        dst_rgb: u32,
        src_alpha: u32,
        dst_alpha: u32,
    ) -> rt::Result<()> {
        ext_command!(
            this,
            self,
            Command::BlendFuncIndexed(buf, [src_rgb, dst_rgb, src_alpha, dst_alpha])
        )
    }
    fn color_maski_oes(
        &self,
        _cx: &mut Cx,
        this: Handle,
        buf: u32,
        r: bool,
        g: bool,
        b: bool,
        a: bool,
    ) -> rt::Result<()> {
        ext_command!(this, self, Command::ColorMaskIndexed(buf, [r, g, b, a]))
    }
}

impl g::OVR_multiview2Impl for Engine {
    fn framebuffer_texture_multiview_ovr(
        &self,
        _cx: &mut Cx,
        this: Handle,
        target: u32,
        attachment: u32,
        texture: Option<Handle>,
        level: i32,
        base_view_index: i32,
        num_views: i32,
    ) -> rt::Result<()> {
        let ctx = self.ext_ctx(this);
        if self.gl_lost(ctx) {
            return Ok(());
        }
        let Some(tex) = self.gl_check_opt(ctx, texture, Kind::Texture) else {
            return Ok(());
        };
        self.gl_send(
            ctx,
            Command::FramebufferTextureMultiview(
                target,
                attachment,
                tex,
                level,
                base_view_index,
                num_views,
            ),
        );
        Ok(())
    }
}

impl g::WEBGL_compressed_texture_astcImpl for Engine {
    fn get_supported_profiles(&self, _cx: &mut Cx, this: Handle) -> rt::Result<Vec<String>> {
        let ctx = self.ext_ctx(this);
        let hdr = self.gl_ctx(ctx, |c| c.ext.has("GL_KHR_texture_compression_astc_hdr"));
        let mut v = vec!["ldr".to_string()];
        if hdr {
            v.push("hdr".into());
        }
        Ok(v)
    }
}

impl g::WebGLVertexArrayObjectOESImpl for Engine {}

/// The extensions with nothing but constants.
macro_rules! constant_only {
    ($($t:ident),* $(,)?) => {
        $(impl g::$t for Engine {})*
    };
}

constant_only!(
    EXT_blend_minmaxImpl,
    EXT_color_buffer_floatImpl,
    EXT_color_buffer_half_floatImpl,
    EXT_conservative_depthImpl,
    EXT_depth_clampImpl,
    EXT_float_blendImpl,
    EXT_frag_depthImpl,
    EXT_render_snormImpl,
    EXT_shader_texture_lodImpl,
    EXT_sRGBImpl,
    EXT_texture_compression_bptcImpl,
    EXT_texture_compression_rgtcImpl,
    EXT_texture_filter_anisotropicImpl,
    EXT_texture_mirror_clamp_to_edgeImpl,
    EXT_texture_norm16Impl,
    KHR_parallel_shader_compileImpl,
    OES_element_index_uintImpl,
    OES_fbo_render_mipmapImpl,
    OES_sample_variablesImpl,
    OES_shader_multisample_interpolationImpl,
    OES_standard_derivativesImpl,
    OES_texture_floatImpl,
    OES_texture_float_linearImpl,
    OES_texture_half_floatImpl,
    OES_texture_half_float_linearImpl,
    WEBGL_blend_func_extendedImpl,
    WEBGL_clip_cull_distanceImpl,
    WEBGL_color_buffer_floatImpl,
    WEBGL_compressed_texture_etcImpl,
    WEBGL_compressed_texture_etc1Impl,
    WEBGL_compressed_texture_s3tcImpl,
    WEBGL_compressed_texture_s3tc_srgbImpl,
    WEBGL_debug_renderer_infoImpl,
    WEBGL_depth_textureImpl,
    WEBGL_render_shared_exponentImpl,
    WEBGL_stencil_texturingImpl,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_angle_renderer_strings_to_maker_and_model() {
        let t = |v: &str, r: &str| trim_renderer(v, r);
        assert_eq!(
            t(
                "Google Inc. (NVIDIA)",
                "ANGLE (NVIDIA, NVIDIA GeForce GTX 1060 6GB (0x00001C03) Direct3D11 vs_5_0 ps_5_0, D3D11)"
            ),
            ("NVIDIA".into(), "NVIDIA GeForce GTX 1060 6GB".into())
        );
        assert_eq!(
            t(
                "Google Inc. (AMD)",
                "ANGLE (AMD, AMD Radeon RX 6800 XT (0x000073BF) Direct3D11 vs_5_0 ps_5_0, D3D11)"
            ),
            ("AMD".into(), "AMD Radeon RX 6800 XT".into())
        );
        assert_eq!(
            t(
                "Google Inc. (Intel)",
                "ANGLE (Intel, Intel(R) UHD Graphics 630 (0x00003E9B) Direct3D11 vs_5_0 ps_5_0, D3D11-27.20.100.8681)"
            ),
            ("Intel".into(), "Intel UHD Graphics 630".into())
        );
        assert_eq!(
            t(
                "Google Inc. (Microsoft)",
                "ANGLE (Microsoft, Microsoft Basic Render Driver (0x0000008C) Direct3D11 vs_5_0 ps_5_0, D3D11)"
            ),
            ("Microsoft".into(), "Microsoft Basic Render Driver".into())
        );
        assert_eq!(
            t(
                "Google Inc. (Google)",
                "ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero) (0x0000C0DE)), SwiftShader driver)"
            ),
            ("Google".into(), "SwiftShader".into())
        );
        // Older ANGLE: no device id, the Direct3D version right after the model.
        assert_eq!(
            t(
                "Google Inc.",
                "ANGLE (NVIDIA, NVIDIA Quadro P2000 Direct3D11 vs_5_0 ps_5_0)"
            ),
            ("NVIDIA".into(), "NVIDIA Quadro P2000".into())
        );
        // Not ANGLE's form: the text before any parenthesis, the maker from the vendor string.
        assert_eq!(
            t("Some Vendor", "Some GPU (driver 1.2)"),
            ("Some Vendor".into(), "Some GPU".into())
        );
    }

    #[test]
    fn an_extension_needs_its_gl_extensions_in_its_versions() {
        let s = ExtState::new(
            vec!["GL_ANGLE_instanced_arrays".into()],
            vec![
                "GL_EXT_texture_compression_dxt1".into(),
                "GL_ANGLE_texture_compression_dxt3".into(),
                "GL_ANGLE_texture_compression_dxt5".into(),
            ],
        );
        let def = |n| &EXTENSIONS[index_of(n)];
        assert!(s.supports(def("ANGLE_instanced_arrays"), false));
        assert!(!s.supports(def("ANGLE_instanced_arrays"), true));
        assert!(s.supports(def("WEBGL_compressed_texture_s3tc"), true));
        assert!(!s.supports(def("OES_texture_float"), false));
        assert!(s.supports(def("WEBGL_debug_renderer_info"), false));
    }
}
