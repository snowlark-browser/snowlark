// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! All of this crate's integration tests in one test binary (one link instead of 29 or more; each file
//! below is a module, so a test's name is `<file>::<test>`: `--test it csp::` runs one file's tests).
//! A new file in `tests/` needs one `mod` line here; `every_test_file_is_listed` fails if it is missing.
//! A test that needs a process of its own (process-wide state it changes) gets its own `[[test]]`
//! target in Cargo.toml instead.

mod accent_color;
mod animated_images;
mod audio_context;
mod audio_output;
mod begin_frames;
mod bench;
mod block_formatting;
mod browser_navigation;
mod cached_document;
mod channel;
mod clip_path;
mod close_watcher;
mod components;
mod compositor_animations;
mod console;
mod content_filter;
mod content_visibility;
mod cross_origin;
mod csp;
mod details;
mod devtools_node;
mod dialog;
mod dir_lang;
mod disabled_controls;
mod discarded_frames;
mod dispatch_path;
mod drag_and_drop;
mod editing;
mod elements_from_point;
mod eme;
mod exec_command;
mod fetch_body;
mod fetch_streaming;
mod filters;
mod find;
mod focus_chain;
mod focus_fixup;
mod focus_visible;
mod form_controls;
mod form_pseudo_classes;
mod forms;
mod fragments;
mod frames;
mod fullscreen;
mod gamepad;
mod gc;
mod geometry;
mod html_parsing_methods;
mod image_map;
mod ime;
mod indexeddb;
mod inert;
mod inline_formatting;
mod intersection;
mod keyboard_editing;
mod live_collections;
mod loading;
mod mask_composite;
mod media;
mod media_capabilities;
mod mouse_coordinates;
mod net_apis;
mod opener_policy;
mod page;
mod page_reachable_panics;
mod password_forms;
mod picker_activation;
mod pointer_boundary;
mod pointer_capture;
mod pointer_cursor;
mod popover;
mod popups;
mod press_outcome;
mod query_filter;
mod render;
mod resize;
mod script_animation_frames;
mod scroll_anchoring;
mod scroll_snap;
mod selector_queries;
mod session_history;
mod shadow_dom;
mod shadow_input;
mod shadow_render;
mod skip_ink;
mod storage_manager;
mod streams;
mod subtle;
mod svg_dom;
mod text_stroke;
mod timeline;
mod touch_events;
mod trusted_types;
mod user_agent;
mod web_audio;
mod web_storage;
mod webgl;
mod wheel_legacy;
mod xpath;

/// `autotests = false` means a file in `tests/` that is not a module above would never run.
#[test]
fn every_test_file_is_listed() {
    let listed = include_str!("it.rs");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        let Some(stem) = name.strip_suffix(".rs") else {
            continue;
        };
        if stem != "it" && !listed.lines().any(|l| l.trim() == format!("mod {stem};")) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "add `mod <name>;` to tests/it.rs for: {missing:?}"
    );
}
