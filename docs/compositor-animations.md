# Transform and opacity animations on the compositor

Adopted 2026-11-09, after the problem was measured on a video site's watch page. Two problems, one design:

1. **Every CSS animation waits on the page thread.** `CompMsg` carries scrolling, display lists and video frames only, so
   a spinner stops whenever the page's script runs (YouTube's watch page while it loads: 10 to 20 frames a second).
2. **Every animation frame repaints the whole viewport.** A 64 px spinner costs a new display list and 9 tiles rastered,
   17 to 24 ms a frame on YouTube (`SNOWLARK_TAB_STATS=1`: "9 painted, 0 reused").

(A third cause, the 15.6 ms timer tick that held every page-thread animation to 32 frames a second, is fixed apart:
`snowlark-paint/src/timer_resolution.rs`.)

## What Chromium does

- **Which animations run there:** `CompositorAnimations::CheckCanStartEffectOnCompositor`
  (`third_party/blink/renderer/core/animation/compositor_animations.cc`): keyframe effects on `transform`, `translate`,
  `rotate`, `scale`, `opacity` (and `filter`, `backdrop-filter`, later `background-color`, `clip-path`), with keyframe
  values that are all of one type, supported timing parameters, the target in a valid compositing state (the exact
  list is in that function: read it before writing ours). Anything else runs on the main thread as now.
- **The layer:** a running compositable animation is a direct compositing reason (`compositing_reason_finder.cc`,
  `kActiveTransformAnimation`, `kActiveOpacityAnimation`), which gives the box its own transform or effect node in the
  property trees. Paint records chunks, each with its property tree state (transform, clip, effect);
  `PaintArtifactCompositor` (`platform/graphics/compositing/paint_artifact_compositor.cc`) merges chunks into cc layers
  and keeps a chunk under a directly composited node in a layer of its own. Content painted after it is a later layer,
  so paint order holds without repainting.
- **The animation:** Blink hands cc the keyframes and timing (`cc::KeyframeModel`, `cc::KeyframeEffect`); the compositor
  thread samples them at each BeginFrame (`cc::AnimationHost::TickAnimations`) and writes the animated node's value, so
  the layer moves with no main-thread frame. Blink keeps its own timeline for `getComputedStyle`, events and promises,
  and syncs the start time back (`NotifyAnimationStarted`).

## Ours, in stages

Each stage is merged alone, with tests that fail without it, before/after numbers in the window and the WPT areas
`css/css-animations css/css-transitions web-animations css/css-transforms` (no subtest lost).

**S1. Layers with a property state.** *Done 2026-11-09.* As built:
`LayerKind::Composited(element)` sits inside the box's own transform and opacity groups (so they are the innermost
steps of its state); `split_layers` gives every piece a `LayerState` and resumes the content after a split piece with
its groups opened again; plain pieces merge back as `PaintArtifactCompositor` merges; pieces at opacity 0 are left
out; tiles are kept per piece slot; a one-tile frame gets a texture of its size. Left for later: will-change boxes
are rastered at their first scale (no re-raster on a scale change), a fraction of a pixel in a translation-only
state is rounded at draw time (cc's raster translation is not done), and fixed or sticky content inside a composited
box with a state stays pinned. The design as written before it: `split_layers` today splits only top-level layer groups, and a layer is drawn as
tiles at an offset. Now: paint marks a box with a direct compositing reason (a running animation that passes the rules
above, or `will-change: transform | opacity`) with a new `LayerKind::Composited(key)` around its stacking context, and
paints its animated transform or opacity as a group of its own. `split_layers` also splits such a layer inside clip,
transform and opacity groups, and gives every piece its state: the stack of clips, transforms and opacities it sits
under, as data. A piece is rastered in its own space; the compositor draws it through its state (Direct2D transform,
clip and opacity layer). An animation frame still comes from the page, but changes only state: the pieces' lists are
equal, so their tiles are reused and nothing is rastered. Fixes problem 2. Display list: the new item updates encode,
decode, `validate` and the fuzz corpus (process isolation rule).

**S2. The compositor samples the animations.** *First half done 2026-11-09.* As built: `snowlark-paint/src/layer_animation.rs` (`LayerAnimation`: keyframe offsets, easings, values as opacities
or 2D transform function lists in px, Web Animations timing, the page timeline's origin as `Ticks`, and the box's
`pre`/`post` matrices around the list); `snowlark-render/src/compositor_animations.rs` turns stylo's CSS animations and
transitions into them (`StyleEngine::compositor_candidates` applies the eligibility rules below); paint pushes the box's
own transform and opacity groups always, `split_layers` marks the piece's state with `StateOp::Animated(key)` after
them, the frame's animations ride in the raster job head (checked in `join`, within `MAX_ANIMATION_BYTES`) to
`FrameData`, and the compositor rewrites the marked steps (`sampled_state`) at each display frame, drawing every frame
while one runs. *Second half, the page asks for no frames of them, done 2026-11-10:* the renderer
records what a layered frame sent (`StyleEngine::set_on_compositor`, after `fit_budget`); such an animation or
transition no longer counts for `animations_need_page_ticks` or the event tracker, and `compositor_next_change` gives
the page the time of its next change (its start, its end, its next iteration while the page has an
`animationiteration` listener), which `animations_frame_at` waits for, as do `CSSAnimation` objects of it. The page's
document times move once per task outside rendering updates (`advance_document_time`, Blink's `AnimationClock` with
its 1/60 s step; a task is one `Page::with`), and `with_style`/`with_layout` restyle CSS animations at that time first
(`Renderer::advance_timeline_for_script`). Fixed on the way, each a frame the loop used to run for nothing: a restyle
now makes an event scan due only when it started, canceled, paused or retimed an animation (it did after any
restyle), a scan outside the update's own keeps its events for the next update, and script effects mark the
rendering dirty only when there are any. *Script animations (`element.animate()`) done 2026-11-10:* the page gives each script effect's
sample a `CompositorTiming` (start time, playback rate, timing) when its animation could run there (Blink's
`Animation::CheckCanStartAnimationOnCompositorInternal` and `ConvertTimingForCompositor`: playing, on its target's
document timeline, no end delay, iterations above 0, a finite active duration backwards);
`ScriptAnimations::compositor_candidates` takes an effect whose properties are all `transform` or `opacity` with replace
keyframes; `StyleEngine::alone_on` applies `HasIncompatibleAnimations` across CSS and script effects, per property (a CSS
animation and a script one on different properties both go; before, any script effect kept the element's CSS ones on the
page). `LayerAnimation` carries the full Web Animations timing (playback rate, negative too, delay, iteration start, the
effect's easing with its before flag). The page asks for the update at the effect's next change
(`Animation::TimeToEffectChange` with `KeyframeEffect::CalculateTimeToEffectChange`), and a task that moves the
page's clock resamples script animations for style reads (`DocumentAnimations::UpdateAnimationTimingIfNeeded`).
Reversed iterations with a non-linear easing go since 2026-11-10. *An animation acts as
`will-change` for what it animates since 2026-11-10:* a box with an opacity or transform animation
current or in effect is a stacking context (and, for a transform, a containing block) whatever value it shows, so its
layer exists from the animation's start, also in its delay; script effects in their delay go to the compositor too.
**S2 is done.** The design as planned: For a composited animation the page sends its keyframes (transform
operation lists resolved to px, or opacities), easing, timing and start time (`Ticks`) with the frame; the compositor
interpolates at each display frame (transform lists as `cc::TransformOperations::Blend`: matching functions per
function, else matrix decomposition, CSS Transforms 2 §16) and writes the piece's state. The page asks no frame for
those animations (`animations_frame_at` leaves them out), only for their events and finish. Fixes problem 1.

*S2 plan (2026-11-09, from Chromium's source, `third_party/blink/renderer/core/animation/`):*

- **Which run there** (`compositor_animations.cc`, `CheckCanStartEffectOnCompositor` and
  `CheckCanStartElementOnCompositor`): a keyframe effect whose only animated properties are `opacity` and `transform`
  (Blink also takes `rotate`, `scale`, `translate`, `filter`; ours later), every keyframe `composite: replace`,
  `iterationComposite: replace`, the target has a box, is not fragmented and has the direct compositing reason (S1's
  `composited_animation_targets`), no other animation of the same property on the target
  (`HasIncompatibleAnimations`), and timing that `ConvertTimingForCompositor` accepts: playback rate not 0, `endDelay`
  0, iteration duration finite and not 0, finite delay. Ours adds one rule until the compositor has matrix
  decomposition: each pair of adjacent transform keyframes has function lists that interpolate function by function
  (same length and function types, `none` as identities) and are 2D. Everything else stays on the page thread.
- **What the compositor gets** (Blink: `cc::KeyframeModel` with `gfx::KeyframedTransformAnimationCurve` /
  `KeyframedFloatAnimationCurve`): the element key, the property, keyframes (offset, value, the keyframe's easing),
  the effect easing, timing (delay, iteration duration, iterations, iteration start, direction, fill, playback rate)
  and the start time on the page clock, with the page clock's origin as `Ticks`. Transform values are the function
  lists with lengths resolved to px against the box's reference box; the box's `transform-origin` and offset go as
  the matrices before and after (`full = translate(-at - origin) · T(t) · translate(origin + at)` as `paint.rs`
  `with_effects` builds it). Ours sends the whole set with every frame and the compositor replaces its set, so pause,
  cancel, seek and keyframe changes need no messages of their own (a paused or changed animation leaves the set; the
  page paints it). The page decides the start time, so nothing is synced back (Blink syncs because cc starts it).
- **Paint:** an element with a compositor animation always pushes its transform and opacity groups (identity, 1.0),
  so the piece's last two state steps are its own; a piece at opacity 0 is kept while one animates its opacity.
- **Compositor:** at each display frame, sample each animation at the page-clock time (Web Animations §4.8 timing,
  then the keyframe interval and its easing), write the piece's own state steps, draw; while any is running it draws
  every display frame, as autoscroll does.
- **Page:** `animations_frame_at` treats an animation on the compositor like one whose effect cannot show, but asks
  for the update at its end and, when it has iteration events, at the next iteration
  (`KeyframeEffect::CalculateTimeToEffectChange`, `keyframe_effect.cc`: "Need service to apply fill / fire events").
  Style, `getComputedStyle`, `getAnimations`, `currentTime` and events are the page's as now.

**S3. Changes while running.** *Done 2026-11-10.* Since S2 the page sends the whole set with every frame and asks
for no update while the compositor runs an animation, so a change only needs the next rendering update: its frame
carries the animation as the page now runs it, or leaves it out for the page to paint. Style changes (play state,
duration, delay, `animation: none`, a new important declaration), `CSSAnimation`/`CSSTransition` objects driven by
script (which mark their document's rendering dirty), and a pause, `reverse()`, `updatePlaybackRate()`, `cancel()` or
`finish()` (a pending task, a `cancel` event or an unresolved finished state) asked for that update already. A seek, a
new `startTime` or `playbackRate`, an effect's `updateTiming()`, `setKeyframes()` or new target, a new effect or a
null timeline of a script animation asked for none, so the compositor kept running it as it was. As built: each
script animation keeps what the compositor got of it with the last frame painted (`CompositorState`: effect, target,
keyframes, composite operation and the `CompositorTiming` it was sampled with; Blink's `Animation::CompositorState`,
made in `Animation::PreCommit`); `Engine::paint` records it (`note_compositor_frame`), and `animations_frame_at` asks
for the next update as soon as the animation no longer gives the same (`compositor_pending`: Blink's
`Animation::SetCompositorPending`, which compares the start time and effective playback rate, with
`Animation::EffectInvalidated` for effect changes, then `PendingAnimations::Add` schedules the frame and `PreCommit`
restarts it on the compositor). A rendering update hands the change over (`compositor_changes_handed_over`), so an
embedder that paints nothing (a hidden tab) is not asked again. Nothing is synced back: the page decides the start
time. Also: an animation (CSS or script, not a transition, which is above important declarations in
the cascade) of a property with an important declaration on its target stays with the page, whole (Blink's
`KeyframeEffect::CheckCanStartAnimationOnCompositor` with `AffectsImportantProperty`). Left: a CSS animation that
script drove through its `CSSAnimation` object stays with the page from then on (stylo's animation follows the model
at every update; Blink keeps it on the compositor); a style change that retimes a CSS animation costs one more update
after its frame (the event scan it makes due, S2). Process isolation: everything the compositor gets is plain data in
`CompMsg`/`FrameData`.

**S4. Filter animations.** *Done 2026-11-12.* A `filter` animation, transition or script effect whose keyframes
hold color functions only (brightness, contrast, grayscale, hue-rotate, invert, opacity, saturate, sepia; `none` is
the empty list; at most `ColorFilters::MAX`, 8), each two adjacent lists interpolating function by function (Filter
Effects 1 §14: the shorter a prefix of the longer by function), runs on the compositor (Blink's
`CheckCanStartEffectOnCompositor` takes no `filter` keyframes with a function that moves pixels,
`kFilterRelatedPropertyMayMovePixels`). Color functions apply to each pixel on its own and keep a transparent pixel
transparent, so they commute with the box's clips, mask and transforms: paint pushes such a box's filter group
around its layer, inside its opacity (transform, opacity, filter, layer), `split_layers` turns it into a
`StateOp::Filter(ColorFilters)` step before `StateOp::Animated`, the compositor applies its functions to the
piece's tiles (Direct2D ColorMatrix effects, each clamped, as the raster does; the tile's edges clamped for linear
filtering) and writes the step from `AnimatedValues::Filter` at each display frame. A current filter animation
makes the box a stacking context (`ANIMATES_FILTER`, as `will-change: filter`). Blur, drop-shadow and `url()`
keyframes, lists that do not interpolate, and a box with a `backdrop-filter` (its backdrop takes the box's filter)
stay with the page: its box is still a layer, which the page repaints at each frame.

**Pseudo-elements.** *Done 2026-11-22.* A generated `::before` or `::after` box is composited and animated apart
from its element, as Blink's pseudo-elements have layout objects and paint layers of their own: the layer and
animation key is `snowlark_layout::composited_key` (the element's id with the pseudo-element in the top two bits;
generations have 30), the box and its fragments carry the originating element (`pseudo_of`), and the style
engine keys composited targets, animation flags and what the compositor runs by element and pseudo-element. Their
CSS animations and transitions go; script animations of pseudo-elements stay with the page.

## Left out on purpose

Pixel-moving filter animations and backdrop-filter animations (a layer holding a backdrop is not split off; the
compositor would have to replay the backdrop), a current filter animation as a containing block (`will-change:
filter` makes one; the box is a stacking context only), scroll-driven animations on the compositor, layer squashing and overlap
heuristics (pieces stay in paint order, so overlap needs none), raster scale changes during a scale animation (raster
once at the start scale, as cc does until the animation ends).
