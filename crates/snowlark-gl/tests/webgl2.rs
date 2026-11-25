// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! WebGL 2.0 contexts on the service: an OpenGL ES 3.0 context, vertex arrays and instanced draws,
//! immutable array textures, fences, uniform blocks, and pixels read back through a buffer.

use snowlark_gl::{Attributes, Backend, Command as C, Kind, Pixels, Query as Q, Reply, Service};

const COLOR: u32 = 0x4000;
const ARRAY_BUFFER: u32 = 0x8892;
const STATIC_DRAW: u32 = 0x88E4;
const STREAM_READ: u32 = 0x88E1;
const FLOAT: u32 = 0x1406;
const TRIANGLES: u32 = 0x0004;
const VERTEX_SHADER: u32 = 0x8B31;
const FRAGMENT_SHADER: u32 = 0x8B30;
const LINK_STATUS: u32 = 0x8B82;
const COMPILE_STATUS: u32 = 0x8B81;
const RGBA: u32 = 0x1908;
const RGBA8: u32 = 0x8058;
const UNSIGNED_BYTE: u32 = 0x1401;
const TEXTURE_2D_ARRAY: u32 = 0x8C1A;
const PIXEL_PACK_BUFFER: u32 = 0x88EB;
const UNIFORM_BUFFER: u32 = 0x8A11;
const VERSION: u32 = 0x1F02;
const ALREADY_SIGNALED: i32 = 0x911A;
const CONDITION_SATISFIED: i32 = 0x911C;
const SYNC_FLUSH_COMMANDS_BIT: u32 = 1;

fn service() -> Service {
    // (WARP: the same pixels on every machine)
    Service::new(true).expect("ANGLE (dev.ps1 copies it beside the test binary)")
}

fn webgl2() -> Attributes {
    Attributes {
        antialias: false,
        webgl2: true,
        ..Attributes::default()
    }
}

/// The pixel at (x, y) from the top of a readback.
fn pixel(rb: &snowlark_gl::Readback, x: u32, y: u32) -> [u8; 4] {
    let row = rb.height - 1 - y;
    let i = ((row * rb.width + x) * 4) as usize;
    rb.rgba[i..i + 4].try_into().unwrap()
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Compiles and links a GLSL ES 3.00 program (client ids 1, 2 and 3) and uses it.
fn program(s: &mut Service, vs: &str, fs: &str) {
    s.submit(vec![
        (1, C::CreateShader(1, VERTEX_SHADER)),
        (1, C::ShaderSource(1, vs.into())),
        (1, C::CompileShader(1)),
        (1, C::CreateShader(2, FRAGMENT_SHADER)),
        (1, C::ShaderSource(2, fs.into())),
        (1, C::CompileShader(2)),
        (1, C::Create(Kind::Program, 3)),
        (1, C::AttachShader(3, 1)),
        (1, C::AttachShader(3, 2)),
        (1, C::LinkProgram(3)),
        (1, C::UseProgram(3)),
    ]);
    assert_eq!(s.query(1, &Q::Shader(1, COMPILE_STATUS)), Reply::Int(1));
    assert_eq!(s.query(1, &Q::Shader(2, COMPILE_STATUS)), Reply::Int(1));
    assert_eq!(s.query(1, &Q::Program(3, LINK_STATUS)), Reply::Int(1));
}

#[test]
fn a_webgl2_context_is_opengl_es_3() {
    let mut s = service();
    s.create_context(1, &webgl2(), 2, 2).unwrap();
    let Reply::String(Some(v)) = s.query(1, &Q::String(VERSION)) else {
        panic!("no version");
    };
    assert!(v.starts_with("OpenGL ES 3."), "{v}");
    // GLSL ES 3.00 compiles; a WebGL 1.0 context would refuse it.
    program(
        &mut s,
        "#version 300 es\nin vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }",
        "#version 300 es\nprecision mediump float; out vec4 o; void main() { o = vec4(1.0); }",
    );
}

#[test]
fn instanced_draws_through_a_vertex_array() {
    let mut s = service();
    s.create_context(1, &webgl2(), 8, 8).unwrap();
    program(
        &mut s,
        "#version 300 es\nlayout(location = 0) in vec2 p; layout(location = 1) in float dx;\n\
         void main() { gl_Position = vec4(p.x + dx, p.y, 0.0, 1.0); }",
        "#version 300 es\nprecision mediump float; out vec4 o; void main() { o = vec4(0, 1, 0, 1); }",
    );
    // A quarter-width triangle at the left edge, drawn twice: shifted by 0 and by 1 (half the
    // buffer).
    s.submit(vec![
        (1, C::Create(Kind::VertexArray, 4)),
        (1, C::BindVertexArray(4)),
        (1, C::Create(Kind::Buffer, 5)),
        (1, C::BindBuffer(ARRAY_BUFFER, 5)),
        (
            1,
            C::BufferData(
                ARRAY_BUFFER,
                0,
                Some(floats(&[-1.0, -1.0, -0.5, -1.0, -1.0, 1.0])),
                STATIC_DRAW,
            ),
        ),
        (1, C::EnableVertexAttribArray(0)),
        (1, C::VertexAttribPointer(0, 2, FLOAT, false, 0, 0)),
        (1, C::Create(Kind::Buffer, 6)),
        (1, C::BindBuffer(ARRAY_BUFFER, 6)),
        (
            1,
            C::BufferData(ARRAY_BUFFER, 0, Some(floats(&[0.0, 1.0])), STATIC_DRAW),
        ),
        (1, C::EnableVertexAttribArray(1)),
        (1, C::VertexAttribPointer(1, 1, FLOAT, false, 0, 0)),
        (1, C::VertexAttribDivisor(1, 1)),
        // (unbinding the vertex array keeps its attributes)
        (1, C::BindVertexArray(0)),
        (1, C::BindVertexArray(4)),
        (1, C::DrawArraysInstanced(TRIANGLES, 0, 3, 2)),
    ]);
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    let rb = s.present(1).unwrap();
    // bottom-left of each copy is drawn; the middle of each half is not
    assert_eq!(pixel(&rb, 0, 7), [0, 255, 0, 255]);
    assert_eq!(pixel(&rb, 4, 7), [0, 255, 0, 255]);
    assert_eq!(pixel(&rb, 3, 7), [0, 0, 0, 0]);
    assert_eq!(pixel(&rb, 7, 7), [0, 0, 0, 0]);
}

#[test]
fn an_array_texture_layer_is_sampled_and_pixels_come_back_through_a_buffer() {
    let mut s = service();
    s.create_context(1, &webgl2(), 2, 2).unwrap();
    program(
        &mut s,
        "#version 300 es\nlayout(location = 0) in vec2 p;\n\
         void main() { gl_Position = vec4(p, 0.0, 1.0); }",
        "#version 300 es\nprecision mediump float; precision mediump sampler2DArray;\n\
         uniform sampler2DArray t; out vec4 o;\n\
         void main() { o = texture(t, vec3(0.5, 0.5, 1.0)); }",
    );
    // Two 1x1 layers: red, then blue; the shader samples layer 1.
    let mut layers = vec![255, 0, 0, 255];
    layers.extend([0, 0, 255, 255]);
    s.submit(vec![
        (1, C::Create(Kind::Texture, 4)),
        (1, C::BindTexture(TEXTURE_2D_ARRAY, 4)),
        (1, C::TexStorage3D(TEXTURE_2D_ARRAY, 1, RGBA8, 1, 1, 2)),
        (
            1,
            C::TexSubImage3D(
                TEXTURE_2D_ARRAY,
                0,
                0,
                0,
                0,
                1,
                1,
                2,
                RGBA,
                UNSIGNED_BYTE,
                Pixels::Data(layers),
            ),
        ),
        (1, C::TexParameteri(TEXTURE_2D_ARRAY, 0x2801, 0x2600)),
        (1, C::TexParameteri(TEXTURE_2D_ARRAY, 0x2800, 0x2600)),
        (1, C::Create(Kind::Buffer, 5)),
        (1, C::BindBuffer(ARRAY_BUFFER, 5)),
        (
            1,
            C::BufferData(
                ARRAY_BUFFER,
                0,
                Some(floats(&[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0])),
                STATIC_DRAW,
            ),
        ),
        (1, C::EnableVertexAttribArray(0)),
        (1, C::VertexAttribPointer(0, 2, FLOAT, false, 0, 0)),
        (1, C::DrawArrays(TRIANGLES, 0, 3)),
        // readPixels into a pixel pack buffer, a fence, then the buffer's bytes.
        (1, C::Create(Kind::Buffer, 6)),
        (1, C::BindBuffer(PIXEL_PACK_BUFFER, 6)),
        (1, C::BufferData(PIXEL_PACK_BUFFER, 16, None, STREAM_READ)),
        (1, C::ReadPixelsOffset(0, 0, 2, 2, RGBA, UNSIGNED_BYTE, 0)),
        (1, C::FenceSync(7)),
    ]);
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    let status = s
        .query(
            1,
            &Q::ClientWaitSync(7, SYNC_FLUSH_COMMANDS_BIT, 1_000_000_000),
        )
        .int();
    assert!(
        status == ALREADY_SIGNALED || status == CONDITION_SATISFIED,
        "{status:#x}"
    );
    let Reply::Bytes(px) = s.query(1, &Q::BufferSubData(PIXEL_PACK_BUFFER, 0, 16)) else {
        panic!("no bytes");
    };
    assert_eq!(&px[0..4], &[0, 0, 255, 255]);
    assert_eq!(&px[12..16], &[0, 0, 255, 255]);
    // The drawing buffer's readback is not sent into the bound pack buffer.
    let rb = s.present(1).unwrap();
    assert_eq!(pixel(&rb, 1, 1), [0, 0, 255, 255]);
    s.submit(vec![(1, C::Delete(Kind::Sync, 7))]);
    assert_eq!(s.query(1, &Q::SyncParameter(7, 0x9114)), Reply::None);
}

#[test]
fn a_uniform_block_is_fed_from_a_buffer_range() {
    let mut s = service();
    s.create_context(1, &webgl2(), 2, 2).unwrap();
    program(
        &mut s,
        "#version 300 es\nlayout(location = 0) in vec2 p;\n\
         void main() { gl_Position = vec4(p, 0.0, 1.0); }",
        "#version 300 es\nprecision mediump float; uniform Colors { vec4 c; }; out vec4 o;\n\
         void main() { o = c; }",
    );
    let block = s.query(1, &Q::UniformBlockIndex(3, "Colors".into())).int();
    assert_eq!(block, 0);
    assert_eq!(
        s.query(1, &Q::ActiveUniformBlockName(3, 0)),
        Reply::String(Some("Colors".into()))
    );
    // The block reads the second 256 bytes of the buffer (an offset every device aligns to).
    let mut data = floats(&[1.0, 0.0, 0.0, 1.0]);
    data.resize(256, 0);
    data.extend(floats(&[0.0, 1.0, 0.0, 1.0]));
    s.submit(vec![
        (1, C::UniformBlockBinding(3, 0, 2)),
        (1, C::Create(Kind::Buffer, 4)),
        (1, C::BindBuffer(UNIFORM_BUFFER, 4)),
        (1, C::BufferData(UNIFORM_BUFFER, 0, Some(data), STATIC_DRAW)),
        (1, C::BindBufferRange(UNIFORM_BUFFER, 2, 4, 256, 16)),
        (1, C::Create(Kind::Buffer, 5)),
        (1, C::BindBuffer(ARRAY_BUFFER, 5)),
        (
            1,
            C::BufferData(
                ARRAY_BUFFER,
                0,
                Some(floats(&[-1.0, -1.0, 3.0, -1.0, -1.0, 3.0])),
                STATIC_DRAW,
            ),
        ),
        (1, C::EnableVertexAttribArray(0)),
        (1, C::VertexAttribPointer(0, 2, FLOAT, false, 0, 0)),
        (1, C::ClearBufferfv(0x1800, 0, [0.0, 0.0, 1.0, 1.0])),
        (1, C::DrawArrays(TRIANGLES, 0, 3)),
    ]);
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    // UNIFORM_BUFFER_START and UNIFORM_BUFFER_SIZE of binding 2
    assert_eq!(
        s.query(1, &Q::IndexedInteger64(0x8A29, 2)),
        Reply::Int64s(vec![256])
    );
    assert_eq!(
        s.query(1, &Q::IndexedInteger64(0x8A2A, 2)),
        Reply::Int64s(vec![16])
    );
    assert_eq!(pixel(&s.present(1).unwrap(), 0, 0), [0, 255, 0, 255]);
    let _ = COLOR;
}
