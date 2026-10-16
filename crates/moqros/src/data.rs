//! Arbitrary (non-image) messages carried as hang JSON or binary data tracks.
//!
//! A data broadcast has a single track, [`DATA_TRACK`], described by the catalog's `json` section
//! ([`DataFormat::Json`]) or its `binary` section ([`DataFormat::Cdr`]). Each MoQ frame is one
//! message payload, as is; the catalog entry also names the ROS message type (`rosType`) so a
//! subscriber can republish it without being told.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::PRIORITY;
use hang::moq_net;

use crate::Result;
use crate::catalog::{self, DATA_TRACK};
use crate::track::{BroadcastWriter, GroupReader, GroupWriter};

/// How each message is serialized on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataFormat {
	/// One UTF-8 JSON value per message, readable by any hang consumer (e.g. a browser).
	Json,
	/// The message's ROS serialization (CDR), as received from the middleware.
	Cdr,
}

crate::names::named_enum!(DataFormat, "data format", {
	Json => "json",
	Cdr => "cdr",
});

/// How messages map onto MoQ groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum DataMode {
	/// Latest value: every message is a group of its own, and a subscriber that falls behind
	/// skips to the newest one. Suits sensor data and state.
	#[default]
	Snapshot,
	/// Lossless log: messages go into one group and are delivered in order. A group that
	/// reaches moq-net's limits (8192 messages or 32 MB) is closed and the next one continues.
	Stream,
}

crate::names::named_enum!(DataMode, "data mode", {
	Snapshot => "snapshot",
	Stream => "stream",
});

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
}

/// Writes messages into one MoQ data broadcast.
pub struct DataPublisher {
	settings: DataSettings,
	// Declared before `broadcast`, so the open group is finished before the broadcast closes.
	track: GroupWriter,
	broadcast: BroadcastWriter,
}

impl DataPublisher {
	pub(crate) fn new(origin: &moq_net::origin::Producer, name: &str, settings: DataSettings) -> Result<Self> {
		let catalog = catalog::data(&settings)?;
		let mut broadcast = BroadcastWriter::create(origin, name)?;
		let track = broadcast.create_track(DATA_TRACK, moq_net::track::Info::default().with_priority(PRIORITY.text))?;
		broadcast.publish_catalog(&catalog)?;
		tracing::info!(
			broadcast = name,
			format = %settings.format,
			mode = %settings.mode,
			ros_type = settings.ros_type.as_deref().unwrap_or_default(),
			"announced data broadcast"
		);

		Ok(Self {
			settings,
			track: GroupWriter::new(track),
			broadcast,
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		self.broadcast.name()
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
				self.track.write_frame(timestamp, payload)?;
				self.track.finish_group()?;
			}
			DataMode::Stream => {
				if !self.track.fits(payload.len()) {
					self.track.finish_group()?;
				}
				self.track.write_frame(timestamp, payload)?;
			}
		}
		Ok(())
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
	use crate::Error;

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

	/// A stream track outgrowing moq-net's group limits continues in a new group.
	#[tokio::test]
	async fn stream_continues_past_a_full_group() {
		let origin = moq_tokio::origin::spawn();
		let mut publisher =
			DataPublisher::new(&origin, "log", settings(DataFormat::Json, DataMode::Stream, None)).unwrap();
		let broadcast = origin.consume().request_broadcast("log").await.unwrap();
		let mut track = broadcast
			.track(DATA_TRACK)
			.unwrap()
			.subscribe(moq_net::track::Subscription::default())
			.await
			.unwrap()
			.ordered();

		// Hold the first group before the second exists, as a subscriber that keeps up does.
		publisher.publish("0").unwrap();
		let mut group = track.next_group().await.unwrap().unwrap();
		let count = moq_net::group::MAX_GROUP_FRAMES + 100;
		for i in 1..count {
			publisher.publish(i.to_string()).unwrap();
		}
		drop(publisher);

		let mut groups = 1;
		let mut received = 0;
		loop {
			while let Some(frame) = group.read_frame().await.unwrap() {
				assert_eq!(frame.payload, received.to_string().as_bytes());
				received += 1;
			}
			// Dropping the publisher ends the track with an error.
			let Ok(Some(next)) = track.next_group().await else {
				break;
			};
			group = next;
			groups += 1;
		}
		assert_eq!((groups, received), (2, count));
	}
}
