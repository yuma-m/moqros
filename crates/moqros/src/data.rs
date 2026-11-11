//! Arbitrary (non-image) messages carried as hang JSON or binary data tracks.
//!
//! A data broadcast has a single track, [`DATA_TRACK`], described by the catalog's `json` section
//! ([`DataFormat::Json`]) or its `binary` section ([`DataFormat::Cdr`]). Each MoQ frame is one
//! message payload, as is; the catalog entry also names the ROS message type (`rosType`) so a
//! subscriber can republish it without being told.
//!
//! Messages published with [`DataPublisher::publish_at`] carry their capture time: the frame
//! timestamps are relative to the capture time in the catalog's `moqros.captureOrigin`, as for
//! video, and [`DataMessage::capture_time`] puts them back together.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::PRIORITY;
use hang::moq_net;

use crate::catalog::{self, DATA_TRACK, MoqrosSection};
use crate::clock::StreamClock;
use crate::track::{BroadcastWriter, GroupWriter, TrackReader};
use crate::{Error, Result};

/// How each message is serialized on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
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
#[non_exhaustive]
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
	/// Serialization of each message.
	pub format: DataFormat,
	/// How messages map onto MoQ groups.
	pub mode: DataMode,
	/// ROS message type, e.g. `sensor_msgs/msg/Imu`.
	pub ros_type: Option<String>,
}

impl DataSettings {
	/// `format` in the default [`DataMode`], without a ROS type.
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
	/// The timeline of [`DataPublisher::publish_at`].
	clock: StreamClock,
	/// What the catalog announces in its `moqros` section.
	section: MoqrosSection,
	// Declared before `broadcast`, so the open group is finished before the broadcast closes.
	track: GroupWriter,
	broadcast: BroadcastWriter,
}

impl DataPublisher {
	pub(crate) fn new(origin: &moq_net::origin::Producer, name: &str, settings: DataSettings) -> Result<Self> {
		let section = MoqrosSection::default();
		let catalog = catalog::data(&settings, &section)?;
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
			clock: StreamClock::default(),
			section,
			track: GroupWriter::new(track),
			broadcast,
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		self.broadcast.name()
	}

	/// The settings this track was created with.
	pub fn settings(&self) -> &DataSettings {
		&self.settings
	}

	/// Send one message payload, serialized as [`DataSettings::format`] says, stamped with the
	/// time it was sent.
	pub fn publish(&mut self, payload: impl Into<Bytes>) -> Result<()> {
		self.write(moq_net::Timestamp::now(), payload.into())
	}

	/// Send one message payload captured at `capture` (e.g. its ROS `header.stamp`, as a time
	/// since the epoch of whatever clock stamped it), so subscribers get it back as
	/// [`DataMessage::capture_time`].
	///
	/// Capture times map onto the broadcast's timeline as video timestamps do: they must
	/// increase, a repeated or slightly earlier one is nudged forward, and a capture clock that
	/// jumps back by over a second (e.g. a looping rosbag) re-anchors the timeline, which
	/// republishes the catalog. Use either this or [`DataPublisher::publish`] on one broadcast.
	pub fn publish_at(&mut self, capture: Duration, payload: impl Into<Bytes>) -> Result<()> {
		let pts = self.clock.micros(capture);
		let origin = self.clock.origin_micros();
		if origin != self.section.capture_origin {
			self.section.capture_origin = origin;
			self.broadcast
				.publish_catalog(&catalog::data(&self.settings, &self.section)?)?;
		}
		let timestamp = moq_net::Timestamp::from_micros(pts).map_err(|_| Error::Timestamp)?;
		self.write(timestamp, payload.into())
	}

	fn write(&mut self, timestamp: moq_net::Timestamp, payload: Bytes) -> Result<()> {
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
	/// Frame timestamp: relative to the start of the broadcast for messages published with
	/// [`DataPublisher::publish_at`], otherwise the send time on the publisher's monotonic clock.
	/// Only differences are meaningful.
	pub timestamp: Duration,
	/// When the message was captured, from [`DataPublisher::publish_at`] (in the publisher's
	/// clock), or `None` if the publisher didn't say.
	pub capture_time: Option<Duration>,
	/// The serialized message, in the track's [`DataFormat`].
	pub payload: Bytes,
}

/// Reads the messages of one data track.
///
/// For [`DataMode::Snapshot`] tracks it always jumps to the newest message, so a slow
/// reader stays at the live edge; [`DataMode::Stream`] tracks are read in order.
pub struct DataSubscriber {
	reader: TrackReader,
	settings: DataSettings,
}

impl DataSubscriber {
	pub(crate) fn new(reader: TrackReader, settings: DataSettings) -> Self {
		Self { reader, settings }
	}

	/// Format, mode and ROS type announced in the catalog.
	pub fn settings(&self) -> &DataSettings {
		&self.settings
	}

	/// Next message, or `None` once the broadcast ends.
	pub async fn next_message(&mut self) -> Result<Option<DataMessage>> {
		let frame = self.reader.next_frame().await?;
		Ok(frame.map(|frame| {
			let timestamp = frame.timestamp.into();
			DataMessage {
				timestamp,
				capture_time: self.reader.catalog().capture_time(timestamp),
				payload: frame.payload,
			}
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
