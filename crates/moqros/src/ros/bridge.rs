//! The handle every bridge returns.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::sync::watch;
use tokio::task::{JoinError, JoinHandle};

use crate::Result;

/// Message counts of a bridge, live through [`Bridge::stats`] and final once it stops.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
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
/// Await it for the final [`BridgeStats`] once it stops; the outer `Result` is the task's
/// (an `Err` means it panicked or was aborted), the inner one the bridge's. Dropping the handle
/// leaves the bridge running.
///
/// The ROS entities a bridge creates on the node outlive it unless they are destroyed with its
/// [`Teardown`] (see [`Bridge::take_teardown`]).
pub struct Bridge {
	task: JoinHandle<Result<BridgeStats>>,
	stats: watch::Receiver<BridgeStats>,
	teardown: Teardown,
}

impl Bridge {
	/// Spawn `run` with the counters it updates.
	pub(crate) fn spawn<F>(run: impl FnOnce(StatsSender) -> F) -> Self
	where
		F: Future<Output = Result<()>> + Send + 'static,
	{
		let (sender, stats) = watch::channel(BridgeStats::default());
		let counters = StatsSender(sender);
		let run = run(counters.clone());
		let task = tokio::spawn(async move {
			run.await?;
			Ok(counters.snapshot())
		});
		Self {
			task,
			stats,
			teardown: Teardown::default(),
		}
	}

	pub(crate) fn with_teardown(mut self, teardown: Teardown) -> Self {
		self.teardown = teardown;
		self
	}

	/// What destroys the ROS entities of this bridge once it has stopped. Taken before awaiting
	/// the bridge, which consumes it.
	pub fn take_teardown(&mut self) -> Teardown {
		std::mem::take(&mut self.teardown)
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

	/// Stop the bridge. Awaiting it then yields a cancelled [`JoinError`].
	pub fn abort(&self) {
		self.task.abort();
	}
}

impl Future for Bridge {
	type Output = std::result::Result<Result<BridgeStats>, JoinError>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		Pin::new(&mut self.task).poll(cx)
	}
}

/// Destroys the ROS entities of a stopped [`Bridge`], e.g. its publisher, so a new bridge on
/// the same topic can use another message type.
///
/// Does nothing while the bridge still runs, or for bridges whose entities go away on their own.
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
		assert_eq!(bridge.await.unwrap().unwrap(), live);
	}

	#[tokio::test]
	async fn abort_cancels_the_task() {
		let bridge = Bridge::spawn(|_| std::future::pending());
		bridge.abort();
		assert!(bridge.await.unwrap_err().is_cancelled());
	}
}
