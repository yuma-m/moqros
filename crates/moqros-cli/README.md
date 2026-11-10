# moqros-cli

[![crates.io](https://img.shields.io/crates/v/moqros-cli.svg)](https://crates.io/crates/moqros-cli)

ROS 2 nodes that bridge topics to and from [Media over QUIC](https://moq.dev) (MoQ) relays, built on the
[`moqros`](https://docs.rs/moqros) library:

- `moqros-pub` publishes ROS topics to a relay: images as video, any other message type as JSON or CDR data.
- `moqros-sub` subscribes to broadcasts and republishes them as ROS topics.
- `moqros-bridge` does both in one node, over one relay connection.

```sh
moqros-pub --url http://localhost:4443/anon --topic /camera/image_raw --reliable
moqros-sub --url http://localhost:4443/anon --broadcast camera/image_raw --topic /remote/image
```

The [moqros repository](https://github.com/yuma-m/moqros) has a Docker sample that runs a relay, the bridges, and
a browser viewer.

## Installing

The bridges need a ROS 2 environment, libvpx, pkg-config, and libclang at build time (see
[Development](https://github.com/yuma-m/moqros#development)).

With cargo:

```sh
source /opt/ros/jazzy/setup.bash
IDL_PACKAGE_FILTER="std_msgs;sensor_msgs;geometry_msgs;nav_msgs;rosgraph_msgs;diagnostic_msgs" cargo install moqros-cli
```

As a ROS 2 package with colcon (`crates/moqros-cli` has an `ament_cargo` `package.xml`):

```sh
pip install colcon-cargo colcon-ros-cargo
cargo install cargo-ament-build

cd ~/ros2_ws/src && git clone https://github.com/yuma-m/moqros
cd ~/ros2_ws
rosdep install --from-paths src -iy
source /opt/ros/jazzy/setup.bash
colcon build
source install/setup.bash
ros2 run moqros-cli moqros-pub --help
```

### Features

| Feature | What |
| --- | --- |
| `av1` | AV1 through libaom. Needs libaom at build time (`apt install libaom-dev`, `brew install aom`). |
| `h264` | H.264 through Cisco's OpenH264 binary, loaded at runtime from `OPENH264_LIBRARY`. |
| `hw` | Hardware encoders through GStreamer. Needs the GStreamer development files at build time. |

VP8 is always available.

## Bridge behavior

- **QoS:** both bridges default to the best-effort `sensor_data` profile. Pass `--reliable` to match a
  reliable publisher, which is recommended for large uncompressed images. With best-effort QoS, large images
  are split into many UDP fragments, and losing any one of them drops the whole frame.
- **Latency:** if the encoder falls behind, `moqros-pub` drops all but the newest pending image, and
  `moqros-sub` always jumps to the newest group.
- **Reconnects:** the relay connection redials automatically, and `moqros-sub` resubscribes when a broadcast
  comes back. It also resubscribes when the relay retracts a broadcast or routes it to another publisher of the
  same name, since the relay doesn't end subscriptions then.
- **Stamps and frames:** `moqros-pub` announces each image topic's `header.stamp` (as the capture time of the
  video timeline) and `header.frame_id` in the broadcast's catalog, and `moqros-sub` republishes the images with
  them. The stamps stay in the publisher's clock, sim time included. `--stamp receive` (or `stamp=receive`)
  stamps them with the local receive time instead, as does a publisher that announces no capture times;
  `--frame-id` (or `frame_id=`) overrides the frame.
- **Shutdown:** on SIGINT or SIGTERM (`docker stop`, systemd), both bridges close the relay session before
  exiting, so the relay ends their broadcasts right away and a restarted `moqros-pub` can announce them again.
- **Logging:** both bridges log their message rate every 10 s (`message rate`).
- **Diagnostics:** every bridge node publishes one `diagnostic_msgs/msg/DiagnosticStatus` per topic and broadcast
  on `/diagnostics` each second (`--diagnostics-period-ms`, 0 turns it off), with the message counts and rate.
  A bridge that has received nothing for 5 s, or dropped more than half of its messages since the last report,
  is a warning; one waiting to restart is an error. View it with `rqt_robot_monitor` or
  `ros2 topic echo /diagnostics`. It needs bindings for `diagnostic_msgs` (include it in `IDL_PACKAGE_FILTER`);
  without them the node logs a warning and runs without diagnostics.
- **Codec:** `moqros-pub --codec vp8|av1|h264`. AV1 needs the `av1` feature. H.264 needs the `h264` feature and
  Cisco's OpenH264 binary:

  ```sh
  export OPENH264_LIBRARY=$(./scripts/fetch_openh264.sh)   # downloads from ciscobinary.openh264.org
  cargo run -p moqros-cli --features h264 --bin moqros-pub -- --codec h264 ...
  ```

  Browsers decode all three through WebCodecs; AV1 needs a reasonably recent browser.
- **Renditions (simulcast):** `moqros-pub --rendition 360p@500000` (repeatable) also encodes each video scaled
  down to 360 rows at 500 kbit/s, into a track of its own in the same broadcast; images no taller than a
  rendition skip it. Browsers pick a rendition themselves; `moqros-sub --max-height 480` (or `max_height=`)
  republishes the largest rendition at most 480 rows tall instead of the largest. Each rendition costs one more
  encode per image, and `bitrate` changes at runtime only apply to the full-size one.
- **Hardware encoding:** built with the `hw` feature, `moqros-pub` encodes on a GPU through GStreamer when it
  can (`--hw auto`, the default): VA-API on Intel and AMD GPUs, and NVENC on NVIDIA GPUs, for each codec the GPU
  supports (H.264 on most, AV1 on recent ones, VP8 on few). It tries each known encoder element and keeps the
  first one that starts and honors keyframe requests, logging which one it uses; with none, it logs a warning
  and encodes in software, so the codec's own feature is still needed. A hardware encoder that fails mid-stream
  (e.g. after a GPU reset) is replaced by software, starting with a keyframe, and an encoder that failed at a
  resolution isn't tried again at that resolution until the process restarts. `--hw off` always encodes in
  software, and `--hw require` fails instead of falling back. Decoding (`moqros-sub`) always runs in software.

  ```sh
  # Build: GStreamer development files. Run: the plugins with the encoders and parsers, and the GPU driver.
  sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-base gstreamer1.0-plugins-bad   # + gstreamer1.0-vaapi on Ubuntu 22.04
  cargo run -p moqros-cli --features hw,av1 --bin moqros-pub -- --codec av1 ...
  ```

  `gst-inspect-1.0 va` and `gst-inspect-1.0 nvcodec` list the encoders GStreamer found; the node needs access to
  the GPU (`/dev/dri/renderD*` for VA-API, the `video` or `render` group). With H.264 on a hardware encoder,
  OpenH264 is only loaded if the bridge has to fall back to software.

## ROS arguments and parameters

Both bridges accept the standard `--ros-args`: remap the node name (`-r __node:=…`) and namespace
(`-r __ns:=…`), and set ROS parameters with `-p` or `--params-file`. Each command-line flag can also be
set as a parameter of the same name in snake_case (`--keyframe-interval-ms` becomes `keyframe_interval_ms`).
A flag on the command line wins over a parameter, and a parameter wins over environment variables and
defaults. Bool flags also take a value (`--reliable=false`). Parameters are read at startup, and
parameters that match no flag are ignored with a warning. The one exception is `bitrate`: setting it on a
running node with video topics (`ros2 param set /moqros_pub bitrate 500000`) changes the video bitrate of every
topic. VP8 and AV1 encoders adjust in place; H.264 and hardware encoders restart with a keyframe. Other
parameters set at runtime only log that the node needs a restart. In a parameter file, key a namespaced node by
its fully qualified name (`/front/moqros_pub`); the bare node name only matches in the root namespace.

```sh
ros2 run moqros-cli moqros-pub --ros-args -r __ns:=/front \
  -p topic:=/front/image_raw -p bitrate:=1000000 -p reliable:=true
ros2 run moqros-cli moqros-sub --ros-args --params-file crates/moqros-cli/config/moqros.yaml
```

A repeatable flag (`--topic`, `--broadcast`) takes a string array parameter, one element per occurrence
(`-p "topic:=[/a, /b]"`). If the flag is also on the command line, the command line replaces the whole
array.

The colcon package installs a launch file (`moqros.launch.py`) and a sample parameter file
(`config/moqros.yaml`):

```sh
ros2 launch moqros-cli moqros.launch.py url:=http://localhost:4443/anon namespace:=camera
```

The launch file uses `$MOQROS_URL` when `url` isn't given, and publishes the broadcast
`<namespace>/image_raw` (`namespace:=/front` gives `front/image_raw`). For a client certificate (see
[Authentication](#authentication)), add `tls_cert:=… tls_key:=…`, or set `$MOQROS_TLS_CERT` and
`$MOQROS_TLS_KEY`.

## Several topics

One `moqros-pub` can publish several topics, and one `moqros-sub` can republish several broadcasts. Repeat
`--topic` or `--broadcast`, and add options after the name, separated by commas:

```sh
moqros-pub --topic /camera/image_raw --topic /imu,format=json --topic /scan,format=cdr,mode=stream
moqros-sub --broadcast camera/image_raw,topic=/remote/image --broadcast imu --broadcast scan,topic=/remote/scan
```

| Bridge | Options | Defaults |
| --- | --- | --- |
| `moqros-pub --topic TOPIC,…` | `format`, `type`, `mode`, `broadcast` | `--format` and `--mode` for every topic. The broadcast name is the topic without its leading slash. |
| `moqros-sub --broadcast NAME,…` | `topic`, `type`, `frame_id`, `stamp` | `--frame-id` and `--stamp` for every broadcast. The topic is `/moqros/<broadcast>` (`/moqros/image` for a lone broadcast). |

`moqros-bridge` does both in one node and over one relay connection, e.g. for a robot that streams its camera
and takes velocity commands.
It takes `--topic` like `moqros-pub` and `--broadcast` like `moqros-sub`, with the same options. The flags
that name a single topic or broadcast (`--type`, and the other side's name) are left out; use the options instead:

```sh
moqros-bridge --topic /camera/image_raw --topic /odom,format=json --broadcast cmd_vel,topic=/cmd_vel
```

`moqros-sub --broadcast-prefix robots` also republishes every broadcast announced under `robots/` (now or later),
on `/moqros/<broadcast>` (`--topic-template '/remote/{name}'` names it otherwise). A broadcast whose name doesn't
make a plain topic name (e.g. `robots/r-2/camera`) is skipped with a warning; see [Security](#security).

Each topic becomes a broadcast of its own. `--type` and `--broadcast` of `moqros-pub`, and `--topic` and
`--type` of `moqros-sub`, name a single topic, so they only work with one `--topic` or `--broadcast`. A
topic without a `type` starts when it appears in the ROS graph, which tells its type. `moqros-sub`
waits for all its broadcasts at once, and starts each one when it's announced. It skips a broadcast whose
topic it can't create (e.g. a message type this build has no bindings for), and goes on with the rest.
A topic or broadcast that stops or fails later is restarted on its own, one second later at first and up to
30 seconds later while it keeps failing, so the others go on. The node exits on SIGINT or SIGTERM, when the
relay connection gives up, or when a topic can't be started at all (e.g. an unavailable codec).

## Other message types

`moqros-pub --format json|cdr` publishes a topic of any message type. The default, `--format video`, is for
`sensor_msgs/msg/Image` and `sensor_msgs/msg/CompressedImage` (JPEG or PNG, e.g. image_transport's
`/camera/image_raw/compressed`), which is decoded before it's encoded as video. `compressedDepth` images aren't
supported.

```sh
moqros-pub --topic /imu --format json                  # the type is looked up in the ROS graph
moqros-pub --topic /scan,format=cdr,type=sensor_msgs/msg/LaserScan,mode=stream
moqros-sub --broadcast imu --topic /moqros/imu         # format and type come from the catalog
```

- **`json`**: each message is one JSON document, in r2r's mapping of the message fields. Any hang consumer can
  read it, including a browser; the
  [web viewer](https://github.com/yuma-m/moqros/tree/main/examples/web) shows the latest message. Large arrays
  (point clouds, images) get big and slow as JSON, so use `cdr` for those.
- **`cdr`**: each message is its ROS serialization, passed through untouched. It's compact and lossless,
  but only ROS-aware consumers can decode it. Images sent as `cdr` arrive uncompressed and bit-exact.
- **`--mode snapshot`** (default): each message is a MoQ group of its own, and subscribers skip to the
  newest one when they fall behind. Suits sensor data and state.
- **`--mode stream`**: messages go into one group and are delivered in order, none skipped. Suits logs and
  commands. The publisher and the relay keep the group's messages, and a subscriber that joins late gets
  them all from the start of the group. A group holds at most 8192 messages or 32 MB (moq-net's limits);
  the next message then starts a new group, and late joiners start there. For long-running, high-rate
  topics use `snapshot`.
- **Type:** without `--type`, `moqros-pub` waits for the topic to appear and asks the ROS graph for its type.
  Relative topic names resolve against the node's namespace, but remappings aren't applied. The type goes
  into the catalog, so `moqros-sub` republishes with the same type (`--type` overrides it).
- **Supported types:** r2r generates message bindings at build time, and the data bridges only handle types
  it generated. With `IDL_PACKAGE_FILTER` set, list every package you need (the Docker image includes
  `std_msgs`, `sensor_msgs`, `geometry_msgs`, `nav_msgs`, `rosgraph_msgs`, and `diagnostic_msgs`).

`moqros-sub` handles both kinds of broadcast: it republishes video as `rgb8` images (`--encoding` or
`encoding=` picks `bgr8`, `rgba8`, `bgra8` or `mono8` instead, e.g. for OpenCV), and data tracks as their
message type. It fails if a broadcast comes back carrying something different from before.

## Security

Keep these in mind before you connect the bridges to a relay beyond localhost:

- **Anyone the relay lets publish can reach ROS.** `moqros-sub` republishes whatever arrives on its broadcasts
  as ROS messages. On a relay with anonymous publishing, someone else can publish under the same name and
  inject messages into your ROS graph, e.g. `geometry_msgs/msg/Twist` on a command topic. Broadcast names
  aren't secret: subscribers can list what's announced under a path.
- **`--broadcast-prefix` lets publishers create topics.** Every broadcast announced under the prefix becomes a
  ROS topic on this node, named after the broadcast and typed by its catalog. Only names that make plain topic
  names (letters, digits, `_` and `/`) are used, and each broadcast and topic is taken once, but use a prefix
  only on a relay where you trust everyone who may publish under it.
- **Decoded pictures are capped.** A few bytes of bitstream can declare a huge picture, so `moqros-sub`
  rejects pictures above 8K UHD (7680×4320 pixels) instead of allocating them, and resubscribes.
- **`http://` relay URLs trust the network.** QUIC always encrypts the stream, but with an `http://` URL the
  client fetches the certificate fingerprint it trusts over plain HTTP, so anyone on the path can swap it
  and read or alter the stream. `--insecure` skips verification entirely. Outside development, give the relay
  a real certificate (`[listen] tls.cert`/`tls.key`) and use `https://` URLs. The bridges log a warning at startup for an
  `http://` URL whose host isn't this machine (`localhost`, `*.localhost`, a loopback address), and for
  `--insecure`.

### Authentication

Replace `public` in the relay's `[auth]` section with `url`, pointing at an auth server: the relay asks it
about every session. `moq auth serve` from the [moq](https://github.com/moq-dev/moq) project is the reference
one. It admits a session by a client certificate (mTLS) or a JWT, and grants publish and subscribe patterns.
See the upstream [authentication documentation](https://github.com/moq-dev/moq/blob/main/doc/bin/relay/auth.md)
for its flags. Give each client only what it needs: the robot publishes, viewers subscribe.

**Bridges: client certificates.** For a device that runs unattended, issue it a certificate from your own CA.
Point the relay's `[listen] tls.root` at the CA, and pass the certificate and its key to the bridge:

```sh
moqros-pub --url https://relay.example.com/robot --tls-cert robot.pem --tls-key robot.key --topic /camera/image_raw
```

`--tls-cert` and `--tls-key` also come from `MOQROS_TLS_CERT` and `MOQROS_TLS_KEY`, or the `tls_cert` and
`tls_key` ROS parameters; in the library, use `ClientConfig::with_client_cert`. Nothing secret goes into the
URL, and you revoke a device by its certificate. `moq auth serve` grants every certificate its CA signed the
same patterns. To scope each device to its own path, run an auth server of your own that decides by the
certificate's name or fingerprint, which the relay passes along with each session.

**Viewers: JWTs.** Browsers present a JWT in the `jwt` query parameter of the relay URL. Issue it from your
own backend once the viewer has signed in, signed with the key `moq auth serve` verifies, and add it to the
viewer's relay URL (see the [web viewer](https://github.com/yuma-m/moqros/tree/main/examples/web)). A token's
`root` is the path it's valid under, and its publish and subscribe patterns say what it may do there.

The bridges accept a token too (`--url "https://relay.example.com/robot?jwt=…"`), but prefer certificates:
a URL ends up in shell history and process lists, and the bridges reuse it on every reconnect without asking
for a new token, so once it expires they can't reconnect. They do hide the token (`jwt=REDACTED`, or a
JWT-shaped path segment as `/REDACTED`) when they log the URL.

## Cloudflare MoQ relay

The bridges and the web viewer also work with [Cloudflare's MoQ relays](https://developers.cloudflare.com/moq/),
which speak the IETF MoQ Transport drafts (draft-16 here). Create a relay in the Cloudflare dashboard
(**Media > Realtime > MoQ Relay**) or through its API; it comes with a publish/subscribe token and a subscribe-only
token. The token goes in the URL path:

```sh
moqros-pub --url "https://draft-16.cloudflare.mediaoverquic.com/$CF_PUB_TOKEN" --topic /camera/image_raw
moqros-sub --url "https://draft-16.cloudflare.mediaoverquic.com/$CF_SUB_TOKEN" --broadcast camera/image_raw
```

For the viewer, put the subscribe-only token's URL in the Relay URL field. Each token covers its whole relay,
so give viewers the subscribe-only one. The bridges hide the token (`/REDACTED`) when they log the URL, but
Cloudflare's own access logs see it, and so do shell history and process lists; prefer short-lived tokens and
revoke them after testing. The relays are a beta product, so the endpoint and the drafts it speaks may change.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/yuma-m/moqros/blob/main/LICENSE-APACHE)
or [MIT license](https://github.com/yuma-m/moqros/blob/main/LICENSE-MIT) at your option.
