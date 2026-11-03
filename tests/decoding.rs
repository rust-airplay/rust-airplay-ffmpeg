// SPDX-License-Identifier: GPL-3.0-or-later
//! What this backend's own programs do with a stream of coded frames.
//!
//! The video fixture is synthetic -- 640x360, 48 coded pictures -- so a test that counts
//! pictures is counting what the encoder put there, not what a capture happened to hold.
//! The audio fixture is forty AAC-ELD frames from a real session, kept because no
//! AAC-ELD encoder exists to synthesize them.
//!
//! A pipeline can accept every frame it is handed and decode none of them, so every test
//! here reads the decoded bytes back and counts them: that is the only evidence that the
//! container, the codec configuration and the program agree.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rust_airplay::{
    AudioCodec, AudioConfig, AudioFrame, Renderer, VideoCodec, VideoConfig, VideoFrame, Volume,
};
use rust_airplay_ffmpeg::{FfmpegRenderer, Options, Program};

/// The pictures of a mirror stream, as Annex-B, with its parameter sets in front.
const VIDEO: &[u8] = include_bytes!("fixtures/mirror-first-frames.h264");

/// Forty AAC-ELD payloads, each behind a four-byte little-endian length.
const ELD: &[u8] = include_bytes!("fixtures/mirror-first-audio.eld");

/// How many frames the audio fixture holds.
const ELD_FRAMES: usize = 40;

/// The samples one AAC-ELD frame carries at 44.1 kHz.
const ELD_SAMPLES: usize = 480;

/// The bytes of one decoded picture, in I420 at 640x360.
const PICTURE_BYTES: usize = 640 * 360 * 3 / 2;

/// How many pictures the video fixture holds.
const PICTURES: usize = 48;

/// The start code every unit of an Annex-B stream is behind.
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// The program to run, when this machine has it.
fn present(name: &str) -> Option<String> {
    rust_airplay_ffmpeg::testing::program_runs(name).then(|| name.to_owned())
}

/// A renderer whose sinks write to a file instead of a device.
///
/// The decoder is the same program as the sink here: `ffmpeg` reading a stream and
/// writing it somewhere is the shortest path from a frame to evidence about it.
fn recording(sink: Program) -> FfmpegRenderer {
    FfmpegRenderer::with_options(Options::new(sink.clone(), sink)).expect("a renderer")
}

/// A path nothing else in this run will use.
///
/// Under the crate's own target directory rather than the machine's temporary one: a
/// test that needs a file to exist should not depend on a directory some sandboxes deny.
fn temporary(name: &str) -> PathBuf {
    let directory = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(&directory).expect("the test directory");
    directory.join(name)
}

/// The bytes a sink wrote, with the file removed afterwards.
fn take(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let _ = fs::remove_file(path);
    bytes
}

/// Runs a program and fails the test with its output when it does.
fn run(command: &mut Command) {
    let output = command.stdin(Stdio::null()).output().expect("a program to run");
    assert!(
        output.status.success(),
        "{} failed: {}",
        command.get_program().to_string_lossy(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The units of the video fixture, each with its four-byte start code.
///
/// The backend hands over access units, and a decoder reads a byte stream: splitting
/// here at start codes is the coarsest thing that is still an Annex-B stream, and it is
/// what proves the program finds its own boundaries.
fn access_units() -> Vec<&'static [u8]> {
    let mut starts = Vec::new();
    let mut at = 0;
    while at + 4 <= VIDEO.len() {
        if VIDEO[at..at + 4] == START_CODE {
            starts.push(at);
            at += 4;
        } else {
            at += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(VIDEO.len());
        units.push(&VIDEO[*start..end]);
    }
    units
}

/// How many pictures the fixture holds: one slice is one picture.
fn pictures_in_fixture() -> usize {
    let mut pictures = 0;
    let mut at = 0;
    while at + 4 <= VIDEO.len() {
        if VIDEO[at..at + 4] == START_CODE {
            let unit = VIDEO.get(at + 4).map_or(0, |byte| byte & 0x1f);
            if unit == 1 || unit == 5 {
                pictures += 1;
            }
            at += 4;
        } else {
            at += 1;
        }
    }
    pictures
}

/// The audio fixture's frames, split on the length prefixes in front of them.
fn eld_frames() -> Vec<&'static [u8]> {
    let mut frames = Vec::with_capacity(ELD_FRAMES);
    let mut at = 0;
    while at + 4 <= ELD.len() {
        let length = u32::from_le_bytes([ELD[at], ELD[at + 1], ELD[at + 2], ELD[at + 3]]) as usize;
        at += 4;
        frames.push(&ELD[at..at + length]);
        at += length;
    }
    frames
}

#[test]
#[ignore = "needs ffmpeg on PATH; run with cargo test -- --ignored"]
fn the_video_path_decodes_every_picture() {
    let Some(ffmpeg) = present("ffmpeg") else {
        eprintln!("skipping: no ffmpeg on this machine");
        return;
    };
    let path = temporary("video.yuv");
    let sink = Program::new(ffmpeg)
        .arg("-y")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("yuv420p")
        // A recorder that resampled the timeline would drop the frames this test counts.
        // The stream is stamped with the moment each frame arrived, and a test hands them
        // over as fast as it can, so every one of them arrives in the same millisecond.
        .arg("-fps_mode")
        .arg("passthrough")
        .arg(path.display().to_string());
    let renderer = recording(sink);
    let video = renderer
        .open_video(VideoConfig { width: Some(640), height: Some(360), codec: VideoCodec::H264 })
        .expect("a video stream");
    for unit in access_units() {
        video
            .push(VideoFrame { data: unit.to_vec(), timestamp: 0, keyframe: false })
            .expect("a frame");
    }
    video.close().expect("a close");
    let decoded = take(&path);
    assert_eq!(
        pictures_in_fixture(),
        PICTURES,
        "the fixture no longer holds the pictures it was checked in with"
    );
    assert_eq!(
        decoded.len() / PICTURE_BYTES,
        PICTURES,
        "the video path decoded {} of {PICTURES} pictures from {} bytes",
        decoded.len() / PICTURE_BYTES,
        VIDEO.len()
    );
}

#[test]
#[ignore = "needs ffmpeg on PATH; run with cargo test -- --ignored"]
fn the_audio_path_decodes_every_eld_frame() {
    let Some(ffmpeg) = present("ffmpeg") else {
        eprintln!("skipping: no ffmpeg on this machine");
        return;
    };
    let path = temporary("eld.pcm");
    let sink =
        Program::new(ffmpeg).arg("-y").arg("-f").arg("s16le").arg(path.display().to_string());
    let renderer = recording(sink);
    let audio = renderer
        .open_audio(AudioConfig { codec: AudioCodec::AacEld, ..AudioConfig::default() })
        .expect("an audio stream");
    for (index, frame) in eld_frames().into_iter().enumerate() {
        audio
            .push(AudioFrame { data: frame.to_vec(), timestamp: (index * ELD_SAMPLES) as u64 })
            .expect("a frame");
    }
    audio.close().expect("a close");
    let pcm = take(&path);
    // Sixteen-bit stereo, one frame per 480 samples: a stream that lost a frame is
    // short by exactly one frame's worth, which is what a decoder fed the wrong frames
    // produces and what no count of pushes would notice.
    assert_eq!(
        pcm.len(),
        ELD_FRAMES * ELD_SAMPLES * 2 * 2,
        "the audio path decoded {} bytes",
        pcm.len()
    );
    assert!(pcm.iter().any(|byte| *byte != 0), "the audio path decoded silence");
}

#[test]
#[ignore = "needs ffmpeg on PATH; run with cargo test -- --ignored"]
fn a_volume_of_zero_is_silence_even_though_every_frame_was_decoded() {
    let Some(ffmpeg) = present("ffmpeg") else {
        eprintln!("skipping: no ffmpeg on this machine");
        return;
    };
    let path = temporary("silent.pcm");
    let sink =
        Program::new(&ffmpeg).arg("-y").arg("-f").arg("s16le").arg(path.display().to_string());
    let renderer = recording(sink);
    renderer.set_volume(Volume::new(0.0)).expect("a volume");
    let audio = renderer
        .open_audio(AudioConfig { codec: AudioCodec::AacEld, ..AudioConfig::default() })
        .expect("an audio stream");
    for (index, frame) in eld_frames().into_iter().enumerate() {
        audio
            .push(AudioFrame { data: frame.to_vec(), timestamp: (index * ELD_SAMPLES) as u64 })
            .expect("a frame");
    }
    audio.close().expect("a close");
    let pcm = take(&path);
    assert_eq!(pcm.len(), ELD_FRAMES * ELD_SAMPLES * 2 * 2, "still decoded");
    assert!(pcm.iter().all(|byte| *byte == 0), "a stream opened at volume zero played something");
}

#[test]
#[ignore = "needs ffmpeg on PATH; run with cargo test -- --ignored"]
fn a_flush_before_the_first_frame_leaves_a_stream_that_still_decodes() {
    let Some(ffmpeg) = present("ffmpeg") else {
        eprintln!("skipping: no ffmpeg on this machine");
        return;
    };
    let path = temporary("flushed.pcm");
    let sink =
        Program::new(&ffmpeg).arg("-y").arg("-f").arg("s16le").arg(path.display().to_string());
    let renderer = recording(sink);
    let audio = renderer
        .open_audio(AudioConfig { codec: AudioCodec::AacEld, ..AudioConfig::default() })
        .expect("an audio stream");
    // A flush replaces the decoder, so the stream it leaves behind has to be declared
    // again: a decoder that starts with a block and no header decodes nothing, and this
    // is the test that says whether the declaration was repeated.
    audio.flush().expect("a flush");
    for (index, frame) in eld_frames().into_iter().enumerate() {
        audio
            .push(AudioFrame { data: frame.to_vec(), timestamp: (index * ELD_SAMPLES) as u64 })
            .expect("a frame");
    }
    audio.close().expect("a close");
    let pcm = take(&path);
    assert_eq!(
        pcm.len(),
        ELD_FRAMES * ELD_SAMPLES * 2 * 2,
        "the stream after the flush decoded {} bytes",
        pcm.len()
    );
}

#[test]
#[ignore = "needs ffmpeg on PATH; run with cargo test -- --ignored"]
fn a_flush_mid_stream_replaces_both_processes_and_decodes_what_follows() {
    let Some(ffmpeg) = present("ffmpeg") else {
        eprintln!("skipping: no ffmpeg on this machine");
        return;
    };
    let path = temporary("flushed-mid.pcm");
    // The sink truncates the file when it starts, so what is read back is what the
    // player started after the flush wrote: that is the thing this test is about.
    let sink =
        Program::new(&ffmpeg).arg("-y").arg("-f").arg("s16le").arg(path.display().to_string());
    let renderer = recording(sink);
    let audio = renderer
        .open_audio(AudioConfig { codec: AudioCodec::AacEld, ..AudioConfig::default() })
        .expect("an audio stream");
    let frames = eld_frames();
    for (index, frame) in frames.iter().take(20).enumerate() {
        audio
            .push(AudioFrame { data: frame.to_vec(), timestamp: (index * ELD_SAMPLES) as u64 })
            .expect("a frame");
    }
    // Let the first half reach the decoder, so the flush has something to discard.
    std::thread::sleep(std::time::Duration::from_millis(300));
    audio.flush().expect("a flush");
    for (index, frame) in frames.iter().enumerate() {
        audio
            .push(AudioFrame { data: frame.to_vec(), timestamp: (index * ELD_SAMPLES) as u64 })
            .expect("a frame");
    }
    audio.close().expect("a close");
    let pcm = take(&path);
    assert_eq!(
        pcm.len(),
        ELD_FRAMES * ELD_SAMPLES * 2 * 2,
        "the stream after a mid-stream flush decoded {} bytes",
        pcm.len()
    );
}

#[test]
#[ignore = "needs ffmpeg on PATH; run with cargo test -- --ignored"]
fn an_alac_stream_decodes_to_what_ffmpeg_makes_of_the_file_it_came_from() {
    let (Some(ffmpeg), Some(ffprobe)) = (present("ffmpeg"), present("ffprobe")) else {
        eprintln!("skipping: no ffmpeg and ffprobe on this machine");
        return;
    };
    let source = temporary("alac.m4a");
    let reference = temporary("alac-reference.pcm");
    let path = temporary("alac.pcm");
    // A second of a tone, encoded losslessly and then decoded the way a file would be:
    // that is the answer the backend's own container has to reproduce.
    run(Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1:sample_rate=44100",
            "-ac",
            "2",
            "-c:a",
            "alac",
        ])
        .arg(&source));
    run(Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&source)
        .args(["-f", "s16le"])
        .arg(&reference));

    let packets = alac_packets(&ffprobe, &source);
    let sink =
        Program::new(&ffmpeg).arg("-y").arg("-f").arg("s16le").arg(path.display().to_string());
    let renderer = recording(sink);
    let audio = renderer
        .open_audio(AudioConfig {
            codec: AudioCodec::Alac,
            // FFmpeg's ALAC encoder writes 4096-sample frames, and the cookie this
            // backend builds has to say what the frames actually carry.
            samples_per_frame: 4096,
            ..AudioConfig::default()
        })
        .expect("an audio stream");
    for (index, packet) in packets.iter().enumerate() {
        audio
            .push(AudioFrame { data: packet.clone(), timestamp: (index * 4096) as u64 })
            .expect("a frame");
    }
    audio.close().expect("a close");
    let decoded = take(&path);
    let expected = take(&reference);
    assert!(!expected.is_empty(), "the reference decoded to nothing");
    assert_eq!(
        decoded,
        expected,
        "the backend's container decoded {} bytes, the file {}",
        decoded.len(),
        expected.len()
    );
    let _ = fs::remove_file(&source);
}

/// The coded packets of an ALAC file, in order.
///
/// `ffprobe` reports each packet's size and its offset in the file, and the file is read
/// back at those offsets: the alternative is to parse the sample tables of an MP4, which
/// is a container this test is not about. The two columns are printed size first, which
/// is `ffprobe`'s own order rather than the order they were asked for.
fn alac_packets(ffprobe_program: &str, path: &Path) -> Vec<Vec<u8>> {
    let output = Command::new(ffprobe_program)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pos,size",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .expect("ffprobe to run");
    assert!(output.status.success(), "ffprobe failed: {}", String::from_utf8_lossy(&output.stderr));
    let file = fs::read(path).expect("the encoded file");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.trim().split(',');
            let size: usize = fields.next().expect("a size").parse().expect("a size");
            let position: usize = fields.next().expect("a position").parse().expect("a position");
            file[position..position + size].to_vec()
        })
        .collect()
}

#[test]
fn a_configuration_ffmpeg_cannot_be_told_about_is_refused_before_a_program_starts() {
    // No program is run: the configuration is checked first, which is what makes this
    // test run on a machine with no FFmpeg at all.
    let program = std::env::current_exe().expect("the test binary's path").display().to_string();
    let renderer = recording(Program::new(program));
    let error = renderer
        .open_audio(AudioConfig {
            codec: AudioCodec::AacEld,
            samples_per_frame: 1024,
            ..AudioConfig::default()
        })
        .expect_err("a frame length the coding cannot state");
    assert!(matches!(error, rust_airplay::RenderError::Unavailable(_)), "{error}");
}
