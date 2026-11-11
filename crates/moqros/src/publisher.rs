//! Publish images to a MoQ relay as hang (WebCodecs-compatible) VP8, H.264, or AV1 broadcasts,
//! and other messages as hang data broadcasts.

use std::time::Duration;

use hang::catalog::PRIORITY;
use hang::moq_net;

use crate::catalog::{self, MoqrosSection, Rendition, VIDEO_TRACK};
use crate::clock::StreamClock;
use crate::codec::{EncodedFrame, Encoder, EncoderSettings};
use crate::convert::I420;
use crate::data::{DataPublisher, DataSettings};
use crate::track::{BroadcastWriter, GroupWriter};
use crate::{ClientConfig, Error, Image, Result, Session};

/// A connection to a relay that can carry any number of image and data broadcasts.
///
/// The connection redials in the background if it drops; broadcasts created
/// here are re-announced automatically once it is back. Clones share the
/// connection, which stays up while any clone is alive.
#[derive(Clone)]
pub struct Publisher {
	origin: moq_net::origin::Producer,
	session: Session,
}

impl Publisher {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config.client()?.with_publisher(&origin).connect(config.url.clone());
		Ok(Self {
			origin,
			session: Session::new(connection),
		})
	}

	pub(crate) fn from_parts(origin: moq_net::origin::Producer, session: Session) -> Self {
		Self { origin, session }
	}

	/// The relay connection, shared with every clone.
	pub fn session(&self) -> &Session {
		&self.session
	}

	/// See [`Session::established`].
	pub async fn established(&self) -> Result<()> {
		self.session.established().await
	}

	/// See [`Session::closed`].
	pub async fn closed(&self) -> Result<()> {
		self.session.closed().await
	}

	/// See [`Session::close`].
	pub async fn close(&self) -> Result<()> {
		self.session.close().await
	}

	/// Create and announce a broadcast named `name` that carries encoded images.
	///
	/// `settings` is [`EncoderSettings`] for one rendition, or [`ImageBroadcastSettings`] for
	/// several. Fails if the codec isn't compiled into this build.
	pub fn create_image_broadcast(
		&self,
		name: &str,
		settings: impl Into<ImageBroadcastSettings>,
	) -> Result<ImagePublisher> {
		ImagePublisher::new(&self.origin, name, settings)
	}

	/// Create and announce a broadcast named `name` that carries messages in a data track.
	pub fn create_data_broadcast(&self, name: &str, settings: DataSettings) -> Result<DataPublisher> {
		DataPublisher::new(&self.origin, name, settings)
	}
}

/// A smaller copy of a video broadcast, for viewers with less bandwidth or smaller screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RenditionSettings {
	/// Picture height in pixels; the width keeps the aspect ratio. Rounded down to an even number.
	pub height: u32,
	/// Target bitrate in bits per second.
	pub bitrate: u32,
}

impl RenditionSettings {
	/// A rendition `height` pixels tall at `bitrate` bit/s.
	pub fn new(height: u32, bitrate: u32) -> Self {
		Self { height, bitrate }
	}
}

/// How [`Publisher::create_image_broadcast`] encodes a video broadcast: its full-size rendition,
/// and optionally smaller ones (simulcast).
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ImageBroadcastSettings {
	/// The full-size rendition, and the codec and timing of every rendition.
	pub encoder: EncoderSettings,
	/// Smaller renditions, each encoded from the same images scaled down. One at least as tall as
	/// the images is skipped until they get taller.
	pub renditions: Vec<RenditionSettings>,
}

impl ImageBroadcastSettings {
	/// Only the full-size rendition, encoded as `encoder` says.
	pub fn new(encoder: EncoderSettings) -> Self {
		Self {
			encoder,
			renditions: Vec::new(),
		}
	}

	/// Set [`ImageBroadcastSettings::renditions`].
	pub fn with_renditions(mut self, renditions: impl IntoIterator<Item = RenditionSettings>) -> Self {
		self.renditions = renditions.into_iter().collect();
		self
	}
}

impl From<EncoderSettings> for ImageBroadcastSettings {
	fn from(encoder: EncoderSettings) -> Self {
		Self::new(encoder)
	}
}

/// What [`ImagePublisher::publish`] or [`ImagePublisher::publish_encoded`] did with a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Published {
	/// The picture was written to the full-size rendition.
	Sent,
	/// Nothing reached the full-size rendition: the encoder dropped the image (e.g. to hold its
	/// bitrate), or the group was full and the next keyframe has to open a new one.
	Dropped,
}

impl Published {
	/// Whether the picture was [sent](Published::Sent).
	pub fn is_sent(self) -> bool {
		self == Self::Sent
	}
}

/// Encodes images and writes them into one MoQ broadcast.
///
/// The codec is chosen by [`EncoderSettings::codec`] (VP8 by default).
///
/// Every keyframe starts a new MoQ group so subscribers (and relays) can join
/// or skip ahead at group boundaries. The catalog is republished whenever the
/// resolution or codec parameters change.
///
/// With [smaller renditions](ImageBroadcastSettings::renditions), each image is also scaled down
/// and encoded once per rendition, into a track of its own (`video_<height>p`). Each rendition
/// has its own groups and keyframes.
///
/// The catalog is also republished whenever what its `moqros` section says changes: the capture
/// time of timestamp 0 (capture times map onto the timeline as for
/// [`DataPublisher::publish_at`](crate::DataPublisher::publish_at)) or the
/// [frame id](ImagePublisher::set_frame_id).
pub struct ImagePublisher {
	// Declared before `broadcast`, so the open groups are finished before the broadcast closes.
	/// The full-size rendition first.
	tracks: Vec<VideoTrack>,
	broadcast: BroadcastWriter,
	/// What the catalog's `moqros` section announces.
	section: MoqrosSection,
	clock: StreamClock,
}

impl ImagePublisher {
	pub(crate) fn new(
		origin: &moq_net::origin::Producer,
		name: &str,
		settings: impl Into<ImageBroadcastSettings>,
	) -> Result<Self> {
		let ImageBroadcastSettings { encoder, renditions } = settings.into();
		let broadcast = BroadcastWriter::create(origin, name)?;
		let track = |track_name: String, settings: EncoderSettings, height| -> Result<VideoTrack> {
			Ok(VideoTrack {
				broadcast: name.to_string(),
				writer: GroupWriter::new(
					broadcast.create_track(&track_name, hang::container::track_info(PRIORITY.video))?,
				),
				encoder: Encoder::new(settings)?,
				name: track_name,
				height,
				rendition: None,
			})
		};
		let mut tracks = vec![track(VIDEO_TRACK.to_string(), encoder.clone(), None)?];
		for rendition in renditions {
			let height = rendition.height & !1;
			if height == 0 {
				return Err(Error::InvalidSettings(format!(
					"rendition height {} is too small",
					rendition.height
				)));
			}
			let name = format!("{VIDEO_TRACK}_{height}p");
			if tracks.iter().any(|track| track.name == name) {
				return Err(Error::InvalidSettings(format!(
					"two renditions are {height} pixels tall"
				)));
			}
			let settings = EncoderSettings {
				bitrate: rendition.bitrate,
				..encoder.clone()
			};
			tracks.push(track(name, settings, Some(height))?);
		}
		tracing::info!(
			broadcast = name,
			codec = %encoder.codec,
			renditions = tracks.len(),
			"announced image broadcast"
		);
		Ok(Self {
			tracks,
			broadcast,
			section: MoqrosSection::default(),
			clock: StreamClock::default(),
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		self.broadcast.name()
	}

	/// The encoder settings of the full-size rendition, with the bitrate last set.
	pub fn settings(&self) -> &EncoderSettings {
		self.main().encoder.settings()
	}

	/// Aim for `bitrate` bit/s in the full-size rendition from the next image on; see
	/// [`Encoder::set_bitrate`]. The catalog announces it from the next keyframe.
	pub fn set_bitrate(&mut self, bitrate: u32) {
		self.main_mut().set_bitrate(bitrate);
	}

	/// Announce `frame_id` (e.g. the ROS `header.frame_id` of the images) in the catalog, so
	/// subscribers can stamp the images they republish with it. An empty or unprintable one,
	/// or one over 256 bytes, counts as none.
	pub fn set_frame_id(&mut self, frame_id: &str) -> Result<()> {
		let frame_id = catalog::is_frame_id(frame_id).then(|| frame_id.to_string());
		if frame_id != self.section.frame_id {
			self.section.frame_id = frame_id;
			self.publish_catalog()?;
		}
		Ok(())
	}

	/// Encode and send one image, in every rendition, and say what became of the full-size one.
	///
	/// Encoding is CPU bound; call this from a blocking-friendly thread when
	/// running inside an async runtime.
	pub fn publish(&mut self, image: &Image) -> Result<Published> {
		let yuv = I420::from_image(image)?;
		let pts = self.clock.micros(image.timestamp);
		let mut stale = self.update_capture_origin();
		// Every rendition is encoded before the catalog is updated, so a new subscriber's first
		// catalog lists all of them.
		let mut frames = Vec::with_capacity(self.tracks.len());
		for track in &mut self.tracks {
			let frame = match track.encode(&yuv, image.timestamp)? {
				Encoded::Frame(frame) => {
					stale |= track.update_rendition(&frame);
					Some(frame)
				}
				Encoded::Dropped => None,
				// Drop the rendition from the catalog until the images grow.
				Encoded::Skipped => {
					stale |= track.rendition.take().is_some();
					None
				}
			};
			frames.push(frame);
		}
		if stale {
			self.publish_catalog()?;
		}
		// The full-size rendition comes first.
		let mut published = Published::Dropped;
		for (index, (track, frame)) in self.tracks.iter_mut().zip(&frames).enumerate() {
			if let Some(frame) = frame
				&& track.write(frame, pts)?
				&& index == 0
			{
				published = Published::Sent;
			}
		}
		Ok(published)
	}

	/// Send an already encoded frame of the full-size rendition (VP8, Annex-B H.264 with SPS/PPS
	/// inline in keyframes, or an AV1 temporal unit with the sequence header in keyframes).
	///
	/// A delta frame is [dropped](Published::Dropped) while no keyframe has opened a group, or
	/// once the group is full.
	pub fn publish_encoded(&mut self, frame: &EncodedFrame) -> Result<Published> {
		let pts = self.clock.micros(frame.timestamp);
		let stale = self.update_capture_origin();
		if self.main_mut().update_rendition(frame) || stale {
			self.publish_catalog()?;
		}
		Ok(match self.main_mut().write(frame, pts)? {
			true => Published::Sent,
			false => Published::Dropped,
		})
	}

	fn main(&self) -> &VideoTrack {
		&self.tracks[0]
	}

	fn main_mut(&mut self) -> &mut VideoTrack {
		&mut self.tracks[0]
	}

	/// Announce the capture time of timestamp 0 from now on, returning whether that changes the
	/// catalog: it does on the first frame, and when the timeline re-anchors.
	fn update_capture_origin(&mut self) -> bool {
		let origin = self.clock.origin_micros();
		let changed = origin != self.section.capture_origin;
		self.section.capture_origin = origin;
		changed
	}

	/// Publish the catalog of the renditions sent so far, once there is one.
	fn publish_catalog(&mut self) -> Result<()> {
		if self.tracks.iter().all(|track| track.rendition.is_none()) {
			return Ok(());
		}
		let catalog = catalog::video(
			self.tracks
				.iter()
				.filter_map(|track| Some((track.name.as_str(), track.rendition.as_ref()?, track.encoder.settings()))),
			&self.section,
		)?;
		self.broadcast.publish_catalog(&catalog)
	}
}

/// One rendition of an [`ImagePublisher`].
struct VideoTrack {
	/// Name of the broadcast, for logs.
	broadcast: String,
	name: String,
	writer: GroupWriter,
	encoder: Encoder,
	/// The height to scale images down to; `None` for the full-size rendition.
	height: Option<u32>,
	/// What the catalog announces; `None` while nothing was sent yet (or since the rendition
	/// was last skipped).
	rendition: Option<Rendition>,
}

/// What a [`VideoTrack`] made of one image.
enum Encoded {
	Frame(EncodedFrame),
	/// The encoder dropped the image.
	Dropped,
	/// The images are no taller than the rendition, so it isn't sent.
	Skipped,
}

impl VideoTrack {
	fn set_bitrate(&mut self, bitrate: u32) {
		if bitrate != self.encoder.settings().bitrate {
			self.encoder.set_bitrate(bitrate);
			// Republished with the next keyframe.
			self.rendition = None;
		}
	}

	/// Encode `yuv`, scaled down to this rendition's height.
	fn encode(&mut self, yuv: &I420, timestamp: Duration) -> Result<Encoded> {
		let frame = match self.height {
			None => self.encoder.encode_yuv(yuv, timestamp)?,
			Some(height) if (height as usize) < yuv.height() => {
				let scaled = yuv.downscale(scaled_width(yuv.width(), yuv.height(), height), height as usize);
				self.encoder.encode_yuv(&scaled, timestamp)?
			}
			Some(_) => return Ok(Encoded::Skipped),
		};
		Ok(frame.map_or(Encoded::Dropped, Encoded::Frame))
	}

	/// Announce what the keyframe `frame` carries from now on, returning whether that changes
	/// the catalog.
	fn update_rendition(&mut self, frame: &EncodedFrame) -> bool {
		if !frame.keyframe {
			return false;
		}
		let rendition = Rendition::of_keyframe(frame);
		if self.rendition.as_ref() == Some(&rendition) {
			return false;
		}
		tracing::info!(
			broadcast = self.broadcast,
			track = self.name,
			codec = %rendition.codec,
			width = rendition.width,
			height = rendition.height,
			"new video rendition"
		);
		self.rendition = Some(rendition);
		true
	}

	/// Write `frame` at stream time `pts`, once the catalog entry is up to date. Returns whether
	/// it was written: a delta frame without an open group to go into is discarded.
	fn write(&mut self, frame: &EncodedFrame, pts: u64) -> Result<bool> {
		if frame.keyframe {
			self.writer.start_group()?;
		} else if self.writer.is_open() && !self.writer.fits(frame.data.len()) {
			// moq-net would abort the group on this write. Close it and drop frames
			// until the next keyframe opens a new one.
			tracing::warn!(
				broadcast = self.broadcast,
				track = self.name,
				"video group is full; raise the bitrate budget or shorten the keyframe interval"
			);
			self.writer.finish_group()?;
		}

		// Nothing decodable can be sent until the first keyframe opens a group.
		if !self.writer.is_open() {
			self.encoder.force_keyframe();
			return Ok(false);
		}

		self.writer.write_container(&hang::container::Frame {
			timestamp: moq_net::Timestamp::from_micros(pts).map_err(|_| Error::Timestamp)?,
			payload: frame.data.clone(),
		})?;
		// Start the next group early, so a long keyframe interval doesn't fill this one.
		if self.writer.nearly_full() {
			self.encoder.force_keyframe();
		}
		Ok(true)
	}
}

/// The even width of a rendition `height` pixels tall of a `width`x`full_height` picture.
fn scaled_width(width: usize, full_height: usize, height: u32) -> usize {
	(width * height as usize / full_height).max(2) & !1
}

#[cfg(test)]
mod tests {
	use bytes::Bytes;
	use hang::catalog::Catalog;

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
		for t in 0..u32::try_from(moq_net::group::MAX_GROUP_FRAMES).unwrap() + 100 {
			publisher
				.publish(&crate::codec::tests::test_pattern(16, 16, t))
				.unwrap();
			keyframes += usize::from(publisher.main().writer.frames() == 1);
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
			codec: Codec::Vp8,
		};
		let limit = moq_net::group::MAX_GROUP_FRAMES as u64;
		for n in 0..limit + 10 {
			let published = publisher.publish_encoded(&frame(n, n == 0)).unwrap();
			assert_eq!(published.is_sent(), n < limit, "frame {n}");
		}
		assert!(!publisher.main().writer.is_open());
		let published = publisher.publish_encoded(&frame(limit + 10, true)).unwrap();
		assert_eq!(published, Published::Sent);
		assert_eq!(publisher.main().writer.frames(), 1);
	}

	/// Smaller renditions are encoded from scaled images into tracks of their own, and listed in
	/// the catalog while the images are taller than them.
	#[tokio::test]
	async fn publishes_smaller_renditions() {
		if !Codec::Vp8.is_available() {
			return;
		}
		let origin = moq_tokio::origin::spawn();
		let settings = ImageBroadcastSettings {
			encoder: EncoderSettings {
				codec: Codec::Vp8,
				..Default::default()
			},
			renditions: vec![
				RenditionSettings {
					height: 24,
					bitrate: 300_000,
				},
				RenditionSettings {
					height: 13,
					bitrate: 100_000,
				},
			],
		};
		let mut publisher = ImagePublisher::new(&origin, "camera", settings).unwrap();
		let names: Vec<_> = publisher.tracks.iter().map(|track| track.name.as_str()).collect();
		assert_eq!(names, ["video", "video_24p", "video_12p"]);

		let broadcast = origin.consume().request_broadcast("camera").await.unwrap();
		let mut catalogs = Catalog::<()>::subscribe(&broadcast).await.unwrap();
		let sizes = |catalog: &Catalog| -> Vec<(String, u32, u32, u64)> {
			catalog
				.video
				.renditions
				.iter()
				.map(|(name, config)| {
					(
						name.clone(),
						config.coded_width.unwrap(),
						config.coded_height.unwrap(),
						config.bitrate.unwrap(),
					)
				})
				.collect()
		};
		assert_eq!(
			publisher
				.publish(&crate::codec::tests::test_pattern(64, 48, 0))
				.unwrap(),
			Published::Sent
		);
		// The first catalog already lists every rendition.
		let catalog = catalogs.next().await.unwrap().unwrap();
		assert_eq!(
			sizes(&catalog),
			[
				("video".to_string(), 64, 48, 2_000_000),
				("video_12p".to_string(), 16, 12, 100_000),
				("video_24p".to_string(), 32, 24, 300_000),
			]
		);

		// Images no taller than a rendition drop it.
		publisher
			.publish(&crate::codec::tests::test_pattern(32, 24, 1))
			.unwrap();
		let catalog = catalogs.next().await.unwrap().unwrap();
		assert_eq!(catalog.video.renditions["video"].coded_height, Some(24));
		let names: Vec<_> = catalog.video.renditions.keys().cloned().collect();
		assert_eq!(names, ["video", "video_12p"]);
	}

	#[tokio::test]
	async fn rejects_bad_renditions() {
		let origin = moq_tokio::origin::spawn();
		let settings = |heights: &[u32]| ImageBroadcastSettings {
			encoder: EncoderSettings::default(),
			renditions: heights
				.iter()
				.map(|&height| RenditionSettings { height, bitrate: 1 })
				.collect(),
		};
		if Codec::available().is_empty() {
			return;
		}
		assert!(matches!(
			ImagePublisher::new(&origin, "a", settings(&[1])),
			Err(Error::InvalidSettings(_))
		));
		assert!(matches!(
			ImagePublisher::new(&origin, "b", settings(&[360, 361])),
			Err(Error::InvalidSettings(_))
		));
	}
}
