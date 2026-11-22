// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The Snowlark style system: stylo's DOM traits over `snowlark-dom`, stylesheets, the Stylist, the restyle
//! traversal and computed values.

mod animations;
pub use animations::{CompositorCandidate, ANIMATES_FILTER, ANIMATES_OPACITY, ANIMATES_TRANSFORM};
pub mod color_scheme;
pub mod computed;
pub mod cssom;
mod damage;
mod declarations;
mod engine;
mod handle;
mod hints;
pub mod i18n;
mod input_devices;
mod prefs;
mod sheets;
mod slots;
mod table;
mod traversal;
pub mod web_animations;

pub use damage::{
    classify, classify_pseudo, establishes_fixed_cb, Classification, ElementRestyle, Restyles,
    StyleChange,
};
pub use declarations::{
    supported_property_names, supports_condition, supports_declaration, DeclarationBlock,
};
pub use engine::{
    highlight_colors_declared, is_style_element, ColorScheme, DeclaredColor, DeviceConfig,
    MediaKind, Parallelism, RegisterPropertyError, StyleAttributeCheck, StyleEngine,
};
pub use handle::flat_parent;
pub use hints::{svg_presentation_declaration, svg_presentation_property};
pub use input_devices::InputDevices;
pub use prefs::init;
pub use sheets::{
    font_face_descriptor, parse_font_face, parse_standalone, parse_with_lock_of, FontFace,
    FontFaceHandler, FontFaceSource, ImportLoader, ImportRequest, ImportResponse, Sheet,
    SheetOrigin, QUIRKS_CSS, UA_CSS,
};
pub use table::StyleData;
pub use traversal::RestyleStats;

/// stylo's computed values and the property ids, re-exported so users of the crate need no stylo
/// dependency of their own for the common cases.
pub use style::properties::{ComputedValues, LonghandId, PropertyId, ShorthandId};
pub use style::selector_parser::PseudoElement;
pub use stylo_dom::ElementState;
