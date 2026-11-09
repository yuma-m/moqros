//! The catalog (`catalog.json`) moqros writes and reads.
//!
//! This is the wire format other hang clients have to agree on, notably the web viewer
//! (`examples/web/src/moq/classify.ts` decides video vs. data the same way [`Kind::of`] does).
//!
//! - A video broadcast lists its full-size rendition, [`VIDEO_TRACK`], and any smaller ones
//!   (`video_<height>p`), in the hang "legacy" container.
//! - A data broadcast lists one track, [`DATA_TRACK`], in the `json` section ([`DataFormat::Json`])
//!   or the `binary` section with MIME type [`CDR_MIME`] ([`DataFormat::Cdr`]), plus its
//!   [`DataMode`] and ROS message type (`rosType`).
//! - A root `moqros` section ([`MoqrosSection`]) carries what hang's sections have no field for:
//!   the capture time of timestamp 0 (`captureOrigin`) and the ROS `frameId`. When the capture
//!   times are wall-clock times since 2020, the capture origin also goes into hang's `clock`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hang::catalog::{
	BinaryConfig, Catalog, Clock, Container, JsonConfig, MOQ_EPOCH_UNIX_MILLIS, Mode, VideoCodec, VideoConfig,
};
use hang::moq_net;
use serde_json::{Map, Value};

use crate::codec::{Codec, EncodedFrame, EncoderSettings};
use crate::{DataFormat, DataMode, DataSettings, Error, Result};

/// Name of the single video track inside every moqros broadcast.
pub const VIDEO_TRACK: &str = "video";

/// Name of the single data track inside every moqros data broadcast.
pub const DATA_TRACK: &str = "data";

/// Media type of [`DataFormat::Cdr`] payloads in the catalog's `binary` section.
pub const CDR_MIME: &str = "application/x-ros2-cdr";

/// Catalog field naming the ROS message type of a data track.
const ROS_TYPE_KEY: &str = "rosType";

/// The catalog moqros reads and writes: hang's, plus the other root sections, among them
/// [`MoqrosSection`].
pub(crate) type MoqrosCatalog = Catalog<Map<String, Value>>;

/// Name of the root section [`MoqrosSection`] lives in.
const SECTION_KEY: &str = "moqros";

/// Longest `frameId` accepted from a catalog.
const MAX_FRAME_ID_LEN: usize = 256;

/// The catalog's `moqros` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MoqrosSection {
	/// Capture time of media timestamp 0 in µs, in whatever clock the publisher stamps with
	/// (see [`StreamClock::origin_micros`](crate::clock::StreamClock::origin_micros)).
	pub(crate) capture_origin: Option<i64>,
	/// ROS `frame_id` of the images.
	pub(crate) frame_id: Option<String>,
}

impl MoqrosSection {
	/// Read the section of `catalog`, leniently: whoever announces the broadcast writes it, so
	/// a malformed field counts as missing rather than failing the catalog.
	pub(crate) fn read(catalog: &MoqrosCatalog) -> Self {
		let Some(section) = catalog.ext.get(SECTION_KEY).and_then(Value::as_object) else {
			return Self::default();
		};
		Self {
			capture_origin: section.get("captureOrigin").and_then(Value::as_i64),
			frame_id: section
				.get("frameId")
				.and_then(Value::as_str)
				.filter(|id| is_frame_id(id))
				.map(String::from),
		}
	}

	/// Write the section into `catalog`, and the capture origin into hang's `clock` when it is a
	/// wall-clock time hang can represent.
	pub(crate) fn write(&self, catalog: &mut MoqrosCatalog) {
		let mut section = Map::new();
		if let Some(origin) = self.capture_origin {
			section.insert("captureOrigin".into(), origin.into());
		}
		if let Some(frame_id) = &self.frame_id {
			section.insert("frameId".into(), frame_id.clone().into());
		}
		if section.is_empty() {
			catalog.ext.remove(SECTION_KEY);
		} else {
			catalog.ext.insert(SECTION_KEY.into(), section.into());
		}
		catalog.clock = self.capture_origin.and_then(hang_clock);
	}

	/// The capture time of a frame with media timestamp `pts`: from the `captureOrigin`, or else
	/// from hang's `clock` of `catalog` (another publisher's), as a time since the Unix epoch.
	pub(crate) fn capture_time(&self, catalog: &MoqrosCatalog, pts: Duration) -> Option<Duration> {
		if let Some(origin) = self.capture_origin {
			return crate::clock::capture_time(origin, pts);
		}
		let pts = moq_net::Timestamp::from_micros(u64::try_from(pts.as_micros()).ok()?).ok()?;
		catalog.clock?.wall_clock(pts).ok()?.duration_since(UNIX_EPOCH).ok()
	}
}

/// hang's clock for a capture origin of `origin` µs since the Unix epoch, if it is after
/// hang's epoch (2020).
fn hang_clock(origin: i64) -> Option<Clock> {
	let since_epoch = u64::try_from(origin).ok()?.checked_sub(MOQ_EPOCH_UNIX_MILLIS * 1000)?;
	// Capture times that far past the present aren't wall-clock times.
	let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
	if u128::from(u64::try_from(origin).ok()?) > now.as_micros() * 2 {
		return None;
	}
	Clock::new(moq_net::Timestamp::from_micros(since_epoch).ok()?).ok()
}

/// Whether `id` can be used as a ROS `frame_id`: short, printable, without whitespace.
pub(crate) fn is_frame_id(id: &str) -> bool {
	!id.is_empty() && id.len() <= MAX_FRAME_ID_LEN && id.chars().all(|c| c.is_ascii_graphic())
}

/// The video rendition of a broadcast: the codec of its latest keyframe and its size.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Rendition {
	pub(crate) codec: VideoCodec,
	pub(crate) width: u32,
	pub(crate) height: u32,
}

impl From<&EncodedFrame> for Rendition {
	fn from(frame: &EncodedFrame) -> Self {
		Self {
			codec: frame.codec.clone(),
			width: frame.width,
			height: frame.height,
		}
	}
}

/// The catalog of a video broadcast with `renditions`, each a track name, what it carries, and the
/// settings it is encoded with, and with `section`.
pub(crate) fn video<'a>(
	renditions: impl IntoIterator<Item = (&'a str, &'a Rendition, &'a EncoderSettings)>,
	section: &MoqrosSection,
) -> Result<MoqrosCatalog> {
	let mut catalog = MoqrosCatalog::default();
	for (track, rendition, settings) in renditions {
		let mut config = VideoConfig::new(rendition.codec.clone());
		config.coded_width = Some(rendition.width);
		config.coded_height = Some(rendition.height);
		config.bitrate = Some(u64::from(settings.bitrate));
		config.framerate = Some(f64::from(settings.max_fps));
		config.container = Container::Legacy;
		catalog.video.insert(track, config)?;
	}
	section.write(&mut catalog);
	Ok(catalog)
}

/// The best video rendition of broadcast `name` whose codec this build can decode: the largest
/// one, or with `max_height` the largest one at most that tall (the smallest if none is).
pub(crate) fn select_video<'a, E>(
	catalog: &'a Catalog<E>,
	name: &str,
	max_height: Option<u32>,
) -> Result<(&'a str, &'a VideoConfig, Codec)> {
	if catalog.video.renditions.is_empty() {
		return Err(Error::NoVideo(name.to_string()));
	}
	let mut offered = Vec::new();
	let decodable: Vec<_> = catalog
		.video
		.ranked()
		.filter_map(|(track, config)| {
			offered.push(config.codec.to_string());
			let codec = Codec::from_catalog(&config.codec).filter(|c| c.is_available())?;
			Some((track.as_str(), config, codec))
		})
		.collect();
	let fits = |(_, config, _): &&(&str, &VideoConfig, Codec)| {
		max_height.is_none_or(|max| config.coded_height.is_some_and(|height| height <= max))
	};
	// Ranked largest first.
	decodable
		.iter()
		.find(fits)
		.or(decodable.last())
		.copied()
		.ok_or_else(|| Error::UnsupportedCodec(offered.join(", ")))
}

/// The catalog of a data broadcast described by `settings`.
pub(crate) fn data(settings: &DataSettings, section: &MoqrosSection) -> Result<MoqrosCatalog> {
	let mut catalog = MoqrosCatalog::default();
	let mut extra = serde_json::Map::new();
	if let Some(ros_type) = &settings.ros_type {
		extra.insert(ROS_TYPE_KEY.into(), ros_type.clone().into());
	}
	let mode = mode_to_catalog(settings.mode);
	match settings.format {
		DataFormat::Json => {
			let mut config = JsonConfig::new(mode);
			config.schema = settings.ros_type.as_ref().map(|ros_type| format!("ros2:{ros_type}"));
			config.extra = extra;
			catalog.json.insert(DATA_TRACK, config)?;
		}
		DataFormat::Cdr => {
			let mut config = BinaryConfig::new(mode);
			config.mime = Some(CDR_MIME.into());
			config.extra = extra;
			catalog.binary.insert(DATA_TRACK, config)?;
		}
	}
	section.write(&mut catalog);
	Ok(catalog)
}

/// The first data track of `catalog` moqros can read, JSON before CDR.
///
/// A `rosType` that isn't a ROS message type name counts as missing: whoever announces the
/// broadcast writes it, and it ends up in errors and logs.
pub(crate) fn find_data<E>(catalog: &Catalog<E>) -> Option<(String, DataSettings)> {
	let settings = |format, mode: &Mode, extra: &serde_json::Map<String, serde_json::Value>| {
		Some(DataSettings {
			format,
			// A consumer must ignore a track whose mode it doesn't know.
			mode: mode_from_catalog(mode)?,
			ros_type: extra
				.get(ROS_TYPE_KEY)
				.and_then(|t| t.as_str())
				.filter(|t| is_ros_type(t))
				.map(String::from),
		})
	};
	let json =
		catalog.json.tracks.iter().filter_map(|(track, config)| {
			Some((track.clone(), settings(DataFormat::Json, &config.mode, &config.extra)?))
		});
	let cdr = catalog
		.binary
		.tracks
		.iter()
		.filter(|(_, config)| config.mime.as_deref() == Some(CDR_MIME))
		.filter_map(|(track, config)| Some((track.clone(), settings(DataFormat::Cdr, &config.mode, &config.extra)?)));
	json.chain(cdr).next()
}

/// Whether `name` looks like a ROS message type, e.g. `sensor_msgs/msg/Imu`: two or three
/// `/`-separated parts, each a letter followed by letters, digits or underscores.
fn is_ros_type(name: &str) -> bool {
	let is_part = |part: &str| {
		let mut chars = part.chars();
		chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
	};
	(2..=3).contains(&name.split('/').count()) && name.split('/').all(is_part)
}

fn mode_to_catalog(mode: DataMode) -> Mode {
	match mode {
		DataMode::Snapshot => Mode::Snapshot,
		DataMode::Stream => Mode::Stream,
	}
}

fn mode_from_catalog(mode: &Mode) -> Option<DataMode> {
	match mode {
		Mode::Snapshot => Some(DataMode::Snapshot),
		Mode::Stream => Some(DataMode::Stream),
		_ => None,
	}
}

/// Which track [`Subscriber::subscribe`](crate::Subscriber::subscribe) reads from a catalog.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Kind {
	Images,
	Data,
	None,
}

impl Kind {
	pub(crate) fn of<E>(catalog: &Catalog<E>) -> Self {
		let decodable = catalog
			.video
			.ranked()
			.any(|(_, config)| Codec::from_catalog(&config.codec).is_some_and(Codec::is_available));
		if decodable {
			Self::Images
		} else if find_data(catalog).is_some() {
			Self::Data
		} else if !catalog.video.renditions.is_empty() {
			// Subscribing fails, but says which codecs the broadcast offers.
			Self::Images
		} else {
			Self::None
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn catalog(json: serde_json::Value) -> MoqrosCatalog {
		serde_json::from_value(json).unwrap()
	}

	fn settings(format: DataFormat, mode: DataMode, ros_type: Option<&str>) -> DataSettings {
		DataSettings {
			format,
			mode,
			ros_type: ros_type.map(String::from),
		}
	}

	#[test]
	fn picks_what_this_build_can_read() {
		let video = |codec: &str| serde_json::json!({ "renditions": { "video": { "codec": codec } } });
		let data = serde_json::json!({ "tracks": { "data": { "mode": "snapshot" } } });

		assert_eq!(Kind::of(&catalog(serde_json::json!({}))), Kind::None);
		assert_eq!(Kind::of(&catalog(serde_json::json!({ "json": data }))), Kind::Data);
		// Video nobody can decode still subscribes, to report the codecs on offer...
		assert_eq!(
			Kind::of(&catalog(serde_json::json!({ "video": video("vp09.00.10.08") }))),
			Kind::Images
		);
		// ...unless there's data to read instead.
		assert_eq!(
			Kind::of(&catalog(
				serde_json::json!({ "video": video("vp09.00.10.08"), "json": data })
			)),
			Kind::Data
		);
		for codec in Codec::available() {
			let codec = match codec {
				Codec::Vp8 => "vp8",
				Codec::H264 => "avc3.42001f",
				Codec::Av1 => "av01.0.08M.08",
			};
			assert_eq!(
				Kind::of(&catalog(serde_json::json!({ "video": video(codec), "json": data }))),
				Kind::Images,
				"{codec}"
			);
		}
	}

	#[test]
	fn reports_the_video_codecs_on_offer() {
		let empty = catalog(serde_json::json!({}));
		assert!(matches!(select_video(&empty, "cam", None), Err(Error::NoVideo(name)) if name == "cam"));
		let vp9 = catalog(serde_json::json!({ "video": { "renditions": { "video": { "codec": "vp09.00.10.08" } } } }));
		assert!(
			matches!(select_video(&vp9, "cam", None), Err(Error::UnsupportedCodec(codecs)) if codecs == "vp09.00.10.08")
		);
	}

	#[test]
	fn data_round_trip() {
		for settings in [
			settings(DataFormat::Json, DataMode::Snapshot, Some("sensor_msgs/msg/Imu")),
			settings(DataFormat::Cdr, DataMode::Stream, Some("std_msgs/msg/String")),
			settings(DataFormat::Json, DataMode::Stream, None),
		] {
			let catalog: MoqrosCatalog =
				serde_json::from_str(&data(&settings, &MoqrosSection::default()).unwrap().to_json().unwrap()).unwrap();
			assert_eq!(find_data(&catalog), Some((DATA_TRACK.to_string(), settings.clone())));
		}
	}

	#[test]
	fn data_entries() {
		let catalog = data(
			&settings(DataFormat::Json, DataMode::Snapshot, Some("std_msgs/msg/String")),
			&MoqrosSection::default(),
		)
		.unwrap();
		let json: serde_json::Value = serde_json::from_str(&catalog.to_json().unwrap()).unwrap();
		assert_eq!(
			json["json"]["tracks"][DATA_TRACK],
			serde_json::json!({
				"mode": "snapshot",
				"schema": "ros2:std_msgs/msg/String",
				"rosType": "std_msgs/msg/String",
			})
		);

		let catalog = data(
			&settings(DataFormat::Cdr, DataMode::Stream, Some("std_msgs/msg/String")),
			&MoqrosSection::default(),
		)
		.unwrap();
		let json: serde_json::Value = serde_json::from_str(&catalog.to_json().unwrap()).unwrap();
		assert_eq!(
			json["binary"]["tracks"][DATA_TRACK],
			serde_json::json!({
				"mode": "stream",
				"mime": CDR_MIME,
				"rosType": "std_msgs/msg/String",
			})
		);
	}

	#[test]
	fn ignores_unreadable_data_tracks() {
		let catalog = catalog(serde_json::json!({
			"json": { "tracks": { "future": { "mode": "window" } } },
			"binary": { "tracks": {
				"thumbs": { "mode": "snapshot", "mime": "image/jpeg" },
				"raw": { "mode": "stream", "mime": CDR_MIME },
			} },
		}));
		assert_eq!(
			find_data(&catalog),
			Some(("raw".to_string(), settings(DataFormat::Cdr, DataMode::Stream, None)))
		);
		assert_eq!(find_data(&MoqrosCatalog::default()), None);
	}

	#[test]
	fn ignores_malformed_ros_types() {
		let ros_type = |ros_type: &str| {
			let catalog = catalog(serde_json::json!({
				"json": { "tracks": { "data": { "mode": "snapshot", "rosType": ros_type } } },
			}));
			find_data(&catalog).unwrap().1.ros_type
		};
		for valid in [
			"std_msgs/msg/String",
			"sensor_msgs/msg/Imu",
			"pkg2/action/Fib_FeedbackMessage",
		] {
			assert_eq!(ros_type(valid).as_deref(), Some(valid));
		}
		for invalid in [
			"",
			"String",
			"std_msgs/msg/String\n2026-10-17T00:00:00Z  INFO moqros_cli: forged",
			"\x1b]52;c;aGk=\x07std_msgs/msg/String",
			"std_msgs/msg/String ",
			"/msg/String",
			"std_msgs//String",
			"a/b/c/d",
			"1pkg/msg/String",
		] {
			assert_eq!(ros_type(invalid), None, "{invalid:?}");
		}
	}

	#[test]
	fn selects_renditions_by_height() {
		let rendition =
			|height| serde_json::json!({ "codec": "vp8", "codedWidth": height * 16 / 9, "codedHeight": height });
		let catalog = catalog(serde_json::json!({ "video": { "renditions": {
			"video": rendition(1080), "video_360p": rendition(360), "video_720p": rendition(720),
		} } }));
		if !Codec::Vp8.is_available() {
			return;
		}
		let track = |max_height| select_video(&catalog, "cam", max_height).unwrap().0;
		assert_eq!(track(None), "video");
		assert_eq!(track(Some(1080)), "video");
		assert_eq!(track(Some(800)), "video_720p");
		assert_eq!(track(Some(360)), "video_360p");
		assert_eq!(track(Some(100)), "video_360p");
	}

	#[test]
	fn moqros_section_round_trip() {
		let settings = settings(DataFormat::Json, DataMode::Snapshot, None);
		for section in [
			MoqrosSection::default(),
			MoqrosSection {
				capture_origin: Some(-33_001),
				frame_id: None,
			},
			MoqrosSection {
				capture_origin: Some(1_794_000_000_000_000),
				frame_id: Some("camera_optical".into()),
			},
		] {
			let json = data(&settings, &section).unwrap().to_json().unwrap();
			let catalog: MoqrosCatalog = serde_json::from_str(&json).unwrap();
			assert_eq!(MoqrosSection::read(&catalog), section, "{json}");
		}
	}

	#[test]
	fn capture_times_from_either_clock() {
		let pts = Duration::from_millis(1500);
		// Sim time: before hang's epoch, so only in the moqros section.
		let sim = MoqrosSection {
			capture_origin: Some(12_000_000),
			frame_id: None,
		};
		let catalog = data(&settings(DataFormat::Json, DataMode::Snapshot, None), &sim).unwrap();
		assert!(catalog.clock.is_none());
		assert_eq!(sim.capture_time(&catalog, pts), Some(Duration::from_millis(13_500)));

		// Wall-clock time: also in hang's clock, which another publisher may send on its own.
		let origin = 1_794_000_000_000_000;
		let wall = MoqrosSection {
			capture_origin: Some(origin),
			frame_id: None,
		};
		let mut catalog = data(&settings(DataFormat::Json, DataMode::Snapshot, None), &wall).unwrap();
		assert!(catalog.clock.is_some());
		let expected = Some(Duration::from_micros(1_794_000_001_500_000));
		assert_eq!(wall.capture_time(&catalog, pts), expected);
		catalog.ext.clear();
		let section = MoqrosSection::read(&catalog);
		assert_eq!(section, MoqrosSection::default());
		assert_eq!(section.capture_time(&catalog, pts), expected);
		catalog.clock = None;
		assert_eq!(section.capture_time(&catalog, pts), None);
	}

	#[test]
	fn ignores_malformed_moqros_sections() {
		for section in [
			serde_json::json!("camera"),
			serde_json::json!({ "captureOrigin": "12", "frameId": 7 }),
			serde_json::json!({ "captureOrigin": 1.5, "frameId": "cam\nera" }),
			serde_json::json!({ "frameId": "" }),
			serde_json::json!({ "frameId": "x".repeat(MAX_FRAME_ID_LEN + 1) }),
		] {
			let catalog = catalog(serde_json::json!({ "moqros": section }));
			assert_eq!(MoqrosSection::read(&catalog), MoqrosSection::default(), "{section}");
		}
	}
}
