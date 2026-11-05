# Changelog

All notable changes to this crate are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the version numbers follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-02

First release. The crate implements `rust_airplay::Renderer` with the FFmpeg command-line
tools, and depends on `rust-airplay = "0.3.2"` for the renderer boundary.

### Added

- **Hardware decoding, opt-in**: `Options::with_hardware_decoding(true)` asks the decoder
  which accelerators its build has, once, and adds `-hwaccel <name>` to every video
  stream. Measured on 4K HEVC, 300 frames: 6.96 s of CPU software against 2.3-3.4 s
  hardware, at 4-5x real time either way, so it buys CPU rather than latency. Only
  `-hwaccel` is added, because `ffplay` rejects `-hwaccel_output_format` -- the
  cheaper explicit download is documented for a sink that can take it.
- **`FfmpegRenderer`**, which starts a decoder and a sink per stream and refuses a machine
  without the programs when it is built rather than when a frame arrives.
- **An audio path that survives having no frame boundaries.** A raw AAC-ELD or ALAC frame
  has no sync word, so the frames are wrapped in a live Matroska stream -- unknown segment
  and cluster lengths, one `SimpleBlock` per frame -- with the codec configuration the
  frames do not carry: an `AudioSpecificConfig` built from the client's own values for ELD,
  and a 24-byte ALAC cookie built from `samples_per_frame`.
- **A video path that does not need one.** Annex-B is written to the sink in the order it
  arrives, stamped with the moment it arrived so the player shows it then rather than on a
  frame-rate grid it had to guess, with a cumulative-deadline pacer for a caller that asks
  for a spacing.
- **A player that does not buffer the stream behind it.** FFmpeg's default input probing is
  five megabytes or five seconds, and a live mirror reaches that cap: measured through this
  backend's own command line, the player took 3401 ms to start at the default and 259 ms
  when the cap is 64 KiB. The player is also told which SDL renderer to use, because the
  software one measured faster than the hardware one on the machine this crate was built
  against (1080p: 778 ms against 1059 ms for thirty frames).
- **A warm video sink**: `Options::with_warm_video` starts the sink with the renderer and
  each session adopts it, and `Options::with_warm_codec` names the coding it is started
  for. A cold player at 60 fps loses the first frames to the receiver's own queue -- measured:
  79 dropped, after which an HEVC stream has no reference to decode with -- and a pipeline
  program needs about a second to reach PLAYING, which is a second of frames queued in front
  of the picture if the open waits for it instead.
- **A sink may read the stream itself**: `Program::raw` with `Dialect::Raw` hands the
  stream over with no FFmpeg arguments, for a pipeline builder that has its own option
  language.
- **A sink may name its environment**: `Program::with_env`, for a player that reads its
  renderer from a variable rather than from its arguments.
- **Volume that reaches the stream that is playing**: the decoder produces PCM, the backend
  scales it, and an `AtomicU32` shared by every stream of a renderer means a slider change
  is heard on the next chunk instead of at the next session.
- **`Options`** naming the decoder and both sinks, with the environment variables
  `AIRPLAY_FFMPEG`, `AIRPLAY_FFMPEG_AUDIO_SINK` and `AIRPLAY_FFMPEG_VIDEO_SINK` as the
  defaults.
- **A bounded process boundary**: a four-buffer queue drained by its own thread, so a player
  that stops reading becomes a refused frame rather than a hang; a 150 ms window to detect a
  refused program at open; five seconds to drain on close; two seconds to reap a kill.
- **A sink that is restarted waits for a key frame.** A P frame describes pictures the new
  decoder never saw, which it reports as a reference error rather than a picture; the restart
  now skips frames until the client sends one it can start from.
- **An arrival report** that carries the client's stamp beside this machine's clock and says
  how far each window of frames fell behind, which is what separates a client that cannot
  keep up from a pipeline that is not keeping up.
- **The measured answer to a 4K mirror that is smooth and a second and a half late**: the
  coding the receiver offers. H.264 alone left the picture 1.48 s behind a stopwatch at
  3840x2160`;60`; offering HEVC removed the delay, with every stage of this backend
  unchanged and measured fast in both cases. See `docs/ARCHITECTURE.md`.
- **Tests that decode**: 48 H.264 pictures and 40 real AAC-ELD frames through the backend's
  own path, and an ALAC stream that has to decode byte for byte to what `ffmpeg` makes of
  the file it was encoded into.
- **A library, not a program.** The crate publishes a renderer and the types around it; the
  command line, the banner and the choice of backend belong to the program embedding it, as
  with [parlor-cli](https://github.com/rust-airplay/parlor-cli).

### Changed

- The crate is a library and nothing else: the command-line receiver that was used to
  develop it is gone, and the manifest declares no binary.
- Formatting follows Google's Rust style: `rustfmt.toml` sets the hundred-column width and
  the "Max" small-item heuristics, so a short call stays on one line.
- The crate is open source on GitHub and nowhere else: `publish = false` keeps an
  accidental `cargo publish` from putting it on crates.io, and the README depends on it
  by git.

### Known limits

- The two media are not synchronised against each other: each stream is played by its own
  program at the rate that program reads it. See `docs/ARCHITECTURE.md`.
- Nothing here draws a window: the sink owns the display.
