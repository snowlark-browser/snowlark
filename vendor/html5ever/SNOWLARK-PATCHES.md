# html5ever 0.40.1, patched for Snowlark

This is html5ever 0.40.1 from crates.io (the first commit that adds this directory is the unmodified
release), used through `[patch.crates-io]` in the workspace `Cargo.toml`. The changes make the parser match
the html5lib tree-construction tests in WPT's `html/syntax/parsing/resources`, which already encode these
HTML Standard changes. Every change is marked `Snowlark patch` in the source; `git log -p vendor/html5ever`
shows the diff.

1. **Processing instructions** (`tokenizer/{interface,mod,states}.rs`, `tree_builder/mod.rs`): `<?target
   data>` is a processing instruction, not a bogus comment. New tokenizer states after `<?`; a target that
   starts with `xml` (any case) or ends in a character a target can't have falls back to the old bogus
   comment; a processing instruction cut off by the end of input is dropped. The tokenizer emits a new
   `ProcessingInstructionToken`; the tree builder inserts it wherever a comment would go, through
   `TreeSink::create_pi`.
2. **`search`** is a special element (`tree_builder/tag_sets.rs`).
3. **Templates and frameset-ok** (`tree_builder/rules.rs`): a `template` start tag in head saves the
   frameset-ok flag and its end (or end of file in template) restores it; in body, a `template` start tag
   sets frameset-ok to "not ok".
4. **Forms in templates** (`tree_builder/rules.rs`, `mod.rs`): "a template on the stack" also counts the
   fragment case's `template` context element; in table, a `form` start tag inside a template is inserted
   (and not remembered as the form element pointer).
5. **`input` in a `select` fragment** is ignored, not inserted (`tree_builder/rules.rs`).
6. **Pops reach the sink** (`tree_builder/mod.rs`, `rules.rs`): every element popped off the stack of open
   elements is reported with `TreeSink::pop` (upstream skipped `pop_until`, `pop_until_current`,
   truncations and the adoption agency's removals in its steps 10, 13.5 and 13.6; an element inserted
   without being pushed, as a void element, which HTML pushes and pops at once, is reported as
   popped), so the sink runs
   "maybe clone an option into selectedcontent" for every option pop, including those at the end of
   input, and knows which elements are still open (`link rel=expect`).

7. **Tags in one scan** (`tokenizer/mod.rs`, a speed change with no behaviour change): in the tag open
   state, a whole tag that sits in the front input chunk and that the tag, attribute name and attribute
   value states would read without a parse error, a character reference, a NULL or a carriage return is
   tokenized by one scan of its bytes (`scan_simple_tag`, `Tokenizer::fast_tag`); attribute values
   share the input's buffer. Anything else goes through the state machine as before, and the tag name
   and attribute name states append runs of characters instead of one character at a time. The
   `tokenizer_tag_scan_matches_state_machine` test in `snowlark-dom` parses each of its inputs both ways.

8. **SVG `script` end tags** (`tree_builder/rules.rs`, `tree_builder/mod.rs`, upstream's FIXME #118):
   the end tag of an SVG `script` element, and a self-closing one, hand the element to the page layer
   like an HTML script's, so that it runs at that point of the parse.

To move to a newer html5ever: replace this directory with the new release, re-apply the changes that
upstream still lacks, and run `cargo test -p snowlark-dom --test html5lib`.
