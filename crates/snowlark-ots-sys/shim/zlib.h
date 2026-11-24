/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

/* What OTS uses of zlib: one-shot `uncompress` for WOFF 1.0 tables. It is routed to the
 * decompressor the caller of snowlark_ots_process passes (ots_shim.cc). */

#ifndef SNOWLARK_OTS_ZLIB_H_
#define SNOWLARK_OTS_ZLIB_H_

typedef unsigned char Bytef;
typedef unsigned long uLong;
typedef uLong uLongf;

#define Z_OK 0
#define Z_DATA_ERROR (-3)

#ifdef __cplusplus
extern "C" {
#endif

int snowlark_ots_uncompress(Bytef* dest, uLongf* dest_len, const Bytef* source, uLong source_len);

#ifdef __cplusplus
}
#endif

#define uncompress snowlark_ots_uncompress

#endif /* SNOWLARK_OTS_ZLIB_H_ */
