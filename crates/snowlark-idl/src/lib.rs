// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark's Web IDL toolchain: a parser for the spec IDL under `idl/`, a linker that builds the model
//! (partials, mixins, typedefs, inheritance, exposure), and the generator that writes the V8 bindings in
//! `crates/snowlark-bindings/src/generated/`.

pub mod ast;
pub mod codegen;
pub mod model;
pub mod parser;
pub mod tool;
