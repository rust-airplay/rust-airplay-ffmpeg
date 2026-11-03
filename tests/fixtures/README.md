# Test fixtures

Two small binary fixtures are checked in so the ignored tests can decode real-shaped input
without capturing anything at test time. Both are embedded with `include_bytes!`, so they
compile into the test binaries.

Both files come from [rust-airplay-gstream](https://github.com/rust-airplay/rust-airplay-gstream),
which belongs to the same project and is distributed under the same license. They are kept
byte for byte, so the sha256 below is the one that repository records.

## mirror-first-frames.h264

A synthetic H.264 Annex-B elementary stream: 640x360, 48 coded pictures, generated from
GStreamer's `videotestsrc` through `openh264enc`. It contains a test pattern only -- no
camera image, no screen capture, no personal data.

| property | value |
| --- | --- |
| format | H.264 Annex-B, one elementary stream, no container, no audio |
| resolution | 640x360 |
| coded pictures | 48 (47 non-IDR slices and 1 IDR picture) |
| access unit delimiters | 48 |
| SPS / PPS | 1 each, before the first slice |
| file size | 15379 bytes |
| sha256 | f980f0f3b895944a3d4b3a90eb8ed685379cc74c8cb8b6420f0c9d6da850c49b |

Every unit starts with a four-byte start code (`00 00 00 01`); the stream holds no three-byte
start codes and decodes completely. `tests/decoding.rs` counts the slices as pictures, and
the decoded output as bytes: 48 pictures at 640 * 360 * 3 / 2 bytes each.

Regeneration is documented in the gstream repository's `tools/make-fixtures.sh`, which runs
the encoder that produced this file.

## mirror-first-audio.eld

40 raw AAC-ELD payloads, each preceded by a four-byte little-endian length: 448 bytes for the
first frame, then 39 payloads of 145 to 190 bytes. Parsing the length prefixes consumes
exactly 6823 bytes.

| property | value |
| --- | --- |
| format | raw AAC-ELD payloads, each with a 4-byte little-endian length prefix |
| frames | 40 |
| file size | 6823 bytes |
| sha256 | bb950cc82253803d5f0a0ab28792c6ed323f189dd6aa52410f17a0d553574aae |

Provenance: the payloads were captured from a real AirPlay session and are kept because no
AAC-ELD encoder is available to synthesize them. They are protocol data, not recorded user
content: coded audio payloads with no metadata or identifying information.

The test wraps them in the Matroska stream the backend writes, decodes them, and requires
exactly `40 * 480 * 2 * 2` bytes of PCM: a stream that lost a frame is short by one frame's
worth, which is what a decoder fed the wrong frames produces and what a count of pushes
would never notice.

## Licensing

Both fixtures belong to this project and are distributed under GPL-3.0-or-later, the crate's
license.
