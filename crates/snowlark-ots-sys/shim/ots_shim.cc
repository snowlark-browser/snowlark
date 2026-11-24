// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A C interface over OTS for the Rust side (snowlark-ots-sys/src/lib.rs declares it): one call
// sanitizes a font file into a buffer the result owns, with OTS's messages. No C++ exception
// crosses it.

#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>

#include <new>
#include <string>

#include "brotli/decode.h"
#include "opentype-sanitiser.h"
#include "ots-memory-stream.h"
#include "zlib.h"

typedef int (*SnowlarkDecompress)(uint8_t* dst, size_t dst_len, const uint8_t* src, size_t src_len,
                                  size_t* written);

namespace {

// The decompressors of the snowlark_ots_process call running on this thread.
thread_local SnowlarkDecompress g_zlib = nullptr;
thread_local SnowlarkDecompress g_brotli = nullptr;

// OTS's messages are kept up to this length; later ones are dropped whole.
constexpr size_t kMaxMessages = 4096;

constexpr uint32_t Tag(char a, char b, char c, char d) {
  return (uint32_t(uint8_t(a)) << 24) | (uint32_t(uint8_t(b)) << 16) |
         (uint32_t(uint8_t(c)) << 8) | uint32_t(uint8_t(d));
}

class Context final : public ots::OTSContext {
 public:
  std::string messages;

  void Message(int level, const char* format, ...) override {
    (void)level;
    if (messages.size() >= kMaxMessages) {
      return;
    }
    char text[512];
    va_list args;
    va_start(args, format);
    int n = vsnprintf(text, sizeof text, format, args);
    va_end(args);
    if (!messages.empty()) {
      messages.push_back('\n');
    }
    if (n <= 0) {
      messages.append("Unspecified OTS error");
    } else {
      messages.append(text);
    }
  }

  // The tables other engines pass through unchanged, as here: the color tables, the OpenType
  // layout tables and the font variation tables. OTS's checks of the layout tables are stricter
  // than the text engines that read them, and refuse fonts that are in wide use (a GPOS lookup
  // with an empty subtable, a DFLT script without a default language system). Every other table
  // OTS knows is checked and written out again (the default); a table it does not know is dropped.
  ots::TableAction GetTableAction(uint32_t tag) override {
    switch (tag) {
      // Color fonts
      case Tag('C', 'B', 'D', 'T'):
      case Tag('C', 'B', 'L', 'C'):
      case Tag('C', 'O', 'L', 'R'):
      case Tag('C', 'P', 'A', 'L'):
      case Tag('s', 'b', 'i', 'x'):
      // CFF 2 outlines and the style attributes
      case Tag('C', 'F', 'F', '2'):
      case Tag('S', 'T', 'A', 'T'):
      // Layout
      case Tag('B', 'A', 'S', 'E'):
      case Tag('G', 'D', 'E', 'F'):
      case Tag('G', 'P', 'O', 'S'):
      case Tag('G', 'S', 'U', 'B'):
      // Variations
      case Tag('a', 'v', 'a', 'r'):
      case Tag('c', 'v', 'a', 'r'):
      case Tag('f', 'v', 'a', 'r'):
      case Tag('g', 'v', 'a', 'r'):
      case Tag('H', 'V', 'A', 'R'):
      case Tag('M', 'V', 'A', 'R'):
      case Tag('V', 'V', 'A', 'R'):
        return ots::TABLE_ACTION_PASSTHRU;
      default:
        return ots::TABLE_ACTION_DEFAULT;
    }
  }
};

}  // namespace

struct SnowlarkOtsResult {
  ots::ExpandingMemoryStream* output = nullptr;
  size_t size = 0;
  std::string messages;
};

extern "C" int snowlark_ots_uncompress(Bytef* dest, uLongf* dest_len, const Bytef* source,
                                       uLong source_len) {
  size_t written = 0;
  if (!g_zlib || g_zlib(dest, *dest_len, source, source_len, &written) != 0) {
    return Z_DATA_ERROR;
  }
  *dest_len = static_cast<uLongf>(written);
  return Z_OK;
}

extern "C" BrotliDecoderResult snowlark_ots_brotli_decompress(size_t encoded_size,
                                                              const uint8_t* encoded,
                                                              size_t* decoded_size,
                                                              uint8_t* decoded) {
  size_t written = 0;
  if (!g_brotli || g_brotli(decoded, *decoded_size, encoded, encoded_size, &written) != 0) {
    return BROTLI_DECODER_RESULT_ERROR;
  }
  *decoded_size = written;
  return BROTLI_DECODER_RESULT_SUCCESS;
}

extern "C" SnowlarkOtsResult* snowlark_ots_process(const uint8_t* data, size_t len, size_t limit,
                                                   SnowlarkDecompress zlib,
                                                   SnowlarkDecompress brotli, int* ok) {
  *ok = 0;
  SnowlarkOtsResult* result = new (std::nothrow) SnowlarkOtsResult;
  if (!result) {
    return nullptr;
  }
  g_zlib = zlib;
  g_brotli = brotli;
  try {
    Context context;
    // Most web fonts are compressed: the stream grows as needed, up to `limit`.
    result->output = new ots::ExpandingMemoryStream(len < limit ? len : limit, limit);
    bool accepted = context.Process(result->output, data, len);
    result->messages.swap(context.messages);
    if (accepted) {
      result->size = static_cast<size_t>(result->output->Tell());
      *ok = 1;
    } else {
      delete result->output;
      result->output = nullptr;
    }
  } catch (...) {
    delete result->output;
    result->output = nullptr;
    result->size = 0;
    try {
      result->messages.assign("Out of memory");
    } catch (...) {
    }
  }
  g_zlib = nullptr;
  g_brotli = nullptr;
  return result;
}

extern "C" const uint8_t* snowlark_ots_output(const SnowlarkOtsResult* result, size_t* len) {
  if (!result->output) {
    *len = 0;
    return nullptr;
  }
  *len = result->size;
  return static_cast<const uint8_t*>(result->output->get());
}

extern "C" const uint8_t* snowlark_ots_messages(const SnowlarkOtsResult* result, size_t* len) {
  *len = result->messages.size();
  return reinterpret_cast<const uint8_t*>(result->messages.data());
}

extern "C" void snowlark_ots_free(SnowlarkOtsResult* result) {
  if (result) {
    delete result->output;
    delete result;
  }
}
