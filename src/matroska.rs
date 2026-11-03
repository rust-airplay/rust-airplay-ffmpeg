// SPDX-License-Identifier: GPL-3.0-or-later
//! Writing a live Matroska stream around coded audio frames.
//!
//! A raw AAC-ELD or ALAC frame has no sync word and no length field: the only boundary
//! between two frames is the buffer the session handed over. `ffmpeg` reads its input as
//! a container, not as a run of buffers, so a stream of those frames has to be given one.
//! Matroska is the container that can be written forwards: the header declares the track
//! and the codec configuration the frames do not carry, and one `SimpleBlock` per frame
//! carries its own length. The segment and its clusters are written with the unknown
//! length encoding, so nothing has to be patched once the end is known and a reader can
//! decode the first block instead of waiting for a complete file.
//!
//! The alternative -- seeking back to fill in a length -- is what a muxer writing to a
//! file does, and a session's audio is not a file: it ends when the client stops.

/// The EBML header that starts every Matroska stream, and names the doctype.
const EBML_HEADER: u64 = 0x1A45_DFA3;
const EBML_VERSION: u64 = 0x4286;
const EBML_READ_VERSION: u64 = 0x42F7;
const EBML_MAX_ID_LENGTH: u64 = 0x42F2;
const EBML_MAX_SIZE_LENGTH: u64 = 0x42F3;
const DOC_TYPE: u64 = 0x4282;
const DOC_TYPE_VERSION: u64 = 0x4287;
const DOC_TYPE_READ_VERSION: u64 = 0x4285;

/// The segment, and the elements a live stream needs inside it.
const SEGMENT: u64 = 0x1853_8067;
const INFO: u64 = 0x1549_A966;
const TIMESTAMP_SCALE: u64 = 0x002A_D7B1;
const MUXING_APP: u64 = 0x4D80;
const WRITING_APP: u64 = 0x5741;
const TRACKS: u64 = 0x1654_AE6B;
const TRACK_ENTRY: u64 = 0xAE;
const TRACK_NUMBER: u64 = 0xD7;
const TRACK_UID: u64 = 0x73C5;
const TRACK_TYPE: u64 = 0x83;
const CODEC_ID: u64 = 0x86;
const CODEC_PRIVATE: u64 = 0x63A2;
const AUDIO: u64 = 0xE1;
const SAMPLING_FREQUENCY: u64 = 0xB5;
const CHANNELS: u64 = 0x9F;
const CLUSTER: u64 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u64 = 0xE7;
const SIMPLE_BLOCK: u64 = 0xA3;

/// The track number this stream writes, as the vint a block starts with.
const TRACK_ONE: u8 = 0x81;

/// The unknown length encoding: a length with every value bit set.
const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// How long one cluster may last before a new one starts.
///
/// A block's timestamp is a signed 16-bit millisecond offset from its cluster, so the
/// cluster has to end before that offset overflows: 32.7 seconds is the format's limit
/// and a minute's worth of audio written into one cluster would wrap the timestamps of
/// everything after the first half. Thirty seconds leaves room for a block that strays.
const MAX_CLUSTER_MS: u64 = 30_000;

/// The track a stream declares: the coding, the configuration its frames do not carry,
/// and what a reader needs to turn a block into samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Track {
    /// The Matroska CodecID: `A_AAC` or `A_ALAC`.
    pub(crate) codec_id: &'static str,
    /// CodecPrivate: the `AudioSpecificConfig` or the ALAC magic cookie **without** the
    /// twelve bytes a Matroska reader adds back around it.
    pub(crate) codec_private: Vec<u8>,
    /// Samples a second.
    pub(crate) sample_rate: u32,
    /// Channels per frame.
    pub(crate) channels: u8,
}

/// A Matroska stream written one block at a time.
#[derive(Debug)]
pub(crate) struct Muxer {
    track: Track,
    /// The timestamp of the cluster that is open, if one is.
    open_cluster_ms: Option<u64>,
}

impl Muxer {
    /// A muxer whose next write is the start of a stream.
    pub(crate) const fn new(track: Track) -> Self {
        Self { track, open_cluster_ms: None }
    }

    /// The bytes that turn an empty file into a stream this track can be read from.
    ///
    /// Written once per stream, before any block, and again by a flush: a flush replaces
    /// the decoder, and a decoder that starts with a block and no header has nothing to
    /// decode with.
    pub(crate) fn header(&self) -> Vec<u8> {
        let mut ebml = Vec::new();
        ebml.extend(element(EBML_VERSION, &uint(1)));
        ebml.extend(element(EBML_READ_VERSION, &uint(1)));
        ebml.extend(element(EBML_MAX_ID_LENGTH, &uint(4)));
        ebml.extend(element(EBML_MAX_SIZE_LENGTH, &uint(8)));
        ebml.extend(element(DOC_TYPE, b"matroska"));
        ebml.extend(element(DOC_TYPE_VERSION, &uint(4)));
        ebml.extend(element(DOC_TYPE_READ_VERSION, &uint(2)));

        let mut info = Vec::new();
        // One millisecond per tick, which is what a block's own offset is counted in.
        info.extend(element(TIMESTAMP_SCALE, &uint(1_000_000)));
        info.extend(element(MUXING_APP, b"rust-airplay-ffmpeg"));
        info.extend(element(WRITING_APP, b"rust-airplay-ffmpeg"));

        let mut audio = Vec::new();
        audio.extend(element(SAMPLING_FREQUENCY, &float(f64::from(self.track.sample_rate))));
        audio.extend(element(CHANNELS, &uint(u64::from(self.track.channels))));

        let mut entry = Vec::new();
        entry.extend(element(TRACK_NUMBER, &uint(1)));
        entry.extend(element(TRACK_UID, &uint(1)));
        // 2 is an audio track; a reader that finds no type skips the entry.
        entry.extend(element(TRACK_TYPE, &uint(2)));
        entry.extend(element(CODEC_ID, self.track.codec_id.as_bytes()));
        entry.extend(element(CODEC_PRIVATE, &self.track.codec_private));
        entry.extend(element(AUDIO, &audio));

        let mut out = Vec::new();
        out.extend(element(EBML_HEADER, &ebml));
        // The segment is entered but never left: its length is unknown until the session
        // ends, and a reader told "unknown" reads until the stream closes.
        out.extend(id_bytes(SEGMENT));
        out.extend_from_slice(&UNKNOWN_SIZE);
        out.extend(element(INFO, &info));
        let tracks = element(TRACK_ENTRY, &entry);
        out.extend(element(TRACKS, &tracks));
        out
    }

    /// The bytes for one coded frame at `timestamp_ms`.
    ///
    /// The timestamp counts milliseconds from the start of the session and only orders
    /// the blocks: `ffmpeg` is asked for raw PCM, which carries the samples in the order
    /// they were written.
    ///
    /// A cluster is started by the first block and replaced when the offset it would need
    /// no longer fits: a client that has been playing for a while keeps writing into the
    /// same segment, and that is what the unknown cluster length is for.
    pub(crate) fn block(&mut self, timestamp_ms: u64, frame: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(frame.len() + 16);
        let fresh = match self.open_cluster_ms {
            Some(start) => timestamp_ms < start || timestamp_ms - start > MAX_CLUSTER_MS,
            None => true,
        };
        if fresh {
            self.open_cluster_ms = Some(timestamp_ms);
            out.extend(id_bytes(CLUSTER));
            out.extend_from_slice(&UNKNOWN_SIZE);
            out.extend(element(CLUSTER_TIMESTAMP, &uint(timestamp_ms)));
        }
        // A block's offset is from the cluster's own timestamp, and the check above is
        // what keeps it inside the 16 bits the field has.
        let start = self.open_cluster_ms.unwrap_or(timestamp_ms);
        let offset = i16::try_from(timestamp_ms.saturating_sub(start)).unwrap_or(0);
        let mut block = Vec::with_capacity(frame.len() + 4);
        block.push(TRACK_ONE);
        block.extend_from_slice(&offset.to_be_bytes());
        // No lacing and no discardable frames: every block is one complete frame.
        block.push(0x80);
        block.extend_from_slice(frame);
        out.extend(element(SIMPLE_BLOCK, &block));
        out
    }
}

/// The bytes of an element identifier, which is written as it is read.
fn id_bytes(id: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(4);
    for shift in [24u32, 16, 8, 0] {
        let byte = ((id >> shift) & 0xFF) as u8;
        if !out.is_empty() || byte != 0 {
            out.push(byte);
        }
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

/// The length of a payload, in the variable-length form every element uses.
fn size_bytes(value: usize) -> Vec<u8> {
    let value = value as u64;
    // The all-ones pattern is reserved for "unknown", so the largest value a given width
    // can hold is one less than the number of bit patterns it has.
    let mut width = 1usize;
    while width < 8 && value > (1u64 << (7 * width)) - 2 {
        width += 1;
    }
    let marked = value | (1u64 << (7 * width));
    let bytes = marked.to_be_bytes();
    bytes[8 - width..].to_vec()
}

/// An element: its identifier, its payload's length, and the payload.
fn element(id: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend(id_bytes(id));
    out.extend(size_bytes(payload.len()));
    out.extend_from_slice(payload);
    out
}

/// An unsigned integer payload, in the fewest bytes that hold it.
fn uint(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    bytes[first..].to_vec()
}

/// A floating point payload: a sample rate is a float in Matroska, not an integer.
fn float(value: f64) -> Vec<u8> {
    value.to_bits().to_be_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> Track {
        Track {
            codec_id: "A_AAC",
            codec_private: vec![0xF8, 0xE8, 0x50],
            sample_rate: 44_100,
            channels: 2,
        }
    }

    #[test]
    fn a_length_is_written_in_the_fewest_bytes_that_hold_it() {
        assert_eq!(size_bytes(0), vec![0x80]);
        assert_eq!(size_bytes(1), vec![0x81]);
        assert_eq!(size_bytes(126), vec![0xFE]);
        // 127 cannot be one byte: the all-ones pattern means unknown.
        assert_eq!(size_bytes(127), vec![0x40, 0x7F]);
        assert_eq!(size_bytes(16_384), vec![0x20, 0x40, 0x00]);
    }

    #[test]
    fn an_identifier_keeps_the_bytes_it_was_given() {
        assert_eq!(id_bytes(0xAE), vec![0xAE]);
        assert_eq!(id_bytes(0x1A45_DFA3), vec![0x1A, 0x45, 0xDF, 0xA3]);
        assert_eq!(id_bytes(0x4286), vec![0x42, 0x86]);
    }

    #[test]
    fn the_header_says_matroska_and_declares_one_audio_track() {
        let header = Muxer::new(track()).header();
        assert_eq!(&header[..4], &[0x1A, 0x45, 0xDF, 0xA3], "an EBML header");
        let text = String::from_utf8_lossy(&header);
        assert!(text.contains("matroska"), "the doctype");
        assert!(text.contains("A_AAC"), "the codec id");
        assert!(!text.contains("A_ALAC"), "one codec only");
        // The segment is opened with the unknown length, which is what lets a live stream
        // be written without ever going back.
        let at = header
            .windows(4)
            .position(|window| window == [0x18, 0x53, 0x80, 0x67])
            .expect("a segment");
        assert_eq!(&header[at + 4..at + 12], &UNKNOWN_SIZE);
    }

    #[test]
    fn a_block_carries_its_length_its_offset_and_a_copy_of_the_frame() {
        let mut muxer = Muxer::new(track());
        // A cluster is opened by the first block, whose offset from it is therefore zero.
        let first = muxer.block(0, &[1, 2, 3]);
        assert_eq!(&first[..4], &[0x1F, 0x43, 0xB6, 0x75], "a cluster");
        // A block: an identifier, a seven-byte payload (track, offset, flags, frame), and
        // the frame itself. Forty milliseconds into the cluster, the offset says so.
        let second = muxer.block(40, &[1, 2, 3]);
        assert_eq!(
            second,
            [0xA3, 0x87, 0x81, 0x00, 0x28, 0x80, 1, 2, 3],
            "the block of the second frame"
        );
    }

    #[test]
    fn a_cluster_is_replaced_before_its_offsets_overflow() {
        let mut muxer = Muxer::new(track());
        let first = muxer.block(0, &[1]);
        assert_eq!(&first[..4], &[0x1F, 0x43, 0xB6, 0x75]);
        let inside = muxer.block(MAX_CLUSTER_MS, &[1]);
        assert_ne!(
            &inside[..4],
            &[0x1F, 0x43, 0xB6, 0x75],
            "a block inside the cluster does not open a new one"
        );
        let after = muxer.block(MAX_CLUSTER_MS + 1, &[1]);
        assert_eq!(&after[..4], &[0x1F, 0x43, 0xB6, 0x75]);
    }
}
