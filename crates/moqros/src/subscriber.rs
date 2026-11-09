//! Subscribe to moqros broadcasts: hang VP8/H.264/AV1 video decoded back into images, or data tracks.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use hang::moq_net;

use crate::catalog::{self, Kind, MoqrosCatalog};
use crate::codec::{Codec, Decoder};
use crate::data::{DataMode, DataSubscriber};
use crate::track::{CatalogWatch, GroupReader, RouteWatch};
use crate::{ClientConfig, Error, Image, PixelFormat, Result, Session};

/// A connection to a relay used to consume image and data broadcasts.
///
/// Clones share the connection, which stays up while any clone is alive.
#[derive(Clone)]
pub struct Subscriber {
	origin: moq_net::origin::Producer,
	session: Session,
}

impl Subscriber {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config
			.client()?
			.with_subscriber(origin.clone())
			.connect(config.url.clone());
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

	/// Wait for `name` to be announced, then subscribe to its best video rendition
	/// whose codec this build can decode.
	pub async fn subscribe_images(&self, name: &str) -> Result<ImageSubscriber> {
		self.subscribe_images_with(name, &ImageOptions::default()).await
	}

	/// Like [`Subscriber::subscribe_images`], picking the rendition by `options`.
	pub async fn subscribe_images_with(&self, name: &str, options: &ImageOptions) -> Result<ImageSubscriber> {
		let (broadcast, catalog, route) = self.catalog(name).await?;
		images(name, broadcast, catalog, route, options).await
	}

	/// Wait for `name` to be announced, then subscribe to its data track.
	pub async fn subscribe_data(&self, name: &str) -> Result<DataSubscriber> {
		let (broadcast, catalog, route) = self.catalog(name).await?;
		data(name, broadcast, catalog, route).await
	}

	/// Wait for `name` to be announced, then subscribe to whatever it carries: its video
	/// if this build can decode it, otherwise its data track.
	pub async fn subscribe(&self, name: &str) -> Result<Subscription> {
		self.subscribe_with(name, &ImageOptions::default()).await
	}

	/// Like [`Subscriber::subscribe`], picking a video rendition by `options`.
	pub async fn subscribe_with(&self, name: &str, options: &ImageOptions) -> Result<Subscription> {
		let (broadcast, catalog, route) = self.catalog(name).await?;
		match Kind::of(catalog.catalog()) {
			Kind::Images => Ok(Subscription::Images(
				images(name, broadcast, catalog, route, options).await?,
			)),
			Kind::Data => Ok(Subscription::Data(data(name, broadcast, catalog, route).await?)),
			Kind::None => Err(Error::NoTracks(name.to_string())),
		}
	}

	/// Follow the broadcasts announced under `prefix` (whole path segments: `robots` covers
	/// `robots/a/camera` but not `robots2`; an empty prefix covers every broadcast), including the
	/// ones announced already.
	pub fn announced(&self, prefix: &str) -> Announcements {
		Announcements {
			announced: self.origin.consume().announced(),
			prefix: prefix.trim_matches('/').to_string(),
		}
	}

	/// Wait for `name` to be announced and read its first catalog. Also returns a watch on
	/// the announcement, which the subscription ends with.
	async fn catalog(&self, name: &str) -> Result<(moq_net::broadcast::Consumer, CatalogWatch, RouteWatch)> {
		let consumer = self.origin.consume();
		let mut announced = consumer.announced();
		loop {
			let update = announced
				.next()
				.await
				.ok_or_else(|| Error::NotAnnounced(name.to_string()))?;
			if update.kind.is_active() && update.prefix.as_str() == name {
				break;
			}
		}

		let broadcast = consumer.request_broadcast(name).await?;
		let mut updates = MoqrosCatalog::subscribe(&broadcast).await?;
		let first = updates.next().await?.ok_or_else(|| Error::NoTracks(name.to_string()))?;
		let route = RouteWatch::new(announced, name);
		Ok((broadcast, CatalogWatch::new(updates, first), route))
	}
}

async fn images(
	name: &str,
	broadcast: moq_net::broadcast::Consumer,
	catalog: CatalogWatch,
	route: RouteWatch,
	options: &ImageOptions,
) -> Result<ImageSubscriber> {
	let (track, config, codec) = catalog::select_video(catalog.catalog(), name, options.max_height)?;
	tracing::info!(
		broadcast = name,
		// The track name comes from the remote catalog: record it as a str (quoted
		// and escaped), not with `%`, so control characters can't forge log lines.
		track,
		codec = %config.codec,
		width = ?config.coded_width,
		height = ?config.coded_height,
		"subscribing"
	);

	let video = GroupReader::subscribe(&broadcast, track, true, route).await?;
	Ok(ImageSubscriber {
		_broadcast: broadcast,
		video,
		codec,
		decoder: Arc::new(Mutex::new(Decoder::new(codec)?)),
	})
}

async fn data(
	name: &str,
	broadcast: moq_net::broadcast::Consumer,
	catalog: CatalogWatch,
	route: RouteWatch,
) -> Result<DataSubscriber> {
	let (track, settings) = catalog::find_data(catalog.catalog()).ok_or_else(|| Error::NoData(name.to_string()))?;
	tracing::info!(
		broadcast = name,
		track,
		format = %settings.format,
		mode = %settings.mode,
		ros_type = settings.ros_type.as_deref().unwrap_or_default(),
		"subscribing"
	);
	let reader = GroupReader::subscribe(&broadcast, &track, settings.mode == DataMode::Snapshot, route).await?;
	Ok(DataSubscriber::new(broadcast, reader, settings, catalog))
}

/// Broadcasts announced under a prefix, from [`Subscriber::announced`].
pub struct Announcements {
	announced: moq_net::announce::Consumer,
	prefix: String,
}

impl Announcements {
	/// The name of the next broadcast announced (or announced again) under the prefix, or `None`
	/// once the connection is gone for good. Names come from whoever announces them.
	pub async fn next(&mut self) -> Option<String> {
		while let Some(update) = self.announced.next().await {
			let name = update.prefix.as_str();
			if update.kind.is_active() && is_under(name, &self.prefix) {
				return Some(name.to_string());
			}
		}
		None
	}
}

/// Whether the broadcast `name` is under `prefix`, by whole path segments.
fn is_under(name: &str, prefix: &str) -> bool {
	prefix.is_empty() || name == prefix || name.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

/// Which video rendition [`Subscriber::subscribe_images_with`] picks, among those whose codec
/// this build can decode. The rendition is picked once, from the catalog at subscription time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageOptions {
	/// The tallest picture wanted: the largest rendition at most this tall, or the smallest if
	/// none is. `None` picks the largest.
	pub max_height: Option<u32>,
}

impl ImageOptions {
	/// Set [`ImageOptions::max_height`].
	pub fn with_max_height(mut self, max_height: u32) -> Self {
		self.max_height = Some(max_height);
		self
	}
}

/// What [`Subscriber::subscribe`] found in a broadcast.
#[non_exhaustive]
pub enum Subscription {
	/// The broadcast carries decodable video.
	Images(ImageSubscriber),
	/// The broadcast carries a data track.
	Data(DataSubscriber),
}

/// One encoded video frame received from a broadcast.
#[derive(Debug, Clone)]
pub struct VideoFrame {
	/// Presentation time relative to the start of the broadcast.
	pub timestamp: Duration,
	/// Whether the frame decodes on its own.
	pub keyframe: bool,
	/// Codec payload (VP8 frame, Annex-B H.264 access unit, or AV1 temporal unit).
	pub data: Bytes,
}

/// Reads frames of one video track, always jumping to the newest group so a slow
/// reader falls back to the live edge instead of accumulating latency.
pub struct ImageSubscriber {
	_broadcast: moq_net::broadcast::Consumer,
	video: GroupReader,
	codec: Codec,
	/// Shared with the blocking task decoding the current frame, which keeps running (and then
	/// lets go) when [`ImageSubscriber::next_image`] is cancelled, so the decoder stays in step
	/// with the frames it was fed.
	decoder: Arc<Mutex<Decoder>>,
}

impl ImageSubscriber {
	/// Codec of the subscribed track.
	pub fn codec(&self) -> Codec {
		self.codec
	}

	/// Accept decoded pictures of up to `max_pixels`; see [`Decoder::set_max_pixels`].
	pub fn set_max_pixels(&mut self, max_pixels: u64) {
		lock(&self.decoder).set_max_pixels(max_pixels);
	}

	/// Decode into `format` images; see [`Decoder::set_output_format`].
	pub fn set_output_format(&mut self, format: PixelFormat) -> Result<()> {
		lock(&self.decoder).set_output_format(format)
	}

	/// Next encoded frame, or `None` once the broadcast ends.
	pub async fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
		let Some(frame) = self.video.next_frame().await? else {
			return Ok(None);
		};
		let frame = hang::container::Frame::decode(frame.payload)?;
		Ok(Some(VideoFrame {
			timestamp: Duration::from_micros(u64::try_from(frame.timestamp.as_micros()).unwrap_or(u64::MAX)),
			keyframe: self.codec.is_keyframe(&frame.payload),
			data: frame.payload,
		}))
	}

	/// Next decoded image (`rgb8` unless [set otherwise](ImageSubscriber::set_output_format)), or
	/// `None` once the broadcast ends.
	///
	/// Decoding and color conversion run on tokio's blocking pool, so a large picture doesn't
	/// stall the other tasks of the runtime.
	///
	/// Fails with [`Error::TooLarge`] for a picture above the decoder's
	/// [`max_pixels`](Decoder::max_pixels).
	pub async fn next_image(&mut self) -> Result<Option<Image>> {
		while let Some(frame) = self.next_frame().await? {
			let decoder = self.decoder.clone();
			let decoded = tokio::task::spawn_blocking(move || lock(&decoder).decode(&frame.data, frame.timestamp))
				.await
				.map_err(|err| Error::Codec(format!("decoder task failed: {err}")))??;
			if let Some(image) = decoded {
				return Ok(Some(image));
			}
		}
		Ok(None)
	}
}

/// Lock `decoder`, recovering it from a panicked decode: the codec state may be off, but the
/// next keyframe resets it.
fn lock(decoder: &Mutex<Decoder>) -> std::sync::MutexGuard<'_, Decoder> {
	decoder.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, Ordering};

	use super::*;
	use crate::codec::tests::test_pattern;
	use crate::{EncoderSettings, ImagePublisher};

	/// Subscribe to `name` on `origin` the way [`Subscriber::subscribe_images`] does.
	async fn subscribe_images(origin: &moq_net::origin::Producer, name: &str) -> ImageSubscriber {
		let consumer = origin.consume();
		let announced = consumer.announced();
		let broadcast = consumer.request_broadcast(name).await.unwrap();
		let mut updates = MoqrosCatalog::subscribe(&broadcast).await.unwrap();
		let first = updates.next().await.unwrap().unwrap();
		images(
			name,
			broadcast,
			CatalogWatch::new(updates, first),
			RouteWatch::new(announced, name),
			&ImageOptions::default(),
		)
		.await
		.unwrap()
	}

	#[test]
	fn matches_prefixes_by_segment() {
		assert!(is_under("robots/a/camera", "robots"));
		assert!(is_under("robots/a/camera", "robots/a"));
		assert!(is_under("robots", "robots"));
		assert!(is_under("anything", ""));
		assert!(!is_under("robots2/camera", "robots"));
		assert!(!is_under("robot", "robots"));
	}

	/// Broadcasts already announced, and later ones, come back when under the prefix.
	#[tokio::test]
	async fn follows_announcements_under_a_prefix() {
		let origin = moq_tokio::origin::spawn();
		let first = origin.create_broadcast("robots/a/camera").unwrap();
		first.announce(Default::default()).unwrap();
		// What `Subscriber::announced("robots/")` returns.
		let mut announced = Announcements {
			announced: origin.consume().announced(),
			prefix: "robots".into(),
		};
		let other = origin.create_broadcast("robots2/camera").unwrap();
		other.announce(Default::default()).unwrap();
		let second = origin.create_broadcast("robots/b/camera").unwrap();
		second.announce(Default::default()).unwrap();
		assert_eq!(announced.next().await.as_deref(), Some("robots/a/camera"));
		assert_eq!(announced.next().await.as_deref(), Some("robots/b/camera"));
	}

	/// Other tasks of a single-threaded runtime run while a picture decodes.
	#[tokio::test(flavor = "current_thread")]
	async fn decoding_yields_to_the_runtime() {
		if Codec::available().is_empty() {
			return;
		}
		let origin = moq_tokio::origin::spawn();
		let mut publisher = ImagePublisher::new(&origin, "camera", EncoderSettings::default()).unwrap();
		// The catalog only exists once the first keyframe is out.
		assert!(publisher.publish(&test_pattern(64, 48, 0)).unwrap());
		let mut images = subscribe_images(&origin, "camera").await;
		assert!(publisher.publish(&test_pattern(64, 48, 1)).unwrap());

		let ran = Arc::new(AtomicBool::new(false));
		tokio::spawn({
			let ran = ran.clone();
			async move { ran.store(true, Ordering::Relaxed) }
		});
		let image = images.next_image().await.unwrap().expect("a decoded image");
		assert_eq!((image.width, image.height), (64, 48));
		assert!(ran.load(Ordering::Relaxed), "the other task never ran while decoding");
	}
}
