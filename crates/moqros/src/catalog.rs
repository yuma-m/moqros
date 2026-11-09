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

use hang::catalog::{BinaryConfig, Catalog, Container, JsonConfig, Mode, VideoCodec, VideoConfig};

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
/// settings it is encoded with.
pub(crate) fn video<'a>(
	renditions: impl IntoIterator<Item = (&'a str, &'a Rendition, &'a EncoderSettings)>,
) -> Result<Catalog> {
	let mut catalog = Catalog::<()>::default();
	for (track, rendition, settings) in renditions {
		let mut config = VideoConfig::new(rendition.codec.clone());
		config.coded_width = Some(rendition.width);
		config.coded_height = Some(rendition.height);
		config.bitrate = Some(u64::from(settings.bitrate));
		config.framerate = Some(f64::from(settings.max_fps));
		config.container = Container::Legacy;
		catalog.video.insert(track, config)?;
	}
	Ok(catalog)
}

/// The best video rendition of broadcast `name` whose codec this build can decode: the largest
/// one, or with `max_height` the largest one at most that tall (the smallest if none is).
pub(crate) fn select_video<'a>(
	catalog: &'a Catalog,
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
pub(crate) fn data(settings: &DataSettings) -> Result<Catalog> {
	let mut catalog = Catalog::<()>::default();
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
	Ok(catalog)
}

/// The first data track of `catalog` moqros can read, JSON before CDR.
///
/// A `rosType` that isn't a ROS message type name counts as missing: whoever announces the
/// broadcast writes it, and it ends up in errors and logs.
pub(crate) fn find_data(catalog: &Catalog) -> Option<(String, DataSettings)> {
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
	pub(crate) fn of(catalog: &Catalog) -> Self {
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

	fn catalog(json: serde_json::Value) -> Catalog {
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
			let catalog: Catalog = serde_json::from_str(&data(&settings).unwrap().to_json().unwrap()).unwrap();
			assert_eq!(find_data(&catalog), Some((DATA_TRACK.to_string(), settings.clone())));
		}
	}

	#[test]
	fn data_entries() {
		let catalog = data(&settings(
			DataFormat::Json,
			DataMode::Snapshot,
			Some("std_msgs/msg/String"),
		))
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

		let catalog = data(&settings(
			DataFormat::Cdr,
			DataMode::Stream,
			Some("std_msgs/msg/String"),
		))
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
		assert_eq!(find_data(&Catalog::default()), None);
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
}
