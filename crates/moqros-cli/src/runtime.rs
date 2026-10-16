//! Process plumbing: logging, signals, and spinning the ROS node.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use moqros::ros::r2r;
use tokio::task::JoinHandle;

pub fn init_logging() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
		)
		.init();
}

/// A node shared between [`spawn_spinner`] and code that adds subscriptions while it spins.
pub struct SharedNode {
	node: Mutex<r2r::Node>,
	/// Set while [`SharedNode::with`] waits, so the spinner lets go of the lock. A mutex
	/// isn't fair: a spinner relocking it right away could starve the waiter.
	waiting: AtomicBool,
}

impl SharedNode {
	pub fn new(node: r2r::Node) -> Arc<Self> {
		Arc::new(Self {
			node: Mutex::new(node),
			waiting: AtomicBool::new(false),
		})
	}

	/// Run `f` on the node, between two spins.
	pub fn with<R>(&self, f: impl FnOnce(&mut r2r::Node) -> R) -> R {
		self.waiting.store(true, Ordering::Relaxed);
		let mut node = self.node.lock().unwrap_or_else(|err| err.into_inner());
		self.waiting.store(false, Ordering::Relaxed);
		f(&mut node)
	}
}

/// Resolves with the signal's name on the first SIGINT (Ctrl-C) or SIGTERM (`docker stop`,
/// systemd), so the bridges can close their relay connection before exiting.
pub async fn shutdown_signal() -> anyhow::Result<&'static str> {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{SignalKind, signal};
		let mut term = signal(SignalKind::terminate()).context("failed to listen for SIGTERM")?;
		tokio::select! {
			res = tokio::signal::ctrl_c() => res.map(|()| "SIGINT").context("failed to listen for SIGINT"),
			_ = term.recv() => Ok("SIGTERM"),
		}
	}
	#[cfg(not(unix))]
	{
		tokio::signal::ctrl_c().await.context("failed to listen for Ctrl-C")?;
		Ok("Ctrl-C")
	}
}

/// Spin `node` on a blocking thread until the returned guard is dropped.
///
/// The tokio runtime waits for blocking tasks on shutdown, so a spinner that never
/// stops would keep the process alive after `main` returns.
pub fn spawn_spinner(node: Arc<SharedNode>) -> Spinner {
	let stop = Arc::new(AtomicBool::new(false));
	let stopped = stop.clone();
	let task = tokio::task::spawn_blocking(move || {
		while !stopped.load(Ordering::Relaxed) {
			if node.waiting.load(Ordering::Relaxed) {
				std::thread::yield_now();
				continue;
			}
			let mut guard = node.node.lock().unwrap_or_else(|err| err.into_inner());
			guard.spin_once(Duration::from_millis(50));
		}
	});
	Spinner { stop, task: Some(task) }
}

/// Stops the [`spawn_spinner`] thread when dropped.
pub struct Spinner {
	stop: Arc<AtomicBool>,
	task: Option<JoinHandle<()>>,
}

impl Spinner {
	/// No spinner at all, for a node that only publishes: `spin_once` would busy-loop with
	/// nothing to wait on. [`Spinner::failed`] never resolves.
	pub fn idle() -> Self {
		Self {
			stop: Arc::new(AtomicBool::new(true)),
			task: None,
		}
	}

	/// Resolves if the spinner thread dies, which only happens when it panics, since
	/// without spinning no ROS message arrives anymore. Never resolves again after that.
	pub async fn failed(&mut self) -> anyhow::Error {
		let Some(task) = self.task.as_mut() else {
			return std::future::pending().await;
		};
		let result = task.await;
		self.task = None;
		match result {
			Err(err) => anyhow::Error::new(err).context("ROS spinner failed"),
			Ok(()) => anyhow::anyhow!("ROS spinner stopped"),
		}
	}
}

impl Drop for Spinner {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}
