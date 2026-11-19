// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Snowlark's V8 bindings. `generated/` is written by `snowlark-idl gen` from the Web IDL and is the only
//! JS-visible surface; `rt` is the handwritten runtime it calls (conversions, wrappers, exceptions) and
//! the hooks the engine implements next to the per-interface traits.

pub mod rt;

#[allow(dead_code, non_snake_case, clippy::too_many_arguments)]
pub mod generated;

pub use generated::{install, Dom, InterfaceId};
