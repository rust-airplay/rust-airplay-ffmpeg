# rust-airplay-ffmpeg

The FFmpeg backend for [`rust-airplay`](https://github.com/rust-airplay/rust-airplay).

`rust-airplay` receives an AirPlay session, decrypts it and hands over coded frames. This
crate decodes them with the FFmpeg command-line tools and plays them: one `ffmpeg` process
decodes the audio to PCM, and a sink program -- `ffplay` by default -- plays each stream. It
implements the core's `Renderer` trait, so a program depends on the core for the protocol
and on this crate for playback.

[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains why audio is wrapped in a container and
video is not, what happens to a frame at each step, and what a flush can and cannot do here.

Measurements behind the defaults, and where to look when a mirror is slow, are in
[docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Requirements

**Building** needs a Rust toolchain and nothing else. This crate links no C library, so there
are no development packages to install and no `-sys` crate in the dependency tree.

**Running** needs `ffmpeg` and, for the default sinks, `ffplay` on `PATH` -- or named
through [`Options`](#options) or the environment. The FFmpeg build needs decoders for H.264,
HEVC, ALAC and AAC-ELD; the usual distribution and package-manager builds have all four.

| Platform | What to install |
| --- | --- |
| Windows | MSYS2 UCRT64 `ffmpeg`, with `C:\msys64\ucrt64\bin` on `PATH` |
| Debian and Ubuntu | `ffmpeg` |
| macOS | `brew install ffmpeg` |

## Using it

```toml
[dependencies]
rust-airplay = "0.3"
rust-airplay-ffmpeg = "0.1"
```

```rust,no_run
use std::sync::Arc;

use rust_airplay::{Config, Renderer};

# async fn start() -> Result<(), Box<dyn std::error::Error>> {
let renderer: Arc<dyn Renderer> =
    Arc::new(rust_airplay_ffmpeg::FfmpegRenderer::new().expect("a machine with FFmpeg"));
let config = Config::new("Living room", [0x42; 32], renderer);
rust_airplay::run(config).await?;
# Ok(())
# }
```

## Testing

The crate has two kinds of test. The unit tests describe the argument builders, the
Matroska writer and the pacer, and need nothing installed:

```sh
cargo test
```

The integration tests decode the crate's own fixtures through real FFmpeg and compare
the bytes that come out -- 48 pictures, and 40 AAC-ELD frames -- so they need ffmpeg on
PATH and are ignored by default:

```sh
cargo test --test decoding -- --ignored
```

The fixtures are in [tests/fixtures](tests/fixtures); [tests/fixtures/README.md](tests/fixtures/README.md)
says where they came from.

## Options

Everything a caller can choose is part of `Options`, and every default is what the
corresponding environment variable would produce, so `Options::default` behaves exactly
like `FfmpegRenderer::new`:

| Method | Meaning | Default |
| --- | --- | --- |
| `Options::new(audio_sink, video_sink)` | the two sinks, spelled out | -- |
| `.with_decoder(program)` | the `ffmpeg` that decodes audio | `ffmpeg` |
| `.with_audio_sink(sink)` | the program that plays audio | `ffplay -nodisp` |
| `.with_video_sink(sink)` | the program that displays video, or `Program::raw` for one that reads the stream itself | `ffplay -framedrop` |
| `.with_warm_video(true)` | start the video sink with the renderer and keep it between sessions | off |
| `.with_warm_codec(codec)` | the coding a warm sink is started for | H.264 |
| `.with_env(name, value)` | a variable the sink program is started with | none |
| `.with_pacing(bool)` / `.without_pacing()` | honour a spacing asked for through `set_pace` | on |

A sink is a program and the arguments that follow the ones describing its input. The backend
supplies what the sink reads -- a Matroska stream for the decoder, 16-bit PCM for an audio
sink, Annex-B for a video sink -- and then appends the sink's own arguments. That makes the
same type serve a player and a recorder:

```rust
use rust_airplay::{Renderer, VideoCodec};
use rust_airplay_ffmpeg::{FfmpegRenderer, Options, Program};

// Record instead of play: the PCM goes into a WAV file, the pictures into a raw stream.
let renderer = FfmpegRenderer::with_options(Options::new(
    Program::new("ffmpeg").args(["-y", "-f", "wav", "audio.wav"]),
    Program::new("ffmpeg").args(["-y", "-f", "rawvideo", "-pix_fmt", "yuv420p", "video.yuv"]),
))?;
# Ok::<(), rust_airplay::RenderError>(())
```

## Environment

| Variable | Effect |
| --- | --- |
| `AIRPLAY_FFMPEG` | the `ffmpeg` executable that decodes audio |
| `AIRPLAY_FFMPEG_AUDIO_SINK` | the audio sink, as a command line |
| `AIRPLAY_FFMPEG_VIDEO_SINK` | the video sink, as a command line |

`Options` reads these when it is constructed, so a program that wants to choose in code
uses `Options::new` and the `with_*` methods instead. A command line is split on
whitespace; quoting is not interpreted, so a path with a space in it belongs in code rather
than in a variable.

## What it does with the stream

- **Audio** is wrapped in a Matroska stream and decoded to 16-bit PCM, which passes through
  the gain before a sink plays it. A raw AAC-ELD or ALAC frame has no sync word and no length
  of its own, so a container is the only way to tell a program where each frame ends.
- **Video** is written to the sink as Annex-B, in the order it was pushed. Start codes let
  the decoder find its own pictures, so there is nothing to frame. A raw stream carries no
  timestamps either, and a player that has to invent them schedules against a guess:
  FFmpeg's is 25 frames a second, an AirPlay client sends 30 or 60, and a 30-frame stream
  shown on a 25-frame grid falls further behind every second until the player starts
  dropping the frames its references live in -- which is what a mirror that goes green has
  lost. The sink is therefore told to stamp each frame with the moment it arrived, which is
  also what stops a client that encodes a little ahead from buying that lead back as
  latency.
- **Volume** is a linear gain applied to the PCM, which is why moving the slider takes effect
  on the next sample rather than on the next stream: a filter in a decoder's argument list
  would be fixed when the process started.
- **A flush** replaces both processes of an audio stream and the decoder of a video stream.
  A decoder cannot be told to forget what it queued, so it is replaced; the key frame a client
  sends after a flush is what makes the new one correct rather than merely empty.
- **A close** closes the pipe, which is the end of stream: the program drains what it has and
  exits. Both streams also stop on `Drop`, because a receiver can drop a stream without
  closing it when a session dies.

## Failure

`RenderError::Unavailable` means the machine cannot do it: no `ffmpeg`, no `ffplay`, no
display, a build without the decoder. `RenderError::Failed` means the request could not be
carried out: a program that refused its arguments, or one that stopped while a session was
using it. The receiver reports either and keeps serving; a backend that retried inside itself
would be hiding its own failures.

## Tests

```sh
cargo test                      # unit tests, no FFmpeg needed
cargo test -- --ignored         # needs ffmpeg and ffprobe on PATH
```

The ignored tests decode the checked-in fixtures and count what comes out: 48 pictures and
40 AAC-ELD frames, and an ALAC stream built at test time that has to decode to exactly what
`ffmpeg` makes of the file it was encoded into. A program that accepts every frame and
decodes none of them is what counting decoded bytes catches. [tests/fixtures/README.md](tests/fixtures/README.md)
records what each fixture is and where it came from.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).

Chinese: [README.zh-CN.md](README.zh-CN.md).