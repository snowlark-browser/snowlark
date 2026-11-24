# taffy 0.16.0, patched for Snowlark

This is taffy 0.16.0 from crates.io (the commit "taffy 0.16.0 from crates.io, unmodified" is the release),
used through `[patch.crates-io]` in the workspace `Cargo.toml`. Every change is marked `Snowlark patch` in
the source; `git log -p vendor/taffy` shows the diff.

1. **Flex container intrinsic main sizes** (`compute/flexbox.rs`, `determine_container_main_size`,
   `item_intrinsic_contribution`): CSS Flexbox 1 §9.9.1.1's ideal algorithm (each item's desired flex
   fraction, the line's chosen flex fraction, items flexed by it and clamped by their min/max main
   sizes) with §9.9.3's contributions (the larger of the min/max-content size and the preferred size,
   clamped by the min/max main size), and §9.9.1.3 for the min-content size of a multi-line container.
   Upstream computed a per-item fraction with scaled shrink factors of at least the flex base size and
   clamped neither by the min/max main sizes nor by the flex base size of an inflexible item.
2. **A min-content height is a max-content height** (`compute_flexbox_layout`): CSS Sizing 3 §5.1, the
   min-content block size of a box is its max-content block size.
3. **A column container's main size** (`determine_container_main_size`) is the longest line's
   hypothetical outer main sizes and gaps (§9.2 step 4: the block size its formatting context gives
   it), not §9.9.1's inline-axis algorithm.
4. (Dropped with 0.16.0, which has it: `stretch` falls back to unsafe `flex-start`, css-align-3 §3.3.)
5. **Overflowing flex lines stay reachable in a scroll container** (`compute/flexbox.rs`,
   `align_flex_lines_per_align_content`): with no overflow-position keyword, `align-content` is safe
   there (css-align-3 §4.4, the default overflow alignment keeps overflowing content reachable).
6. **Collapsed flex items** (`style/mod.rs` `Style::item_is_collapsed`, `FlexboxItemStyle::is_collapsed`,
   `compute/flexbox.rs` `collapsed_item_struts`): `visibility: collapse` per css-flexbox-1 §4.4 and
   §9.4 step 10 (a first round for the struts, then layout without the collapsed items, each line at
   least as tall as its collapsed items' struts). Upstream had a TODO.
7. **A measured item's cross size is definite only when §9.8 says so** (`compute/flexbox.rs`,
   `measure_item_main_size`): the flex base size and content size suggestion measurements pass
   whether the known (stretched) cross size is definite; `measure_child_size` called every known size
   definite, so percentages in an item would resolve against a stretched size in a multi-line or
   auto-height container.
8. **Flexible grid tracks of an indefinite axis are sized again** (`compute/grid/mod.rs`, agent/grid) once
   the grid container's size is known, as percentage tracks are (css-grid-2 §11.7 with a definite free
   space; Chrome and Firefox agree): `minmax(0, .5fr)` rows of an auto-height grid end at half its height.
   WPT css-grid/layout-algorithm/flex-sizing-rows-indefinite-height*.

9. **`last baseline` alignment of flex and grid items** (`style/alignment.rs`, `compute/flexbox.rs`,
   `compute/grid`): a keyword
   of its own. The items of a line's last-baseline group sit at the line's end with their last baselines
   lined up, and count towards its cross size (css-align-3 §9.3, css-flexbox-1 §9.4).

10. **A single-line column container's intrinsic width takes no item layout** (`compute/flexbox.rs`,
    `column_intrinsic_cross_size`): §9.9.2, "the min-content/max-content cross size of a single-line
    flex container is the largest min-content contribution/max-content contribution (respectively)
    of its flex items", as Blink's `ComputeMinMaxSizes`. Upstream ran the whole algorithm for it,
    laying out every item at its min-content or max-content width for a flex base size it then
    dropped. Items with an aspect ratio and collapsed items keep the full algorithm.
11. **Subgrids** (`compute/grid/subgrid.rs`, `compute/grid/mod.rs`, `track_sizing.rs`, `types/grid_item.rs`,
    `style/mod.rs`, `style/grid.rs`, `tree/traits.rs`): css-grid-2 §9. `Style::subgrid` marks
    `grid-template-columns`/`-rows: subgrid` (the `<line-name-list>` in the line names). A subgrid's own
    items, and those of subgrids nested in it in the same axis, take part in its parent's track sizing in
    the subgridded axes as items of the parent (`GridItem::sizing`, `is_subgrid_item`), with the subgrid's
    margin, border and padding and half the gutter difference as extra margin (`extra_margin`); in an axis
    the subgrid has tracks of its own, their extra margin is the space outside the item's area in them. The
    subgrid itself takes no part there and is stretched. The parent then hands the subgrid its used tracks
    (`LayoutGridContainer::set_subgrid_tracks`), and the tree lays it out with `Style::subgrid_view`: those
    tracks at fixed sizes, the parent's line names before its own, its gutter (its gap, or the parent's for
    `normal`), and its items clamped to that explicit grid. Upstream had no subgrid.
12. **An item's percentage padding counts in its flex base size** (`compute/flexbox.rs`,
    `determine_flex_base_size`): the item is measured without a main-axis parent size, so its own
    percentage padding and border resolved to zero there in a row container. css-flexbox-1 §4.2 resolves
    them against the container's inner width, as `FlexItem::padding` is; the difference is added to the
    measured base size and min-content size (`pb_correction`). Upstream measured them as zero.
13. **Percentage heights in a row container whose height is not definite** (`compute/flexbox.rs`,
    `AlgoConstants::percent_basis`): a height its parent imposed without making it definite (a flex item
    that is not stretched, sized from its contents, css-flexbox-1 §9.8) is not a basis for the items'
    percentage heights and min/max heights, nor the parent size their own layout resolves against; they
    behave as `auto` (CSS2 §10.5). Upstream resolved them against that height.

To move to a newer taffy: replace this directory with the new release, re-apply the changes that upstream
still lacks, and run `.\dev.ps1 test --release --package snowlark-layout`.
