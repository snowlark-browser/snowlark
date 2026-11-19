# url 2.5.8, patched for Snowlark

Upstream rust-url predates several URL Standard changes (file URLs keeping their host and empty path
segments, `^` in the path percent-encode set, opaque paths encoding a space before `?`/`#`, ASCII domains
that are only lowercased, blob origins). Snowlark needs the standard as written, so:

1. **The basic URL parser as written** (`src/whatwg.rs`): the state machine of URL Standard §4.4 over a
   URL record (scheme, username, password, host, port, path as an opaque path or a list of segments,
   query, fragment), the URL serializer, and the conversions between that record and `Url`.
   `ParseOptions::parse` (so `Url::parse` and `Url::join`) runs it; the query encoding override is
   honoured as "percent-encode after encoding". `ParseOptions::syntax_violation_callback` is removed: this
   parser reports no syntax violations.
2. **The API setters** (`src/quirks.rs`): `set_protocol`, `set_username`, `set_password`, `set_host`,
   `set_hostname`, `set_port`, `set_pathname`, `set_search` and `set_hash` follow the URL class's setter
   steps (URL Standard §6.1), running the basic URL parser with a state override on the record.
3. **The domain parser** (`src/host.rs`): with beStrict false an ASCII domain is lowercased and not
   validated by Unicode ToASCII (URL Standard §3.3 "domain parser" step 5); an empty result or a
   forbidden domain code point fails.
4. **Blob URL origins** (`src/origin.rs`): only an `http`, `https` or `file` URL in the path gives its
   origin; anything else is opaque.

`Url`'s own component setters (`set_path`, `set_query`, `set_host`...) are upstream's and keep using
`src/parser.rs`, whose otherwise unused entry points carry `#![allow(dead_code)]`. The upstream tests
(and their WPT data copy) are not vendored; `crates/snowlark-net/tests/url_standard.rs` and WPT `url/`
cover the patches.
