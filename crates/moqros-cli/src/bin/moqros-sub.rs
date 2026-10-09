//! Subscribe to a MoQ image broadcast, decode it and republish it as a ROS 2 topic.

use clap::Parser;
use moqros::ros::spawn_moq_to_ros;
use moqros_cli::{QosArgs, RelayArgs, init_logging};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Broadcast name to subscribe to.
	#[arg(long, default_value = "image_raw")]
	broadcast: String,

	/// Topic to publish decoded `rgb8` images on.
	#[arg(long, default_value = "/moqros/image")]
	topic: String,

	/// `header.frame_id` of the republished images.
	#[arg(long, default_value = "camera")]
	frame_id: String,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, mut node) = moqros_cli::init::<Cli>("moqros_sub")?;

	let subscriber = moqros::Subscriber::connect(&cli.relay.config())?;
	let bridge = spawn_moq_to_ros(
		&mut node,
		&cli.topic,
		cli.qos.profile(),
		&cli.frame_id,
		subscriber,
		&cli.broadcast,
	)?;
	// The node only publishes, so it never needs spinning (spin_once would busy-loop
	// with nothing to wait on); keep it alive for the publisher's sake.
	let _node = node;

	tracing::info!(broadcast = cli.broadcast, topic = cli.topic, url = %cli.relay.url, "bridging MoQ -> ROS");

	tokio::select! {
		res = bridge => res??,
		_ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
	}
	Ok(())
}
