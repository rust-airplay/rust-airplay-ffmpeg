// SPDX-License-Identifier: GPL-3.0-or-later
//! The renderer the core hands a session.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rust_airplay::{
    AudioConfig, AudioStream, RenderError, Renderer, VideoConfig, VideoStream, Volume,
};

use crate::audio::FfmpegAudio;
use crate::options::{Options, Program};
use crate::process::{as_render_error, lock, probe};
use crate::video::{FfmpegVideo, WarmSink, WarmSlot};

/// A renderer that plays through the FFmpeg command-line tools.
///
/// One renderer serves one session's audio and video at once, which is why everything
/// that changes is behind an atomic or a lock: the core calls it from the thread that
/// received a control message while the media threads are pushing frames through the
/// streams it opened.
///
/// A machine without the programs is refused when the renderer is built rather than when
/// a frame arrives, so that a program can report what is wrong once, at startup, and
/// choose another backend.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use rust_airplay::{Config, Renderer};
///
/// # async fn start() -> Result<(), Box<dyn std::error::Error>> {
/// let renderer: Arc<dyn Renderer> =
///     Arc::new(rust_airplay_ffmpeg::FfmpegRenderer::new().expect("a machine with FFmpeg"));
/// let config = Config::new("Living room", [0x42; 32], renderer);
/// rust_airplay::run(config).await?;
/// # Ok(())
/// # }
/// ```
pub struct FfmpegRenderer {
    options: Options,
    /// The linear gain every audio stream applies, and the one a session sets.
    ///
    /// Shared rather than copied so that a stream opened before the client moved its
    /// slider and one opened after play at the same volume, and so that a change reaches
    /// the stream that is playing without a lock held across a read of PCM.
    gain: Arc<AtomicU32>,
    /// The video sink kept running between sessions, when warm video is on.
    warm: WarmSlot,
}

impl FfmpegRenderer {
    /// Builds a renderer from the environment and the programs this backend prefers.
    ///
    /// # Errors
    ///
    /// Returns [`RenderError::Unavailable`] when a program it needs is not on this
    /// machine, which is reported once, at startup.
    pub fn new() -> Result<Self, RenderError> {
        Self::with_options(Options::default())
    }

    /// Builds a renderer that decodes with these programs and plays into these sinks.
    ///
    /// # Errors
    ///
    /// The same as [`FfmpegRenderer::new`]: a program that cannot be started is a
    /// machine that cannot play, and the message names it.
    pub fn with_options(options: Options) -> Result<Self, RenderError> {
        probe(options.decoder()).map_err(as_render_error)?;
        probe(options.audio_sink().name()).map_err(as_render_error)?;
        if options.video_sink().name() != options.audio_sink().name() {
            probe(options.video_sink().name()).map_err(as_render_error)?;
        }
        log::debug!(
            "decoding with {}, playing audio through {}, video through {}",
            options.decoder(),
            options.audio_sink(),
            options.video_sink()
        );
        let renderer = Self {
            options,
            gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            warm: std::sync::Mutex::new(None),
        };
        if renderer.options.warm_video() {
            // Started now rather than at the first frame: this is what "warm" means, and
            // the second a pipeline needs to reach PLAYING is spent while no client is
            // waiting for a picture.
            let (stream, warm) = FfmpegVideo::warm(
                renderer.options.warm_codec(),
                renderer.options.video_sink(),
                renderer.options.pacing(),
            )?;
            drop(stream);
            *lock(&renderer.warm) = Some(warm);
        }
        Ok(renderer)
    }

    /// The program that decodes audio.
    #[must_use]
    pub fn decoder(&self) -> &str {
        self.options.decoder()
    }

    /// The program that plays audio.
    #[must_use]
    pub fn audio_sink(&self) -> &Program {
        self.options.audio_sink()
    }

    /// The program that displays video.
    #[must_use]
    pub fn video_sink(&self) -> &Program {
        self.options.video_sink()
    }

    /// The volume every stream plays at.
    #[must_use]
    pub fn volume(&self) -> Volume {
        Volume::new(f64::from(f32::from_bits(self.gain.load(Ordering::Relaxed))))
    }
}

impl std::fmt::Debug for FfmpegRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FfmpegRenderer")
            .field("decoder", &self.options.decoder())
            .field("audio_sink", &self.options.audio_sink())
            .field("video_sink", &self.options.video_sink())
            .field("pacing", &self.options.pacing())
            .field("volume", &self.volume())
            .finish()
    }
}

impl Renderer for FfmpegRenderer {
    fn open_audio(&self, config: AudioConfig) -> Result<Box<dyn AudioStream>, RenderError> {
        let stream = FfmpegAudio::open(
            config,
            self.options.decoder(),
            self.options.audio_sink(),
            Arc::clone(&self.gain),
        )?;
        Ok(Box::new(stream))
    }

    fn open_video(&self, config: VideoConfig) -> Result<Box<dyn VideoStream>, RenderError> {
        if !self.options.warm_video() {
            let stream =
                FfmpegVideo::open(config.codec, self.options.video_sink(), self.options.pacing())?;
            return Ok(Box::new(stream));
        }
        let slot = lock(&self.warm);
        // A sink already playing for this coding, with no session on it, is adopted as it
        // is: that is the whole point of keeping one.
        if let Some(warm) = slot.as_ref() {
            if warm.available(config.codec) && !warm.claimed.swap(true, Ordering::SeqCst) {
                let adopted = WarmSink {
                    codec: warm.codec,
                    process: Arc::clone(&warm.process),
                    claimed: Arc::clone(&warm.claimed),
                };
                drop(slot);
                return Ok(Box::new(FfmpegVideo::adopt(adopted, self.options.pacing())));
            }
        }
        drop(slot);
        let (stream, warm) =
            FfmpegVideo::warm(config.codec, self.options.video_sink(), self.options.pacing())?;
        *lock(&self.warm) = Some(warm);
        Ok(Box::new(stream))
    }

    fn set_volume(&self, volume: Volume) -> Result<(), RenderError> {
        // The stream that is playing shares this atomic, so the change reaches it without
        // a lock and without a restart; the next stream starts at the same value.
        self.gain.store((volume.level() as f32).to_bits(), Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::FfmpegRenderer;
    use crate::options::{Options, Program};
    use rust_airplay::{RenderError, Renderer, Volume};

    /// A program every machine that runs these tests has: the test binary itself.
    ///
    /// It answers `-version` with an error rather than a version, which is the point --
    /// the probe asks only whether a program can be started, not what it prints.
    fn present() -> String {
        std::env::current_exe().expect("the test binary's path").display().to_string()
    }

    #[test]
    fn a_program_that_is_not_on_this_machine_is_refused_when_the_renderer_is_built() {
        let options = Options::new(
            Program::new("no-such-player-airplay"),
            Program::new("no-such-player-airplay"),
        );
        let error = FfmpegRenderer::with_options(options).expect_err("a missing program");
        assert!(matches!(error, RenderError::Unavailable(_)), "{error}");
        assert!(error.to_string().contains("no-such-player-airplay"), "{error}");
    }

    #[test]
    fn the_volume_outlives_the_stream_it_was_set_on() {
        let program = present();
        let renderer = FfmpegRenderer::with_options(Options::new(
            Program::new(&program),
            Program::new(&program),
        ))
        .expect("a renderer");
        renderer.set_volume(Volume::new(0.25)).expect("a renderer without a stream takes a volume");
        assert_eq!(renderer.volume().level(), 0.25);
        renderer.set_volume(Volume::new(4.0)).expect("a volume is clamped");
        assert_eq!(renderer.volume().level(), 1.0);
    }

    #[test]
    fn the_renderer_says_what_it_will_run() {
        let program = present();
        let renderer = FfmpegRenderer::with_options(Options::new(
            Program::new(&program).arg("-nodisp"),
            Program::new(&program),
        ))
        .expect("a renderer");
        assert_eq!(renderer.audio_sink().name(), program);
        // Debug is what a caller logs; it names the programs and nothing that changes.
        let described = format!("{renderer:?}");
        assert!(described.contains("FfmpegRenderer"), "{described}");
        assert!(described.contains("volume"), "{described}");
    }
}
