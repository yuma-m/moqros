//! Publish images to a MoQ relay as hang (WebCodecs-compatible) VP8 or H.264 broadcasts,
//! and other messages as hang data broadcasts.

use hang::catalog::{Catalog, Container, PRIORITY, VideoCodec, VideoConfig};
use hang::moq_net;

use crate::clock::StreamClock;
use crate::codec::{EncodedFrame, Encoder, EncoderSettings};
use crate::data::{DataPublisher, DataSettings};
use crate::{ClientConfig, Error, Image, Result};

/// Name of the single video track inside every moqros broadcast.
pub const VIDEO_TRACK: &str = "video";

/// A connection to a relay that can carry any number of image and data broadcasts.
///
/// The connection redials in the background if it drops; broadcasts created
/// here are re-announced automatically once it is back. Clones share the
/// connection, which stays up while any clone is alive.
#[derive(Clone)]
pub struct Publisher {
	origin: moq_net::origin::Producer,
	connection: moq_tokio::Connection,
}

impl Publisher {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config.client()?.with_publisher(&origin).connect(config.url.clone());
		Ok(Self { origin, connection })
	}

	pub(crate) fn from_parts(origin: moq_net::origin::Producer, connection: moq_tokio::Connection) -> Self {
		Self { origin, connection }
	}

	/// Wait until the first session with the relay is established.
	///
	/// Fails if the relay refuses the session, e.g. for missing credentials.
	pub async fn established(&self) -> Result<()> {
		crate::client::established(&self.connection).await
	}

	/// Resolves when the connection gives up for good.
	pub async fn closed(&self) -> Result<()> {
		Ok(self.connection.closed().await?)
	}

	/// Close the connection for every clone, without redialing.
	///
	/// Data already queued is delivered first, for at most about a second. Call this before
	/// exiting so the relay ends this session's broadcasts right away; otherwise it holds them
	/// until the session times out.
	pub async fn close(&self) -> Result<()> {
		Ok(self.connection.clone().close().await?)
	}

	/// Create and announce a broadcast named `name` that carries encoded images.
	///
	/// Fails if `settings.codec` isn't compiled into this build.
	pub fn create_image_broadcast(&self, name: &str, settings: EncoderSettings) -> Result<ImagePublisher> {
		ImagePublisher::new(&self.origin, name, settings)
	}

	/// Create and announce a broadcast named `name` that carries messages in a data track.
	pub fn create_data_broadcast(&self, name: &str, settings: DataSettings) -> Result<DataPublisher> {
		DataPublisher::new(&self.origin, name, settings)
	}
}

/// Encodes images and writes them into one MoQ broadcast.
///
/// The codec is chosen by [`EncoderSettings::codec`] (VP8 by default).
///
/// Every keyframe starts a new MoQ group so subscribers (and relays) can join
/// or skip ahead at group boundaries. The catalog is republished whenever the
/// resolution or codec parameters change.
pub struct ImagePublisher {
	name: String,
	broadcast: moq_net::broadcast::Producer,
	catalog: moq_net::track::Producer,
	video: moq_net::track::Producer,
	group: Option<moq_net::group::Producer>,
	/// What [`Self::group`] has used of moq-net's per-group limits.
	budget: GroupBudget,
	encoder: Encoder,
	rendition: Option<(u32, u32, VideoCodec)>,
	clock: StreamClock,
}

impl ImagePublisher {
	fn new(origin: &moq_net::origin::Producer, name: &str, settings: EncoderSettings) -> Result<Self> {
		let encoder = Encoder::new(settings)?;
		let broadcast = origin.create_broadcast(name)?;
		broadcast.announce(Default::default())?;

		let catalog = broadcast.create_track(Catalog::DEFAULT_NAME, Catalog::default_track_info())?;
		let video = broadcast.create_track(VIDEO_TRACK, hang::container::track_info(PRIORITY.video))?;
		tracing::info!(broadcast = name, codec = %encoder.settings().codec, "announced image broadcast");

		Ok(Self {
			name: name.to_string(),
			broadcast,
			catalog,
			video,
			group: None,
			budget: GroupBudget::default(),
			encoder,
			rendition: None,
			clock: StreamClock::default(),
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		&self.name
	}

	/// Encode and send one image. Returns whether a frame was written.
	///
	/// Encoding is CPU bound; call this from a blocking-friendly thread when
	/// running inside an async runtime.
	pub fn publish(&mut self, image: &Image) -> Result<bool> {
		match self.encoder.encode(image)? {
			Some(frame) => {
				self.publish_encoded(&frame)?;
				Ok(true)
			}
			None => Ok(false),
		}
	}

	/// Send an already encoded frame (VP8, or Annex-B H.264 with SPS/PPS inline in keyframes).
	pub fn publish_encoded(&mut self, frame: &EncodedFrame) -> Result<()> {
		if frame.keyframe {
			self.update_catalog(frame)?;
			if let Some(group) = self.group.take() {
				group.finish()?;
			}
			self.group = Some(self.video.append_group()?);
			self.budget = GroupBudget::default();
		} else if self.group.is_some() && !self.budget.fits(frame.data.len()) {
			// moq-net would abort the group on this write. Close it and drop frames
			// until the next keyframe opens a new one.
			tracing::warn!(
				broadcast = %self.name,
				"video group is full; raise the bitrate budget or shorten the keyframe interval"
			);
			if let Some(group) = self.group.take() {
				group.finish()?;
			}
		}

		// Nothing decodable can be sent until the first keyframe opens a group.
		let Some(group) = self.group.as_mut() else {
			self.encoder.force_keyframe();
			return Ok(());
		};

		let timestamp = self.clock.micros(frame.timestamp);
		let frame = hang::container::Frame {
			timestamp: moq_net::Timestamp::from_micros(timestamp).map_err(|_| Error::Timestamp)?,
			payload: frame.data.clone(),
		};
		frame.write_to(group)?;
		self.budget.add(frame.payload.len());
		// Start the next group early, so a long keyframe interval doesn't fill this one.
		if self.budget.nearly_full() {
			self.encoder.force_keyframe();
		}
		Ok(())
	}

	fn update_catalog(&mut self, frame: &EncodedFrame) -> Result<()> {
		let codec = frame.codec.clone();
		let rendition = (frame.width, frame.height, codec.clone());
		if self.rendition.as_ref() == Some(&rendition) {
			return Ok(());
		}

		let settings = self.encoder.settings();
		let mut config = VideoConfig::new(codec.clone());
		config.coded_width = Some(frame.width);
		config.coded_height = Some(frame.height);
		config.bitrate = Some(settings.bitrate as u64);
		config.framerate = Some(settings.max_fps as f64);
		config.container = Container::Legacy;

		let mut catalog = Catalog::<()>::default();
		catalog.video.insert(VIDEO_TRACK, config)?;

		let mut group = self.catalog.append_group()?;
		group.write_frame(moq_net::Timestamp::now(), catalog.to_json()?)?;
		group.finish()?;

		tracing::info!(
			broadcast = %self.name,
			codec = %codec,
			width = frame.width,
			height = frame.height,
			"published catalog"
		);
		self.rendition = Some(rendition);
		Ok(())
	}
}

/// How much of moq-net's per-group limits a group has used, so writers can start a new
/// group before a write would abort the current one.
#[derive(Debug, Default)]
pub(crate) struct GroupBudget {
	frames: usize,
	bytes: u64,
}

impl GroupBudget {
	/// Bytes a frame may add on top of its payload (hang's timestamp varint).
	const OVERHEAD: u64 = 8;

	/// Whether a frame with `payload` bytes still fits into the group.
	pub(crate) fn fits(&self, payload: usize) -> bool {
		self.frames < moq_net::group::MAX_GROUP_FRAMES
			&& self.bytes + payload as u64 + Self::OVERHEAD <= moq_net::group::MAX_CACHE_BYTES
	}

	/// Account for a frame with `payload` bytes.
	pub(crate) fn add(&mut self, payload: usize) {
		self.frames += 1;
		self.bytes += payload as u64 + Self::OVERHEAD;
	}

	/// Whether the group has used three quarters of either limit.
	fn nearly_full(&self) -> bool {
		self.frames >= moq_net::group::MAX_GROUP_FRAMES / 4 * 3 || self.bytes >= moq_net::group::MAX_CACHE_BYTES / 4 * 3
	}
}

impl Drop for ImagePublisher {
	fn drop(&mut self) {
		if let Some(group) = self.group.take() {
			let _ = group.finish();
		}
		self.broadcast.close();
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use bytes::Bytes;

	use super::*;
	use crate::codec::Codec;

	fn publisher(settings: EncoderSettings) -> ImagePublisher {
		ImagePublisher::new(&moq_tokio::origin::spawn(), "camera", settings).unwrap()
	}

	/// A keyframe interval too long for one group starts the next group early.
	#[tokio::test]
	async fn long_keyframe_interval_starts_groups_early() {
		if Codec::available().is_empty() {
			return;
		}
		let mut publisher = publisher(EncoderSettings {
			keyframe_interval: Duration::from_secs(3600),
			..Default::default()
		});
		let mut keyframes = 0;
		for t in 0..moq_net::group::MAX_GROUP_FRAMES as u32 + 100 {
			publisher
				.publish(&crate::codec::tests::test_pattern(16, 16, t))
				.unwrap();
			keyframes += usize::from(publisher.budget.frames == 1);
		}
		assert_eq!(keyframes, 2);
	}

	/// Encoded frames that would overflow a group are dropped until the next keyframe.
	#[tokio::test]
	async fn full_group_waits_for_a_keyframe() {
		if Codec::available().is_empty() {
			return;
		}
		let mut publisher = publisher(EncoderSettings::default());
		let frame = |n: u64, keyframe: bool| EncodedFrame {
			data: Bytes::from_static(&[0; 16]),
			keyframe,
			width: 16,
			height: 16,
			timestamp: Duration::from_millis(n * 33),
			codec: VideoCodec::VP8,
		};
		let limit = moq_net::group::MAX_GROUP_FRAMES as u64;
		for n in 0..limit + 10 {
			publisher.publish_encoded(&frame(n, n == 0)).unwrap();
		}
		assert!(publisher.group.is_none());
		publisher.publish_encoded(&frame(limit + 10, true)).unwrap();
		assert_eq!(publisher.budget.frames, 1);
	}
}
