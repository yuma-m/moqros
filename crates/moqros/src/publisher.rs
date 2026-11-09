//! Publish images to a MoQ relay as hang (WebCodecs-compatible) VP8, H.264, or AV1 broadcasts,
//! and other messages as hang data broadcasts.

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
pub struct RenditionSettings {
	/// Picture height in pixels; the width keeps the aspect ratio. Rounded down to an even number.
	pub height: u32,
	/// Target bitrate in bits per second.
	pub bitrate: u32,
}

/// How [`Publisher::create_image_broadcast`] encodes a video broadcast: its full-size rendition,
/// and optionally smaller ones (simulcast).
#[derive(Debug, Clone, Default)]
pub struct ImageBroadcastSettings {
	/// The full-size rendition, and the codec and timing of every rendition.
	pub encoder: EncoderSettings,
	/// Smaller renditions, each encoded from the same images scaled down. One at least as tall as
	/// the images is skipped until they get taller.
	pub renditions: Vec<RenditionSettings>,
}

impl From<EncoderSettings> for ImageBroadcastSettings {
	fn from(encoder: EncoderSettings) -> Self {
		Self {
			encoder,
			renditions: Vec::new(),
		}
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
pub struct ImagePublisher {
	// Declared before `broadcast`, so the open groups are finished before the broadcast closes.
	/// The full-size rendition first.
	tracks: Vec<VideoTrack>,
	broadcast: BroadcastWriter,
	clock: StreamClock,
}

/// One rendition of an [`ImagePublisher`].
struct VideoTrack {
	name: String,
	writer: GroupWriter,
	encoder: Encoder,
	/// The height to scale images down to; `None` for the full-size rendition.
	height: Option<u32>,
	/// What the catalog announces; `None` while nothing was sent yet (or since the rendition
	/// was last skipped).
	rendition: Option<Rendition>,
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
			clock: StreamClock::default(),
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		self.broadcast.name()
	}

	/// The encoder settings of the full-size rendition, with the bitrate last set.
	pub fn settings(&self) -> &EncoderSettings {
		self.tracks[0].encoder.settings()
	}

	/// Aim for `bitrate` bit/s in the full-size rendition from the next image on; see
	/// [`Encoder::set_bitrate`]. The catalog announces it from the next keyframe.
	pub fn set_bitrate(&mut self, bitrate: u32) {
		let main = &mut self.tracks[0];
		if bitrate != main.encoder.settings().bitrate {
			main.encoder.set_bitrate(bitrate);
			// Republished with the next keyframe.
			main.rendition = None;
		}
	}

	/// Encode and send one image, in every rendition. Returns whether a full-size frame was written.
	///
	/// Encoding is CPU bound; call this from a blocking-friendly thread when
	/// running inside an async runtime.
	pub fn publish(&mut self, image: &Image) -> Result<bool> {
		let yuv = I420::from_image(image)?;
		let pts = self.clock.micros(image.timestamp);
		// Every rendition is encoded before the catalog is updated, so a new subscriber's first
		// catalog lists all of them.
		let mut frames = Vec::with_capacity(self.tracks.len());
		let mut stale = false;
		for index in 0..self.tracks.len() {
			let frame = match self.tracks[index].height {
				None => self.tracks[index].encoder.encode_yuv(&yuv, image.timestamp)?,
				Some(height) if (height as usize) < yuv.height() => {
					let scaled = yuv.downscale(scaled_width(yuv.width(), yuv.height(), height), height as usize);
					self.tracks[index].encoder.encode_yuv(&scaled, image.timestamp)?
				}
				// No larger than the images: drop the rendition from the catalog until they grow.
				Some(_) => {
					stale |= self.tracks[index].rendition.take().is_some();
					continue;
				}
			};
			if let Some(frame) = frame {
				stale |= self.update_rendition(index, &frame);
				frames.push((index, frame));
			}
		}
		if stale {
			self.publish_catalog()?;
		}
		let sent = frames.first().is_some_and(|(index, _)| *index == 0);
		for (index, frame) in frames {
			self.write(index, &frame, pts)?;
		}
		Ok(sent)
	}

	/// Send an already encoded frame of the full-size rendition (VP8, Annex-B H.264 with SPS/PPS
	/// inline in keyframes, or an AV1 temporal unit with the sequence header in keyframes).
	pub fn publish_encoded(&mut self, frame: &EncodedFrame) -> Result<()> {
		let pts = self.clock.micros(frame.timestamp);
		if self.update_rendition(0, frame) {
			self.publish_catalog()?;
		}
		self.write(0, frame, pts)
	}

	/// Announce what the keyframe `frame` of the track at `index` carries from now on, returning
	/// whether that changes the catalog.
	fn update_rendition(&mut self, index: usize, frame: &EncodedFrame) -> bool {
		if !frame.keyframe {
			return false;
		}
		let rendition = Rendition::from(frame);
		if self.tracks[index].rendition.as_ref() == Some(&rendition) {
			return false;
		}
		tracing::info!(
			broadcast = %self.name(),
			track = self.tracks[index].name,
			codec = %rendition.codec,
			width = rendition.width,
			height = rendition.height,
			"new video rendition"
		);
		self.tracks[index].rendition = Some(rendition);
		true
	}

	/// Write `frame` at stream time `pts` into the track at `index`, whose catalog entry is
	/// up to date.
	fn write(&mut self, index: usize, frame: &EncodedFrame, pts: u64) -> Result<()> {
		let name = self.broadcast.name().to_string();
		let track = &mut self.tracks[index];
		if frame.keyframe {
			track.writer.start_group()?;
		} else if track.writer.is_open() && !track.writer.fits(frame.data.len()) {
			// moq-net would abort the group on this write. Close it and drop frames
			// until the next keyframe opens a new one.
			tracing::warn!(
				broadcast = name,
				track = track.name,
				"video group is full; raise the bitrate budget or shorten the keyframe interval"
			);
			track.writer.finish_group()?;
		}

		// Nothing decodable can be sent until the first keyframe opens a group.
		if !track.writer.is_open() {
			track.encoder.force_keyframe();
			return Ok(());
		}

		track.writer.write_container(&hang::container::Frame {
			timestamp: moq_net::Timestamp::from_micros(pts).map_err(|_| Error::Timestamp)?,
			payload: frame.data.clone(),
		})?;
		// Start the next group early, so a long keyframe interval doesn't fill this one.
		if track.writer.nearly_full() {
			track.encoder.force_keyframe();
		}
		Ok(())
	}

	/// Publish the catalog of the renditions sent so far.
	fn publish_catalog(&mut self) -> Result<()> {
		let catalog = catalog::video(
			self.tracks
				.iter()
				.filter_map(|track| Some((track.name.as_str(), track.rendition.as_ref()?, track.encoder.settings()))),
			&MoqrosSection::default(),
		)?;
		self.broadcast.publish_catalog(&catalog)
	}
}

/// The even width of a rendition `height` pixels tall of a `width`x`full_height` picture.
fn scaled_width(width: usize, full_height: usize, height: u32) -> usize {
	(width * height as usize / full_height).max(2) & !1
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use bytes::Bytes;
	use hang::catalog::{Catalog, VideoCodec};

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
			keyframes += usize::from(publisher.tracks[0].writer.frames() == 1);
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
		assert!(!publisher.tracks[0].writer.is_open());
		publisher.publish_encoded(&frame(limit + 10, true)).unwrap();
		assert_eq!(publisher.tracks[0].writer.frames(), 1);
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
		assert!(
			publisher
				.publish(&crate::codec::tests::test_pattern(64, 48, 0))
				.unwrap()
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
