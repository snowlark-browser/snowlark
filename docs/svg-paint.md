# Inline SVG painted from the DOM

Status: step 1 done 2026-11-16; presentation attributes parsed one by one 2026-11-17; `d` parsed once, clip paths of any shapes and linear and radial gradients 2026-11-17; patterns 2026-11-22; focal points, markers, `pathLength` and steps 4-6 open.

## Where we are

An outermost `svg` element in an HTML document is a replaced box. Two paths draw it:

1. **The image path** (`snowlark-render/src/inline_svg.rs`, then `svg.rs`). Every time anything inside the svg is
   restyled or mutated, `Renderer::discover` writes the whole subtree out as a standalone SVG document
   (`inline_svg::serialize`), hashes the markup, and when the hash changed parses it again (`SvgImage::parse`, a
   roxmltree pass for the natural size), makes a new image and rebuilds the element's box. At paint, the image is
   rasterized by usvg and resvg from the markup.
2. **The paint path** (`svg_paint.rs`, since 2026-11-08). At paint, `Renderer::record_inline_svgs` walks the svg's
   elements and their computed styles and records display list items (path fills and strokes, glyph runs, clips,
   opacity groups). An svg it cannot record (`svg_paint::Unsupported`, a reason string) falls back to the image path.
   Recordings are cached by image id, content box size and markup hash; svgs with the same markup share one.

The chart suites' svgs are drawn by the paint path, so in the window the image path's raster is gone. What is left is
the serializer: it still runs on every change, before paint, only to decide whether the svg changed (the hash), to
find what the svg fetches, and to keep markup ready for a fallback. A profile (headless, scored
time): `inline_svg::serialize` is 12.7% of Charts-observable-plot and 6.3% of React-Stockcharts-SVG; the
presentation-hint CSS text (`StyleEngine::hints_cache`, snowlark-style) another 3.7% and 6.6%; `SvgImage::parse` 0.8%
each. Headless runs do not paint, so `svg_paint::record` is not in that profile; in the window it is (about 1.5 ms an
iteration on the complex-DOM pages' icons before recordings were shared).

## The reference design

- The outermost `svg` is a replaced layout object; containers (`g`, `a`, nested `svg`, `use` instances) and shapes
  are layout objects of their own, painted from the element and its computed style. Nothing is serialized.
- A shape's path is parsed once per attribute change: the path element keeps its parsed path data (a byte stream of
  segments) and the shape's layout object builds its path from it, only when the geometry changed. Basic shapes build
  their path from computed lengths when layout of the shape is needed.
- Resources (gradients, patterns, clip paths, masks, markers, filters) are layout objects too. Each client looks its
  resources up through a per-object resources cache, and a resource change invalidates its clients' paint.
- `use` builds a shadow tree of instances: clones that are real elements, styled through the cascade (with the `use`
  as their parent), laid out and painted like any other content.
- Markers are painted by the shape painter at each vertex, from the marker's content as a container.
- Text and `textPath` are laid out by an SVG text layout algorithm over the inline layout of the text element.
- Paint is recorded per layout object into cached display items and reused for objects whose paint was not
  invalidated.
- Presentation attributes are parsed per property into a property value set, cached and shared by attribute values.

## What the serializer carries, and where each part goes

| What `inline_svg.rs` does | Where it is today in the paint path | Moves to |
|---|---|---|
| Computed styles written as `style` attributes (`style_of`, `serialized_value`, `PROPERTIES`) | `svg_paint` reads `ComputedValues` directly | done |
| Geometry properties and `transform` from computed values (`computed_attribute`, `css_transform`, `css_matrix`) | `SvgQuery::shape_path`, `local_transform` | done |
| Animated attribute values (`Dom::animated_attr(s)`) | `SvgQuery::attr` reads presented values | done; change detection must see them (step 1) |
| Basic shapes, `path`, `g`, `a`, opacity, visibility, `display`, conditional processing | `svg_paint::Recorder` | done |
| `text`, `tspan` with one position per element | `svg_paint::Recorder::text` | done; per-character positions, decoration, stroke, baselines later (step 4) |
| Clip paths (`write_referenced`, `style_reference`) | shapes under their transforms, the `clipPath`'s transform and units: one as a path clip (a rectangle along the axes as a clip rectangle), several as an alpha mask | done; `text` and `use` in a clip path, and a clip path's own `clip-path`, fall back |
| Paint servers: gradients, patterns (`write_referenced`, `write_resources`) | linear and radial gradients (`Gradient`): the fill's path as a clip, the stroke as a mask, under the gradient's units and transform | done: patterns as a recorded tile (`Item::FillPattern`: the tile's own list beside the list, repeated by the raster side through an image brush at device pixels); a pattern inside a pattern's tile ("pattern in a pattern") and radial focal points ("gradient focal point") fall back |
| Markers (`marker_path`, `marker_instances`) | falls back ("markers") | step 3: vertices from the parsed path, the marker's content recorded as a container per vertex |
| `use` and its instance ids (`use_reference`, `prefix`, `prefixed_refs`, `write_instances`) | falls back ("use") | step 5: record the referenced content under the `use`'s transform; instance styling needs the shadow tree's cascade (the DOM already builds a `use` shadow tree: `inline_svg.rs` reads `dom.shadow_root(el)`) |
| External resources: `use` of another document, `url()` of another document (`External`, `svg_resource.rs`) | falls back | after step 5; the fetches stay where they are (see step 1) |
| `textPath` (`text_path_source`) | falls back | step 4, with `svg_text.rs`'s layout |
| Masks, filters (`drawn_filter`), `mix-blend-mode` | fall back | step 6: mask and filter items exist for CSS; SVG filters through `filter_ref.rs` |
| Bounding boxes for `objectBoundingBox` units (`object_bbox`, `stroke_bbox`, `attribute_bbox`) | `svg_geometry` has the boxes | with the resource that needs them |
| Dash scaling by `pathLength` (`dash_scale`, `shape_length`) | falls back ("pathLength") | step 3 |
| `<image>` and its fetch (`Serialized::images`) | falls back ("image") | step 6, drawn as an image item |
| Nested `svg`, `switch`, `foreignObject`, `animateMotion`, `vector-effect`, `zoom`, `calc()` geometry | fall back | as found in real pages |

## Steps

Each step: a pixel diff against headless Chrome on SVG test pages, WPT `svg` with no subtest lost, the
Charts-observable-plot and React-Stockcharts-SVG A/Bs (at least 3 pairs), the micro gate, and the WPT gate.

1. **No markup for svgs the paint path draws.** At discover, one walk over the svg's subtree decides whether the svg
   needs markup at all: anything that fetches (`use`, `image`, `feImage`, an `href` or `url()` to another document)
   or anything `svg_paint` does not record yet (paint servers, markers, filters, masks, nested `svg`, `textPath`,
   `animateMotion`, a clip path outside the svg) keeps the image path exactly as it is. For the others the same walk
   builds a fingerprint in place of the markup hash: the subtree's names, attributes (with animated values) and text
   hashed, and each element's `ComputedValues` held. The svg changed when the hash differs or any element's computed
   values are not equal to the held ones (`snowlark_style::computed::same_values`: every style struct by value). The
   fingerprint's key (the hash and the held styles' addresses, unique while they are held) caches recordings, and an
   svg whose fingerprint draws like a recorded one at the same size takes its recording, so equal icons record once.
   The image of such an svg has its natural size from the root's attributes and no markup; markup is written at paint
   only if the recording fails or the image is drawn in part, and the svg then goes back to the image path.
2. **Paint servers**: linear and radial gradients, then patterns.
3. **Clip paths of any content, markers, `pathLength`.**
4. **Text**: per-character positions, decoration, stroke, baselines, `textPath`.
5. **`use`**, same-document first.
6. **Masks, filters, `image`.** Then the image path is left for SVG images (`<img src=x.svg>`), which can move to the
   paint path the same way.

Separately: presentation attributes are parsed per property and kept by value (2026-11-17; `hints_cache` was 6.0% of
Stockcharts and 6.4% of observable-plot before). A `path`'s data is parsed once per attribute change (2026-11-17): the hint
parses it with the attribute's grammar into the `d` property's value, and `SvgQuery::shape_path` builds the path from the
computed commands (`path_data_of`), as the reference engine's path element keeps one parsed byte stream that the style's
path value and the shape's layout object share.

What real pages still fall back on (`--render` of 15 sites, 2026-11-17, after the clip paths and gradients): `filter`
(YouTube's player icons, 5 per watch page; The Verge), `image` (reddit, 11), `vector-effect` (Spotify, 9), `use`
(YouTube, CNN), "effect attribute" (`filter`/`mask`/`marker` attributes: YouTube, MDN, CNN), a pattern (the Guardian),
`nested svg` and `pathLength` (reddit). Step 6 (filters) and `vector-effect` reach more pages than the rest of step 3.
