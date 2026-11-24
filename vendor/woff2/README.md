# woff2 (decoder)

The WOFF 2.0 decoder of [google/woff2](https://github.com/google/woff2) at commit
`1c69169e9e1811dccd6c54c532fedda300233968`, unmodified: `LICENSE`, `include/woff2/decode.h` and `output.h`, and
the `src/` files the decoder is built from (`buffer.h`, `file.h`, `port.h`, `round.h`, `store_bytes.h`,
`table_tags.*`, `variable_length.*`, `woff2_common.*`, `woff2_dec.cc`, `woff2_out.cc`). The encoder and the
command-line tools are left out.

OTS (`vendor/ots`) calls it for WOFF 2.0 files; `crates/snowlark-ots-sys` compiles both. To update: take the same
files of the new commit and name it here.
