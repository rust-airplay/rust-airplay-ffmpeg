// SPDX-License-Identifier: GPL-3.0-or-later
//! What a caller can choose: which FFmpeg decodes, and which program plays.

use std::fmt;

/// The program that decodes audio.
pub const DECODER: &str = "ffmpeg";

/// The program that plays, and the default for both sinks.
///
/// `ffplay` ships with FFmpeg and reads either a container or raw PCM from its standard
/// input, which is exactly what this backend hands a sink. A machine that installed only
/// the `ffmpeg` binary can name another program; one with neither has no playback and the
/// renderer says so at startup.
pub const PLAYER: &str = "ffplay";

/// Environment variable naming the program that decodes audio.
pub const DECODER_ENV: &str = "AIRPLAY_FFMPEG";

/// Environment variable naming the program that plays audio, and its arguments.
pub const AUDIO_SINK_ENV: &str = "AIRPLAY_FFMPEG_AUDIO_SINK";

/// Environment variable naming the program that displays video, and its arguments.
pub const VIDEO_SINK_ENV: &str = "AIRPLAY_FFMPEG_VIDEO_SINK";

/// The longest interval a caller may ask the video path to pace at, in seconds.
///
/// A client says how far apart its frames are, and a stream that has stalled reports an
/// interval that describes the stall. Pacing against that would freeze the picture for as
/// long as the client was away, so the value is bounded rather than trusted.
pub const MAX_PACE_SECONDS: f64 = 1.0;

/// Which language a sink's arguments are written in.
///
/// The default is FFmpeg's: the backend puts the description of what the program reads in
/// front of the sink's own arguments, which is what makes @@ffplay@@ or a recorder one
/// line. A sink that reads the stream itself -- a pipeline builder, a player with its own
/// option language -- takes no description from here, and `Dialect::Raw` is how it says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// The backend describes the input, then the sink's arguments follow.
    Ffmpeg,
    /// The sink is given the stream as it is and no arguments of the backend's.
    Raw,
}

/// A program and the arguments it is run with.
///
/// A sink is one of these: the program reads the stream this backend produces on its
/// standard input, and the arguments are appended to the ones that describe the input.
/// `Program::new("ffplay")` is the player; `Program::new("ffmpeg").arg("-f").arg("wav")
/// .arg("out.wav")` is a sink that records the stream instead of playing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    name: String,
    arguments: Vec<String>,
    dialect: Dialect,
    environment: Vec<(String, String)>,
}

impl Program {
    /// A program with no arguments of its own.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arguments: Vec::new(),
            dialect: Dialect::Ffmpeg,
            environment: Vec::new(),
        }
    }

    /// A program that reads the stream itself, with no description of it from this backend.
    ///
    /// The stream is Annex-B for video and 16-bit PCM for audio, on standard input; what
    /// to do with it is the program's own option language. `gst-launch-1.0` is the reason
    /// this exists: its pipeline is not a list of FFmpeg options.
    #[must_use]
    pub fn raw(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            arguments: Vec::new(),
            dialect: Dialect::Raw,
            environment: Vec::new(),
        }
    }

    /// Names one variable for the program this sink starts.
    ///
    /// A player reads some of its behaviour from the environment rather than from its
    /// arguments -- SDL's renderer choice is the one this backend needs -- and a backend
    /// that made a caller export a variable would be one that only works in the right
    /// shell.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.environment.push((name.into(), value.into()));
        self
    }

    /// The variables the program is started with.
    #[must_use]
    pub fn environment(&self) -> &[(String, String)] {
        &self.environment
    }

    /// Which language this sink's arguments are in.
    #[must_use]
    pub const fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// The same program, with one more argument.
    #[must_use]
    pub fn arg(mut self, argument: impl Into<String>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    /// The same program, with more arguments.
    #[must_use]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// The program to run.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The arguments that follow the ones describing the input.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
}

impl fmt::Display for Program {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.name)?;
        for argument in &self.arguments {
            write!(formatter, " {argument}")?;
        }
        Ok(())
    }
}

/// Everything a caller can choose about how a session is played.
///
/// The defaults are what the environment variables would produce, so
/// [`Options::default`] behaves exactly like [`crate::FfmpegRenderer::new`].
#[derive(Clone, Debug)]
pub struct Options {
    decoder: String,
    audio_sink: Program,
    video_sink: Program,
    pacing: bool,
    warm_video: bool,
    /// The coding a warm video sink is started for.
    warm_codec: rust_airplay::VideoCodec,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            decoder: named(DECODER_ENV).unwrap_or_else(|| DECODER.to_owned()),
            // A player that opens no window for audio, because an audio-only session
            // should not show one.
            audio_sink: named_program(AUDIO_SINK_ENV)
                .unwrap_or_else(|| Program::new(PLAYER).arg("-nodisp")),
            // A player that drops a frame it cannot display in time rather than falling
            // further behind: the frame was decoded either way, so only the display is
            // skipped and the stream's references stay intact.
            video_sink: named_program(VIDEO_SINK_ENV)
                .unwrap_or_else(|| Program::new(PLAYER).arg("-framedrop")),
            pacing: true,
            warm_video: false,
            warm_codec: rust_airplay::VideoCodec::H264,
        }
    }
}

impl Options {
    /// Options for these two sinks.
    ///
    /// The decoder is [`DECODER`] and pacing is on, which is the same pair
    /// [`Options::default`] chooses when the environment says nothing.
    #[must_use]
    pub fn new(audio_sink: Program, video_sink: Program) -> Self {
        Self {
            decoder: DECODER.to_owned(),
            audio_sink,
            video_sink,
            pacing: true,
            warm_video: false,
            warm_codec: rust_airplay::VideoCodec::H264,
        }
    }

    /// Uses this program for decoding audio.
    #[must_use]
    pub fn with_decoder(mut self, program: impl Into<String>) -> Self {
        self.decoder = program.into();
        self
    }

    /// Plays audio with this program.
    #[must_use]
    pub fn with_audio_sink(mut self, sink: Program) -> Self {
        self.audio_sink = sink;
        self
    }

    /// Displays video with this program.
    #[must_use]
    pub fn with_video_sink(mut self, sink: Program) -> Self {
        self.video_sink = sink;
        self
    }

    /// Keeps the video sink running between sessions.
    ///
    /// A pipeline program needs about a second to build its graph, open its window and
    /// reach PLAYING. Started at the first frame, that second is spent with the receiver's
    /// own queue filling behind it: measured with a GStreamer sink, thirty-six frames were
    /// dropped in the first second and the picture never recovered, and delaying the open
    /// to wait for the pipeline instead leaves a second of frames queued in front of the
    /// picture. A sink started with the renderer is already playing when a client connects,
    /// so neither happens.
    #[must_use]
    pub const fn with_warm_video(mut self, warm: bool) -> Self {
        self.warm_video = warm;
        self
    }

    /// Whether the video sink is kept running between sessions.
    #[must_use]
    pub const fn warm_video(&self) -> bool {
        self.warm_video
    }

    /// Names the coding a warm sink is started for.
    ///
    /// A warm sink starts before a client connects, so the coding it is started for
    /// cannot be read off a stream that has not arrived: a receiver that offers HEVC
    /// and warms an H.264 sink pays at the first frame the start it meant to avoid.
    #[must_use]
    pub const fn with_warm_codec(mut self, codec: rust_airplay::VideoCodec) -> Self {
        self.warm_codec = codec;
        self
    }

    /// The coding a warm sink is started for.
    #[must_use]
    pub const fn warm_codec(&self) -> rust_airplay::VideoCodec {
        self.warm_codec
    }

    /// Turns pacing on or off.
    ///
    /// On, a wait asked for through [`rust_airplay::VideoStream::set_pace`] is applied.
    /// The receiver of rust-airplay 0.3.1 and later paces video itself and never calls
    /// that, so this matters only to a caller driving the trait directly.
    #[must_use]
    pub const fn with_pacing(mut self, pacing: bool) -> Self {
        self.pacing = pacing;
        self
    }

    /// Turns pacing off, for a sink that schedules on its own clock.
    #[must_use]
    pub const fn without_pacing(self) -> Self {
        self.with_pacing(false)
    }

    /// The program that decodes audio.
    #[must_use]
    pub fn decoder(&self) -> &str {
        &self.decoder
    }

    /// The program that plays audio.
    #[must_use]
    pub fn audio_sink(&self) -> &Program {
        &self.audio_sink
    }

    /// The program that displays video.
    #[must_use]
    pub fn video_sink(&self) -> &Program {
        &self.video_sink
    }

    /// Whether a wait asked for through `set_pace` is applied.
    #[must_use]
    pub const fn pacing(&self) -> bool {
        self.pacing
    }
}

/// A program named by the environment, when it names one.
fn named(variable: &str) -> Option<String> {
    std::env::var(variable)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// A program and its arguments named by the environment, when it names one.
///
/// The value is a command line split on whitespace. Quoting is not interpreted: a path
/// with a space in it is a programme name nobody sets this way, and a caller that needs
/// one passes [`Options::new`] and [`Program`] instead.
fn named_program(variable: &str) -> Option<Program> {
    let line = named(variable)?;
    let mut words = line.split_whitespace().map(str::to_owned);
    let name = words.next()?;
    Some(Program {
        name,
        arguments: words.collect(),
        dialect: Dialect::Ffmpeg,
        environment: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::{MAX_PACE_SECONDS, Options, Program};

    #[test]
    fn a_sink_keeps_the_arguments_it_was_given_separately() {
        let sink = Program::new("ffmpeg").arg("-f").arg("wav").args(["out.wav"]);
        assert_eq!(sink.name(), "ffmpeg");
        assert_eq!(sink.arguments(), ["-f", "wav", "out.wav"]);
        // The display form is the command line, which is what a message quotes.
        assert_eq!(sink.to_string(), "ffmpeg -f wav out.wav");
    }

    #[test]
    fn the_two_sinks_are_chosen_apart() {
        // A session can have audio without video, and a caller that named one sink should
        // not have the other replaced.
        let options = Options::new(Program::new("one"), Program::new("two"))
            .with_audio_sink(Program::new("three"))
            .with_pacing(false);
        assert_eq!(options.audio_sink().name(), "three");
        assert_eq!(options.video_sink().name(), "two");
        assert!(!options.pacing());
        assert!(
            Options::new(Program::new("one"), Program::new("two"))
                .without_pacing()
                .with_pacing(true)
                .pacing()
        );
    }

    #[test]
    fn a_pace_cannot_be_asked_for_beyond_the_bound() {
        assert_eq!(MAX_PACE_SECONDS, 1.0);
    }
}
