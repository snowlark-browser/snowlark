/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

/* Prints, as a Rust `Layout` literal, where the fields snowlark-media reads lie in the FFmpeg
 * structures of the headers it is compiled against, with the libraries' major versions (their
 * file names). tools/ffmpeg/layout.sh runs it for each FFmpeg release the player supports. */

#include <stddef.h>
#include <stdio.h>

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/frame.h>
#include <libswresample/swresample.h>
#include <libswscale/swscale.h>

#define AT(type, field) printf("        %s: %zu,\n", #type "_" #field, offsetof(type, field))

int main(void) {
    printf("    Layout {\n");
    printf("        avcodec: %d,\n        avformat: %d,\n        avutil: %d,\n", LIBAVCODEC_VERSION_MAJOR,
           LIBAVFORMAT_VERSION_MAJOR, LIBAVUTIL_VERSION_MAJOR);
    printf("        swscale: %d,\n        swresample: %d,\n", LIBSWSCALE_VERSION_MAJOR, LIBSWRESAMPLE_VERSION_MAJOR);
    AT(AVFormatContext, pb);
    AT(AVFormatContext, nb_streams);
    AT(AVFormatContext, streams);
    AT(AVFormatContext, start_time);
    AT(AVFormatContext, duration);
    AT(AVStream, codecpar);
    AT(AVStream, time_base);
    AT(AVStream, start_time);
    AT(AVStream, duration);
    AT(AVCodecParameters, codec_type);
    AT(AVCodecParameters, codec_id);
    AT(AVCodecParameters, format);
    AT(AVCodecParameters, width);
    AT(AVCodecParameters, height);
    AT(AVCodecParameters, sample_rate);
    AT(AVCodecParameters, ch_layout);
    AT(AVPacket, data);
    AT(AVPacket, size);
    AT(AVPacket, stream_index);
    AT(AVPacket, flags);
    AT(AVPacket, pts);
    AT(AVPacket, dts);
    AT(AVPacket, duration);
    AT(AVFrame, data);
    AT(AVFrame, linesize);
    AT(AVFrame, extended_data);
    AT(AVFrame, width);
    AT(AVFrame, height);
    AT(AVFrame, nb_samples);
    AT(AVFrame, format);
    AT(AVFrame, pts);
    AT(AVFrame, best_effort_timestamp);
    AT(AVFrame, sample_rate);
    AT(AVFrame, ch_layout);
    printf("    },\n");
    return 0;
}
