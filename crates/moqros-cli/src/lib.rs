//! Shared plumbing for the moqros bridge binaries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::builder::BoolishValueParser;
use clap::{ArgAction, Args, Command, Parser};
use moqros::ros::r2r::ParameterValue;
use moqros::ros::{QosProfile, r2r};
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
	let cli = parse_args(strip_ros_args(std::env::args()), flags).unwrap_or_else(|err| err.exit());
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

/// Parse `T` from `args` (program name first, ROS arguments already stripped) and
/// `param_flags`.
///
/// The parameter flags go right after the program name. With `args_override_self` a
/// later occurrence of a flag wins, so the command line overrides them, while
/// environment variables and defaults only apply to flags given neither way.
fn parse_args<T: Parser>(args: Vec<String>, param_flags: Vec<String>) -> clap::error::Result<T> {
	let mut args = args.into_iter();
	let args = args.next().into_iter().chain(param_flags).chain(args);
	let matches = T::command().args_override_self(true).try_get_matches_from(args)?;
	T::from_arg_matches(&matches)
}

/// Command-line flags (`--name=value`) for the ROS parameters named after an argument
/// of `command`.
fn param_flags<'a>(
	command: &Command,
	params: impl IntoIterator<Item = (&'a str, &'a ParameterValue)>,
) -> anyhow::Result<Vec<String>> {
	let mut flags = Vec::new();
	for (name, value) in params {
		let Some(long) = command
			.get_arguments()
			.find(|arg| arg.get_id() == name)
			.and_then(|arg| arg.get_long())
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
			// Debug keeps the fraction of whole numbers, so `1.0` isn't turned into `1`.
			ParameterValue::Double(value) => format!("{value:?}"),
			ParameterValue::String(value) => value.clone(),
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

/// Spin `node` on a blocking thread until the returned guard is dropped.
///
/// The tokio runtime waits for blocking tasks on shutdown, so a spinner that never
/// stops would keep the process alive after `main` returns.
pub fn spawn_spinner(mut node: r2r::Node) -> Spinner {
	let stop = Arc::new(AtomicBool::new(false));
	let stopped = stop.clone();
	let task = tokio::task::spawn_blocking(move || {
		while !stopped.load(Ordering::Relaxed) {
			node.spin_once(Duration::from_millis(50));
		}
	});
	Spinner { stop, task }
}

/// Stops the [`spawn_spinner`] thread when dropped.
pub struct Spinner {
	stop: Arc<AtomicBool>,
	task: JoinHandle<()>,
}

impl Spinner {
	/// Resolves if the spinner thread dies, which only happens when it panics, since
	/// without spinning no ROS message arrives anymore.
	pub async fn failed(&mut self) -> anyhow::Error {
		match (&mut self.task).await {
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
	use super::*;

	fn strip(args: &[&str]) -> Vec<String> {
		strip_ros_args(args.iter().map(|arg| arg.to_string()))
	}

	#[derive(Debug, Parser)]
	struct TestCli {
		#[arg(long, default_value = "/image_raw")]
		topic: String,

		#[arg(long)]
		broadcast: Option<String>,

		#[arg(long, default_value_t = 30.0)]
		fps: f32,

		#[arg(long, default_value_t = 2000)]
		keyframe_interval_ms: u64,

		#[command(flatten)]
		relay: RelayArgs,

		#[command(flatten)]
		qos: QosArgs,
	}

	/// Only parsed by `params_override_env`, so no other test sees its variable.
	#[derive(Debug, Parser)]
	struct EnvCli {
		#[arg(long, env = "MOQROS_TEST_BITRATE", default_value_t = 1)]
		bitrate: u32,
	}

	fn flags<T: Parser>(params: &[(&str, ParameterValue)]) -> anyhow::Result<Vec<String>> {
		param_flags(&T::command(), params.iter().map(|(name, value)| (*name, value)))
	}

	fn parse<T: Parser>(args: &[&str], params: &[(&str, ParameterValue)]) -> clap::error::Result<T> {
		let args = std::iter::once("bin")
			.chain(args.iter().copied())
			.map(String::from)
			.collect();
		parse_args(args, flags::<T>(params).unwrap())
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

	#[test]
	fn params_override_env() {
		// SAFETY: no other test reads or writes this variable.
		unsafe { std::env::set_var("MOQROS_TEST_BITRATE", "bogus") };
		let bitrate = ("bitrate", ParameterValue::Integer(2));
		// An invalid environment variable is never looked at when a parameter or flag is set.
		assert_eq!(parse::<EnvCli>(&[], std::slice::from_ref(&bitrate)).unwrap().bitrate, 2);
		assert_eq!(parse::<EnvCli>(&["--bitrate", "3"], &[]).unwrap().bitrate, 3);
		assert!(parse::<EnvCli>(&[], &[]).is_err());

		unsafe { std::env::set_var("MOQROS_TEST_BITRATE", "4") };
		assert_eq!(parse::<EnvCli>(&[], &[]).unwrap().bitrate, 4);
		assert_eq!(parse::<EnvCli>(&[], std::slice::from_ref(&bitrate)).unwrap().bitrate, 2);
		assert_eq!(parse::<EnvCli>(&["--bitrate=3"], &[bitrate]).unwrap().bitrate, 3);
		unsafe { std::env::remove_var("MOQROS_TEST_BITRATE") };
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
		];
		assert!(flags::<TestCli>(&params).unwrap().is_empty());
	}

	#[test]
	fn arrays_are_rejected() {
		let params = [("topic", ParameterValue::StringArray(vec!["/a".into()]))];
		let err = flags::<TestCli>(&params).unwrap_err();
		assert!(err.to_string().contains("`topic`"), "{err}");
	}

	#[test]
	fn invalid_param_values_fail_validation() {
		let err = parse::<TestCli>(&[], &[("keyframe_interval_ms", string("soon"))]).unwrap_err();
		assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
		let err = parse::<TestCli>(&[], &[("url", string("not a url"))]).unwrap_err();
		assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
	}
}
