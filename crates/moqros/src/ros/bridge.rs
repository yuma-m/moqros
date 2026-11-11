//! The handle every bridge returns.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::{Error, Result};

/// Message counts of a bridge, live through [`Bridge::stats`] and final in its [`BridgeEnd`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BridgeStats {
	/// Messages received from the source: the ROS topic, or the broadcast.
	pub received: u64,
	/// Messages sent on: published to the broadcast, or on the ROS topic.
	pub sent: u64,
	/// Messages skipped, e.g. images replaced while the encoder was busy, or frames that
	/// didn't decode.
	pub dropped: u64,
	/// When the last message was received.
	pub last_received: Option<Instant>,
}

/// A running bridge between a ROS topic and a MoQ broadcast.
///
/// Await it for its [`BridgeEnd`] once it stops. Dropping the handle leaves the bridge running.
pub struct Bridge {
	task: JoinHandle<Result<()>>,
	stats: watch::Receiver<BridgeStats>,
	teardown: Teardown,
	bitrate: Option<BitrateControl>,
}

impl Bridge {
	/// Spawn `run` with the counters it updates.
	pub(crate) fn spawn<F>(run: impl FnOnce(StatsSender) -> F) -> Self
	where
		F: Future<Output = Result<()>> + Send + 'static,
	{
		let (sender, stats) = watch::channel(BridgeStats::default());
		Self {
			task: tokio::spawn(run(StatsSender(sender))),
			stats,
			teardown: Teardown::default(),
			bitrate: None,
		}
	}

	pub(crate) fn with_bitrate_control(mut self, bitrate: BitrateControl) -> Self {
		self.bitrate = Some(bitrate);
		self
	}

	/// What changes the bitrate of a video bridge while it runs; `None` for other bridges.
	pub fn bitrate_control(&self) -> Option<BitrateControl> {
		self.bitrate.clone()
	}

	pub(crate) fn with_teardown(mut self, teardown: Teardown) -> Self {
		self.teardown = teardown;
		self
	}

	/// The counts so far.
	pub fn stats(&self) -> BridgeStats {
		*self.stats.borrow()
	}

	/// A receiver notified whenever the counts change.
	pub fn watch_stats(&self) -> watch::Receiver<BridgeStats> {
		self.stats.clone()
	}

	/// Whether the bridge has stopped.
	pub fn is_finished(&self) -> bool {
		self.task.is_finished()
	}

	/// Stop the bridge. Awaiting it then ends with [`Error::BridgeAborted`].
	pub fn abort(&self) {
		self.task.abort();
	}
}

impl Future for Bridge {
	type Output = BridgeEnd;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let result = match Pin::new(&mut self.task).poll(cx) {
			Poll::Pending => return Poll::Pending,
			Poll::Ready(Ok(result)) => result,
			Poll::Ready(Err(err)) if err.is_cancelled() => Err(Error::BridgeAborted),
			Poll::Ready(Err(err)) => Err(Error::BridgePanicked(err.to_string())),
		};
		let stats = *self.stats.borrow();
		Poll::Ready(BridgeEnd {
			result,
			stats,
			teardown: std::mem::take(&mut self.teardown),
		})
	}
}

/// How a [`Bridge`] stopped, and what it leaves behind.
#[non_exhaustive]
pub struct BridgeEnd {
	/// Why the bridge stopped: `Ok` when its topic or broadcast ended, [`Error::BridgeAborted`]
	/// after [`Bridge::abort`], [`Error::BridgePanicked`] if its task panicked.
	pub result: Result<()>,
	/// The final counts, however the bridge stopped.
	pub stats: BridgeStats,
	/// Destroys the ROS entities the bridge created on the node, which otherwise outlive it.
	pub teardown: Teardown,
}

/// Changes the target bitrate of a running video [`Bridge`] (see
/// [`ImagePublisher::set_bitrate`](crate::ImagePublisher::set_bitrate)).
#[derive(Clone, Debug)]
pub struct BitrateControl(watch::Sender<u32>);

impl BitrateControl {
	/// A control starting at `bitrate`, and the receiving end the bridge polls.
	pub(crate) fn new(bitrate: u32) -> (Self, watch::Receiver<u32>) {
		let (sender, receiver) = watch::channel(bitrate);
		(Self(sender), receiver)
	}

	/// Aim for `bitrate` bit/s from the next image on.
	pub fn set(&self, bitrate: u32) {
		self.0.send_replace(bitrate);
	}

	/// The bitrate last set.
	pub fn get(&self) -> u32 {
		*self.0.borrow()
	}
}

/// Destroys the ROS entities of a stopped [`Bridge`], e.g. its publisher, so a new bridge on
/// the same topic can use another message type.
///
/// Does nothing for bridges whose entities go away on their own.
#[derive(Default)]
pub struct Teardown(Option<TeardownFn>);

type TeardownFn = Box<dyn FnOnce(&mut r2r::Node) + Send>;

impl Teardown {
	pub(crate) fn new(teardown: impl FnOnce(&mut r2r::Node) + Send + 'static) -> Self {
		Self(Some(Box::new(teardown)))
	}

	/// Destroy the entities on `node`, the node the bridge was spawned on.
	pub fn run(self, node: &mut r2r::Node) {
		if let Some(teardown) = self.0 {
			teardown(node);
		}
	}
}

/// The side of a [`Bridge`]'s counters its tasks update.
#[derive(Clone)]
pub(crate) struct StatsSender(watch::Sender<BridgeStats>);

impl StatsSender {
	pub(crate) fn received(&self) {
		self.0.send_modify(|stats| {
			stats.received += 1;
			stats.last_received = Some(Instant::now());
		});
	}

	pub(crate) fn sent(&self) {
		self.0.send_modify(|stats| stats.sent += 1);
	}

	pub(crate) fn dropped(&self) {
		self.0.send_modify(|stats| stats.dropped += 1);
	}

	pub(crate) fn snapshot(&self) -> BridgeStats {
		*self.0.borrow()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn reports_live_and_final_counts() {
		let (go, wait) = tokio::sync::oneshot::channel::<()>();
		let bridge = Bridge::spawn(|stats| async move {
			stats.received();
			stats.sent();
			stats.received();
			stats.dropped();
			wait.await.unwrap();
			Ok(())
		});
		let mut watch = bridge.watch_stats();
		watch.wait_for(|stats| stats.dropped == 1).await.unwrap();
		let live = bridge.stats();
		assert_eq!((live.received, live.sent, live.dropped), (2, 1, 1));
		assert!(live.last_received.is_some());

		go.send(()).unwrap();
		let end = bridge.await;
		assert!(end.result.is_ok());
		assert_eq!(end.stats, live);
	}

	#[tokio::test]
	async fn a_failed_bridge_keeps_its_counts() {
		let bridge = Bridge::spawn(|stats| async move {
			stats.received();
			Err(Error::NoVideo("camera".into()))
		});
		let end = bridge.await;
		assert!(matches!(end.result, Err(Error::NoVideo(_))));
		assert_eq!(end.stats.received, 1);
	}

	#[tokio::test]
	async fn abort_cancels_the_task() {
		let bridge = Bridge::spawn(|_| std::future::pending());
		bridge.abort();
		assert!(matches!(bridge.await.result, Err(Error::BridgeAborted)));
	}

	#[tokio::test]
	async fn a_panic_ends_the_bridge_with_an_error() {
		let bridge = Bridge::spawn(|_| async { panic!("boom") });
		assert!(matches!(bridge.await.result, Err(Error::BridgePanicked(_))));
	}
}
