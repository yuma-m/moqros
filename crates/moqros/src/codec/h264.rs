//! H.264 via Cisco's prebuilt OpenH264 shared library.
//!
//! moqros never compiles OpenH264 from source: Cisco pays the MPEG LA royalties only
//! for binaries downloaded from Cisco, so the library is loaded at runtime from a path
//! given by [`set_openh264_library`] or the `OPENH264_LIBRARY` environment variable.
//! `scripts/fetch_openh264.sh` downloads the right binary for the current platform.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bytes::Bytes;
use hang::catalog::VideoCodec;
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
	BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
	UsageType,
};
use openh264::formats::YUVSource;

use super::{DecoderBackend, EncoderBackend, EncoderSettings, annexb_nals};
use crate::convert::{I420, Planes, yuv_to_rgb8};
use crate::{Error, Result};

/// Environment variable naming Cisco's OpenH264 shared library.
pub const OPENH264_LIBRARY_ENV: &str = "OPENH264_LIBRARY";

static LIBRARY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Use Cisco's OpenH264 binary at `path` for H.264. Overrides `OPENH264_LIBRARY`.
///
/// The file's SHA-256 must match a known Cisco release.
pub fn set_openh264_library(path: impl AsRef<Path>) {
	*LIBRARY.lock().unwrap() = Some(path.as_ref().to_path_buf());
}

fn load_api() -> Result<OpenH264API> {
	let path = LIBRARY
		.lock()
		.unwrap()
		.clone()
		.or_else(|| std::env::var_os(OPENH264_LIBRARY_ENV).map(PathBuf::from))
		.ok_or_else(|| {
			Error::CodecUnavailable(format!(
				"H.264 needs Cisco's OpenH264 binary: run scripts/fetch_openh264.sh and set {OPENH264_LIBRARY_ENV}"
			))
		})?;
	OpenH264API::from_blob_path(&path)
		.map_err(|err| Error::CodecUnavailable(format!("failed to load {}: {err}", path.display())))
}

fn codec_error(err: openh264::Error) -> Error {
	Error::Codec(format!("openh264: {err}"))
}

struct Source<'a>(&'a I420);

impl YUVSource for Source<'_> {
	fn dimensions(&self) -> (usize, usize) {
		(self.0.width(), self.0.height())
	}

	fn strides(&self) -> (usize, usize, usize) {
		let w = self.0.width();
		(w, w / 2, w / 2)
	}

	fn y(&self) -> &[u8] {
		self.0.y()
	}

	fn u(&self) -> &[u8] {
		self.0.u()
	}

	fn v(&self) -> &[u8] {
		self.0.v()
	}
}

pub(crate) struct H264Encoder {
	inner: Encoder,
}

impl H264Encoder {
	pub fn new(settings: &EncoderSettings) -> Result<Self> {
		let config = EncoderConfig::new()
			.usage_type(UsageType::CameraVideoRealTime)
			.profile(Profile::Baseline)
			.complexity(Complexity::Low)
			.rate_control_mode(RateControlMode::Bitrate)
			.bitrate(BitRate::from_bps(settings.bitrate))
			.max_frame_rate(FrameRate::from_hz(settings.max_fps))
			.intra_frame_period(IntraFramePeriod::from_num_frames(settings.gop_frames()))
			.skip_frames(true)
			.num_threads(settings.threads() as u16);
		let inner = Encoder::with_api_config(load_api()?, config).map_err(codec_error)?;
		Ok(Self { inner })
	}
}

impl EncoderBackend for H264Encoder {
	fn encode(&mut self, frame: &I420, _pts: u64, force_keyframe: bool) -> Result<Option<(Bytes, bool)>> {
		if force_keyframe {
			self.inner.force_intra_frame();
		}
		let bitstream = self.inner.encode(&Source(frame)).map_err(codec_error)?;
		let keyframe = match bitstream.frame_type() {
			FrameType::IDR | FrameType::I => true,
			FrameType::P | FrameType::IPMixed => false,
			FrameType::Skip | FrameType::Invalid => return Ok(None),
		};
		let data = bitstream.to_vec();
		Ok((!data.is_empty()).then(|| (data.into(), keyframe)))
	}

	/// `avc3.PPCCLL` (SPS/PPS inline), read from the keyframe's SPS.
	fn catalog_codec(&self, keyframe: &[u8]) -> VideoCodec {
		let sps = annexb_nals(keyframe).find(|nal| nal.first().map(|h| h & 0x1f) == Some(7) && nal.len() >= 4);
		let (profile, constraints, level) = sps.map_or((0x42, 0xc0, 0x1f), |sps| (sps[1], sps[2], sps[3]));
		VideoCodec::H264(hang::catalog::H264 {
			inline: true,
			profile,
			constraints,
			level,
		})
	}
}

pub(crate) struct H264Decoder {
	inner: Decoder,
}

impl H264Decoder {
	pub fn new() -> Result<Self> {
		let inner = Decoder::with_api_config(load_api()?, DecoderConfig::new()).map_err(codec_error)?;
		Ok(Self { inner })
	}
}

impl DecoderBackend for H264Decoder {
	fn decode(&mut self, data: &[u8]) -> Result<Option<(u32, u32, Vec<u8>)>> {
		let Some(yuv) = self.inner.decode(data).map_err(codec_error)? else {
			return Ok(None);
		};
		let (width, height) = yuv.dimensions();
		let (ys, us, vs) = yuv.strides();
		let planes = Planes {
			width,
			height,
			y: (yuv.y(), ys),
			u: (yuv.u(), us),
			v: (yuv.v(), vs),
		};
		Ok(Some((width as u32, height as u32, yuv_to_rgb8(&planes))))
	}
}

// SAFETY: OpenH264 encoder/decoder instances are single-threaded objects that may move
// between threads; the wrappers own them exclusively.
unsafe impl Send for H264Encoder {}
unsafe impl Send for H264Decoder {}

/// These tests need Cisco's binary: `OPENH264_LIBRARY=... cargo test --features h264`.
#[cfg(test)]
mod tests {
	use super::super::{Codec, tests};

	fn available() -> bool {
		let ok = std::env::var_os(super::OPENH264_LIBRARY_ENV).is_some();
		if !ok {
			eprintln!("OPENH264_LIBRARY not set; skipping H.264 test");
		}
		ok
	}

	#[test]
	fn round_trip() {
		if available() {
			tests::round_trip(Codec::H264);
		}
	}

	#[test]
	fn keyframes() {
		if available() {
			tests::keyframes(Codec::H264);
		}
	}

	#[test]
	fn catalog_codec_from_sps() {
		if available() {
			let mut encoder = super::super::Encoder::new(super::super::EncoderSettings {
				codec: Codec::H264,
				..Default::default()
			})
			.unwrap();
			let frame = encoder.encode(&tests::test_pattern(64, 48, 0)).unwrap().unwrap();
			assert!(frame.codec.to_string().starts_with("avc3.42"), "{}", frame.codec);
		}
	}
}
