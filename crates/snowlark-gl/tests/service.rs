// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The service on ANGLE: contexts, the drawing buffer, a triangle, readbacks, and what it does with
//! messages a client should never send.

use snowlark_gl::{Attributes, Backend, Command as C, Kind, Query as Q, Reply, Service};

const COLOR: u32 = 0x4000;
const ARRAY_BUFFER: u32 = 0x8892;
const STATIC_DRAW: u32 = 0x88E4;
const FLOAT: u32 = 0x1406;
const TRIANGLES: u32 = 0x0004;
const VERTEX_SHADER: u32 = 0x8B31;
const FRAGMENT_SHADER: u32 = 0x8B30;
const LINK_STATUS: u32 = 0x8B82;
const COMPILE_STATUS: u32 = 0x8B81;
const RGBA: u32 = 0x1908;
const UNSIGNED_BYTE: u32 = 0x1401;

fn service() -> Service {
    // (WARP: the same pixels on every machine)
    Service::new(true).expect("ANGLE (dev.ps1 copies it beside the test binary)")
}

fn plain() -> Attributes {
    Attributes {
        antialias: false,
        ..Attributes::default()
    }
}

/// The pixel at (x, y) from the top of a readback.
fn pixel(rb: &snowlark_gl::Readback, x: u32, y: u32) -> [u8; 4] {
    let row = rb.height - 1 - y;
    let i = ((row * rb.width + x) * 4) as usize;
    rb.rgba[i..i + 4].try_into().unwrap()
}

#[test]
fn a_cleared_drawing_buffer_is_presented_then_cleared() {
    let mut s = service();
    s.create_context(1, &plain(), 4, 3).unwrap();
    s.submit(vec![
        (1, C::ClearColor([1.0, 0.0, 0.0, 1.0])),
        (1, C::Clear(COLOR)),
    ]);
    let rb = s.present(1).unwrap();
    assert_eq!((rb.width, rb.height), (4, 3));
    assert_eq!(pixel(&rb, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&rb, 3, 2), [255, 0, 0, 255]);
    // WebGL 1.0 §2.2: not preserved, so cleared to transparent after presenting; the clear color
    // the page set is kept.
    let again = s.present(1).unwrap();
    assert_eq!(pixel(&again, 1, 1), [0, 0, 0, 0]);
    let color = s.query(1, &Q::Floats(0x0C22, 4));
    assert_eq!(color, Reply::Floats(vec![1.0, 0.0, 0.0, 1.0]));
}

#[test]
fn a_preserved_drawing_buffer_keeps_its_pixels() {
    let mut s = service();
    let a = Attributes {
        preserve_drawing_buffer: true,
        ..plain()
    };
    s.create_context(1, &a, 2, 2).unwrap();
    s.submit(vec![
        (1, C::ClearColor([0.0, 0.0, 1.0, 1.0])),
        (1, C::Clear(COLOR)),
    ]);
    s.present(1).unwrap();
    assert_eq!(pixel(&s.present(1).unwrap(), 1, 1), [0, 0, 255, 255]);
}

const VS: &str = "attribute vec2 p; void main() { gl_Position = vec4(p, 0.0, 1.0); }";
const FS: &str = "precision mediump float; uniform vec4 c; void main() { gl_FragColor = c; }";

/// A program drawing a triangle over the left half of the buffer, in the uniform color.
fn triangle(s: &mut Service) -> i32 {
    let verts: Vec<u8> = [-1.0f32, -1.0, 0.0, -1.0, -1.0, 1.0]
        .iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();
    s.submit(vec![
        (1, C::CreateShader(1, VERTEX_SHADER)),
        (1, C::ShaderSource(1, VS.into())),
        (1, C::CompileShader(1)),
        (1, C::CreateShader(2, FRAGMENT_SHADER)),
        (1, C::ShaderSource(2, FS.into())),
        (1, C::CompileShader(2)),
        (1, C::Create(Kind::Program, 3)),
        (1, C::AttachShader(3, 1)),
        (1, C::AttachShader(3, 2)),
        (1, C::BindAttribLocation(3, 0, "p".into())),
        (1, C::LinkProgram(3)),
        (1, C::UseProgram(3)),
        (1, C::Create(Kind::Buffer, 4)),
        (1, C::BindBuffer(ARRAY_BUFFER, 4)),
        (1, C::BufferData(ARRAY_BUFFER, 0, Some(verts), STATIC_DRAW)),
        (1, C::EnableVertexAttribArray(0)),
        (1, C::VertexAttribPointer(0, 2, FLOAT, false, 0, 0)),
    ]);
    assert_eq!(s.query(1, &Q::Shader(1, COMPILE_STATUS)), Reply::Int(1));
    assert_eq!(s.query(1, &Q::Program(3, LINK_STATUS)), Reply::Int(1));
    s.query(1, &Q::UniformLocation(3, "c".into())).int()
}

#[test]
fn a_triangle_draws_and_reads_back() {
    let mut s = service();
    s.create_context(1, &plain(), 8, 8).unwrap();
    let c = triangle(&mut s);
    assert!(c >= 0);
    s.submit(vec![
        (1, C::Uniformf(c, 4, vec![0.0, 1.0, 0.0, 1.0])),
        (1, C::DrawArrays(TRIANGLES, 0, 3)),
    ]);
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    // readPixels: the bottom-left pixel is inside the triangle, the top-right one is not.
    let Reply::Bytes(px) = s.query(1, &Q::ReadPixels(0, 0, 8, 8, RGBA, UNSIGNED_BYTE, 256)) else {
        panic!("no pixels");
    };
    assert_eq!(&px[0..4], &[0, 255, 0, 255]);
    let top_right = ((7 * 8) + 7) * 4;
    assert_eq!(&px[top_right..top_right + 4], &[0, 0, 0, 0]);
    let rb = s.present(1).unwrap();
    assert_eq!(pixel(&rb, 0, 7), [0, 255, 0, 255]);
    assert_eq!(pixel(&rb, 7, 0), [0, 0, 0, 0]);
}

#[test]
fn shaders_are_checked_as_webgl_requires() {
    let mut s = service();
    s.create_context(1, &plain(), 1, 1).unwrap();
    // WebGL 1.0 §6.20: identifiers starting with webgl_ are reserved.
    s.submit(vec![
        (1, C::CreateShader(1, VERTEX_SHADER)),
        (
            1,
            C::ShaderSource(
                1,
                "attribute vec4 webgl_x; void main() { gl_Position = webgl_x; }".into(),
            ),
        ),
        (1, C::CompileShader(1)),
    ]);
    assert_eq!(s.query(1, &Q::Shader(1, COMPILE_STATUS)), Reply::Int(0));
    let Reply::String(Some(log)) = s.query(1, &Q::ShaderInfoLog(1)) else {
        panic!("no log");
    };
    assert!(!log.is_empty());
}

#[test]
fn reads_are_bounded_by_the_client_buffer() {
    let mut s = service();
    s.create_context(1, &plain(), 8, 8).unwrap();
    // A buffer too small for the rectangle: ANGLE refuses (INVALID_OPERATION) and writes nothing.
    assert_eq!(
        s.query(1, &Q::ReadPixels(0, 0, 8, 8, RGBA, UNSIGNED_BYTE, 16)),
        Reply::None
    );
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0x0502));
}

#[test]
fn unknown_ids_and_malformed_commands_do_nothing() {
    let mut s = service();
    s.create_context(1, &plain(), 2, 2).unwrap();
    s.submit(vec![
        // ids the client never created, and a context that does not exist
        (1, C::BindBuffer(ARRAY_BUFFER, 99)),
        (1, C::Delete(Kind::Texture, 7)),
        (2, C::Clear(COLOR)),
        // uniform values that do not fill whole elements
        (1, C::Uniformf(0, 3, vec![1.0, 2.0])),
        (1, C::Uniformf(0, 9, vec![1.0; 9])),
        (1, C::UniformMatrix(0, 5, false, vec![0.0; 25])),
    ]);
    assert_eq!(s.query(2, &Q::Error), Reply::None);
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    assert!(s.present(2).is_none());
}

#[test]
fn contexts_keep_their_own_state_and_resize() {
    let mut s = service();
    s.create_context(1, &plain(), 2, 2).unwrap();
    s.create_context(2, &plain(), 3, 3).unwrap();
    s.submit(vec![
        (1, C::ClearColor([1.0, 1.0, 1.0, 1.0])),
        (2, C::ClearColor([0.0, 1.0, 0.0, 1.0])),
        (1, C::Clear(COLOR)),
        (2, C::Clear(COLOR)),
    ]);
    assert_eq!(pixel(&s.present(1).unwrap(), 0, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&s.present(2).unwrap(), 2, 2), [0, 255, 0, 255]);
    s.resize(1, 5, 4);
    let rb = s.present(1).unwrap();
    assert_eq!((rb.width, rb.height), (5, 4));
    assert_eq!(s.query(1, &Q::ResetStatus), Reply::Int(0));
}

#[test]
fn attributes_are_granted() {
    let mut s = service();
    let created = s
        .create_context(
            1,
            &Attributes {
                alpha: false,
                depth: false,
                stencil: true,
                ..plain()
            },
            2,
            2,
        )
        .unwrap();
    assert!(!created.attributes.alpha);
    assert!(created.attributes.stencil);
    assert!(created.limits.max_texture_size >= 2048);
    assert!(created.limits.max_vertex_attribs >= 8);
    // alpha: false reads back opaque
    s.submit(vec![
        (1, C::ClearColor([0.0, 0.0, 0.0, 0.0])),
        (1, C::Clear(COLOR)),
    ]);
    assert_eq!(pixel(&s.present(1).unwrap(), 0, 0)[3], 255);
}

#[test]
fn a_context_lists_the_extensions_it_can_turn_on() {
    let mut s = service();
    let created = s.create_context(1, &plain(), 2, 2).unwrap();
    if std::env::var_os("SNOWLARK_PRINT_GL_EXTENSIONS").is_some() {
        eprintln!(
            "on: {}\nrequestable: {}",
            created.extensions.join(" "),
            created.requestable.join(" ")
        );
    }
    // ANGLE's WebGL mode starts with the extensions off; the page's getExtension turns them on.
    assert!(created
        .requestable
        .iter()
        .any(|e| e == "GL_ANGLE_instanced_arrays"));
    s.submit(vec![(
        1,
        C::RequestExtension("GL_ANGLE_instanced_arrays".into()),
    )]);
    assert_eq!(s.query(1, &Q::Error), Reply::Int(0));
    let on = s.query(1, &Q::String(0x1F03));
    let Reply::String(Some(on)) = on else {
        panic!("GL_EXTENSIONS: {on:?}")
    };
    assert!(on.split(' ').any(|e| e == "GL_ANGLE_instanced_arrays"));
}
