//! Hardware encoders through GStreamer: VA-API on Intel/AMD GPUs and NVENC on NVIDIA GPUs.
//!
//! Each encoder runs as `appsrc ! videoconvert ! <encoder> [! parser] ! appsink`, with the appsink
//! caps fixing the output format. GStreamer only registers the `va` and `nvcodec` elements when a
//! usable device exists, and a candidate must also encode two probe frames as keyframes (the
//! second on request), so a missing GPU, driver, or plugin, or an encoder that holds frames back
//! or ignores keyframe requests, is ruled out before any real frame is encoded.
//!
//! Element properties differ between plugins and versions, so each is only set when the element
//! has it and accepts the value.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use bytes::Bytes;
use gst::glib;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use super::{Codec, EncodedChunk, EncoderBackend, EncoderSettings};
use crate::convert::I420;
use crate::{Error, Result};

/// How long a candidate may take to start and encode its probe frame. Hardware encoders can be
/// slow to open (NVENC loads CUDA), but this only runs when an encoder is created.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one frame may take to come out of a running encoder.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

/// Real frames are timestamped after the probe frames, which are encoded before this.
const PTS_OFFSET: Duration = Duration::from_secs(1);

/// Property values for one family of encoder elements; each is set only if the element has it.
type Properties = fn(&EncoderSettings) -> Vec<(&'static str, String)>;

/// A GStreamer encoder element that may be backed by hardware.
#[derive(Clone, Copy)]
pub(crate) struct Candidate {
	pub element: &'static str,
	pub codec: Codec,
	pub properties: Properties,
}

/// `va` plugin (GStreamer ≥ 1.22 for H.264, ≥ 1.24 for AV1). The `lp` variants use the
/// low-power fixed-function encoder of Intel GPUs.
fn va(settings: &EncoderSettings) -> Vec<(&'static str, String)> {
	vec![
		("rate-control", "cbr".into()),
		("bitrate", kbps(settings)),
		("key-int-max", settings.gop_frames().to_string()),
		("b-frames", "0".into()),
		("ref-frames", "1".into()),
	]
}

/// Legacy `vaapi` plugin (gstreamer-vaapi), which is what Ubuntu 22.04 (Humble) has.
fn vaapi(settings: &EncoderSettings) -> Vec<(&'static str, String)> {
	vec![
		("rate-control", "cbr".into()),
		("bitrate", kbps(settings)),
		("keyframe-period", settings.gop_frames().to_string()),
		("max-bframes", "0".into()),
		("refs", "1".into()),
	]
}

/// `nvcodec` plugin. Older versions name some properties differently, so both names are listed.
fn nvenc(settings: &EncoderSettings) -> Vec<(&'static str, String)> {
	vec![
		("rc-mode", "cbr".into()),
		("bitrate", kbps(settings)),
		("gop-size", settings.gop_frames().to_string()),
		("bframes", "0".into()),
		("b-frames", "0".into()),
		("rc-lookahead", "0".into()),
		("zerolatency", "true".into()),
		("zero-reorder-delay", "true".into()),
		("preset", "low-latency-hq".into()),
		("tune", "ultra-low-latency".into()),
	]
}

fn kbps(settings: &EncoderSettings) -> String {
	(settings.bitrate / 1000).max(1).to_string()
}

const fn candidate(element: &'static str, codec: Codec, properties: Properties) -> Candidate {
	Candidate {
		element,
		codec,
		properties,
	}
}

/// Hardware encoders, preferred first. Elements this GStreamer doesn't know are skipped.
const CANDIDATES: &[Candidate] = &[
	candidate("vah264lpenc", Codec::H264, va),
	candidate("vah264enc", Codec::H264, va),
	candidate("nvautogpuh264enc", Codec::H264, nvenc),
	candidate("nvh264enc", Codec::H264, nvenc),
	candidate("vaapih264enc", Codec::H264, vaapi),
	candidate("vaav1lpenc", Codec::Av1, va),
	candidate("vaav1enc", Codec::Av1, va),
	candidate("nvautogpuav1enc", Codec::Av1, nvenc),
	candidate("nvav1enc", Codec::Av1, nvenc),
	candidate("vavp8enc", Codec::Vp8, va),
	candidate("vaapivp8enc", Codec::Vp8, vaapi),
];

#[cfg(not(test))]
fn candidates() -> Vec<Candidate> {
	CANDIDATES.to_vec()
}

#[cfg(test)]
thread_local! {
	/// Replaces [`CANDIDATES`] on this thread, so tests can run software elements through the backend.
	pub(crate) static CANDIDATES_OVERRIDE: std::cell::RefCell<Option<Vec<Candidate>>> = const {
		std::cell::RefCell::new(None)
	};
}

#[cfg(test)]
fn candidates() -> Vec<Candidate> {
	CANDIDATES_OVERRIDE
		.with(|o| o.borrow().clone())
		.unwrap_or_else(|| CANDIDATES.to_vec())
}

fn init() -> Result<()> {
	static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
	INIT.get_or_init(|| gst::init().map_err(|err| err.to_string()))
		.clone()
		.map_err(|err| Error::CodecUnavailable(format!("GStreamer: {err}")))
}

/// Picture width and height.
type Dims = (usize, usize);

/// GStreamer element name.
type Element = &'static str;

/// `(element, codec, size)` combinations that failed to start or failed mid-stream. They aren't
/// tried again, so other topics and later encoders don't wait for the same probe to fail.
static FAILED: LazyLock<Mutex<HashSet<(Element, Codec, Dims)>>> = LazyLock::new(Default::default);

/// `(codec, size)` combinations hardware encoders were tried for.
static ATTEMPTED: LazyLock<Mutex<HashSet<(Codec, Dims)>>> = LazyLock::new(Default::default);

/// Whether this is the first time hardware encoders are tried for `codec` at `dims` in this process.
pub(crate) fn first_attempt(codec: Codec, dims: (usize, usize)) -> bool {
	ATTEMPTED.lock().unwrap().insert((codec, dims))
}

fn mark_failed(element: &'static str, codec: Codec, dims: (usize, usize)) {
	FAILED.lock().unwrap().insert((element, codec, dims));
}

fn has_failed(element: &'static str, codec: Codec, dims: (usize, usize)) -> bool {
	FAILED.lock().unwrap().contains(&(element, codec, dims))
}

fn gst_error(context: &str, err: impl std::fmt::Display) -> Error {
	Error::Codec(format!("GStreamer {context}: {err}"))
}

/// What the pipeline reports from its streaming threads.
enum Event {
	/// An encoded frame, with the PTS of the input it came from.
	Sample(Option<gst::ClockTime>, Bytes),
	/// The encoder dropped the input with this PTS (rate control).
	Dropped(Option<gst::ClockTime>),
	Error(String),
}

pub(crate) struct GstEncoder {
	element: &'static str,
	codec: Codec,
	dims: (usize, usize),
	pipeline: gst::Pipeline,
	src: gst_app::AppSrc,
	encoder: gst::Element,
	events: mpsc::Receiver<Event>,
	/// Set when output went missing or was discarded, so the next frame must be a keyframe.
	resync: bool,
}

impl GstEncoder {
	/// Start the first hardware encoder for `settings.codec` that works at `dims`, trying
	/// `preferred` (the element used before) first and skipping ones that already failed there.
	pub fn new(settings: &EncoderSettings, dims: (usize, usize), preferred: Option<&'static str>) -> Result<Self> {
		init()?;
		let codec = settings.codec;
		let mut candidates: Vec<Candidate> = candidates()
			.into_iter()
			.filter(|c| c.codec == codec && gst::ElementFactory::find(c.element).is_some())
			.collect();
		if candidates.is_empty() {
			return Err(Error::CodecUnavailable(format!(
				"no hardware {codec} encoder found (is a GPU with VA-API or NVENC and its GStreamer plugin installed?)"
			)));
		}
		candidates.sort_by_key(|c| Some(c.element) != preferred);

		let mut errors = Vec::new();
		for candidate in candidates {
			if has_failed(candidate.element, codec, dims) {
				errors.push(format!("{}: failed before at {}x{}", candidate.element, dims.0, dims.1));
				continue;
			}
			match Self::start(candidate, settings, dims) {
				Ok(encoder) => return Ok(encoder),
				Err(err) => {
					tracing::debug!(element = candidate.element, %err, "hardware encoder unusable");
					mark_failed(candidate.element, codec, dims);
					errors.push(format!("{}: {err}", candidate.element));
				}
			}
		}
		Err(Error::CodecUnavailable(format!(
			"no usable hardware {codec} encoder ({})",
			errors.join("; ")
		)))
	}

	pub fn element(&self) -> &'static str {
		self.element
	}

	fn start(candidate: Candidate, settings: &EncoderSettings, (width, height): (usize, usize)) -> Result<Self> {
		let make = |name: &str| {
			gst::ElementFactory::make(name)
				.build()
				.map_err(|err| gst_error(name, err))
		};

		let fps = gst::Fraction::approximate_f32(settings.max_fps.max(1.0)).unwrap_or(gst::Fraction::new(30, 1));
		let src = gst_app::AppSrc::builder()
			.caps(
				&gst_video::VideoInfo::builder(gst_video::VideoFormat::I420, width as u32, height as u32)
					.fps(fps)
					.build()
					.map_err(|err| gst_error("caps", err))?
					.to_caps()
					.map_err(|err| gst_error("caps", err))?,
			)
			.format(gst::Format::Time)
			.is_live(true)
			.build();
		let convert = make("videoconvert")?;
		let encoder = make(candidate.element)?;
		set_properties(&encoder, &(candidate.properties)(settings));

		let (parser, output) = match candidate.codec {
			// SPS/PPS before every IDR, as Annex-B access units (`avc3`).
			Codec::H264 => {
				let parser = make("h264parse")?;
				parser.set_property("config-interval", -1i32);
				let caps = gst::Caps::builder("video/x-h264")
					.field("stream-format", "byte-stream")
					.field("alignment", "au")
					.build();
				(Some(parser), caps)
			}
			// Low-overhead OBUs, one temporal unit per buffer, as WebCodecs expects.
			Codec::Av1 => {
				let caps = gst::Caps::builder("video/x-av1")
					.field("stream-format", "obu-stream")
					.field("alignment", "tu")
					.build();
				(Some(make("av1parse")?), caps)
			}
			Codec::Vp8 => (None, gst::Caps::new_empty_simple("video/x-vp8")),
		};

		// Encoded frames, errors, and dropped frames all arrive on one channel. Every bus
		// message is consumed here, so none pile up on the bus of a long-running stream.
		let (tx, events) = mpsc::channel();
		let sink = gst_app::AppSink::builder()
			.caps(&output)
			.sync(false)
			.callbacks(
				gst_app::AppSinkCallbacks::builder()
					.new_sample({
						let tx = tx.clone();
						move |sink| {
							let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
							let buffer = sample.buffer_owned().ok_or(gst::FlowError::Error)?;
							let pts = buffer.pts();
							let map = buffer
								.into_mapped_buffer_readable()
								.map_err(|_| gst::FlowError::Error)?;
							tx.send(Event::Sample(pts, Bytes::from_owner(map)))
								.map_err(|_| gst::FlowError::Flushing)?;
							Ok(gst::FlowSuccess::Ok)
						}
					})
					.build(),
			)
			.build();

		let pipeline = gst::Pipeline::new();
		let bus = pipeline.bus().expect("pipelines have a bus");
		let element = candidate.element;
		let encoder_name = encoder.name();
		bus.set_sync_handler(move |_, msg| {
			let event = match msg.view() {
				gst::MessageView::Error(err) => Some(Event::Error(format!(
					"{element}: {}: {}{}",
					err.src().map(|s| s.name().to_string()).unwrap_or_default(),
					err.error(),
					err.debug().map(|d| format!(" ({d})")).unwrap_or_default()
				))),
				// Video encoders post a QoS message for each frame they drop.
				gst::MessageView::Qos(qos) if qos.src().is_some_and(|s| s.name() == encoder_name) => {
					Some(Event::Dropped(qos.get().3))
				}
				_ => None,
			};
			if let Some(event) = event {
				let _ = tx.send(event);
			}
			gst::BusSyncReply::Drop
		});

		let mut chain: Vec<&gst::Element> = vec![src.upcast_ref(), &convert, &encoder];
		chain.extend(parser.as_ref());
		chain.push(sink.upcast_ref());
		pipeline
			.add_many(chain.iter().copied())
			.map_err(|err| gst_error("pipeline", err))?;
		gst::Element::link_many(chain.iter().copied()).map_err(|err| gst_error("link", err))?;

		let mut this = Self {
			element: candidate.element,
			codec: candidate.codec,
			dims: (width, height),
			pipeline,
			src,
			encoder,
			events,
			resync: false,
		};
		if this.pipeline.set_state(gst::State::Playing).is_err() {
			return Err(this.error_or("failed to start"));
		}

		// Encoders open their device and negotiate formats on the first frame, so black probe
		// frames show whether this one works: both must come out as keyframes, the second
		// because it was requested (each MoQ group has to start with one).
		let probe = I420::black(width, height);
		for (i, pts) in [Duration::ZERO, PTS_OFFSET / 2].into_iter().enumerate() {
			let data = this
				.encode_at(&probe, pts, i == 1, PROBE_TIMEOUT)?
				.ok_or_else(|| Error::Codec(format!("{}: dropped a probe frame", this.element)))?;
			if !this.codec.is_keyframe(&data) {
				return Err(Error::Codec(format!(
					"{}: {}",
					this.element,
					if i == 0 {
						"first frame isn't a keyframe"
					} else {
						"ignores keyframe requests"
					}
				)));
			}
		}
		Ok(this)
	}

	/// Encode `frame` at `pts` and wait up to `timeout` for its output. `Ok(None)` means the
	/// encoder dropped it.
	///
	/// Output is matched to its input by PTS. Output of an earlier frame that came too late is
	/// discarded, and the next frame is made a keyframe so decoders don't miss a reference.
	fn encode_at(
		&mut self,
		frame: &I420,
		pts: Duration,
		force_keyframe: bool,
		timeout: Duration,
	) -> Result<Option<Bytes>> {
		let pts = gst::ClockTime::from_nseconds(pts.as_nanos() as u64);
		if force_keyframe || std::mem::take(&mut self.resync) {
			let event = gst_video::UpstreamForceKeyUnitEvent::builder()
				.all_headers(true)
				.build();
			// Sent straight to the encoder, which applies it to the next frame it receives.
			let pad = self.encoder.static_pad("src").expect("encoders have a src pad");
			if !pad.send_event(event) {
				tracing::debug!(element = self.element, "encoder ignored the keyframe request");
			}
		}
		let mut buffer = gst::Buffer::from_slice(frame.data().to_vec());
		buffer.get_mut().expect("new buffers are writable").set_pts(pts);
		if self.src.push_buffer(buffer).is_err() {
			return Err(self.error_or("failed to push a frame"));
		}

		let deadline = Instant::now() + timeout;
		loop {
			let remaining = deadline.saturating_duration_since(Instant::now());
			match self.events.recv_timeout(remaining) {
				Ok(Event::Sample(Some(out), data)) if out == pts => return Ok(Some(data)),
				Ok(Event::Sample(out, _)) => {
					tracing::debug!(element = self.element, ?out, %pts, "discarding late encoder output");
					self.resync = true;
				}
				Ok(Event::Dropped(Some(out))) if out == pts => return Ok(None),
				Ok(Event::Dropped(_)) => {}
				Ok(Event::Error(err)) => return Err(Error::Codec(err)),
				Err(_) => {
					return Err(Error::Codec(format!(
						"{}: no output within {}ms",
						self.element,
						timeout.as_millis()
					)));
				}
			}
		}
	}

	/// An error the pipeline already reported, or `what` if there is none.
	fn error_or(&self, what: &str) -> Error {
		while let Ok(event) = self.events.try_recv() {
			if let Event::Error(err) = event {
				return Error::Codec(err);
			}
		}
		Error::Codec(format!("{}: {what}", self.element))
	}
}

impl EncoderBackend for GstEncoder {
	fn encode(&mut self, frame: &I420, pts: u64, force_keyframe: bool) -> Result<Option<EncodedChunk>> {
		#[cfg(test)]
		if tests::FAIL_NEXT.take() {
			mark_failed(self.element, self.codec, self.dims);
			return Err(Error::Codec(format!("{}: injected failure", self.element)));
		}
		let pts = PTS_OFFSET + Duration::from_micros(pts);
		match self.encode_at(frame, pts, force_keyframe, FRAME_TIMEOUT) {
			Ok(Some(data)) if !data.is_empty() => {
				let keyframe = self.codec.is_keyframe(&data);
				Ok(Some(EncodedChunk { data, keyframe }))
			}
			Ok(_) => Ok(None),
			Err(err) => {
				// Don't come back to an encoder that broke at this size.
				mark_failed(self.element, self.codec, self.dims);
				Err(err)
			}
		}
	}
}

impl Drop for GstEncoder {
	fn drop(&mut self) {
		let _ = self.pipeline.set_state(gst::State::Null);
	}
}

/// Set each property the element has to the value parsed for its type, skipping (with a debug
/// log) properties it lacks or values it rejects, as these differ between plugin versions.
fn set_properties(element: &gst::Element, properties: &[(&str, String)]) {
	for (name, value) in properties {
		let Some(pspec) = element.find_property(name) else {
			continue;
		};
		let writable = pspec.flags().contains(glib::ParamFlags::WRITABLE)
			&& !pspec.flags().contains(glib::ParamFlags::CONSTRUCT_ONLY);
		match glib::Value::deserialize(value, pspec.value_type()) {
			Ok(parsed) if writable && in_range(&pspec, &parsed) => element.set_property_from_value(name, &parsed),
			_ => tracing::debug!(element = %element.name(), property = name, value, "skipping property"),
		}
	}
}

/// Whether a numeric value is within the property's range (glib panics on values outside it).
/// Deserialized enums and booleans are always valid.
fn in_range(pspec: &glib::ParamSpec, value: &glib::Value) -> bool {
	macro_rules! check {
		($($spec:ty => $t:ty),*) => {
			$(if let (Some(spec), Ok(v)) = (pspec.downcast_ref::<$spec>(), value.get::<$t>()) {
				return (spec.minimum()..=spec.maximum()).contains(&v);
			})*
		};
	}
	check!(
		glib::ParamSpecUInt => u32,
		glib::ParamSpecInt => i32,
		glib::ParamSpecUInt64 => u64,
		glib::ParamSpecInt64 => i64,
		glib::ParamSpecFloat => f32,
		glib::ParamSpecDouble => f64
	);
	true
}

#[cfg(test)]
mod tests {
	use std::cell::Cell;

	use super::super::{Encoder, Hardware, tests};
	use super::*;

	thread_local! {
		/// Makes the next hardware `encode` on this thread fail, like a GPU reset would.
		pub(super) static FAIL_NEXT: Cell<bool> = const { Cell::new(false) };
	}

	fn vp8enc(settings: &EncoderSettings) -> Vec<(&'static str, String)> {
		vec![
			("target-bitrate", settings.bitrate.to_string()),
			("end-usage", "cbr".into()),
			("deadline", "1".into()),
			("lag-in-frames", "0".into()),
			("keyframe-max-dist", settings.gop_frames().to_string()),
		]
	}

	fn av1enc(settings: &EncoderSettings) -> Vec<(&'static str, String)> {
		vec![
			("target-bitrate", kbps(settings)),
			("end-usage", "cbr".into()),
			("usage-profile", "realtime".into()),
			("cpu-used", "9".into()),
			("lag-in-frames", "0".into()),
			("keyframe-max-dist", settings.gop_frames().to_string()),
		]
	}

	fn openh264enc(settings: &EncoderSettings) -> Vec<(&'static str, String)> {
		vec![
			("bitrate", settings.bitrate.to_string()),
			("gop-size", settings.gop_frames().to_string()),
			("rate-control", "bitrate".into()),
		]
	}

	/// Software GStreamer encoders standing in for hardware ones, so CI covers this backend.
	const SOFTWARE: &[Candidate] = &[
		candidate("vp8enc", Codec::Vp8, vp8enc),
		candidate("av1enc", Codec::Av1, av1enc),
		candidate("openh264enc", Codec::H264, openh264enc),
	];

	/// Route this thread's encoders through the stand-ins for `codec`, or return `false` if this
	/// build can't run them (codec not compiled in, or no GStreamer plugin installed).
	fn use_software(codec: Codec) -> bool {
		let installed: Vec<Candidate> = SOFTWARE
			.iter()
			.filter(|c| c.codec == codec && init().is_ok() && gst::ElementFactory::find(c.element).is_some())
			.copied()
			.collect();
		if !codec.is_available() || installed.is_empty() {
			eprintln!("{codec} not compiled in or no stand-in GStreamer encoder installed; skipping");
			return false;
		}
		// The checks decode H.264 with OpenH264.
		if codec == Codec::H264 && std::env::var_os("OPENH264_LIBRARY").is_none() {
			eprintln!("OPENH264_LIBRARY not set; skipping H.264 test");
			return false;
		}
		CANDIDATES_OVERRIDE.with(|o| *o.borrow_mut() = Some(installed));
		true
	}

	/// The backend encodes with `codec` on a stand-in element, and the shared codec checks pass.
	fn software_backend(codec: Codec) {
		if !use_software(codec) {
			return;
		}
		let mut encoder = Encoder::new(EncoderSettings {
			codec,
			hardware: Hardware::Require,
			..Default::default()
		})
		.unwrap();
		let frame = encoder.encode(&tests::test_pattern(64, 48, 0)).unwrap().unwrap();
		let element = encoder.hardware_encoder().expect("encoding through GStreamer");
		assert!(SOFTWARE.iter().any(|c| c.element == element), "{element}");
		assert!(frame.keyframe, "{codec}: first frame must be a keyframe");
		assert_eq!(Codec::from_catalog(&frame.codec), Some(codec));

		tests::round_trip(codec);
		tests::keyframes(codec);
		tests::pixel_formats(codec);
		tests::irregular_timestamps(codec);
		tests::forced_keyframe(codec);
		tests::rejects_invalid_images(codec);
		tests::keyframe_after_clock_jump(codec);
	}

	#[test]
	fn vp8_through_gstreamer() {
		software_backend(Codec::Vp8);
	}

	/// `av1enc` negotiates the AV1 pipeline and starts with a keyframe, but ignores keyframe
	/// requests (and on GStreamer 1.20 also holds frames back), so the probe turns it down.
	#[test]
	fn encoders_ignoring_keyframe_requests_are_rejected() {
		if !use_software(Codec::Av1) {
			return;
		}
		let mut encoder = Encoder::new(EncoderSettings {
			codec: Codec::Av1,
			hardware: Hardware::Require,
			..Default::default()
		})
		.unwrap();
		let err = encoder.encode(&tests::test_pattern(64, 48, 0)).unwrap_err();
		let message = err.to_string();
		assert!(
			message.contains("av1enc: ignores keyframe requests") || message.contains("av1enc: no output"),
			"{err}"
		);
	}

	#[test]
	fn h264_through_gstreamer() {
		software_backend(Codec::H264);
	}

	/// A hardware encoder failing mid-stream is replaced by software in auto mode, and isn't
	/// tried again at that size; in require mode the error surfaces.
	#[test]
	fn mid_stream_failures_fall_back_to_software() {
		if !use_software(Codec::Vp8) {
			return;
		}
		let settings = EncoderSettings {
			codec: Codec::Vp8,
			keyframe_interval: Duration::from_secs(60),
			..Default::default()
		};
		// A size no other test uses, since failures are remembered process-wide.
		let image = |t| tests::test_pattern(80, 64, t);
		let mut encoder = Encoder::new(settings.clone()).unwrap();
		assert!(encoder.encode(&image(0)).unwrap().unwrap().keyframe);
		assert_eq!(encoder.hardware_encoder(), Some("vp8enc"));

		FAIL_NEXT.set(true);
		let frame = encoder
			.encode(&image(1))
			.unwrap()
			.expect("software encodes the frame instead");
		assert!(frame.keyframe, "the software encoder starts with a keyframe");
		assert_eq!(encoder.hardware_encoder(), None);
		assert!(encoder.encode(&image(2)).unwrap().is_none_or(|f| !f.keyframe));

		// A new encoder at that size skips the failed element.
		let mut encoder = Encoder::new(settings.clone()).unwrap();
		encoder.encode(&image(0)).unwrap();
		assert_eq!(encoder.hardware_encoder(), None);
		let mut encoder = Encoder::new(EncoderSettings {
			hardware: Hardware::Require,
			..settings.clone()
		})
		.unwrap();
		let err = encoder.encode(&image(0)).unwrap_err();
		assert!(err.to_string().contains("vp8enc: failed before at 80x64"), "{err}");

		// Other sizes still use it, and require mode reports a mid-stream failure.
		let image = |t| tests::test_pattern(96, 64, t);
		let mut encoder = Encoder::new(EncoderSettings {
			hardware: Hardware::Require,
			..settings
		})
		.unwrap();
		encoder.encode(&image(0)).unwrap();
		assert_eq!(encoder.hardware_encoder(), Some("vp8enc"));
		FAIL_NEXT.set(true);
		assert!(encoder.encode(&image(1)).is_err());
	}

	#[test]
	fn missing_elements_fall_back_to_software() {
		let Some(&codec) = Codec::available().first() else {
			return;
		};
		if codec == Codec::H264 {
			return;
		}
		CANDIDATES_OVERRIDE.with(|o| *o.borrow_mut() = Some(vec![candidate("moqros-no-such-encoder", codec, va)]));
		let settings = EncoderSettings {
			codec,
			..Default::default()
		};
		let mut encoder = Encoder::new(settings.clone()).unwrap();
		assert!(
			encoder
				.encode(&tests::test_pattern(32, 32, 0))
				.unwrap()
				.unwrap()
				.keyframe
		);
		assert_eq!(encoder.hardware_encoder(), None);

		let mut encoder = Encoder::new(EncoderSettings {
			hardware: Hardware::Require,
			..settings
		})
		.unwrap();
		assert!(matches!(
			encoder.encode(&tests::test_pattern(32, 32, 0)),
			Err(Error::CodecUnavailable(_))
		));
	}

	/// Runs the shared checks on the real hardware encoders; needs a GPU, so it only runs with
	/// `MOQROS_HW_TEST=1`. Pictures an encoder rejects as too small fall back to software.
	#[test]
	fn hardware_encoders() {
		if std::env::var_os("MOQROS_HW_TEST").is_none() {
			eprintln!("MOQROS_HW_TEST not set; skipping hardware encoder test");
			return;
		}
		for &codec in Codec::available() {
			let mut encoder = Encoder::new(EncoderSettings {
				codec,
				hardware: Hardware::Require,
				..Default::default()
			})
			.unwrap();
			match encoder.encode(&tests::test_pattern(64, 48, 0)) {
				Ok(frame) => {
					let frame = frame.unwrap();
					eprintln!("{codec}: {} ({})", encoder.hardware_encoder().unwrap(), frame.codec);
					tests::round_trip(codec);
					tests::keyframes(codec);
					tests::pixel_formats(codec);
					tests::forced_keyframe(codec);
					tests::keyframe_after_clock_jump(codec);
				}
				Err(err) => eprintln!("{codec}: no hardware encoder: {err}"),
			}
		}
	}
}
