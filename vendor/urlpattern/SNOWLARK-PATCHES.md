# urlpattern 0.6.0, patched for Snowlark

The upstream crate (rust-urlpattern, MIT) as published, without its benchmark and test data, with:

1. **`ignoreCase` and the literal prefix and suffix** (`src/matcher.rs`): the matcher strips a literal prefix and
   suffix from the input before running what is left of the component's expression. Upstream compares them case
   sensitively whatever `ignoreCase` says, so `{ pathname: '/ABC/(d+)' }` with `ignoreCase` did not match
   `/abc/DD`. Under `ignoreCase` they now compare case-insensitively, as the whole component's expression with
   the `i` flag does (URL Pattern Standard, "compile a component").

Snowlark runs the crate with ECMAScript regular expressions (`RegexSyntax::EcmaScript`) through V8:
`crates/snowlark-page/src/engine/urlpattern.rs` and `crates/snowlark-js/src/ecma_regexp.rs`.
