# OTS (OpenType Sanitizer)

The library sources of [khaledhosny/ots](https://github.com/khaledhosny/ots) at commit
`c5d249ce90ab0aa3e76eb0dfad28746f407201d0`, unmodified: `LICENSE`, `include/` and `src/`, without the Graphite
table parsers (`feat`, `glat`, `gloc`, `sile`, `silf`, `sill`, `graphite.h`; the library is built without
`OTS_GRAPHITE`). Tests, tools, documentation and the Meson build are left out.

`crates/snowlark-ots-sys` compiles it with the WOFF 2.0 decoder in `vendor/woff2`. OTS's two uses of zlib and
Brotli go through that crate's `shim/zlib.h` and `shim/brotli/decode.h` to Rust decoders, so neither C library is
vendored.

To update: take `include/`, `src/` and `LICENSE` of the new commit (again without the Graphite files), name the
commit here, and run `snowlark-paint`'s `web_font` tests and the `css/css-fonts` and `css/WOFF2` WPT areas.
