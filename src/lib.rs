// SPDX-License-Identifier: GPL-3.0-or-later
//! An FFmpeg renderer for [`rust_airplay`].
//!
//! `rust-airplay` receives an AirPlay session, decrypts it and hands over coded frames.
//! This crate decodes them with the FFmpeg command-line tools and plays them: one
//! `ffmpeg` process decodes the audio and one program -- `ffplay` by default -- plays
//! each stream. It implements the core's [`rust_airplay::Renderer`] trait, so a program
//! depends on the core for the protocol and on this crate for playback.
//!
//! # Why programs and not a library
//!
//! A linked decoder would put FFmpeg's C libraries, their headers and their build
//! configuration between anyone who builds this crate and the music. Running the tools
//! instead means a machine with FFmpeg installed builds this crate with no development
//! packages at all, and a decoder that crashes on one frame takes only its own process
//! with it.
//!
//! The price is that a stream's format has to be one a program can read from its
//! standard input, which decides how each medium is delivered:
//!
//! - **Video** is Annex-B, written straight through. H.264 and HEVC carry start codes,
//!   so a decoder finds its own picture boundaries in a byte stream.
//! - **Audio** is a coded frame with no sync word and no length, so it is wrapped in a
//!   live Matroska stream and decoded to PCM, which the backend scales by the volume a
//!   client set before a sink plays it.
//!
//! ```text
//! audio: frames -> Matroska -> ffmpeg -> PCM -> gain -> sink
//! video: Annex-B -> sink (ffplay decodes and shows it)
//! ```
//!
//! `docs/ARCHITECTURE.md` explains what a frame becomes at each step, and what a flush
//! can and cannot do here.
//!
//! # Requirements
//!
//! `ffmpeg` and `ffplay` on `PATH`, or named through [`Options`]; a build of FFmpeg
//! with a decoder for H.264, HEVC, ALAC and AAC-ELD, which the usual distributions have.
//!
//! # Using it
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use rust_airplay::{Config, Renderer};
//!
//! # async fn start() -> Result<(), Box<dyn std::error::Error>> {
//! let renderer: Arc<dyn Renderer> =
//!     Arc::new(rust_airplay_ffmpeg::FfmpegRenderer::new().expect("a machine with FFmpeg"));
//! let config = Config::new("Living room", [0x42; 32], renderer);
//! rust_airplay::run(config).await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Sinks
//!
//! A sink is a program and the arguments that follow the ones describing its input: the
//! backend supplies what the program reads -- Matroska for the audio decoder, 16-bit PCM
//! for an audio sink, Annex-B for a video sink -- and then appends the sink's own
//! arguments. [`Options`] names the programs, and [`DECODER`], [`AUDIO_SINK_ENV`] and
//! [`VIDEO_SINK_ENV`] do the same without code. [`Program::raw`] is for a sink that
//! reads the stream itself and brings its own option language.
//!
//! # Time
//!
//! Neither medium is scheduled against the client's clock. Video is written as it arrives
//! and the player is told to show each frame when it gets it, rather than on a frame-rate
//! grid it had to guess; [`rust_airplay::VideoStream::set_pace`] stays for a caller that
//! drives the trait directly, and the wait it applies is a cumulative deadline rather than
//! a sleep per frame. This is also why the player is given a small input-probing cap: the
//! default reads seconds of a live stream before it shows any of it.
//!
//! # Failure
//!
//! [`rust_airplay::RenderError::Unavailable`] means the machine cannot do it: no
//! `ffmpeg`, no `ffplay`, no display, a build without the decoder. [`rust_airplay::RenderError::Failed`]
//! means the request could not be carried out: a program that refused its arguments, or
//! one that stopped while a session was using it. The receiver reports either and keeps
//! serving; a backend that retried inside itself would be hiding its own failures.

#![deny(missing_docs)]

mod audio;
mod codec;
mod matroska;
mod options;
mod pacing;
mod process;
mod renderer;
mod video;

pub use crate::options::{
    AUDIO_SINK_ENV, DECODER, DECODER_ENV, Dialect, MAX_PACE_SECONDS, Options, PLAYER, Program,
    VIDEO_SINK_ENV,
};
pub use crate::renderer::FfmpegRenderer;

/// Helpers for this crate's own tests.
///
/// Not part of the API. These exist so that the tests drive the same code the renderer
/// does -- one argument builder, one probe -- rather than a copy of it that can drift.
#[doc(hidden)]
pub mod testing {
    use rust_airplay::{AudioConfig, VideoCodec};

    use crate::options::Program;
    use crate::{audio, process, video};

    /// The arguments the audio decoder is started with.
    ///
    /// # Errors
    ///
    /// Returns what the configuration could not be described with.
    pub fn decoder_arguments(config: AudioConfig) -> Vec<String> {
        audio::decoder_arguments(config)
    }

    /// The arguments a sink is started with for this audio configuration.
    pub fn sink_arguments(config: AudioConfig, sink: &Program) -> Vec<String> {
        audio::sink_arguments(config, sink)
    }

    /// The arguments a video sink is started with.
    ///
    /// # Errors
    ///
    /// Returns what the coding could not be named to FFmpeg with, as text.
    pub fn video_arguments(codec: VideoCodec, sink: &Program) -> Result<Vec<String>, String> {
        video::arguments(codec, sink, None).map_err(|error| error.to_string())
    }

    /// The hardware acceleration a listing names, if any.
    ///
    /// The listing is what `ffmpeg -hwaccels` prints; this exposes the choice the
    /// renderer makes so that a test can read it without starting a program.
    pub fn video_hardware(listing: &str) -> Option<&'static str> {
        video::hardware(listing).map(|hardware| hardware.accel)
    }

    /// Whether this machine can start a program.
    pub fn program_runs(program: &str) -> bool {
        process::probe(program).is_ok()
    }
}
