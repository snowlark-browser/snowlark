/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

/* What the WOFF 2.0 decoder uses of Brotli: one-shot `BrotliDecoderDecompress`. It is routed to
 * the decompressor the caller of snowlark_ots_process passes (ots_shim.cc). */

#ifndef SNOWLARK_OTS_BROTLI_DECODE_H_
#define SNOWLARK_OTS_BROTLI_DECODE_H_

#include <stddef.h>
#include <stdint.h>

typedef enum {
  BROTLI_DECODER_RESULT_ERROR = 0,
  BROTLI_DECODER_RESULT_SUCCESS = 1
} BrotliDecoderResult;

#ifdef __cplusplus
extern "C" {
#endif

BrotliDecoderResult snowlark_ots_brotli_decompress(size_t encoded_size, const uint8_t* encoded,
                                                   size_t* decoded_size, uint8_t* decoded);

#ifdef __cplusplus
}
#endif

#define BrotliDecoderDecompress snowlark_ots_brotli_decompress

#endif /* SNOWLARK_OTS_BROTLI_DECODE_H_ */
