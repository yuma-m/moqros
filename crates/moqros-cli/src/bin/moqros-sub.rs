//! Subscribe to MoQ broadcasts and republish each as a ROS 2 topic: video decoded into
//! images, or data tracks as the message type they carry.

use anyhow::{Context, bail};
use clap::Parser;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moqros::ros::MoqToRos;
use moqros_cli::{BroadcastSpec, QosArgs, RelayArgs, init_logging};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Broadcast to subscribe to; repeat for several. Options after the name override
	/// the flags below for that broadcast: `chatter,topic=/chatter,type=std_msgs/msg/String,frame_id=cam`.
	#[arg(long, default_value = "image_raw")]
	broadcast: Vec<BroadcastSpec>,

	/// Topic to publish on: decoded `rgb8` images for video, or the data track's messages.
	/// Single broadcast only; defaults to `/moqros/image` for one broadcast, and to
	/// `/moqros/<broadcast>` for each of several.
	#[arg(long)]
	topic: Option<String>,

	/// `header.frame_id` of the republished images.
	#[arg(long, default_value = "camera")]
	frame_id: String,

	/// Message type to republish a data track as; defaults to the type its catalog names.
	/// Single broadcast only.
	#[arg(id = "type", long = "type")]
	ros_type: Option<String>,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, mut node) = moqros_cli::init::<Cli>("moqros_sub")?;
	let broadcasts = moqros_cli::sub_broadcasts(cli.broadcast, cli.topic, cli.ros_type, &cli.frame_id)?;

	cli.relay.warn_if_insecure();
	let subscriber = moqros::Subscriber::connect(&cli.relay.config())?;
	tracing::info!(url = %cli.relay.redacted_url(), broadcasts = broadcasts.len(), "connecting to the relay");

	// A topic's type depends on what its broadcast carries, so wait for each broadcast
	// (all at once) before creating its publisher.
	let mut subscribing: FuturesUnordered<_> = broadcasts
		.into_iter()
		.map(|broadcast| {
			let subscriber = subscriber.clone();
			async move { (MoqToRos::subscribe(subscriber, &broadcast.broadcast).await, broadcast) }
		})
		.collect();
	let mut bridges = FuturesUnordered::new();
	// Created once, so a signal arriving while the loop body runs isn't missed.
	let shutdown = moqros_cli::shutdown_signal();
	tokio::pin!(shutdown);

	// The node only publishes, so it never needs spinning (spin_once would busy-loop
	// with nothing to wait on).
	loop {
		tokio::select! {
			Some((ready, broadcast)) = subscribing.next() => {
				let bridge = ready.spawn(
					&mut node,
					&broadcast.topic,
					cli.qos.profile(),
					&broadcast.frame_id,
					broadcast.ros_type.as_deref(),
				);
				match bridge {
					Ok(bridge) => {
						tracing::info!(broadcast = broadcast.broadcast, topic = broadcast.topic, "bridging MoQ -> ROS");
						bridges.push(async move { (broadcast.broadcast, bridge.await) });
					}
					// E.g. a message type this build has no bindings for: the other
					// broadcasts can still go on.
					Err(err) => {
						tracing::error!(%err, broadcast = broadcast.broadcast, topic = broadcast.topic, "skipping broadcast");
						if subscribing.is_empty() && bridges.is_empty() {
							bail!("no broadcast could be republished");
						}
					}
				}
			}
			Some((broadcast, res)) = bridges.next() => {
				res.context("bridge panicked")?.with_context(|| format!("broadcast {broadcast}"))?;
				return Ok(());
			}
			// E.g. the relay refused the session: no broadcast would ever be announced.
			res = subscriber.closed() => return res.context("relay connection closed"),
			res = &mut shutdown => {
				tracing::info!(signal = res?, "shutting down");
				if let Err(err) = subscriber.close().await {
					tracing::warn!(%err, "the relay connection didn't close cleanly");
				}
				return Ok(());
			}
		}
	}
}
