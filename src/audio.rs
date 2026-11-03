// SPDX-License-Identifier: GPL-3.0-or-later
//! The audio stream: Matroska into a decoder, PCM out of it, a gain, a sink.
//!
//! A coded audio frame has no sync word, so it cannot be written to a program the way
//! video is. The frames are wrapped in a Matroska stream (see the `matroska` module),
//! `ffmpeg` decodes that to 16-bit PCM, and the PCM passes through the gain this backend
//! owns before a sink plays it.
//!
//! The gain is here rather than in the decoder's own filter chain for one reason: a
//! filter's volume is fixed when the process starts, and a client moves its slider while
//! the stream is playing. A decoder whose arguments were fixed at open can only be
//! replaced, and replacing it drops the queue and makes a gap; a gain applied to the PCM
//! on its way past costs nothing and takes effect on the next sample.
//!
//! ```text
//! frames -> Matroska -> ffmpeg -f matroska -i pipe:0 -f s16le -ac 2 -ar 44100 pipe:1
//!        -> gain -> sink
//! ```
//!
//! A flush replaces both processes and starts the stream again with a fresh header: the
//! queued PCM and the decoder's window of samples are both stale, and neither can be
//! told to forget.

use std::io::Read;
use std::process::ChildStdout;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rust_airplay::{AudioConfig, AudioFrame, AudioStream, RenderError};

use crate::codec;
use crate::matroska::{Muxer, Track};
use crate::options::{Dialect, Program};
use crate::process::{self, Process, lock};

/// How much PCM is read, gained and handed on at a time.
///
/// About ninety milliseconds at 44.1 kHz stereo: large enough that the copy is not the
/// cost, small enough that a volume change is heard before the next frame arrives.
const PCM_CHUNK: usize = 16 * 1024;

/// An open audio stream.
pub(crate) struct FfmpegAudio {
    decoder: Arc<Process>,
    /// The program that plays this stream, and the one playing it.
    ///
    /// The player is replaced rather than restarted on a flush: the thread carrying PCM
    /// from the decoder holds a reference to the player it was started for, and a process
    /// restarted in place would take that thread's stale PCM as if it were the new
    /// stream's.
    sink_program: Program,
    sink: Mutex<Arc<Process>>,
    muxer: Mutex<Muxer>,
    /// The track a flush has to declare again, because the process it was declared to is
    /// gone.
    track: Track,
    gain: Arc<AtomicU32>,
    pump: Mutex<Option<JoinHandle<()>>>,
    config: AudioConfig,
}

impl FfmpegAudio {
    /// Starts the decoder, the sink, and the thread between them.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Unavailable`] for a configuration FFmpeg cannot be told
    /// about and for a machine without the programs; [`RenderError::Failed`] when a
    /// program refuses what it was asked to do.
    pub(crate) fn open(
        config: AudioConfig,
        decoder: &str,
        sink: &Program,
        gain: Arc<AtomicU32>,
    ) -> Result<Self, RenderError> {
        let track = codec::track(config)?;
        let (process, pcm) =
            Process::start_piped(decoder, &decoder_arguments(config), sink.environment())
                .map_err(process::as_render_error)?;
        let decoder = Arc::new(process);
        // Started before the decoder is fed anything: its standard input is where the PCM
        // is going, and a decoder that filled a pipe with nowhere to go would block
        // before the first frame.
        let sink_process =
            Process::start(sink.name(), &sink_arguments(config, sink), sink.environment())
                .map_err(process::as_render_error)?;
        let player = Arc::new(sink_process);
        let muxer = Muxer::new(track.clone());
        decoder.push(muxer.header()).map_err(process::as_render_error)?;
        let pump = spawn_pump(pcm, Arc::clone(&player), Arc::clone(&gain))?;
        Ok(Self {
            decoder,
            sink_program: sink.clone(),
            sink: Mutex::new(player),
            muxer: Mutex::new(muxer),
            track,
            gain,
            pump: Mutex::new(Some(pump)),
            config,
        })
    }

    /// Wraps one coded frame in the stream the decoder reads.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Failed`] when the decoder has stopped or is not taking
    /// frames.
    fn send_frame(&self, frame: AudioFrame) -> Result<(), RenderError> {
        // A client's timestamp counts samples, not seconds: dividing by the rate is what
        // turns it into the millisecond the container's block needs.
        let rate = u64::from(self.config.sample_rate).max(1);
        let timestamp_ms = frame.timestamp.saturating_mul(1_000) / rate;
        let bytes = lock(&self.muxer).block(timestamp_ms, &frame.data);
        self.decoder.push(bytes).map_err(process::as_render_error)
    }

    /// Drops what is queued and starts a clean stream.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Failed`] when a new process cannot be started, which
    /// leaves the stream stopped: a flush that half succeeded is not one to keep pushing
    /// into.
    fn restart_stream(&self) -> Result<(), RenderError> {
        // The pump reads from the decoder that is about to be replaced, so it is detached
        // rather than joined: it ends on its own when its pipe closes, and a flush that
        // waited for it would wait for a sink that is being replaced too.
        *lock(&self.pump) = None;
        self.decoder.abort();
        // The pump that was carrying PCM holds the player it was started for. Stopping
        // that player makes the old pump's next write fail, which is how it ends; the new
        // stream gets a new player so the two pumps can never write into one queue.
        lock(&self.sink).abort();
        let pcm = self
            .decoder
            .restart(true)
            .map_err(process::as_render_error)?
            .ok_or_else(|| RenderError::Failed("the decoder has no output".to_owned()))?;
        let sink = Arc::new(
            Process::start(
                self.sink_program.name(),
                &sink_arguments(self.config, &self.sink_program),
                self.sink_program.environment(),
            )
            .map_err(process::as_render_error)?,
        );
        let muxer = Muxer::new(self.track.clone());
        self.decoder.push(muxer.header()).map_err(process::as_render_error)?;
        *lock(&self.muxer) = muxer;
        *lock(&self.sink) = Arc::clone(&sink);
        *lock(&self.pump) = Some(spawn_pump(pcm, sink, Arc::clone(&self.gain))?);
        Ok(())
    }

    /// Ends the stream and waits for what was written to be played.
    ///
    /// # Errors
    ///
    /// Returns the first thing that went wrong, after both processes have been stopped:
    /// a decoder that failed on the last frame has still played every frame before it.
    fn end_stream(&self) -> Result<(), RenderError> {
        let decoded = self.decoder.finish();
        // The pump has to finish before the sink is closed, or the last chunk of PCM is
        // written to a pipe nobody is reading.
        if let Some(pump) = lock(&self.pump).take() {
            let _ = pump.join();
        }
        let played = Arc::clone(&lock(&self.sink)).finish();
        decoded.map_err(process::as_render_error)?;
        played.map_err(process::as_render_error)
    }
}

impl std::fmt::Debug for FfmpegAudio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FfmpegAudio")
            .field("codec", &self.config.codec.name())
            .field("sample_rate", &self.config.sample_rate)
            .field("channels", &self.config.channels)
            .field("decoder", &self.decoder.description())
            .field("sink", &lock(&self.sink).description())
            .finish()
    }
}

impl AudioStream for FfmpegAudio {
    fn push(&self, frame: AudioFrame) -> Result<(), RenderError> {
        self.send_frame(frame)
    }

    fn flush(&self) -> Result<(), RenderError> {
        self.restart_stream()
    }

    fn close(&self) -> Result<(), RenderError> {
        self.end_stream()
    }
}

impl Drop for FfmpegAudio {
    fn drop(&mut self) {
        // A receiver can drop a stream without closing it when a session dies, and a
        // backend that only stops on close would leave a decoder and a player running.
        self.decoder.abort();
        lock(&self.sink).abort();
    }
}

/// The arguments the decoder is started with: a Matroska stream in, PCM out.
pub(crate) fn decoder_arguments(config: AudioConfig) -> Vec<String> {
    vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        // The same cap the video sink is given: a decoder that reads five seconds ahead
        // of what it plays is a stream five seconds late. A Matroska header is a few
        // hundred bytes, so this is ample.
        "-probesize".to_owned(),
        "65536".to_owned(),
        "-analyzeduration".to_owned(),
        "0".to_owned(),
        "-f".to_owned(),
        "matroska".to_owned(),
        "-i".to_owned(),
        "pipe:0".to_owned(),
        "-vn".to_owned(),
        "-f".to_owned(),
        "s16le".to_owned(),
        "-ac".to_owned(),
        config.channels.to_string(),
        "-ar".to_owned(),
        config.sample_rate.to_string(),
        "pipe:1".to_owned(),
    ]
}

/// The arguments the sink is started with: the PCM it reads, then what the caller chose.
///
/// The channel count is named with @@-ch_layout@@ and not @@-ac@@, which is the difference
/// between a player that starts and one that refuses the stream: @@ffmpeg@@ accepts either
/// before an input, and @@ffplay@@ -- whose player does not take @@@-ac@@@ -- passes the
/// unknown option to the demuxer, which has no such field, and exits with "Option not
/// found". A raw PCM stream says nothing about its layout, so this is the only place the
/// sink can be told, and a session that loses it loses the sound rather than the picture.
pub(crate) fn sink_arguments(config: AudioConfig, sink: &Program) -> Vec<String> {
    // A sink that reads the stream itself brings its own option language: an FFmpeg
    // demuxer description in front of it is not one of its options.
    if sink.dialect() == Dialect::Raw {
        return sink.arguments().to_vec();
    }
    let mut arguments = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-probesize".to_owned(),
        "65536".to_owned(),
        "-analyzeduration".to_owned(),
        "0".to_owned(),
        "-f".to_owned(),
        "s16le".to_owned(),
        "-ch_layout".to_owned(),
        layout(config.channels).to_owned(),
        "-ar".to_owned(),
        config.sample_rate.to_string(),
        "-i".to_owned(),
        "pipe:0".to_owned(),
    ];
    arguments.extend(sink.arguments().iter().cloned());
    arguments
}

/// The channel layout FFmpeg names these channels by.
///
/// The decoder is told a channel count, and the sink is told a layout; letting the second
/// guess while the first is exact would hand a multichannel stream to a player as stereo.
/// [@@crate::codec::track@@] refuses a count this table cannot name, so the fallback is
/// unreachable for a configuration that got this far.
fn layout(channels: u8) -> &'static str {
    match channels {
        1 => "mono",
        2 => "stereo",
        3 => "3.0",
        4 => "quad",
        5 => "5.0",
        6 => "5.1",
        7 => "6.1",
        8 => "7.1",
        _ => "stereo",
    }
}

/// Starts the thread that carries PCM from the decoder to the sink.
fn spawn_pump(
    pcm: ChildStdout,
    sink: Arc<Process>,
    gain: Arc<AtomicU32>,
) -> Result<JoinHandle<()>, RenderError> {
    std::thread::Builder::new()
        .name("airplay-ffmpeg-gain".to_owned())
        .spawn(move || pump(pcm, &sink, &gain))
        .map_err(|error| RenderError::Failed(format!("the gain stage cannot start: {error}")))
}

/// Reads PCM, applies the gain, and hands it to the sink until one of them stops.
fn pump(mut pcm: ChildStdout, sink: &Process, gain: &AtomicU32) {
    let mut buffer = vec![0u8; PCM_CHUNK];
    // A read is not aligned to a sample, so the odd byte of a split sample waits here for
    // the byte that completes it.
    let mut carry: Option<u8> = None;
    loop {
        let read = match pcm.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => read,
            Err(error) => {
                log::debug!("the decoder's output stopped: {error}");
                return;
            }
        };
        let mut chunk = Vec::with_capacity(read + 1);
        if let Some(byte) = carry.take() {
            chunk.push(byte);
        }
        chunk.extend_from_slice(&buffer[..read]);
        let whole = chunk.len() & !1;
        if whole < chunk.len() {
            carry = Some(chunk[whole]);
        }
        chunk.truncate(whole);
        apply_gain(&mut chunk, f32::from_bits(gain.load(Ordering::Relaxed)));
        if let Err(error) = sink.push(chunk) {
            log::debug!("the sink stopped taking PCM: {error}");
            return;
        }
    }
}

/// Scales 16-bit little-endian samples by a linear gain, in place.
///
/// The gain is the one the client's slider set: [`Volume::level`] is a linear amplitude
/// from zero to one, and multiplying by it is what a volume control does.
fn apply_gain(samples: &mut [u8], gain: f32) {
    if gain >= 1.0 || samples.is_empty() {
        return;
    }
    for sample in samples.chunks_exact_mut(2) {
        let value = i16::from_le_bytes([sample[0], sample[1]]);
        let scaled = (f32::from(value) * gain).round();
        let clamped = scaled.clamp(f32::from(i16::MIN), f32::from(i16::MAX));
        sample.copy_from_slice(&(clamped as i16).to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_gain, decoder_arguments, sink_arguments};
    use crate::options::Program;
    use rust_airplay::AudioConfig;

    /// Samples as the little-endian bytes a decoder writes.
    fn samples(values: &[i16]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    #[test]
    fn a_gain_of_one_leaves_every_sample_as_it_was() {
        let original = samples(&[0, 16_384, -16_384, 32_767]);
        let mut gained = original.clone();
        apply_gain(&mut gained, 1.0);
        assert_eq!(gained, original);
    }

    #[test]
    fn a_gain_of_half_halves_every_sample() {
        let mut gained = samples(&[0, 16_384, -16_384, 32_767]);
        apply_gain(&mut gained, 0.5);
        // 32767 halved is 16383.5, which rounds away from zero.
        assert_eq!(gained, samples(&[0, 8_192, -8_192, 16_384]));
    }

    #[test]
    fn a_gain_of_zero_is_silence_and_never_overflows() {
        let mut gained = samples(&[i16::MIN, 32_767, -1]);
        apply_gain(&mut gained, 0.0);
        assert_eq!(gained, samples(&[0, 0, 0]));
        // A gain above one is not asked for by the volume type, and if one arrives it
        // clips rather than wrapping a loud sample into a quiet one.
        let mut loud = samples(&[32_767, -32_768]);
        apply_gain(&mut loud, 2.0);
        assert_eq!(loud, samples(&[32_767, -32_768]));
    }

    #[test]
    fn a_sink_that_reads_the_stream_itself_gets_no_pcm_description() {
        // The video path honours `Dialect::Raw`; audio has to as well, or a pipeline
        // builder is started with an FFmpeg demuxer description it cannot parse.
        let sink = Program::raw("gst-launch-1.0").args(["fdsrc", "!", "audioconvert", "!"]);
        let arguments = sink_arguments(AudioConfig::default(), &sink);
        assert_eq!(arguments, ["fdsrc", "!", "audioconvert", "!"]);
    }

    #[test]
    fn a_layout_is_named_for_every_channel_count_that_is_accepted() {
        // The decoder is told the count; the sink is told the layout. Every count the codec
        // layer accepts has to have a name here, or a surround stream plays as stereo.
        let config = AudioConfig { channels: 6, ..AudioConfig::default() };
        let arguments = sink_arguments(config, &Program::new("ffplay"));
        let at = arguments.iter().position(|argument| argument == "-ch_layout").expect("a layout");
        assert_eq!(arguments[at + 1], "5.1");
    }

    #[test]
    fn the_two_programs_are_given_the_formats_they_expect() {
        let config = AudioConfig::default();
        let decoder = decoder_arguments(config);
        assert!(decoder.contains(&"matroska".to_owned()), "{decoder:?}");
        assert!(decoder.contains(&"s16le".to_owned()), "{decoder:?}");
        let sink = sink_arguments(config, &Program::new("ffplay").arg("-nodisp"));
        assert_eq!(&sink[..5], ["-hide_banner", "-loglevel", "error", "-probesize", "65536"]);
        assert_eq!(sink.last().map(String::as_str), Some("-nodisp"));
        // The sink is told the format of what it reads; without it ffplay guesses.
        assert!(sink.contains(&"s16le".to_owned()), "{sink:?}");
        // And it is not allowed to read seconds ahead of what it plays.
        assert!(sink.contains(&"0".to_owned()), "{sink:?}");
        // The decoder has the same cap, for the same reason.
        assert_eq!(&decoder[3..5], ["-probesize", "65536"]);
        assert!(decoder.windows(2).any(|pair| pair == ["-analyzeduration", "0"]));
    }
}
