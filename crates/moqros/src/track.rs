//! MoQ plumbing shared by the image and data broadcasts: announcing a broadcast with its catalog,
//! writing groups within moq-net's limits, and reading a track group after group.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::Catalog;
use hang::moq_net;
use serde::Serialize;

use crate::catalog::{MoqrosCatalog, MoqrosSection};
use crate::error::Wrap;
use crate::{Error, Result};

/// One announced broadcast and its catalog track. Closes the broadcast when dropped.
pub(crate) struct BroadcastWriter {
	name: String,
	broadcast: moq_net::broadcast::Producer,
	catalog: moq_net::track::Producer,
}

impl BroadcastWriter {
	/// Create and announce the broadcast `name`, with an empty catalog track.
	pub(crate) fn create(origin: &moq_net::origin::Producer, name: &str) -> Result<Self> {
		let broadcast = origin.create_broadcast(name).moq()?;
		broadcast.announce(Default::default()).moq()?;
		let catalog = broadcast
			.create_track(Catalog::DEFAULT_NAME, Catalog::default_track_info())
			.moq()?;
		Ok(Self {
			name: name.to_string(),
			broadcast,
			catalog,
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub(crate) fn name(&self) -> &str {
		&self.name
	}

	pub(crate) fn create_track(&self, name: &str, info: moq_net::track::Info) -> Result<moq_net::track::Producer> {
		self.broadcast.create_track(name, info).moq()
	}

	/// Replace the catalog subscribers see.
	pub(crate) fn publish_catalog<E: Serialize>(&mut self, catalog: &Catalog<E>) -> Result<()> {
		let mut group = self.catalog.append_group().moq()?;
		group
			.write_frame(moq_net::Timestamp::now(), catalog.to_json().hang()?)
			.moq()?;
		group.finish().moq()?;
		Ok(())
	}
}

impl Drop for BroadcastWriter {
	fn drop(&mut self) {
		self.broadcast.close();
	}
}

/// Writes frames into the groups of one track, keeping each group within moq-net's limits.
/// Finishes the open group when dropped.
pub(crate) struct GroupWriter {
	track: moq_net::track::Producer,
	group: Option<moq_net::group::Producer>,
	/// What [`Self::group`] has used of moq-net's per-group limits.
	budget: GroupBudget,
}

impl GroupWriter {
	pub(crate) fn new(track: moq_net::track::Producer) -> Self {
		Self {
			track,
			group: None,
			budget: GroupBudget::default(),
		}
	}

	/// Finish the open group, if any, and start a new one.
	pub(crate) fn start_group(&mut self) -> Result<()> {
		self.finish_group()?;
		self.group = Some(self.track.append_group().moq()?);
		self.budget = GroupBudget::default();
		Ok(())
	}

	/// Finish the open group, if any.
	pub(crate) fn finish_group(&mut self) -> Result<()> {
		if let Some(group) = self.group.take() {
			group.finish().moq()?;
		}
		Ok(())
	}

	pub(crate) fn is_open(&self) -> bool {
		self.group.is_some()
	}

	/// Whether a frame with `payload` bytes still fits into the open group.
	pub(crate) fn fits(&self, payload: usize) -> bool {
		self.budget.fits(payload)
	}

	/// Whether the open group has used three quarters of either limit.
	pub(crate) fn nearly_full(&self) -> bool {
		self.budget.nearly_full()
	}

	/// Write one frame into the open group, starting one if there is none.
	pub(crate) fn write_frame(&mut self, timestamp: moq_net::Timestamp, payload: Bytes) -> Result<()> {
		let size = payload.len();
		self.group()?.write_frame(timestamp, payload).moq()?;
		self.budget.add(size);
		Ok(())
	}

	/// Write one frame in the hang container into the open group, starting one if there is none.
	pub(crate) fn write_container(&mut self, frame: &hang::container::Frame) -> Result<()> {
		frame.write_to(self.group()?).hang()?;
		self.budget.add(frame.payload.len());
		Ok(())
	}

	fn group(&mut self) -> Result<&mut moq_net::group::Producer> {
		if self.group.is_none() {
			self.start_group()?;
		}
		Ok(self.group.as_mut().expect("group started above"))
	}

	/// Frames written to the open group.
	#[cfg(test)]
	pub(crate) fn frames(&self) -> usize {
		self.budget.frames
	}
}

impl Drop for GroupWriter {
	fn drop(&mut self) {
		let _ = self.finish_group();
	}
}

/// How much of moq-net's per-group limits a group has used, so writers can start a new
/// group before a write would abort the current one.
#[derive(Debug, Default)]
struct GroupBudget {
	frames: usize,
	bytes: u64,
}

impl GroupBudget {
	/// Bytes a frame may add on top of its payload (hang's timestamp varint).
	const OVERHEAD: u64 = 8;

	fn fits(&self, payload: usize) -> bool {
		self.frames < moq_net::group::MAX_GROUP_FRAMES
			&& self.bytes + payload as u64 + Self::OVERHEAD <= moq_net::group::MAX_CACHE_BYTES
	}

	fn add(&mut self, payload: usize) {
		self.frames += 1;
		self.bytes += payload as u64 + Self::OVERHEAD;
	}

	fn nearly_full(&self) -> bool {
		self.frames >= moq_net::group::MAX_GROUP_FRAMES / 4 * 3 || self.bytes >= moq_net::group::MAX_CACHE_BYTES / 4 * 3
	}
}

/// The catalog of a subscribed broadcast, following its updates.
pub(crate) struct CatalogWatch {
	/// `None` once the catalog track ended or failed: the last catalog stays.
	updates: Option<Box<hang::catalog::Consumer<serde_json::Map<String, serde_json::Value>>>>,
	catalog: Box<MoqrosCatalog>,
	section: MoqrosSection,
}

impl CatalogWatch {
	/// Follow `updates`, whose first catalog was `first`.
	pub(crate) fn new(
		updates: hang::catalog::Consumer<serde_json::Map<String, serde_json::Value>>,
		first: MoqrosCatalog,
	) -> Self {
		Self {
			updates: Some(Box::new(updates)),
			section: MoqrosSection::read(&first),
			catalog: Box::new(first),
		}
	}

	/// The current catalog.
	pub(crate) fn catalog(&self) -> &MoqrosCatalog {
		&self.catalog
	}

	/// The `moqros` section of the current catalog.
	pub(crate) fn section(&self) -> &MoqrosSection {
		&self.section
	}

	/// Whether the current catalog maps media timestamps to capture times.
	pub(crate) fn has_capture_times(&self) -> bool {
		self.section.capture_origin.is_some() || self.catalog.clock.is_some()
	}

	/// The capture time of a frame with media timestamp `pts`, by the current catalog.
	pub(crate) fn capture_time(&self, pts: Duration) -> Option<Duration> {
		self.section.capture_time(&self.catalog, pts)
	}

	/// Resolves once the next catalog has replaced the current one. Never resolves after the
	/// catalog track ended; the media tracks end with it anyway.
	pub(crate) async fn changed(&mut self) {
		let Some(updates) = self.updates.as_mut() else {
			return std::future::pending().await;
		};
		match updates.next().await {
			Ok(Some(catalog)) => {
				self.section = MoqrosSection::read(&catalog);
				*self.catalog = catalog;
			}
			Ok(None) => self.updates = None,
			Err(err) => {
				tracing::debug!(%err, "catalog update failed, keeping the last catalog");
				self.updates = None;
			}
		}
	}
}

/// Watches the announcement of one subscribed broadcast.
///
/// A relay doesn't end a subscription when the broadcast's publisher retracts it: the track
/// just goes quiet. Nor does it move the subscription when another publisher of the same name
/// takes over. So a subscription ends on any change to the broadcast's route, and the caller
/// resubscribes to whatever serves the name next.
///
/// That includes [`Updated`](moq_net::announce::Kind::Updated), though moq-net delivers a
/// re-priced route in place: a takeover by another publisher also arrives as `Updated` (the path
/// stays covered throughout), and it can't be told apart from a mere re-pricing. Route changes
/// are rare, so an occasional needless resubscribe is the cheaper mistake.
pub(crate) struct RouteWatch {
	announced: moq_net::announce::Consumer,
	name: String,
}

impl RouteWatch {
	/// Watch `name` on `announced`, which has already delivered its current announcement.
	pub(crate) fn new(announced: moq_net::announce::Consumer, name: &str) -> Self {
		Self {
			announced,
			name: name.to_string(),
		}
	}

	/// Resolves on the next update to this broadcast's announcement: retracted, re-announced,
	/// or re-routed. Never resolves once the origin closes; the track ends then anyway.
	async fn changed(&mut self) -> moq_net::announce::Kind {
		while let Some(update) = self.announced.next().await {
			if update.prefix.as_str() == self.name {
				return update.kind;
			}
		}
		std::future::pending().await
	}
}

/// Reads the frames of one track, group after group, until the track ends or its broadcast's
/// route changes.
pub(crate) struct GroupReader {
	frames: Frames,
	route: RouteWatch,
	ended: bool,
}

impl GroupReader {
	/// Subscribe to `track`. With `skip`, the reader jumps to a newer group as soon as one
	/// arrives, dropping the rest of the current one.
	async fn subscribe(
		broadcast: &moq_net::broadcast::Consumer,
		track: &str,
		skip: bool,
		route: RouteWatch,
	) -> Result<Self> {
		let track = broadcast
			.track(track)
			.moq()?
			.subscribe(moq_net::track::Subscription::default())
			.await
			.moq()?
			.ordered();
		Ok(Self {
			frames: Frames {
				track,
				group: None,
				skip,
			},
			route,
			ended: false,
		})
	}

	/// Next frame, or `None` once the track ends or the broadcast's route changes.
	async fn next_frame(&mut self) -> Result<Option<moq_net::frame::Frame>> {
		if self.ended {
			return Ok(None);
		}
		tokio::select! {
			// Frames first: whatever arrived before the route changed (e.g. a last stop command
			// sent just before the retraction) is still delivered.
			biased;
			frame = self.frames.next() => {
				let frame = frame?;
				self.ended = frame.is_none();
				Ok(frame)
			}
			kind = self.route.changed() => {
				tracing::info!(broadcast = self.route.name, ?kind, "broadcast announcement changed, ending the subscription");
				self.ended = true;
				Ok(None)
			}
		}
	}
}

/// A subscribed broadcast whose first catalog has arrived, before one of its tracks is read.
pub(crate) struct OpenBroadcast {
	broadcast: moq_net::broadcast::Consumer,
	catalog: CatalogWatch,
	route: RouteWatch,
}

impl OpenBroadcast {
	/// Wait for `name` to be announced on `origin` and read its first catalog. Also watches the
	/// announcement, which the subscription ends with.
	pub(crate) async fn open(origin: &moq_net::origin::Producer, name: &str) -> Result<Self> {
		let consumer = origin.consume();
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

		let broadcast = consumer.request_broadcast(name).await.moq()?;
		let mut updates = MoqrosCatalog::subscribe(&broadcast).await.hang()?;
		let first = updates
			.next()
			.await
			.hang()?
			.ok_or_else(|| Error::NoTracks(name.to_string()))?;
		Ok(Self {
			broadcast,
			catalog: CatalogWatch::new(updates, first),
			route: RouteWatch::new(announced, name),
		})
	}

	/// The first catalog.
	pub(crate) fn catalog(&self) -> &MoqrosCatalog {
		self.catalog.catalog()
	}

	/// Subscribe to `track`; `skip` as for [`GroupReader::subscribe`].
	pub(crate) async fn subscribe(self, track: &str, skip: bool) -> Result<TrackReader> {
		Ok(TrackReader {
			frames: GroupReader::subscribe(&self.broadcast, track, skip, self.route).await?,
			catalog: self.catalog,
			_broadcast: self.broadcast,
		})
	}
}

/// Reads one track of a broadcast while following the broadcast's catalog.
pub(crate) struct TrackReader {
	_broadcast: moq_net::broadcast::Consumer,
	frames: GroupReader,
	catalog: CatalogWatch,
}

impl TrackReader {
	/// The current catalog of the broadcast.
	pub(crate) fn catalog(&self) -> &CatalogWatch {
		&self.catalog
	}

	/// Next frame, or `None` once the track ends or the broadcast's route changes.
	///
	/// A catalog update that arrived first is applied first, so a new capture origin comes
	/// before the frames stamped against it.
	pub(crate) async fn next_frame(&mut self) -> Result<Option<moq_net::frame::Frame>> {
		loop {
			tokio::select! {
				biased;
				() = self.catalog.changed() => {}
				frame = self.frames.next_frame() => return frame,
			}
		}
	}
}

/// The frames of one track, group after group.
struct Frames {
	track: moq_net::track::Ordered,
	group: Option<moq_net::group::Consumer>,
	/// Jump to a newer group as soon as one arrives, dropping the rest of the current one.
	skip: bool,
}

impl Frames {
	/// Next frame, or `None` once the track ends.
	async fn next(&mut self) -> Result<Option<moq_net::frame::Frame>> {
		loop {
			let Some(group) = self.group.as_mut() else {
				match self.track.next_group().await.moq()? {
					Some(group) => self.group = Some(group),
					None => return Ok(None),
				}
				continue;
			};

			let frame = if self.skip {
				tokio::select! {
					biased;
					// A newer group starts with a keyframe (or a complete message), so the rest of
					// this one can go.
					next = self.track.next_group() => {
						match next.moq()? {
							Some(group) => self.group = Some(group),
							None => return Ok(None),
						}
						continue;
					}
					frame = group.read_frame() => frame,
				}
			} else {
				group.read_frame().await
			};
			match frame {
				Ok(Some(frame)) => return Ok(Some(frame)),
				Ok(None) => self.group = None,
				Err(err) => {
					tracing::debug!(%err, "group ended early, waiting for the next one");
					self.group = None;
				}
			}
		}
	}
}
