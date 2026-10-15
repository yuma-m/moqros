//! MoQ plumbing shared by the image and data broadcasts: announcing a broadcast with its catalog,
//! writing groups within moq-net's limits, and reading a track group after group.

use bytes::Bytes;
use hang::catalog::Catalog;
use hang::moq_net;

use crate::Result;

/// One announced broadcast and its catalog track. Closes the broadcast when dropped.
pub(crate) struct BroadcastWriter {
	name: String,
	broadcast: moq_net::broadcast::Producer,
	catalog: moq_net::track::Producer,
}

impl BroadcastWriter {
	/// Create and announce the broadcast `name`, with an empty catalog track.
	pub(crate) fn create(origin: &moq_net::origin::Producer, name: &str) -> Result<Self> {
		let broadcast = origin.create_broadcast(name)?;
		broadcast.announce(Default::default())?;
		let catalog = broadcast.create_track(Catalog::DEFAULT_NAME, Catalog::default_track_info())?;
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
		Ok(self.broadcast.create_track(name, info)?)
	}

	/// Replace the catalog subscribers see.
	pub(crate) fn publish_catalog(&mut self, catalog: &Catalog) -> Result<()> {
		let mut group = self.catalog.append_group()?;
		group.write_frame(moq_net::Timestamp::now(), catalog.to_json()?)?;
		group.finish()?;
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
		self.group = Some(self.track.append_group()?);
		self.budget = GroupBudget::default();
		Ok(())
	}

	/// Finish the open group, if any.
	pub(crate) fn finish_group(&mut self) -> Result<()> {
		if let Some(group) = self.group.take() {
			group.finish()?;
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
		self.group()?.write_frame(timestamp, payload)?;
		self.budget.add(size);
		Ok(())
	}

	/// Write one frame in the hang container into the open group, starting one if there is none.
	pub(crate) fn write_container(&mut self, frame: &hang::container::Frame) -> Result<()> {
		frame.write_to(self.group()?)?;
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
	pub(crate) async fn subscribe(
		broadcast: &moq_net::broadcast::Consumer,
		track: &str,
		skip: bool,
		route: RouteWatch,
	) -> Result<Self> {
		let track = broadcast
			.track(track)?
			.subscribe(moq_net::track::Subscription::default())
			.await?
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
	pub(crate) async fn next_frame(&mut self) -> Result<Option<moq_net::frame::Frame>> {
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
				match self.track.next_group().await? {
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
						match next? {
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
