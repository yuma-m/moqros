//! Shared plumbing for the moqros bridge binaries.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::builder::BoolishValueParser;
use clap::parser::ValueSource;
use clap::{ArgAction, ArgMatches, Args, Command, Parser};
use moqros::ros::r2r::ParameterValue;
use moqros::ros::{QosProfile, r2r};

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

/// Parse `T` from the command line and create the ROS node.
///
/// Arguments between `--ros-args` and `--` are left to rcl, so the binaries accept
/// remapping (`-r __node:=…`, `-r __ns:=…`), `-p` and `--params-file` like any ROS
/// node. Each ROS parameter named after an argument (`bitrate`, `keyframe_interval_ms`,
/// …) fills that argument unless it was given on the command line: flags win over
/// parameters, which win over environment variables and defaults.
pub fn init<T: Parser>(node_name: &str) -> anyhow::Result<(T, r2r::Node)> {
	let args = strip_ros_args(std::env::args());
	let command = T::command();
	let matches = command.clone().get_matches_from(&args);

	let ctx = r2r::Context::create().context("failed to create ROS context")?;
	let node = r2r::Node::create(ctx, node_name, "").context("failed to create ROS node")?;

	let extra = param_args(&command, &matches, &node)?;
	let matches = if extra.is_empty() {
		matches
	} else {
		command
			.try_get_matches_from(args.into_iter().chain(extra))
			.context("invalid ROS parameter")?
	};
	Ok((T::from_arg_matches(&matches)?, node))
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

/// Command-line flags for the node's ROS parameters that `matches` did not get on the
/// command line.
fn param_args(command: &Command, matches: &ArgMatches, node: &r2r::Node) -> anyhow::Result<Vec<String>> {
	let mut extra = Vec::new();
	for (name, param) in node.params.lock().unwrap().iter() {
		if name == "use_sim_time" {
			continue;
		}
		let Some(long) = command
			.get_arguments()
			.find(|arg| arg.get_id() == name.as_str())
			.and_then(|arg| arg.get_long())
		else {
			// Shared parameter files (`/**`) may hold parameters for other nodes.
			tracing::debug!(name, "ignoring unknown ROS parameter");
			continue;
		};
		if matches.value_source(name) == Some(ValueSource::CommandLine) {
			continue;
		}
		let value = match &param.value {
			ParameterValue::Bool(value) => value.to_string(),
			ParameterValue::Integer(value) => value.to_string(),
			ParameterValue::Double(value) => value.to_string(),
			ParameterValue::String(value) => value.clone(),
			value => bail!("ROS parameter `{name}` has unsupported type: {value:?}"),
		};
		// One token, so values starting with `-` aren't taken for flags.
		extra.push(format!("--{long}={value}"));
	}
	Ok(extra)
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
	tokio::task::spawn_blocking(move || {
		while !stopped.load(Ordering::Relaxed) {
			node.spin_once(Duration::from_millis(50));
		}
	});
	Spinner(stop)
}

/// Stops the [`spawn_spinner`] thread when dropped.
pub struct Spinner(Arc<AtomicBool>);

impl Drop for Spinner {
	fn drop(&mut self) {
		self.0.store(true, Ordering::Relaxed);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn strip(args: &[&str]) -> Vec<String> {
		strip_ros_args(args.iter().map(|arg| arg.to_string()))
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
}
