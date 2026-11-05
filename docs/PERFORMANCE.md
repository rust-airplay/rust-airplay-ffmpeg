# Performance

Every number here was measured on the machine this backend was developed against: Windows,
a Ryzen-class 32-thread CPU, a Radeon 610M, GStreamer 1.28 and FFmpeg 8.1.1 on the PATH,
wireless from an iPhone to a wired host. They are here so that a change to a default can be
argued with a measurement instead of a feeling.

## What a mirror has to beat

A 4K mirror arrives at 3108x2160 at 60 fps. That is one frame every 16.7 ms, and the
backend has to decode, scale to the window and present it inside that budget, with room
left for the frames that arrive in a burst.

| Work | Time per frame | Headroom at 60 fps |
| --- | --- | --- |
| Decode H.264 High, 4K, software | 10.2 ms (98 fps) | 1.6x |
| Decode HEVC, 4K, software | 12.5 ms (80 fps) | 1.3x |
| Decode + scale + present, 4K, 1280x720 window | 5.6-6.2 ms (160-180 fps) | 2.7x |

The present stage is faster than the decode stage because the window is smaller than the
frame; the numbers in the last row are what the player achieves end to end with wallclock
stamps, which is the configuration this backend ships.

## Options that were measured, and what won

| Setting | Measured | Chosen |
| --- | --- | --- |
| SDL renderer | hardware 1059 ms vs software 778 ms for 30 frames at 1080p; 152 vs 178 fps at 4K | software, named through `Program::with_env` |
| Input probing | FFmpeg default 3401 ms to start; 64 KiB cap 259 ms | `-probesize 65536 -analyzeduration 0` |
| Decoder threads | default 168 fps, `-threads 8` 154 fps at 4K HEVC | default |
| `-vf scale` to the window | 155-177 fps against 160-180 default | none: within noise, and a forced scale distorts a non-16:9 phone frame |
| Hardware decode, `-hwaccel d3d11va`, 4K HEVC 300 frames, decode only | 6.96 s of CPU and 0.59 s of wall software; 2.3-3.4 s of CPU and 1.2-1.5 s of wall hardware -- 4-5x real time either way | opt-in: `Options::with_hardware_decoding(true)` |
| Hardware decode with an explicit download, `-hwaccel_output_format d3d11 -vf hwdownload,format=nv12` | 1.92 s of CPU and 0.88 s of wall, the fastest hardware path measured | not addable for `ffplay`: it rejects `-hwaccel_output_format` ("Option not found"), so this is a sink of the caller's own |
| Wallclock stamps | `-use_wallclock_as_timestamps 1` equals the GStreamer sink's `sync=false`: a frame is shown when it arrives | used |
| Warm video sink | cold, a 4K 60 fps session dropped **79 frames** at the start and the HEVC decoder then had no reference to decode with (`Could not find ref with POC`), which is a frozen picture; warm, the same session drops none, because the receiver's queue is eight frames deep and a cold player needs about 500 ms to fill it | on by default |

The one setting that moved a measured delay by more than a second was not in this list at
all: offering HEVC instead of H.264 took a 4K mirror from **1.48 s behind a stopwatch** to
no delay the stopwatch could see, with every stage above unchanged. See the coding section
of [ARCHITECTURE](ARCHITECTURE.md).

## What the backend costs while it plays

A session is four programs: the receiver, the player, the audio decoder and the audio
player. The receiver's own work per frame is a clone of the access unit into a queue and
one write to a pipe: the arrival report shows a push of 0.1 ms mean and 0.7 ms worst at 4K
60 fps, which is under one percent of the frame interval.

## Where to look when a mirror is slow

1. The arrival report: `raw <client stamp> at <local clock>` says how old a frame was when
   it arrived, and `behind per window` says whether the client is keeping up.
2. `video <n> fps` in the same report: a client sending far fewer frames than the offer
   asked for is a client that cannot encode the offer.
3. If both look healthy, the delay is the coding negotiation: offering HEVC is the
   difference measured above.
