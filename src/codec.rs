// SPDX-License-Identifier: GPL-3.0-or-later
//! The configuration a coded audio frame does not carry.
//!
//! An AirPlay audio frame is a payload and nothing else: no sample rate, no channel
//! count, and nothing that says which coding it is. A decoder needs all three before it
//! can read the first frame, and FFmpeg is told them the way a container tells a decoder
//! -- a codec identifier and a block of private data. This module builds that block.
//!
//! Two of the values are AirPlay's and are not negotiable: an AAC-ELD stream here is
//! 44.1 kHz, stereo, 480 samples a frame, and an ALAC stream is 16 bits and 352 samples a
//! frame. The rest is taken from [`AudioConfig`], which is what the client declared.

use rust_airplay::{AudioCodec, AudioConfig, RenderError};

use crate::matroska::Track;

/// The sample rates an `AudioSpecificConfig` can name by index.
///
/// A rate outside this table is written explicitly in the configuration, which this
/// backend does not do: an AirPlay client declares one of these.
const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The object type of AAC Enhanced Low Delay, in the five bits the field has room for.
///
/// The field is five bits wide and values above 31 are written as 31 followed by six
/// more bits: `32 + 7` is 39, which is ELD.
const ESCAPE: u32 = 31;
const ELD_OFFSET: u32 = 7;

/// The AAC channel configurations, which count channels one-based and only up to seven.
const MAX_AAC_CHANNELS: u8 = 7;

/// The channel counts a PCM stream has a layout name for.
const MAX_CHANNELS: u8 = 8;

/// The track a stream of these frames is declared with.
///
/// # Errors
///
/// Returns [`RenderError::Unavailable`] for a coding this backend has no FFmpeg decoder
/// for, and for a configuration the coding cannot express: a frame length an ELD
/// configuration has no field for, or a sample rate outside the table.
pub(crate) fn track(config: AudioConfig) -> Result<Track, RenderError> {
    // The decoder is told a count and the sink is told a layout, and there are eight
    // layouts a PCM stream can be named by. A count outside them would have to be guessed
    // at one end or the other, which is how a surround stream is played as stereo.
    if config.channels == 0 || config.channels > MAX_CHANNELS {
        return Err(RenderError::Unavailable(format!(
            "a PCM layout has no name for {} channels",
            config.channels
        )));
    }
    let (codec_id, codec_private) = match config.codec {
        AudioCodec::AacEld => ("A_AAC", eld_config(config)?),
        AudioCodec::Alac => ("A_ALAC", alac_config(config)),
        other => {
            return Err(RenderError::Unavailable(format!(
                "this backend does not decode {}",
                other.name()
            )));
        }
    };
    Ok(Track {
        codec_id,
        codec_private,
        sample_rate: config.sample_rate,
        channels: config.channels,
    })
}

/// The `AudioSpecificConfig` for this ELD stream.
///
/// The layout is the one the coding defines: the object type, the sample rate index and
/// the channel configuration, and then the ELD-specific part -- a flag for a frame
/// length of 480 rather than 512, three resilience flags, and a flag for low delay SBR,
/// which AirPlay does not use. Twenty-four bits, three bytes.
fn eld_config(config: AudioConfig) -> Result<Vec<u8>, RenderError> {
    let index =
        SAMPLE_RATES.iter().position(|rate| *rate == config.sample_rate).ok_or_else(|| {
            RenderError::Unavailable(format!(
                "{} Hz cannot be named by an AAC-ELD configuration",
                config.sample_rate
            ))
        })?;
    if config.channels == 0 || config.channels > MAX_AAC_CHANNELS {
        return Err(RenderError::Unavailable(format!(
            "an AAC-ELD configuration has no field for {} channels",
            config.channels
        )));
    }
    let long_frame = match config.samples_per_frame {
        // The flag says which of the two frame lengths the stream uses, and an ELD
        // stream has only those two.
        480 => true,
        512 => false,
        other => {
            return Err(RenderError::Unavailable(format!(
                "an AAC-ELD frame is 480 or 512 samples, not {other}"
            )));
        }
    };
    let mut bits = Bits::new();
    bits.put(ESCAPE, 5);
    bits.put(ELD_OFFSET, 6);
    bits.put(index as u32, 4);
    bits.put(u32::from(config.channels), 4);
    bits.put(u32::from(long_frame), 1);
    bits.put(0, 3);
    bits.put(0, 1);
    Ok(bits.finish())
}

/// The ALAC magic cookie, less the twelve bytes a Matroska reader adds back.
///
/// An ALAC decoder reads its elementary stream parameters from this block, and it is the
/// one place they are stated: the frame length (which is the client's `spf`), the bit
/// depth, the Rice parameters, the channel count and the sample rate. The values that
/// are usually zero -- the maximum coded frame size and the average bit rate -- are zero
/// because nothing this backend does with them: it hands the decoder frames it did not
/// size.
fn alac_config(config: AudioConfig) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&u32::from(config.samples_per_frame).to_be_bytes());
    // compatibleVersion
    out.push(0);
    // bitDepth: AirPlay's ALAC is 16-bit.
    out.push(16);
    // pb, mb, kb -- the Rice parameters Apple's encoder uses.
    out.extend_from_slice(&[40, 10, 14]);
    out.push(config.channels);
    // maxRun
    out.extend_from_slice(&255u16.to_be_bytes());
    // maxFrameBytes and avgBitRate: not consulted by a decoder.
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&config.sample_rate.to_be_bytes());
    out
}

/// A bit writer, most significant bit first, because that is how a configuration is
/// read.
struct Bits {
    bytes: Vec<u8>,
    used: u32,
}

impl Bits {
    const fn new() -> Self {
        Self { bytes: Vec::new(), used: 0 }
    }

    /// Appends `count` bits of `value`, most significant first.
    fn put(&mut self, value: u32, count: u32) {
        for bit in (0..count).rev() {
            if self.used % 8 == 0 {
                self.bytes.push(0);
            }
            let set = (value >> bit) & 1;
            if set == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - (self.used % 8));
            }
            self.used += 1;
        }
    }

    /// The bytes written so far, padded to a byte boundary.
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{alac_config, track};
    use rust_airplay::{AudioCodec, AudioConfig, RenderError};

    #[test]
    fn an_eld_configuration_is_the_four_bytes_airplay_declares() {
        let config = AudioConfig { codec: AudioCodec::AacEld, ..AudioConfig::default() };
        let track = track(config).expect("an ELD track");
        assert_eq!(track.codec_id, "A_AAC");
        // 39 (ELD), 44100, two channels, a 480-sample frame, no resilience tools and no
        // low delay SBR. This is the value a session's own configuration says.
        assert_eq!(track.codec_private, vec![0xF8, 0xE8, 0x50]);
    }

    #[test]
    fn an_eld_frame_length_that_is_neither_480_nor_512_is_refused() {
        let config = AudioConfig {
            codec: AudioCodec::AacEld,
            samples_per_frame: 1024,
            ..AudioConfig::default()
        };
        let error = track(config).expect_err("a frame the configuration cannot state");
        assert!(matches!(error, RenderError::Unavailable(_)), "{error}");
    }

    #[test]
    fn a_channel_count_no_pcm_layout_can_name_is_refused_before_a_program_starts() {
        // ALAC's cookie carries any channel count, so nothing upstream stops a session
        // from declaring nine channels; the sink would then be handed a layout that is not
        // the stream. Refusing here is what keeps the two ends agreeing.
        let config = AudioConfig { codec: AudioCodec::Alac, channels: 9, ..AudioConfig::default() };
        let error = track(config).expect_err("a layout no PCM stream can be named by");
        assert!(matches!(error, RenderError::Unavailable(_)), "{error}");
        assert!(error.to_string().contains('9'), "{error}");
    }

    #[test]
    fn an_alac_cookie_states_the_frame_length_the_client_declared() {
        // A client's spf is 352 for ALAC, and the decoder's buffer is sized from it.
        let config = AudioConfig {
            codec: AudioCodec::Alac,
            samples_per_frame: 352,
            ..AudioConfig::default()
        };
        let track = track(config).expect("an ALAC track");
        assert_eq!(track.codec_id, "A_ALAC");
        let cookie = alac_config(config);
        assert_eq!(cookie.len(), 24);
        assert_eq!(&cookie[..4], &352u32.to_be_bytes(), "the frame length");
        assert_eq!(cookie[4], 0, "the compatible version");
        assert_eq!(cookie[5], 16, "the bit depth");
        assert_eq!(cookie[6..9], [40, 10, 14], "the Rice parameters");
        assert_eq!(cookie[9], 2, "the channel count");
        assert_eq!(&cookie[10..12], &[0, 255], "the maximum run");
        assert_eq!(&cookie[20..24], &44_100u32.to_be_bytes(), "the rate");
    }

    #[test]
    fn a_coding_with_no_decoder_is_refused() {
        // The enum is non-exhaustive, so a backend that matches on it has to say what it
        // does with a coding it does not know rather than guess a decoder.
        let unknown = match AudioCodec::from_compression_type(255) {
            Some(codec) => codec,
            None => return,
        };
        let config = AudioConfig { codec: unknown, ..AudioConfig::default() };
        assert!(track(config).is_err());
    }
}
