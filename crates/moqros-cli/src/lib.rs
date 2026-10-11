//! Shared plumbing for the moqros bridge binaries.

use std::any::TypeId;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use clap::builder::BoolishValueParser;
use clap::error::ErrorKind;
use clap::{ArgAction, Args, Command, Parser, ValueEnum};
use moqros::ros::r2r::ParameterValue;
use moqros::ros::{QosProfile, ROS_IMAGE_TYPE, r2r};
use tokio::task::JoinHandle;

/// Relay connection flags.
#[derive(Debug, Clone, Args)]
pub struct RelayArgs {
	/// Relay URL; its path scopes broadcast names.
	#[arg(long, env = "MOQROS_URL", default_value = "http://localhost:4443/anon")]
	pub url: moqros::Url,

	/// Disable TLS certificate verification (development only).
	#[arg(long, env = "MOQROS_INSECURE", action = ArgAction::Set, value_parser = BoolishValueParser::new(),
		num_args = 0..=1, require_equals = true, default_value_t = false, default_missing_value = "true")]
	pub insecure: bool,
}

impl RelayArgs {
	pub fn config(&self) -> moqros::ClientConfig {
		moqros::ClientConfig {
			url: self.url.clone(),
			insecure: self.insecure,
		}
	}
}

/// What `moqros-pub` sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
	/// Encode `sensor_msgs/msg/Image` messages as video.
	Video,
	/// Any message type, as one JSON document per message.
	Json,
	/// Any message type, in its ROS serialization (CDR).
	Cdr,
}

impl Format {
	/// The data track format, or `None` for video.
	pub fn data(self) -> Option<moqros::DataFormat> {
		match self {
			Self::Video => None,
			Self::Json => Some(moqros::DataFormat::Json),
			Self::Cdr => Some(moqros::DataFormat::Cdr),
		}
	}
}

/// `NAME[,key=value...]` split into the name and its options.
type Spec<'a> = (&'a str, Vec<(&'a str, &'a str)>);

/// Split `NAME[,key=value...]` into the name and its options, rejecting keys not in `keys`.
fn split_spec<'a>(spec: &'a str, keys: &[&str]) -> Result<Spec<'a>, String> {
	let mut parts = spec.split(',');
	let name = parts.next().unwrap_or_default().trim();
	if name.is_empty() {
		return Err(format!("{spec:?} doesn't start with a name"));
	}
	let mut options: Vec<(&str, &str)> = Vec::new();
	for part in parts {
		let (key, value) = part
			.split_once('=')
			.ok_or_else(|| format!("expected key=value, got {part:?}"))?;
		let (key, value) = (key.trim(), value.trim());
		if !keys.contains(&key) {
			return Err(format!("unknown option {key:?} (expected one of: {})", keys.join(", ")));
		}
		if options.iter().any(|(k, _)| *k == key) {
			return Err(format!("option {key:?} given twice"));
		}
		options.push((key, value));
	}
	Ok((name, options))
}

/// A topic for `moqros-pub`: `TOPIC[,format=…][,type=…][,mode=…][,broadcast=…]`.
///
/// Options left out fall back to the matching command-line flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSpec {
	pub topic: String,
	pub format: Option<Format>,
	pub ros_type: Option<String>,
	pub mode: Option<moqros::DataMode>,
	pub broadcast: Option<String>,
}

impl FromStr for TopicSpec {
	type Err = String;

	fn from_str(spec: &str) -> Result<Self, String> {
		let (topic, options) = split_spec(spec, &["format", "type", "mode", "broadcast"])?;
		let mut parsed = Self {
			topic: topic.to_string(),
			format: None,
			ros_type: None,
			mode: None,
			broadcast: None,
		};
		for (key, value) in options {
			match key {
				"format" => parsed.format = Some(Format::from_str(value, true)?),
				"type" => parsed.ros_type = Some(value.to_string()),
				"mode" => parsed.mode = Some(value.parse().map_err(|err| format!("{err}"))?),
				_ => parsed.broadcast = Some(value.to_string()),
			}
		}
		Ok(parsed)
	}
}

/// What `moqros-pub` does with one topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubTopic {
	pub topic: String,
	pub broadcast: String,
	pub format: Format,
	pub mode: moqros::DataMode,
	/// Looked up in the ROS graph when `None` (unless the format is video).
	pub ros_type: Option<String>,
}

/// Fill in each topic's options from the flags: `format` and `mode` apply to every topic,
/// while `ros_type` and `broadcast` name one topic, so they need exactly one.
pub fn pub_topics(
	specs: Vec<TopicSpec>,
	format: Format,
	mode: moqros::DataMode,
	ros_type: Option<String>,
	broadcast: Option<String>,
) -> anyhow::Result<Vec<PubTopic>> {
	ensure!(!specs.is_empty(), "no topic to publish");
	ensure!(
		specs.len() == 1 || (ros_type.is_none() && broadcast.is_none()),
		"--type and --broadcast need a single --topic; give several topics `type=` and `broadcast=` options instead"
	);
	let mut broadcasts = HashSet::new();
	specs
		.into_iter()
		.map(|spec| {
			let topic = PubTopic {
				broadcast: spec
					.broadcast
					.or(broadcast.clone())
					.unwrap_or_else(|| moqros::broadcast_name_for_topic(&spec.topic)),
				format: spec.format.unwrap_or(format),
				mode: spec.mode.unwrap_or(mode),
				ros_type: spec.ros_type.or(ros_type.clone()),
				topic: spec.topic,
			};
			if topic.format == Format::Video
				&& let Some(ros_type) = topic.ros_type.as_deref().filter(|t| *t != ROS_IMAGE_TYPE)
			{
				bail!(
					"{}: video needs {ROS_IMAGE_TYPE} messages, not {ros_type}; use format=json or cdr",
					topic.topic
				);
			}
			ensure!(
				broadcasts.insert(topic.broadcast.clone()),
				"several topics publish the broadcast {:?}",
				topic.broadcast
			);
			Ok(topic)
		})
		.collect()
}

/// A broadcast for `moqros-sub`: `BROADCAST[,topic=…][,type=…][,frame_id=…]`.
///
/// Options left out fall back to the matching command-line flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastSpec {
	pub broadcast: String,
	pub topic: Option<String>,
	pub ros_type: Option<String>,
	pub frame_id: Option<String>,
}

impl FromStr for BroadcastSpec {
	type Err = String;

	fn from_str(spec: &str) -> Result<Self, String> {
		let (broadcast, options) = split_spec(spec, &["topic", "type", "frame_id"])?;
		let mut parsed = Self {
			broadcast: broadcast.to_string(),
			topic: None,
			ros_type: None,
			frame_id: None,
		};
		for (key, value) in options {
			let value = Some(value.to_string());
			match key {
				"topic" => parsed.topic = value,
				"type" => parsed.ros_type = value,
				_ => parsed.frame_id = value,
			}
		}
		Ok(parsed)
	}
}

/// What `moqros-sub` does with one broadcast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubBroadcast {
	pub broadcast: String,
	pub topic: String,
	/// Taken from the catalog when `None`.
	pub ros_type: Option<String>,
	pub frame_id: String,
}

/// Topic of a lone broadcast that neither `topic=` nor `--topic` names.
pub const DEFAULT_SUB_TOPIC: &str = "/moqros/image";

/// Fill in each broadcast's options from the flags. `topic` and `ros_type` name one
/// broadcast, so they need exactly one. Without either `topic=` or `--topic`, a lone
/// broadcast goes to [`DEFAULT_SUB_TOPIC`], and each of several to `/moqros/<broadcast>`.
pub fn sub_broadcasts(
	specs: Vec<BroadcastSpec>,
	topic: Option<String>,
	ros_type: Option<String>,
	frame_id: &str,
) -> anyhow::Result<Vec<SubBroadcast>> {
	ensure!(!specs.is_empty(), "no broadcast to subscribe to");
	let single = specs.len() == 1;
	ensure!(
		single || (topic.is_none() && ros_type.is_none()),
		"--topic and --type need a single --broadcast; give several broadcasts `topic=` and `type=` options instead"
	);
	let mut topics = HashSet::new();
	specs
		.into_iter()
		.map(|spec| {
			let broadcast = SubBroadcast {
				topic: spec.topic.or(topic.clone()).unwrap_or_else(|| match single {
					true => DEFAULT_SUB_TOPIC.to_string(),
					false => format!("/moqros/{}", spec.broadcast.trim_matches('/')),
				}),
				ros_type: spec.ros_type.or(ros_type.clone()),
				frame_id: spec.frame_id.unwrap_or_else(|| frame_id.to_string()),
				broadcast: spec.broadcast,
			};
			ensure!(
				topics.insert(broadcast.topic.clone()),
				"several broadcasts publish on {}",
				broadcast.topic
			);
			Ok(broadcast)
		})
		.collect()
}

/// ROS QoS flags.
#[derive(Debug, Clone, Args)]
pub struct QosArgs {
	/// Use reliable QoS instead of the best-effort sensor-data profile.
	#[arg(long, action = ArgAction::Set, value_parser = BoolishValueParser::new(),
		num_args = 0..=1, require_equals = true, default_value_t = false, default_missing_value = "true")]
	pub reliable: bool,
}

impl QosArgs {
	pub fn profile(&self) -> QosProfile {
		if self.reliable {
			QosProfile::default().keep_last(5)
		} else {
			QosProfile::sensor_data()
		}
	}
}

/// Create the ROS node and parse `T` from the command line and the node's ROS parameters.
///
/// Arguments between `--ros-args` and `--` are left to rcl, so the binaries accept
/// remapping (`-r __node:=…`, `-r __ns:=…`), `-p` and `--params-file` like any ROS
/// node. Each ROS parameter named after an argument (`bitrate`, `keyframe_interval_ms`,
/// …) fills that argument: flags win over parameters, which win over environment
/// variables and defaults.
pub fn init<T: Parser>(node_name: &str) -> anyhow::Result<(T, r2r::Node)> {
	let args = strip_ros_args(std::env::args());
	// Answer `--help` and `--version` without bringing up ROS.
	if let Some(err) = help_or_version(T::command(), &args) {
		err.exit();
	}

	let ctx = r2r::Context::create().context("failed to create ROS context")?;
	let node = r2r::Node::create(ctx, node_name, "").context("failed to create ROS node")?;

	let flags = param_flags(
		&T::command(),
		node.params
			.lock()
			.unwrap()
			.iter()
			.map(|(name, param)| (name.as_str(), &param.value)),
	)?;
	let cli = match parse_args(args, flags) {
		Ok(cli) => cli,
		Err(ParseError::CommandLine(err)) => err.exit(),
		Err(ParseError::Param(err)) => {
			// Just clap's first line; its usage hints are about the command line.
			let message = err.to_string();
			let message = message.lines().next().unwrap_or_default().trim_start_matches("error: ");
			bail!("invalid ROS parameter: {message}");
		}
	};
	Ok((cli, node))
}

/// Drop the ROS arguments (`--ros-args … [--]`, possibly repeated) from `args`.
pub fn strip_ros_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
	let mut in_ros_args = false;
	args.into_iter()
		.filter(|arg| match (in_ros_args, arg.as_str()) {
			(false, "--ros-args") | (true, "--") => {
				in_ros_args = !in_ros_args;
				false
			}
			_ => !in_ros_args,
		})
		.collect()
}

/// The error to exit with if `args` ask for help or the version.
///
/// Clap answers these as soon as it meets the flag, before checking anything else.
fn help_or_version(command: Command, args: &[String]) -> Option<clap::Error> {
	let err = command.try_get_matches_from(args).err()?;
	matches!(err.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion).then_some(err)
}

/// Why [`parse_args`] failed.
#[derive(Debug)]
enum ParseError {
	/// The command line itself is invalid.
	CommandLine(clap::Error),
	/// The command line is fine on its own, so a ROS parameter is invalid.
	Param(clap::Error),
}

/// Parse `T` from `args` (program name first, ROS arguments already stripped) and
/// `param_flags`.
///
/// The parameter flags go right after the program name. With `args_override_self` a
/// later occurrence of a flag wins, so the command line overrides them, while
/// environment variables and defaults only apply to flags given neither way. Repeatable
/// flags collect every occurrence instead, so parameter flags that the command line
/// gives again are dropped up front.
fn parse_args<T: Parser>(args: Vec<String>, param_flags: Vec<String>) -> Result<T, ParseError> {
	let flag = |arg: &str| arg.split_once('=').map_or(arg, |(flag, _)| flag).to_string();
	let given: HashSet<String> = args
		.iter()
		.skip(1)
		.filter(|arg| arg.starts_with("--"))
		.map(|arg| flag(arg))
		.collect();
	let param_flags: Vec<String> = param_flags
		.into_iter()
		.filter(|param| !given.contains(&flag(param)))
		.collect();
	let parse = |param_flags: Vec<String>| {
		let mut args = args.iter().cloned();
		let args = args.next().into_iter().chain(param_flags).chain(args);
		let matches = T::command().args_override_self(true).try_get_matches_from(args)?;
		T::from_arg_matches(&matches)
	};
	parse(param_flags).map_err(|err| match parse(Vec::new()) {
		Ok(_) => ParseError::Param(err),
		Err(err) => ParseError::CommandLine(err),
	})
}

/// Command-line flags (`--name=value`) for the ROS parameters named after an argument
/// of `command`.
fn param_flags<'a>(
	command: &Command,
	params: impl IntoIterator<Item = (&'a str, &'a ParameterValue)>,
) -> anyhow::Result<Vec<String>> {
	let mut flags = Vec::new();
	for (name, value) in params {
		let Some((long, arg)) = command
			.get_arguments()
			.find(|arg| arg.get_id() == name)
			.and_then(|arg| Some((arg.get_long()?, arg)))
		else {
			// r2r reads `use_sim_time` itself.
			if name != "use_sim_time" {
				tracing::warn!(name, "ignoring unknown ROS parameter");
			}
			continue;
		};
		let value = match value {
			// An empty YAML value: leave the flag to its environment variable or default.
			ParameterValue::NotSet => continue,
			ParameterValue::Bool(value) => value.to_string(),
			ParameterValue::Integer(value) => value.to_string(),
			// Keep the fraction of whole numbers for strings (`1.0` stays `1.0`), but drop
			// it for numbers so `2000000.0` still fits an integer flag.
			ParameterValue::Double(value) if arg.get_value_parser().type_id() == TypeId::of::<String>() => {
				format!("{value:?}")
			}
			ParameterValue::Double(value) => value.to_string(),
			ParameterValue::String(value) => value.clone(),
			// A list fills a repeatable flag, one occurrence per element.
			ParameterValue::StringArray(values) if matches!(arg.get_action(), ArgAction::Append) => {
				flags.extend(values.iter().map(|value| format!("--{long}={value}")));
				continue;
			}
			value => bail!("ROS parameter `{name}` has unsupported type: {value:?}"),
		};
		// One token, so values starting with `-` aren't taken for flags.
		flags.push(format!("--{long}={value}"));
	}
	Ok(flags)
}

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

#[cfg(test)]
mod tests {
	use clap::CommandFactory;

	use super::*;

	fn strip(args: &[&str]) -> Vec<String> {
		strip_ros_args(args.iter().map(|arg| arg.to_string()))
	}

	#[derive(Debug, Parser)]
	#[command(version = "1.2.3")]
	struct TestCli {
		#[arg(long, default_value = "/image_raw")]
		topic: String,

		#[arg(long)]
		broadcast: Option<String>,

		#[arg(long, default_value_t = 30.0)]
		fps: f32,

		#[arg(long, default_value_t = 2000)]
		keyframe_interval_ms: u64,

		#[arg(long, value_enum, default_value_t = Format::Video)]
		format: Format,

		#[arg(id = "type", long = "type")]
		ros_type: Option<String>,

		#[arg(long, default_value_t = moqros::DataMode::default())]
		mode: moqros::DataMode,

		#[command(flatten)]
		relay: RelayArgs,

		#[command(flatten)]
		qos: QosArgs,
	}

	#[derive(Debug, Parser)]
	struct MultiCli {
		#[arg(long, default_value = "/image_raw")]
		topic: Vec<TopicSpec>,
	}

	/// Only parsed by `params_override_env`, in a child process of its own.
	#[derive(Debug, Parser)]
	struct EnvCli {
		#[arg(long, env = "MOQROS_TEST_BITRATE", default_value_t = 1)]
		bitrate: u32,
	}

	fn flags<T: Parser>(params: &[(&str, ParameterValue)]) -> anyhow::Result<Vec<String>> {
		param_flags(&T::command(), params.iter().map(|(name, value)| (*name, value)))
	}

	fn args(args: &[&str]) -> Vec<String> {
		std::iter::once("bin")
			.chain(args.iter().copied())
			.map(String::from)
			.collect()
	}

	fn parse<T: Parser>(args_: &[&str], params: &[(&str, ParameterValue)]) -> Result<T, ParseError> {
		parse_args(args(args_), flags::<T>(params).unwrap())
	}

	fn string(value: &str) -> ParameterValue {
		ParameterValue::String(value.to_string())
	}

	#[test]
	fn strips_ros_args() {
		assert_eq!(strip(&["bin", "--topic", "a"]), ["bin", "--topic", "a"]);
		assert_eq!(
			strip(&[
				"bin",
				"--topic",
				"a",
				"--ros-args",
				"-r",
				"__node:=n",
				"-p",
				"bitrate:=1"
			]),
			["bin", "--topic", "a"]
		);
		assert_eq!(
			strip(&[
				"bin",
				"--ros-args",
				"-r",
				"__ns:=/x",
				"--",
				"--reliable",
				"--ros-args",
				"-p",
				"a:=1"
			]),
			["bin", "--reliable"]
		);
	}

	#[test]
	fn params_fill_flags() {
		let cli: TestCli = parse(
			&[],
			&[
				("topic", string("/front/image_raw")),
				("keyframe_interval_ms", ParameterValue::Integer(500)),
				("reliable", ParameterValue::Bool(true)),
				("url", string("http://relay:4443/anon")),
			],
		)
		.unwrap();
		assert_eq!(cli.topic, "/front/image_raw");
		assert_eq!(cli.keyframe_interval_ms, 500);
		assert!(cli.qos.reliable);
		assert_eq!(cli.relay.url.as_str(), "http://relay:4443/anon");
		assert!(!cli.relay.insecure);
		assert_eq!(cli.broadcast, None);
	}

	#[test]
	fn data_params_fill_flags() {
		let cli: TestCli = parse(&[], &[]).unwrap();
		assert_eq!(
			(cli.format, cli.ros_type, cli.mode),
			(Format::Video, None, moqros::DataMode::Snapshot)
		);

		let cli: TestCli = parse(
			&[],
			&[
				("format", string("json")),
				("type", string("sensor_msgs/msg/Imu")),
				("mode", string("stream")),
			],
		)
		.unwrap();
		assert_eq!(cli.format, Format::Json);
		assert_eq!(cli.format.data(), Some(moqros::DataFormat::Json));
		assert_eq!(cli.ros_type.as_deref(), Some("sensor_msgs/msg/Imu"));
		assert_eq!(cli.mode, moqros::DataMode::Stream);

		let cli: TestCli = parse(&["--format", "cdr", "--type", "std_msgs/msg/String"], &[]).unwrap();
		assert_eq!(cli.format.data(), Some(moqros::DataFormat::Cdr));
		assert_eq!(cli.ros_type.as_deref(), Some("std_msgs/msg/String"));
		assert!(matches!(
			parse::<TestCli>(&[], &[("format", string("xml"))]),
			Err(ParseError::Param(_))
		));
	}

	#[test]
	fn command_line_overrides_params() {
		let params = [
			("topic", string("/param")),
			("reliable", ParameterValue::Bool(true)),
			("insecure", ParameterValue::Bool(false)),
		];
		let cli: TestCli = parse(&["--topic", "/flag", "--reliable=false", "--insecure"], &params).unwrap();
		assert_eq!(cli.topic, "/flag");
		assert!(!cli.qos.reliable);
		assert!(cli.relay.insecure);
	}

	/// Setting a variable while other tests read the environment is undefined behavior,
	/// so this test reruns itself in a child process for each value of the variable.
	#[test]
	fn params_override_env() {
		const VAR: &str = "MOQROS_TEST_BITRATE";
		let bitrate = ("bitrate", ParameterValue::Integer(2));
		match std::env::var(VAR).ok().as_deref() {
			None => {
				for value in ["bogus", "4"] {
					let output = std::process::Command::new(std::env::current_exe().unwrap())
						.args(["--exact", "tests::params_override_env"])
						.env(VAR, value)
						.output()
						.unwrap();
					let stdout = String::from_utf8_lossy(&output.stdout);
					assert!(
						output.status.success() && stdout.contains("1 passed"),
						"{VAR}={value}:\n{stdout}"
					);
				}
			}
			// An invalid variable is never looked at when a parameter or flag is set.
			Some("bogus") => {
				assert_eq!(parse::<EnvCli>(&[], std::slice::from_ref(&bitrate)).unwrap().bitrate, 2);
				assert_eq!(parse::<EnvCli>(&["--bitrate", "3"], &[]).unwrap().bitrate, 3);
				assert!(matches!(parse::<EnvCli>(&[], &[]), Err(ParseError::CommandLine(_))));
				let help = help_or_version(EnvCli::command(), &args(&["--help"]));
				assert_eq!(help.map(|err| err.kind()), Some(ErrorKind::DisplayHelp));
			}
			Some(value) => {
				assert_eq!(value, "4");
				assert_eq!(parse::<EnvCli>(&[], &[]).unwrap().bitrate, 4);
				assert_eq!(parse::<EnvCli>(&[], std::slice::from_ref(&bitrate)).unwrap().bitrate, 2);
				assert_eq!(parse::<EnvCli>(&["--bitrate=3"], &[bitrate]).unwrap().bitrate, 3);
			}
		}
	}

	#[test]
	fn doubles_keep_their_fraction() {
		let cli: TestCli = parse(
			&[],
			&[
				("broadcast", ParameterValue::Double(1.0)),
				("fps", ParameterValue::Double(12.5)),
			],
		)
		.unwrap();
		assert_eq!(cli.broadcast.as_deref(), Some("1.0"));
		assert_eq!(cli.fps, 12.5);

		// Whole numbers still fit integer flags.
		let cli: TestCli = parse(&[], &[("keyframe_interval_ms", ParameterValue::Double(500.0))]).unwrap();
		assert_eq!(cli.keyframe_interval_ms, 500);

		// YAML `fps: 15` is an integer parameter.
		let cli: TestCli = parse(&[], &[("fps", ParameterValue::Integer(15))]).unwrap();
		assert_eq!(cli.fps, 15.0);
	}

	#[test]
	fn values_starting_with_a_dash_stay_values() {
		let cli: TestCli = parse(&[], &[("broadcast", string("-x"))]).unwrap();
		assert_eq!(cli.broadcast.as_deref(), Some("-x"));
	}

	#[test]
	fn unset_and_unknown_params_are_ignored() {
		let params = [
			("broadcast", ParameterValue::NotSet),
			("use_sim_time", ParameterValue::Bool(false)),
			("bitrat", ParameterValue::Integer(1)),
			("keyframe-interval-ms", ParameterValue::Integer(1)),
			// Clap's own arguments aren't parameters.
			("help", ParameterValue::Bool(true)),
			("version", string("2")),
		];
		assert!(flags::<TestCli>(&params).unwrap().is_empty());
	}

	#[test]
	fn arrays_fill_repeatable_flags() {
		let topics = |cli: MultiCli| cli.topic.into_iter().map(|spec| spec.topic).collect::<Vec<_>>();
		assert_eq!(topics(parse(&[], &[]).unwrap()), ["/image_raw"]);
		let params = [(
			"topic",
			ParameterValue::StringArray(vec!["/a".into(), "/b,format=json".into()]),
		)];
		let cli: MultiCli = parse(&[], &params).unwrap();
		assert_eq!(cli.topic[1].format, Some(Format::Json));
		assert_eq!(topics(cli), ["/a", "/b"]);
		// A single string still works, and the command line replaces the whole list.
		assert_eq!(topics(parse(&[], &[("topic", string("/c"))]).unwrap()), ["/c"]);
		assert_eq!(topics(parse(&["--topic", "/d"], &params).unwrap()), ["/d"]);
	}

	#[test]
	fn parses_topic_specs() {
		let spec: TopicSpec = "/scan, format=cdr ,type=sensor_msgs/msg/LaserScan,mode=stream,broadcast=lidar"
			.parse()
			.unwrap();
		assert_eq!(
			spec,
			TopicSpec {
				topic: "/scan".into(),
				format: Some(Format::Cdr),
				ros_type: Some("sensor_msgs/msg/LaserScan".into()),
				mode: Some(moqros::DataMode::Stream),
				broadcast: Some("lidar".into()),
			}
		);
		assert_eq!("/image_raw".parse::<TopicSpec>().unwrap().format, None);
		for bad in [
			"",
			",format=json",
			"/a,json",
			"/a,codec=vp8",
			"/a,format=xml",
			"/a,mode=x",
			"/a,type=x,type=y",
		] {
			assert!(bad.parse::<TopicSpec>().is_err(), "{bad:?}");
		}

		let spec: BroadcastSpec = "camera/image_raw,topic=/out,frame_id=cam,type=sensor_msgs/msg/Image"
			.parse()
			.unwrap();
		assert_eq!(
			(spec.broadcast.as_str(), spec.topic.as_deref(), spec.frame_id.as_deref()),
			("camera/image_raw", Some("/out"), Some("cam"))
		);
		assert!("chatter,format=json".parse::<BroadcastSpec>().is_err());
	}

	#[test]
	fn resolves_pub_topics() {
		let specs = |specs: &[&str]| {
			specs
				.iter()
				.map(|s| s.parse::<TopicSpec>().unwrap())
				.collect::<Vec<_>>()
		};
		let snapshot = moqros::DataMode::Snapshot;

		let topics = pub_topics(
			specs(&[
				"/camera/image_raw",
				"/chatter,format=json",
				"/log,mode=stream,broadcast=logs",
			]),
			Format::Video,
			snapshot,
			None,
			None,
		)
		.unwrap();
		let summary: Vec<_> = topics
			.iter()
			.map(|t| (t.broadcast.as_str(), t.format, t.mode))
			.collect();
		assert_eq!(
			summary,
			[
				("camera/image_raw", Format::Video, snapshot),
				("chatter", Format::Json, snapshot),
				("logs", Format::Video, moqros::DataMode::Stream),
			]
		);

		// Flags that name one topic.
		let one = pub_topics(
			specs(&["/a"]),
			Format::Cdr,
			snapshot,
			Some("t/msg/T".into()),
			Some("b".into()),
		)
		.unwrap();
		assert_eq!(
			(one[0].broadcast.as_str(), one[0].ros_type.as_deref()),
			("b", Some("t/msg/T"))
		);
		assert!(
			pub_topics(
				specs(&["/a", "/b"]),
				Format::Json,
				snapshot,
				Some("t/msg/T".into()),
				None
			)
			.is_err()
		);
		assert!(pub_topics(specs(&["/a", "/b"]), Format::Json, snapshot, None, Some("b".into())).is_err());

		assert!(pub_topics(specs(&["/a", "a,format=json"]), Format::Video, snapshot, None, None).is_err());
		assert!(
			pub_topics(
				specs(&["/a,type=std_msgs/msg/String"]),
				Format::Video,
				snapshot,
				None,
				None
			)
			.is_err()
		);
		assert!(
			pub_topics(
				specs(&["/a,type=sensor_msgs/msg/Image"]),
				Format::Video,
				snapshot,
				None,
				None
			)
			.is_ok()
		);
		assert!(pub_topics(Vec::new(), Format::Video, snapshot, None, None).is_err());
	}

	#[test]
	fn resolves_sub_broadcasts() {
		let specs = |specs: &[&str]| {
			specs
				.iter()
				.map(|s| s.parse::<BroadcastSpec>().unwrap())
				.collect::<Vec<_>>()
		};
		let topics = |broadcasts: Vec<SubBroadcast>| broadcasts.into_iter().map(|b| b.topic).collect::<Vec<_>>();

		assert_eq!(
			topics(sub_broadcasts(specs(&["image_raw"]), None, None, "camera").unwrap()),
			[DEFAULT_SUB_TOPIC]
		);
		assert_eq!(
			topics(sub_broadcasts(specs(&["image_raw"]), Some("/x".into()), None, "camera").unwrap()),
			["/x"]
		);
		let broadcasts = sub_broadcasts(
			specs(&[
				"sample/image_raw,topic=/moqros/image",
				"chatter",
				"/front/imu/,frame_id=imu",
			]),
			None,
			None,
			"camera",
		)
		.unwrap();
		assert_eq!(broadcasts[2].frame_id, "imu");
		assert_eq!(
			topics(broadcasts),
			["/moqros/image", "/moqros/chatter", "/moqros/front/imu"]
		);

		assert!(sub_broadcasts(specs(&["a", "b"]), Some("/x".into()), None, "camera").is_err());
		assert!(sub_broadcasts(specs(&["a", "b"]), None, Some("t/msg/T".into()), "camera").is_err());
		assert!(sub_broadcasts(specs(&["a,topic=/x", "b,topic=/x"]), None, None, "camera").is_err());
	}

	#[test]
	fn arrays_are_rejected() {
		let params = [("topic", ParameterValue::StringArray(vec!["/a".into()]))];
		let err = flags::<TestCli>(&params).unwrap_err();
		assert!(err.to_string().contains("`topic`"), "{err}");
	}

	#[test]
	fn invalid_param_values_are_blamed_on_the_param() {
		for (name, value) in [("keyframe_interval_ms", "soon"), ("url", "not a url")] {
			match parse::<TestCli>(&[], &[(name, string(value))]) {
				Err(ParseError::Param(err)) => assert_eq!(err.kind(), ErrorKind::ValueValidation, "{name}"),
				other => panic!("{name}: {other:?}"),
			}
		}
	}

	#[test]
	fn invalid_command_lines_are_blamed_on_the_command_line() {
		let params = [("keyframe_interval_ms", string("soon"))];
		for args in [&["--fps=fast"][..], &["--unknown"]] {
			assert!(
				matches!(parse::<TestCli>(args, &params), Err(ParseError::CommandLine(_))),
				"{args:?}"
			);
		}
	}

	#[test]
	fn detects_help_and_version() {
		let kind = |args_: &[&str]| help_or_version(TestCli::command(), &args(args_)).map(|err| err.kind());
		assert_eq!(kind(&["--help"]), Some(ErrorKind::DisplayHelp));
		assert_eq!(kind(&["-V"]), Some(ErrorKind::DisplayVersion));
		// Arguments after the help flag aren't looked at.
		assert_eq!(kind(&["-h", "--fps=fast"]), Some(ErrorKind::DisplayHelp));
		assert_eq!(kind(&["--topic", "/a"]), None);
		assert_eq!(kind(&["--fps=fast"]), None);
	}
}
