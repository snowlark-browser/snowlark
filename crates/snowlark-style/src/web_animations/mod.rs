// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Web Animations 1 (https://drafts.csswg.org/web-animations-1/) on the style side: the timing model's
//! calculations ([`timing`]), easing functions ([`easing`]), keyframe processing ([`keyframes`]) and
//! script animations' place in the cascade ([`effects`]). The animation objects and their play state
//! machine belong to the page (`snowlark-page`'s `engine::animation`).

pub mod css;
pub mod easing;
pub mod effects;
pub mod keyframes;
pub mod timing;

pub use css::{CssAnimationEvent, CssEventKind, CssKey, CssTiming};
pub use easing::Easing;
pub use effects::{CompositorTiming, EffectSample, ScriptAnimations, ScriptCandidate};
pub use keyframes::{process_sequence, CompositeOperation, Keyframe, PropertyIndexed, RawKeyframe};
pub use timing::{ComputedTiming, FillMode, Phase, PlaybackDirection, Timing};
