//! Flags shared by the bridge binaries.

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
	/// Encode `sensor_msgs/msg/Image` messages as video.
	Video,
	/// Any message type, as one JSON document per message.
	Json,
	/// Any message type, in its ROS serialization (CDR).
	Cdr,
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
	#[arg(long, default_value_t = 2_000_000)]
	pub bitrate: u32,

	/// Expected maximum input frame rate of the video.
	#[arg(long, default_value_t = 30.0)]
	pub fps: f32,

	/// Maximum keyframe (MoQ group) interval of the video in milliseconds.
	#[arg(long, default_value_t = 2000)]
	pub keyframe_interval_ms: u64,
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
}
