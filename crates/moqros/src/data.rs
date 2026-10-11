//! Arbitrary (non-image) messages carried as hang JSON or binary data tracks.
//!
//! A data broadcast has a single track, [`DATA_TRACK`], described by the catalog's `json` section
//! ([`DataFormat::Json`]) or its `binary` section ([`DataFormat::Cdr`]). Each MoQ frame is one
//! message payload, as is; the catalog entry also names the ROS message type (`rosType`) so a
//! subscriber can republish it without being told.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use bytes::Bytes;
use hang::catalog::{BinaryConfig, Catalog, JsonConfig, Mode, PRIORITY};
use hang::moq_net;

use crate::subscriber::GroupReader;
use crate::{Error, Result};

/// Name of the single data track inside every moqros data broadcast.
pub const DATA_TRACK: &str = "data";

/// Media type of [`DataFormat::Cdr`] payloads in the catalog's `binary` section.
pub const CDR_MIME: &str = "application/x-ros2-cdr";

/// Catalog field naming the ROS message type of a data track.
const ROS_TYPE_KEY: &str = "rosType";

/// How each message is serialized on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataFormat {
	/// One UTF-8 JSON value per message, readable by any hang consumer (e.g. a browser).
	Json,
	/// The message's ROS serialization (CDR), as received from the middleware.
	Cdr,
}

impl fmt::Display for DataFormat {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Json => "json",
			Self::Cdr => "cdr",
		})
	}
}

impl FromStr for DataFormat {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self> {
		match s.to_ascii_lowercase().as_str() {
			"json" => Ok(Self::Json),
			"cdr" => Ok(Self::Cdr),
			other => Err(Error::InvalidSettings(format!("unknown data format {other:?}"))),
		}
	}
}

/// How messages map onto MoQ groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DataMode {
	/// Latest value: every message is a group of its own, and a subscriber that falls behind
	/// skips to the newest one. Suits sensor data and state.
	#[default]
	Snapshot,
	/// Lossless log: all messages go into one group and are delivered in order.
	Stream,
}

impl DataMode {
	fn to_catalog(self) -> Mode {
		match self {
			Self::Snapshot => Mode::Snapshot,
			Self::Stream => Mode::Stream,
		}
	}

	fn from_catalog(mode: &Mode) -> Option<Self> {
		match mode {
			Mode::Snapshot => Some(Self::Snapshot),
			Mode::Stream => Some(Self::Stream),
			_ => None,
		}
	}
}

impl fmt::Display for DataMode {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Snapshot => "snapshot",
			Self::Stream => "stream",
		})
	}
}

impl FromStr for DataMode {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self> {
		match s.to_ascii_lowercase().as_str() {
			"snapshot" => Ok(Self::Snapshot),
			"stream" => Ok(Self::Stream),
			other => Err(Error::InvalidSettings(format!("unknown data mode {other:?}"))),
		}
	}
}

/// Describes a data track: given to [`Publisher::create_data_broadcast`](crate::Publisher::create_data_broadcast),
/// and read back from the catalog by [`DataSubscriber::settings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSettings {
	pub format: DataFormat,
	pub mode: DataMode,
	/// ROS message type, e.g. `sensor_msgs/msg/Imu`.
	pub ros_type: Option<String>,
}

impl DataSettings {
	pub fn new(format: DataFormat) -> Self {
		Self {
			format,
			mode: DataMode::default(),
			ros_type: None,
		}
	}

	/// Add the [`DATA_TRACK`] entry to `catalog`.
	pub(crate) fn insert_into(&self, catalog: &mut Catalog) -> Result<()> {
		let mut extra = serde_json::Map::new();
		if let Some(ros_type) = &self.ros_type {
			extra.insert(ROS_TYPE_KEY.into(), ros_type.clone().into());
		}
		let mode = self.mode.to_catalog();
		match self.format {
			DataFormat::Json => {
				let mut config = JsonConfig::new(mode);
				config.schema = self.ros_type.as_ref().map(|ros_type| format!("ros2:{ros_type}"));
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
		Ok(())
	}

	/// The first data track of `catalog` moqros can read, JSON before CDR.
	pub(crate) fn from_catalog(catalog: &Catalog) -> Option<(String, Self)> {
		let settings = |format, mode: &Mode, extra: &serde_json::Map<String, serde_json::Value>| {
			Some(Self {
				format,
				// A consumer must ignore a track whose mode it doesn't know.
				mode: DataMode::from_catalog(mode)?,
				ros_type: extra.get(ROS_TYPE_KEY).and_then(|t| t.as_str()).map(String::from),
			})
		};
		let json = catalog.json.tracks.iter().filter_map(|(track, config)| {
			Some((track.clone(), settings(DataFormat::Json, &config.mode, &config.extra)?))
		});
		let cdr = catalog
			.binary
			.tracks
			.iter()
			.filter(|(_, config)| config.mime.as_deref() == Some(CDR_MIME))
			.filter_map(|(track, config)| {
				Some((track.clone(), settings(DataFormat::Cdr, &config.mode, &config.extra)?))
			});
		json.chain(cdr).next()
	}
}

/// Writes messages into one MoQ data broadcast.
pub struct DataPublisher {
	name: String,
	settings: DataSettings,
	broadcast: moq_net::broadcast::Producer,
	_catalog: moq_net::track::Producer,
	track: moq_net::track::Producer,
	/// The one open group of a [`DataMode::Stream`] track.
	group: Option<moq_net::group::Producer>,
}

impl DataPublisher {
	pub(crate) fn new(origin: &moq_net::origin::Producer, name: &str, settings: DataSettings) -> Result<Self> {
		let mut catalog = Catalog::<()>::default();
		settings.insert_into(&mut catalog)?;

		let broadcast = origin.create_broadcast(name)?;
		broadcast.announce(Default::default())?;
		let catalog_track = broadcast.create_track(Catalog::DEFAULT_NAME, Catalog::default_track_info())?;
		let track = broadcast.create_track(DATA_TRACK, moq_net::track::Info::default().with_priority(PRIORITY.text))?;

		let mut group = catalog_track.append_group()?;
		group.write_frame(moq_net::Timestamp::now(), catalog.to_json()?)?;
		group.finish()?;
		tracing::info!(
			broadcast = name,
			format = %settings.format,
			mode = %settings.mode,
			ros_type = settings.ros_type.as_deref().unwrap_or_default(),
			"announced data broadcast"
		);

		Ok(Self {
			name: name.to_string(),
			settings,
			broadcast,
			_catalog: catalog_track,
			track,
			group: None,
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn settings(&self) -> &DataSettings {
		&self.settings
	}

	/// Send one message payload, serialized as [`DataSettings::format`] says.
	pub fn publish(&mut self, payload: impl Into<Bytes>) -> Result<()> {
		let payload = payload.into();
		let timestamp = moq_net::Timestamp::now();
		match self.settings.mode {
			DataMode::Snapshot => {
				let mut group = self.track.append_group()?;
				group.write_frame(timestamp, payload)?;
				group.finish()?;
			}
			DataMode::Stream => {
				let group = match &mut self.group {
					Some(group) => group,
					None => self.group.insert(self.track.append_group()?),
				};
				group.write_frame(timestamp, payload)?;
			}
		}
		Ok(())
	}
}

impl Drop for DataPublisher {
	fn drop(&mut self) {
		if let Some(group) = self.group.take() {
			let _ = group.finish();
		}
		self.broadcast.close();
	}
}

/// One message received from a data broadcast.
#[derive(Debug, Clone)]
pub struct DataMessage {
	/// Send time on the publisher's monotonic clock; only differences are meaningful.
	pub timestamp: Duration,
	pub payload: Bytes,
}

/// Reads the messages of one data track.
///
/// For [`DataMode::Snapshot`] tracks it always jumps to the newest message, so a slow
/// reader stays at the live edge; [`DataMode::Stream`] tracks are read in order.
pub struct DataSubscriber {
	_broadcast: moq_net::broadcast::Consumer,
	reader: GroupReader,
	settings: DataSettings,
}

impl DataSubscriber {
	pub(crate) fn new(broadcast: moq_net::broadcast::Consumer, reader: GroupReader, settings: DataSettings) -> Self {
		Self {
			_broadcast: broadcast,
			reader,
			settings,
		}
	}

	/// Format, mode and ROS type announced in the catalog.
	pub fn settings(&self) -> &DataSettings {
		&self.settings
	}

	/// Next message, or `None` once the broadcast ends.
	pub async fn next_message(&mut self) -> Result<Option<DataMessage>> {
		Ok(self.reader.next_frame().await?.map(|frame| DataMessage {
			timestamp: frame.timestamp.into(),
			payload: frame.payload,
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn names_round_trip() {
		for format in [DataFormat::Json, DataFormat::Cdr] {
			assert_eq!(format.to_string().parse::<DataFormat>().unwrap(), format);
		}
		for mode in [DataMode::Snapshot, DataMode::Stream] {
			assert_eq!(mode.to_string().parse::<DataMode>().unwrap(), mode);
		}
		assert_eq!("JSON".parse::<DataFormat>().unwrap(), DataFormat::Json);
		assert!(matches!("xml".parse::<DataFormat>(), Err(Error::InvalidSettings(_))));
		assert!(matches!("log".parse::<DataMode>(), Err(Error::InvalidSettings(_))));
	}

	fn settings(format: DataFormat, mode: DataMode, ros_type: Option<&str>) -> DataSettings {
		DataSettings {
			format,
			mode,
			ros_type: ros_type.map(String::from),
		}
	}

	#[test]
	fn catalog_round_trip() {
		for settings in [
			settings(DataFormat::Json, DataMode::Snapshot, Some("sensor_msgs/msg/Imu")),
			settings(DataFormat::Cdr, DataMode::Stream, Some("std_msgs/msg/String")),
			settings(DataFormat::Json, DataMode::Stream, None),
		] {
			let mut catalog = Catalog::default();
			settings.insert_into(&mut catalog).unwrap();
			let catalog: Catalog = serde_json::from_str(&catalog.to_json().unwrap()).unwrap();
			assert_eq!(
				DataSettings::from_catalog(&catalog),
				Some((DATA_TRACK.to_string(), settings.clone()))
			);
		}
	}

	#[test]
	fn catalog_entries() {
		let mut catalog = Catalog::default();
		settings(DataFormat::Json, DataMode::Snapshot, Some("std_msgs/msg/String"))
			.insert_into(&mut catalog)
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

		let mut catalog = Catalog::default();
		settings(DataFormat::Cdr, DataMode::Stream, Some("std_msgs/msg/String"))
			.insert_into(&mut catalog)
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
	fn ignores_unreadable_tracks() {
		let catalog: Catalog = serde_json::from_value(serde_json::json!({
			"json": { "tracks": { "future": { "mode": "window" } } },
			"binary": { "tracks": {
				"thumbs": { "mode": "snapshot", "mime": "image/jpeg" },
				"raw": { "mode": "stream", "mime": CDR_MIME },
			} },
		}))
		.unwrap();
		assert_eq!(
			DataSettings::from_catalog(&catalog),
			Some(("raw".to_string(), settings(DataFormat::Cdr, DataMode::Stream, None)))
		);
		assert_eq!(DataSettings::from_catalog(&Catalog::default()), None);
	}
}
