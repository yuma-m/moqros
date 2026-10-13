//! Subscribe to ROS 2 topics and publish each as a MoQ broadcast: images as video, or
//! any message type as JSON or CDR data.

use clap::Parser;
use moqros_cli::{Bridges, EncoderArgs, Format, QosArgs, Relay, RelayArgs, TopicSpec, init_logging};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
	/// Topic to read; repeat for several. Options after the name override the flags
	/// below for that topic: `/chatter,format=json,type=std_msgs/msg/String,mode=stream,broadcast=chat`.
	#[arg(long, default_value = "/image_raw")]
	topic: Vec<TopicSpec>,

	/// Broadcast name; defaults to the topic without its leading slash. Single topic only.
	#[arg(long)]
	broadcast: Option<String>,

	/// How to send the topics: `video` encodes `sensor_msgs/msg/Image`; `json` and `cdr`
	/// forward any message type as data.
	#[arg(long, value_enum, default_value_t = Format::Video)]
	format: Format,

	/// Message type of the topic (e.g. `sensor_msgs/msg/Imu`); looked up in the ROS graph
	/// if omitted. Single topic only.
	#[arg(id = "type", long = "type")]
	ros_type: Option<String>,

	/// For `json` and `cdr`: `snapshot` sends each message on its own so subscribers skip
	/// to the newest; `stream` delivers every message in order.
	#[arg(long, default_value_t = moqros::DataMode::default())]
	mode: moqros::DataMode,

	#[command(flatten)]
	encoder: EncoderArgs,

	#[command(flatten)]
	relay: RelayArgs,

	#[command(flatten)]
	qos: QosArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	init_logging();
	let (cli, node) = moqros_cli::init::<Cli>("moqros_pub")?;
	let topics = moqros_cli::pub_topics(cli.topic, cli.format, cli.mode, cli.ros_type, cli.broadcast)?;

	cli.relay.warn_if_insecure();
	let publisher = moqros::Publisher::connect(&cli.relay.config())?;
	tracing::info!(url = %cli.relay.redacted_url(), topics = topics.len(), "connecting to the relay");
	let bridges = Bridges {
		topics,
		broadcasts: Vec::new(),
		encoder: cli.encoder.settings(),
		qos: cli.qos.profile(),
	};
	moqros_cli::run_bridges(node, Relay::publisher(publisher), bridges).await
}
