// SPDX-License-Identifier: GPL-3.0-or-later
//! The video stream: Annex-B written to a program that decodes and shows it.
//!
//! H.264 and HEVC carry start codes, so a decoder finds its own picture boundaries in a
//! byte stream and nothing has to be framed for it. That is why video needs no container
//! here and audio does: a program that reads its standard input is enough, and a decoder
//! that crashes on one frame takes only its own process with it.
//!
//! What a raw stream does not carry is a schedule. A player showing one has to decide when
//! each picture is due, and it has only itself to ask: with no timestamps it invents a
//! grid -- FFmpeg's guess is 25 frames a second -- and every frame is then shown on that
//! grid rather than when it arrived. A mirror is judged by how quickly the picture follows
//! the device, so this backend asks the player to stamp each frame with the moment it
//! arrived (`-use_wallclock_as_timestamps 1`) and to show it as soon as it is decoded.
//! Nothing then waits for a grid, and a client that encodes a little ahead, as a mirroring
//! client does to smooth its own network, does not buy that lead back as latency.
//!
//! A flush replaces the process. A decoder cannot be told to forget what it has queued,
//! so the one thing that can make it forget is a new decoder; the key frame a client
//! sends after a flush is what makes the new one correct rather than merely empty.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rust_airplay::{RenderError, VideoCodec, VideoFrame, VideoStream};

use crate::options::{MAX_PACE_SECONDS, Program};
use crate::pacing::Pacer;
use crate::process::{self, Process, lock};

/// Where a renderer keeps a video sink between sessions.
pub(crate) type WarmSlot = Mutex<Option<WarmSink>>;

/// An accelerator a build has.
///
/// Only `-hwaccel` is named: `ffplay` -- the default sink, and the program that decodes
/// here -- refuses `-hwaccel_output_format` outright ("Option not found"), so the flags
/// this backend adds have to be the ones every program in the family takes. The faster
/// form, where the frame is downloaded back explicitly, is a matter for a sink of the
/// caller's own; `docs/PERFORMANCE.md` carries both measurements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hardware {
    /// The name `-hwaccel` takes: `d3d11va`, `vaapi` or `videotoolbox`.
    pub(crate) accel: &'static str,
}

/// The hardware acceleration a build says it has, in the order this backend prefers.
///
/// The listing is what `ffmpeg -hwaccels` prints. The order is not the listing's: a
/// display that is not the decoder's needs the frame copied back, and these are the
/// accelerators whose download is a single copy on the platforms that have them.
pub(crate) fn hardware(listing: &str) -> Option<Hardware> {
    const PREFERRED: [&str; 3] = ["d3d11va", "vaapi", "videotoolbox"];
    PREFERRED
        .into_iter()
        .find(|accel| listing.lines().any(|line| line.trim() == *accel))
        .map(|accel| Hardware { accel })
}

/// How long after a restart a second one is refused.
///
/// A sink that dies as soon as it starts would otherwise be restarted for every frame; a
/// sink that dies once in a session is worth exactly one restart.
const RESTART_FLOOR: Duration = Duration::from_secs(5);

/// How many frames are counted before the stream reports how fast it is arriving.
///
/// A window rather than a single interval: a frame that arrives late because the network
/// hiccuped is not the frame rate, and a report built from one of those would be wrong
/// every time.
const ARRIVAL_WINDOW: u64 = 30;

/// A video sink the renderer keeps between sessions.
pub(crate) struct WarmSink {
    /// The coding the sink was started for.
    pub(crate) codec: VideoCodec,
    /// The running program.
    pub(crate) process: Arc<Process>,
    /// Whether a session is writing to it. A warm sink with a session on it is not
    /// available to another.
    pub(crate) claimed: Arc<AtomicBool>,
}

impl WarmSink {
    /// Whether this sink can be handed to a session for this coding.
    pub(crate) fn available(&self, codec: VideoCodec) -> bool {
        self.codec == codec && self.process.is_running()
    }
}

/// An open video stream.
pub(crate) struct FfmpegVideo {
    process: Arc<Process>,
    /// The claim this stream holds, when its sink outlives it.
    claim: Option<Arc<AtomicBool>>,
    pacer: Mutex<Pacer>,
    /// The interval a caller asked for; zero means it asked for none.
    interval: Mutex<Duration>,
    /// Whether this backend was built to honour [`rust_airplay::VideoStream::set_pace`].
    pacing: bool,
    /// Set when the process has been closed, so a later push says why it is refused.
    closed: AtomicBool,
    /// Set once the first unit has been handed over, so a stream that never produced one
    /// says so by saying nothing.
    started: AtomicBool,
    /// What has arrived, for the rate it is arriving at.
    arrival: Mutex<Arrival>,
    /// When the sink was last restarted after dying, so it cannot be restarted in a loop.
    last_restart: Mutex<Option<Instant>>,
    /// Set when the sink was replaced and has not yet been given a key frame: a frame that
    /// refers to pictures the new decoder never saw is a decoder error, not a picture.
    awaiting_keyframe: AtomicBool,
}

impl FfmpegVideo {
    /// Starts the program that will show this stream.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Unavailable`] for a coding this backend cannot name to
    /// FFmpeg or a program that is not on this machine, and
    /// [`RenderError::Failed`] for a program that refused the arguments it was given.
    pub(crate) fn open(
        codec: VideoCodec,
        sink: &Program,
        pacing: bool,
        hardware: Option<Hardware>,
    ) -> Result<Self, RenderError> {
        let arguments = arguments(codec, sink, hardware)?;
        log::debug!("video sink: {} {}", sink.name(), arguments.join(" "));
        log::info!("video    opening {} {}", sink.name(), arguments.join(" "));
        let process = Process::start(sink.name(), &arguments, sink.environment())
            .map_err(process::as_render_error)?;
        // A pipeline program takes about a second to reach PLAYING: gst-launch builds the
        // graph, opens the hardware sink's window and prerolls before it reads the pipe.
        // Until then nothing drains it, and the receiver's own queue is eight frames deep:
        // measured with a GStreamer sink, thirty-six frames were dropped in the first
        // second, which the receiver answers by discarding the picture until a key frame
        // that a mirroring client never sends. The receiver does allow two seconds of
        // startup queueing, so this wait spends that budget rather than overflowing
        // behind it.
        if sink.dialect() == crate::options::Dialect::Raw {
            std::thread::sleep(std::time::Duration::from_millis(1200));
        }
        Ok(Self {
            process: Arc::new(process),
            claim: None,
            awaiting_keyframe: AtomicBool::new(false),
            pacer: Mutex::new(Pacer::default()),
            interval: Mutex::new(Duration::ZERO),
            pacing,
            closed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            arrival: Mutex::new(Arrival::default()),
            last_restart: Mutex::new(None),
        })
    }

    /// Starts a sink for the renderer to keep, and hands a stream the claim on it.
    ///
    /// The sink is started here rather than at the first frame because a pipeline program
    /// needs about a second to reach PLAYING; a sink started with the renderer is playing
    /// before any client connects, so that second is spent while nobody is waiting for a
    /// picture instead of with a queue of frames building up behind it.
    ///
    /// # Errors
    ///
    /// The same as [`FfmpegVideo::open`].
    pub(crate) fn warm(
        codec: VideoCodec,
        sink: &Program,
        pacing: bool,
        hardware: Option<Hardware>,
    ) -> Result<(Self, WarmSink), RenderError> {
        let arguments = arguments(codec, sink, hardware)?;
        log::info!("video    keeping {} warm: {}", sink.name(), arguments.join(" "));
        let process = Process::start(sink.name(), &arguments, sink.environment())
            .map_err(process::as_render_error)?;
        let process = Arc::new(process);
        let claimed = Arc::new(AtomicBool::new(true));
        let stream = Self {
            process: Arc::clone(&process),
            claim: Some(Arc::clone(&claimed)),
            awaiting_keyframe: AtomicBool::new(false),
            pacer: Mutex::new(Pacer::default()),
            interval: Mutex::new(Duration::ZERO),
            pacing,
            closed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            arrival: Mutex::new(Arrival::default()),
            last_restart: Mutex::new(None),
        };
        Ok((stream, WarmSink { codec, process, claimed }))
    }

    /// Takes the claim on a sink the renderer is keeping.
    pub(crate) fn adopt(warm: WarmSink, pacing: bool) -> Self {
        Self {
            process: Arc::clone(&warm.process),
            claim: Some(Arc::clone(&warm.claimed)),
            awaiting_keyframe: AtomicBool::new(false),
            pacer: Mutex::new(Pacer::default()),
            interval: Mutex::new(Duration::ZERO),
            pacing,
            closed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            arrival: Mutex::new(Arrival::default()),
            last_restart: Mutex::new(None),
        }
    }

    /// Hands one access unit to the decoder.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Failed`] when the program has stopped or is not taking
    /// frames, which the receiver reports and the session continues.
    fn send_frame(&self, frame: VideoFrame) -> Result<(), RenderError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(RenderError::Failed("the video stream was closed".to_owned()));
        }
        // A sink that was just replaced has none of the pictures the next frame refers to:
        // handing it a P frame is a decoder error, not a picture. Everything is skipped
        // until the client sends a key frame it can start from.
        if self.awaiting_keyframe.load(Ordering::SeqCst) {
            if !frame.keyframe {
                return Ok(());
            }
            self.awaiting_keyframe.store(false, Ordering::SeqCst);
        }
        let interval = *lock(&self.interval);
        if self.pacing && !interval.is_zero() {
            lock(&self.pacer).wait(interval);
        }
        // Said before the frame is handed over, because the frame is moved into the pipe:
        // what the stream is arriving at is the one measurement a session that looks slow
        // cannot get from anywhere else.
        if let Some(report) = lock(&self.arrival).observe(&frame) {
            log::info!("video    {report}");
        }
        if !self.started.swap(true, Ordering::SeqCst) {
            log::info!(
                "video    first unit, {} bytes{}",
                frame.data.len(),
                if frame.keyframe { ", a key frame" } else { "" }
            );
            // How old the frame already is says which side of the network the delay is
            // on. The client's own timestamp is NTP fixed point since 1900, so against
            // a phone whose clock is set -- every phone's is -- this is the age of the
            // unit when it arrived here.
            // Two readings, because the client's timestamp is either whole NTP seconds
            // or 32.32 fixed point and one run says which: the other reading is absurd.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs_f64())
                .unwrap_or(f64::NAN)
                + 2_208_988_800.0;
            let plain = now - frame.timestamp as f64;
            let fixed = now - ntp_seconds(frame.timestamp);
            log::info!(
                "video    timestamp {} (plain {:.3} s old, fixed {:.3} s old)",
                frame.timestamp,
                plain,
                fixed
            );
        }
        // How long the sink takes is the difference between a picture that follows the
        // device and one the receiver has to throw frames away to keep up with: the
        // receiver's queue is a few frames deep, and a push that takes longer than a frame
        // period fills it.
        let started = Instant::now();
        let outcome = self.process.push(frame.data);
        let took = started.elapsed();
        lock(&self.arrival).record(took);
        if took > Duration::from_millis(100) {
            log::warn!(
                "video    the sink took {} ms to accept a frame{}",
                took.as_millis(),
                match self.process.report() {
                    report if report.is_empty() => String::new(),
                    report => format!(": {report}"),
                }
            );
        }
        if let Err(error) = outcome {
            let error = process::as_render_error(error);
            let ended = self
                .process
                .exit_status()
                .map_or_else(|| "still running".to_owned(), |status| status.to_string());
            let report = self.process.report();
            // A refused frame ends the receiver's video writer for the rest of the session:
            // its writer loop breaks on the first error, and every frame after that is
            // dropped while the window keeps the last picture it showed. That is a frozen
            // mirror, which a person reads as a picture that is seconds late. A sink that
            // died is therefore restarted here and the frame is given up rather than the
            // session: the picture returns at the next key frame, and a sink that dies
            // again within RESTART_FLOOR is not restarted at all.
            if self.recover() {
                log::warn!(
                    "video    the sink ended ({ended}); restarted it. Said: {report} ({error})"
                );
                return Ok(());
            }
            log::warn!(
                "video    {error}: the sink ended ({ended}) and was not restarted. Said: {report}"
            );
            return Err(error);
        }
        Ok(())
    }

    /// Starts the sink again after it died, at most once per RESTART_FLOOR.
    fn recover(&self) -> bool {
        let mut last = lock(&self.last_restart);
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < RESTART_FLOOR) {
            return false;
        }
        if self.process.restart(false).is_err() {
            return false;
        }
        *last = Some(now);
        lock(&self.pacer).reset();
        *lock(&self.arrival) = Arrival::default();
        // The replacement decoder has none of the pictures the next frame refers to.
        // Without this a P frame is written into an empty decoder, which reports it as a
        // reference error and shows nothing: the frozen picture a restart is meant to fix.
        self.awaiting_keyframe.store(true, Ordering::SeqCst);
        true
    }

    /// Replaces the decoder, which is the only way to empty its queue.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Failed`] when the new process cannot be started.
    fn restart_stream(&self) -> Result<(), RenderError> {
        self.process.restart(false).map_err(process::as_render_error)?;
        lock(&self.pacer).reset();
        *lock(&self.arrival) = Arrival::default();
        self.awaiting_keyframe.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Records the interval a caller wants frames played at.
    ///
    /// The receiver of rust-airplay 0.3.1 and later paces video itself and never calls
    /// this; it stays for a caller that drives the trait directly.
    fn pace(&self, seconds: f64) {
        // A value that is not a number is not an interval: zero is what "do not wait"
        // means, and a client that sent a nonsense timestamp must not hang the stream.
        let seconds = if seconds.is_finite() { seconds.clamp(0.0, MAX_PACE_SECONDS) } else { 0.0 };
        *lock(&self.interval) = Duration::from_secs_f64(seconds);
    }

    /// Ends the stream and waits for the program to show what it has left.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Failed`] when the program fails or does not exit.
    fn end_stream(&self) -> Result<(), RenderError> {
        self.closed.store(true, Ordering::SeqCst);
        // A sink the renderer keeps warm is released rather than closed: closing its pipe
        // is the end of stream, and the next session is meant to write into the same one.
        if let Some(claim) = &self.claim {
            claim.store(false, Ordering::SeqCst);
            lock(&self.pacer).reset();
            *lock(&self.arrival) = Arrival::default();
            return Ok(());
        }
        self.process.finish().map_err(process::as_render_error)
    }
}

impl std::fmt::Debug for FfmpegVideo {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FfmpegVideo")
            .field("sink", &self.process.description())
            .field("pacing", &self.pacing)
            .field("interval", &lock(&self.interval))
            .finish()
    }
}

impl VideoStream for FfmpegVideo {
    fn push(&self, frame: VideoFrame) -> Result<(), RenderError> {
        self.send_frame(frame)
    }

    fn set_pace(&self, interval_seconds: f64) -> Result<(), RenderError> {
        self.pace(interval_seconds);
        Ok(())
    }

    fn flush(&self) -> Result<(), RenderError> {
        self.restart_stream()
    }

    fn close(&self) -> Result<(), RenderError> {
        self.end_stream()
    }
}

impl Drop for FfmpegVideo {
    fn drop(&mut self) {
        // A warm sink outlives the session that used it, so the claim has to go with the
        // session: a sink left claimed is one no later session can adopt.
        if let Some(claim) = &self.claim {
            claim.store(false, Ordering::SeqCst);
        }
    }
}

/// One window of arrived frames.
#[derive(Debug, Default)]
struct Arrival {
    frames: u64,
    bytes: u64,
    first: Option<f64>,
    last: Option<f64>,
    /// How long the sink took for the slowest frame of this window.
    worst_push: Duration,
    /// How long the sink took, summed over the window.
    total_push: Duration,
    /// When the first frame of this window arrived, on this machine's clock.
    first_at: Option<f64>,
    /// When the last frame of this window arrived.
    last_at: Option<f64>,
}

impl Arrival {
    /// Records a frame and, once a window is full, says what the window held.
    ///
    /// The timestamps are the client's own, so the rate they give is the rate the client
    /// captured at and not the rate the network happened to deliver at. They are also the
    /// reason this is a measurement and not a setting: nothing in a coded frame says how
    /// many of them make a second.
    /// Records how long the sink took for one frame.
    fn record(&mut self, took: Duration) {
        self.total_push += took;
        if took > self.worst_push {
            self.worst_push = took;
        }
    }

    fn observe(&mut self, frame: &VideoFrame) -> Option<String> {
        let arrived = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs_f64())
            .unwrap_or(f64::NAN);
        if self.first_at.is_none() {
            self.first_at = Some(arrived);
        }
        self.last_at = Some(arrived);
        self.frames += 1;
        self.bytes += frame.data.len() as u64;
        let at = ntp_seconds(frame.timestamp);
        if self.first.is_none() {
            self.first = Some(at);
        }
        self.last = Some(at);
        if self.frames < ARRIVAL_WINDOW {
            return None;
        }
        let report = match (self.first, self.last) {
            (Some(first), Some(last)) if last - first > 0.05 => {
                let fps = (self.frames - 1) as f64 / (last - first);
                // The client's own stamp for the last frame of the window, with this
                // machine's clock beside it: the receiver knows the offset between the two
                // clocks, and against that offset these two numbers are the age of the
                // picture when it got here.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|since| since.as_secs_f64())
                    .unwrap_or(f64::NAN);
                // The wall time this window took against the time the client says it
                // captured. Equal is the healthy case: frames arrive as fast as they were
                // made. Larger means the picture is falling further behind with every
                // window -- the client cannot encode what the offered display asks of it,
                // so its own queue grows and the mirror drifts instead of stuttering.
                let wall = match (self.first_at, self.last_at) {
                    (Some(first_at), Some(last_at)) => last_at - first_at,
                    _ => f64::NAN,
                };
                let behind = wall - (last - first);
                let behind = if behind > 0.1 {
                    format!(", {behind:.2} s behind per window")
                } else {
                    String::new()
                };
                format!(
                    "{fps:.1} fps, {:.1} kB per unit, sink {:.1} ms mean, {:.1} ms worst, raw {} at {now:.3}{behind}",
                    self.bytes as f64 / self.frames as f64 / 1024.0,
                    self.total_push.as_secs_f64() * 1_000.0 / self.frames as f64,
                    self.worst_push.as_secs_f64() * 1_000.0,
                    frame.timestamp
                )
            }
            // A client that stamps every frame the same, or stamps none, cannot be
            // measured; saying so is better than dividing by zero and reporting a rate.
            _ => format!("{} frames, timestamps unusable", self.frames),
        };
        *self = Self::default();
        Some(report)
    }
}

/// The seconds a 64-bit NTP fixed-point timestamp names.
///
/// The high half is seconds and the low half is a fraction of one, which is what a
/// mirroring client counts its frames in.
fn ntp_seconds(timestamp: u64) -> f64 {
    (timestamp >> 32) as f64 + f64::from((timestamp & 0xFFFF_FFFF) as u32) / 4_294_967_296.0
}

/// The arguments the sink is started with: what it reads, then what the caller chose.
///
/// The input format is named rather than probed. A decoder given a stream on a pipe has
/// no file name to go by, and probing a live stream costs the first frames while the
/// prober decides it does not have enough yet.
///
/// # Errors
///
/// Returns [`RenderError::Unavailable`] for a coding this backend cannot name to
/// FFmpeg.
pub(crate) fn arguments(
    codec: VideoCodec,
    sink: &Program,
    hardware: Option<Hardware>,
) -> Result<Vec<String>, RenderError> {
    // A sink that reads the stream itself is given it and nothing else: it has its own
    // option language, and an FFmpeg argument in front of a pipeline is not one of its
    // options.
    if sink.dialect() == crate::options::Dialect::Raw {
        return Ok(sink.arguments().to_vec());
    }
    let format = demuxer(codec)?;
    let mut arguments: Vec<String> = [
        "-hide_banner",
        "-loglevel",
        "error",
        // How much of the stream the player may read before it starts showing any of it.
        // FFmpeg's defaults are a cap of five megabytes and five seconds of input, and a
        // live mirror reaches that cap: measured on a captured stream, the player took
        // 3401 ms to work out what it was reading and only then began, which is a picture
        // permanently three seconds behind. 64 KiB is a few frames -- enough for the
        // parameter sets at the front of the stream -- and the same measurement is 259 ms.
        "-probesize",
        "65536",
        "-analyzeduration",
        "0",
        // Let the decoder hand a frame over as soon as it has one instead of holding it
        // for the reordering window a file could afford.
        //
        // Not `-fflags nobuffer`, which reads like the right flag for a live stream and is
        // not: it stops the demuxer buffering, and the raw H.264 demuxer needs the first
        // packets buffered to find the parameter sets. A stream given it decodes nothing at
        // all, which is a good deal worse than latency.
        "-flags",
        "low_delay",
        // Show each frame when it arrives rather than on a grid the player invented. This
        // is the difference between a picture that follows the device and one that follows
        // a guess: an AirPlay client encodes a little ahead to smooth its own network, and
        // a player that schedules against capture time hands that lead back as latency.
        "-use_wallclock_as_timestamps",
        "1",
        "-f",
        format,
        "-i",
        "pipe:0",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if let Some(hardware) = hardware {
        // An input option: after `-i` it names nothing. The frame comes back to system
        // memory by the decoder's own download, because what reads this is a player that
        // presents through SDL and not a sink that can take a GPU surface.
        let input =
            arguments.iter().position(|argument| argument == "-i").unwrap_or(arguments.len());
        arguments.splice(input..input, ["-hwaccel", hardware.accel].into_iter().map(str::to_owned));
    }
    // The sink's own arguments come last, so a caller can override any of the above.
    arguments.extend(sink.arguments().iter().cloned());
    Ok(arguments)
}

/// The FFmpeg demuxer that reads this coding from a byte stream.
///
/// FFmpeg calls HEVC `hevc`, not the `h265` the core uses for the same thing: the two
/// names come from different worlds and only one of them opens a stream.
fn demuxer(codec: VideoCodec) -> Result<&'static str, RenderError> {
    match codec {
        VideoCodec::H264 => Ok("h264"),
        VideoCodec::H265 => Ok("hevc"),
        other => {
            Err(RenderError::Unavailable(format!("this backend does not decode {}", other.name())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Arrival, Hardware, arguments, demuxer, ntp_seconds};
    use crate::options::Program;
    use rust_airplay::{VideoCodec, VideoFrame};

    fn frame(seconds: f64, bytes: usize) -> VideoFrame {
        VideoFrame {
            data: vec![0; bytes],
            timestamp: (seconds * 4_294_967_296.0) as u64,
            keyframe: false,
        }
    }

    #[test]
    fn the_two_codings_name_the_demuxers_that_read_them() {
        assert_eq!(demuxer(VideoCodec::H264).unwrap(), "h264");
        // Not "h265": FFmpeg's demuxer for the same coding is called hevc, and a stream
        // named to it as h265 opens nothing.
        assert_eq!(demuxer(VideoCodec::H265).unwrap(), "hevc");
    }

    #[test]
    fn a_sinks_arguments_follow_the_ones_that_describe_the_input() {
        let sink = Program::new("ffmpeg").arg("-f").arg("null").arg("-");
        let arguments = arguments(VideoCodec::H264, &sink, None).expect("arguments");
        assert_eq!(
            arguments,
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-probesize",
                "65536",
                "-analyzeduration",
                "0",
                "-flags",
                "low_delay",
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-f",
                "null",
                "-"
            ]
        );
    }

    #[test]
    fn the_sink_is_told_to_stamp_frames_when_they_arrive() {
        let sink = Program::new("ffplay").arg("-framedrop");
        let arguments = arguments(VideoCodec::H264, &sink, None).expect("arguments");
        let at = arguments
            .iter()
            .position(|argument| argument == "-use_wallclock_as_timestamps")
            .expect("the timestamp mode");
        assert_eq!(arguments[at + 1], "1");
        // It has to be an input option: after `-i` the stream is already open, and the
        // player has already decided what to do about a stream with no timestamps.
        let input = arguments.iter().position(|argument| argument == "pipe:0").expect("an input");
        assert!(at < input, "{arguments:?}");
        // And the player's own way of not falling behind: a frame it is too late to show
        // is skipped at the display, which leaves the stream's references intact.
        assert!(arguments.iter().any(|argument| argument == "-framedrop"));
    }

    #[test]
    fn a_sink_that_reads_the_stream_itself_is_given_no_ffmpeg_arguments() {
        // The GStreamer pipeline this backend can hand a stream to is the reason the
        // dialect exists: an FFmpeg option in front of a gst-launch pipeline is not a gst
        // option, and the pipeline would refuse to start.
        let sink = Program::raw("gst-launch-1.0").args([
            "fdsrc",
            "!",
            "h264parse",
            "!",
            "avdec_h264",
            "!",
            "d3d11videosink",
            "sync=false",
        ]);
        let arguments = arguments(VideoCodec::H264, &sink, None).expect("arguments");
        assert_eq!(
            arguments,
            ["fdsrc", "!", "h264parse", "!", "avdec_h264", "!", "d3d11videosink", "sync=false"]
        );
    }

    #[test]
    fn hardware_decoding_is_asked_for_before_the_input() {
        // `-hwaccel` names how the input is opened, so after `-i` it would name nothing.
        // Only that flag is added: `ffplay` rejects `-hwaccel_output_format`, and it is
        // the program that decodes here.
        let sink = Program::new("ffplay").arg("-framedrop");
        let hardware = Hardware { accel: "d3d11va" };
        let arguments = arguments(VideoCodec::H265, &sink, Some(hardware)).expect("arguments");
        let at = |needle: &str| arguments.iter().position(|argument| argument == needle);
        let input = at("-i").expect("an input");
        let accel = at("-hwaccel").expect("the accelerator");
        assert_eq!(arguments[accel + 1], "d3d11va");
        assert!(accel < input, "the accelerator has to be an input option: {arguments:?}");
        assert!(at("-hwaccel_output_format").is_none(), "{arguments:?}");
        assert!(at("-vf").is_none(), "{arguments:?}");
        // The sink's own arguments stay last, so a caller can still override any of this.
        assert_eq!(arguments.last().map(String::as_str), Some("-framedrop"));
    }

    #[test]
    fn a_listing_names_the_accelerator_this_backend_prefers_to_use() {
        use super::hardware;
        // What `ffmpeg -hwaccels` prints, in the build's own order.
        let listing = "Hardware acceleration methods:\ncuda\nvaapi\ndxva2\nqsv\nd3d11va\nvulkan\n";
        let chosen = hardware(listing).expect("an accelerator");
        assert_eq!(chosen.accel, "d3d11va");
        assert_eq!(hardware("Hardware acceleration methods:\n").map(|h| h.accel), None);
        assert_eq!(hardware("").map(|h| h.accel), None);
        // A method the platform cannot download from is not a choice this backend makes.
        assert_eq!(hardware("cuda\nvulkan\n").map(|h| h.accel), None);
    }

    #[test]
    fn a_window_of_arrivals_reports_the_rate_the_client_captured_at() {
        let mut arrival = Arrival::default();
        let mut report = None;
        for index in 0..30 {
            report = arrival.observe(&frame(f64::from(index) / 30.0, 2048)).or(report);
        }
        let report = report.expect("a report after a window");
        assert!(report.contains("30.0 fps"), "{report}");
        assert!(report.contains("2.0 kB"), "{report}");
    }

    #[test]
    fn a_window_whose_timestamps_do_not_advance_says_so() {
        let mut arrival = Arrival::default();
        let mut report = None;
        for _ in 0..30 {
            report = arrival.observe(&frame(1.0, 16)).or(report);
        }
        let report = report.expect("a report after a window");
        assert!(report.contains("timestamps unusable"), "{report}");
    }

    #[test]
    fn an_ntp_timestamp_is_seconds_and_a_fraction() {
        assert!((ntp_seconds(0) - 0.0).abs() < 1e-9);
        // One and a half seconds: the high half is 1, the low half is half of 2^32.
        assert!((ntp_seconds((1u64 << 32) | (1u64 << 31)) - 1.5).abs() < 1e-9);
    }
}
