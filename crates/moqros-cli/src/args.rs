//! Flags shared by the bridge binaries.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use clap::builder::BoolishValueParser;
use clap::{ArgAction, Args, ValueEnum};
use moqros::ros::QosProfile;

use crate::url::{insecurity, redact_url};

/// Relay connection flags.
#[derive(Debug, Clone, Args)]
pub struct RelayArgs {
	/// Relay URL; its path scopes broadcast names.
	#[arg(long, env = "MOQROS_URL", default_value = "http://localhost:4443/anon")]
	pub url: moqros::Url,

	/// Disable TLS certificate verification (development only).
	#[arg(long, env = "MOQROS_INSECURE", action = ArgAction::Set, value_parser = BoolishValueParser::new(),
		num_args = 0..=1, require_equals = true, default_value_t = false, default_missing_value = "true")]
	pub insecure: bool,

	/// PEM client certificate chain to present to the relay (mTLS).
	#[arg(long, env = "MOQROS_TLS_CERT", requires = "tls_key")]
	pub tls_cert: Option<PathBuf>,

	/// PEM private key of `--tls-cert`.
	#[arg(long, env = "MOQROS_TLS_KEY", requires = "tls_cert")]
	pub tls_key: Option<PathBuf>,
}

impl RelayArgs {
	pub fn config(&self) -> moqros::ClientConfig {
		moqros::ClientConfig {
			url: self.url.clone(),
			insecure: self.insecure,
			tls_cert: self.tls_cert.clone(),
			tls_key: self.tls_key.clone(),
		}
	}

	/// The relay URL with its tokens and password hidden, for logging.
	pub fn redacted_url(&self) -> moqros::Url {
		redact_url(&self.url)
	}

	/// Log a warning if these settings let anyone on the network path read or alter the stream.
	pub fn warn_if_insecure(&self) {
		if let Some(reason) = insecurity(&self.url, self.insecure) {
			tracing::warn!("{reason}; anyone on the network path can read or alter the stream");
		}
	}
}

/// What `moqros-pub` sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
	/// Encode `sensor_msgs/msg/Image` or `sensor_msgs/msg/CompressedImage` messages as video.
	Video,
	/// Any message type, as one JSON document per message.
	Json,
	/// Any message type, in its ROS serialization (CDR).
	Cdr,
}

impl fmt::Display for Format {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.to_possible_value().expect("no variant is skipped").get_name())
	}
}

impl Format {
	/// The data track format, or `None` for video.
	pub fn data(self) -> Option<moqros::DataFormat> {
		match self {
			Self::Video => None,
			Self::Json => Some(moqros::DataFormat::Json),
			Self::Cdr => Some(moqros::DataFormat::Cdr),
		}
	}
}

/// ROS QoS flags.
#[derive(Debug, Clone, Args)]
pub struct QosArgs {
	/// Use reliable QoS instead of the best-effort sensor-data profile.
	#[arg(long, action = ArgAction::Set, value_parser = BoolishValueParser::new(),
		num_args = 0..=1, require_equals = true, default_value_t = false, default_missing_value = "true")]
	pub reliable: bool,
}

impl QosArgs {
	pub fn profile(&self) -> QosProfile {
		if self.reliable {
			QosProfile::default().keep_last(5)
		} else {
			QosProfile::sensor_data()
		}
	}
}

/// Video encoder flags.
#[derive(Debug, Clone, Args)]
pub struct EncoderArgs {
	/// Video codec: `vp8`, `av1` (built with the `av1` feature), or `h264` (built with the
	/// `h264` feature, with OPENH264_LIBRARY pointing at Cisco's OpenH264 binary).
	#[arg(long, default_value_t = moqros::Codec::default())]
	pub codec: moqros::Codec,

	/// Hardware video encoding (built with the `hw` feature): `auto` uses a VA-API or NVENC
	/// encoder through GStreamer when one works and falls back to software, `off` always
	/// encodes in software, and `require` fails without a hardware encoder.
	#[arg(long, default_value_t = moqros::Hardware::default())]
	pub hw: moqros::Hardware,

	/// Target video bitrate in bits per second.
	#[arg(long, default_value_t = moqros::EncoderSettings::default().bitrate)]
	pub bitrate: u32,

	/// Expected maximum input frame rate of the video.
	#[arg(long, default_value_t = moqros::EncoderSettings::default().max_fps)]
	pub fps: f32,

	/// Maximum keyframe (MoQ group) interval of the video in milliseconds.
	#[arg(long, default_value_t = default_keyframe_interval_ms())]
	pub keyframe_interval_ms: u64,

	/// A smaller rendition of each video, as `HEIGHTp@BITRATE` (e.g. `360p@500000`); repeat for
	/// several. Viewers pick the one that suits them.
	#[arg(id = "rendition", long = "rendition", value_parser = parse_rendition)]
	pub renditions: Vec<moqros::RenditionSettings>,
}

fn default_keyframe_interval_ms() -> u64 {
	u64::try_from(moqros::EncoderSettings::default().keyframe_interval.as_millis()).unwrap_or(u64::MAX)
}

/// Parse `HEIGHT[p]@BITRATE`, e.g. `360p@500000`.
fn parse_rendition(spec: &str) -> Result<moqros::RenditionSettings, String> {
	let invalid = || format!("expected HEIGHTp@BITRATE (e.g. 360p@500000), got {spec:?}");
	let (height, bitrate) = spec.split_once('@').ok_or_else(invalid)?;
	let height: u32 = height.trim().trim_end_matches('p').parse().map_err(|_| invalid())?;
	let bitrate: u32 = bitrate.trim().parse().map_err(|_| invalid())?;
	if height < 2 || bitrate == 0 {
		return Err(invalid());
	}
	Ok(moqros::RenditionSettings { height, bitrate })
}

impl EncoderArgs {
	pub fn settings(&self) -> moqros::EncoderSettings {
		moqros::EncoderSettings {
			codec: self.codec,
			bitrate: self.bitrate,
			max_fps: self.fps,
			keyframe_interval: Duration::from_millis(self.keyframe_interval_ms),
			hardware: self.hw,
			..Default::default()
		}
	}

	/// The settings of every rendition.
	pub fn broadcast_settings(&self) -> moqros::ImageBroadcastSettings {
		moqros::ImageBroadcastSettings {
			encoder: self.settings(),
			renditions: self.renditions.clone(),
		}
	}
}

/// Diagnostics flags.
#[derive(Debug, Clone, Args)]
pub struct DiagnosticsArgs {
	/// Period in milliseconds of the bridge statistics published on `/diagnostics`
	/// (`diagnostic_msgs/msg/DiagnosticArray`); 0 turns them off.
	#[arg(long, default_value_t = 1000)]
	pub diagnostics_period_ms: u64,
}

impl DiagnosticsArgs {
	pub fn period(&self) -> Option<Duration> {
		(self.diagnostics_period_ms > 0).then(|| Duration::from_millis(self.diagnostics_period_ms))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_renditions() {
		let rendition = |height, bitrate| moqros::RenditionSettings { height, bitrate };
		assert_eq!(parse_rendition("360p@500000"), Ok(rendition(360, 500_000)));
		assert_eq!(parse_rendition("720@2000000"), Ok(rendition(720, 2_000_000)));
		for bad in [
			"360p",
			"@500000",
			"360p@",
			"p@1",
			"1p@100",
			"360p@0",
			"360p@1.5M",
			"-360p@1",
		] {
			assert!(parse_rendition(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn formats_display_their_flag_values() {
		for format in Format::value_variants() {
			assert_eq!(Format::from_str(&format.to_string(), false), Ok(*format));
		}
	}

	#[test]
	fn encoder_flags_default_to_the_library_defaults() {
		#[derive(clap::Parser)]
		struct Cli {
			#[command(flatten)]
			encoder: EncoderArgs,
		}
		let settings = <Cli as clap::Parser>::parse_from(["test"]).encoder.settings();
		let defaults = moqros::EncoderSettings::default();
		assert_eq!(settings.bitrate, defaults.bitrate);
		assert_eq!(settings.max_fps, defaults.max_fps);
		assert_eq!(settings.keyframe_interval, defaults.keyframe_interval);
		assert_eq!(settings.codec, defaults.codec);
		assert_eq!(settings.hardware, defaults.hardware);
	}
}
