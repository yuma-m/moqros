# MoQROS

[![crates.io](https://img.shields.io/crates/v/moqros.svg)](https://crates.io/crates/moqros)
[![docs.rs](https://img.shields.io/docsrs/moqros)](https://docs.rs/moqros)
[![crates.io (moqros-cli)](https://img.shields.io/crates/v/moqros-cli.svg?label=moqros-cli)](https://crates.io/crates/moqros-cli)

![MoQROS Logo](https://github.com/yuma-m/moqros/raw/main/images/logo.png)

Stream ROS 2 topics over [Media over QUIC](https://moq.dev) (MoQ), in Rust: images as video, and any other
message type as JSON or CDR data.

moqros converts each image to I420, encodes it (VP8 or H.264), and publishes it to a MoQ relay as a
[hang](https://docs.rs/hang) broadcast. That's the WebCodecs-friendly format played by the official
[`@moq/watch`](https://www.npmjs.com/package/@moq/watch) web component. Topics of any other type go into a
hang data track instead (see [Other message types](#other-message-types)). It can also go the other way:
subscribe to a broadcast, decode it, and republish it as a ROS topic.

```
 ROS 2 topic ──► moqros-pub ──► moq-relay ──► browser (examples/web: videos + data)
 Image / any msg       hang         QUIC/WebTransport    └──► moqros-sub ──► ROS 2 topic
```

## Layout

| Path | What |
| --- | --- |
| `crates/moqros` | The library ([crates.io](https://crates.io/crates/moqros), [docs.rs](https://docs.rs/moqros)). The core (conversion, codec, MoQ publish/subscribe) is ROS-independent; the `ros` feature adds r2r bridges. |
| `crates/moqros-cli` | Bridge nodes ([crates.io](https://crates.io/crates/moqros-cli)): `moqros-pub` (ROS → MoQ), `moqros-sub` (MoQ → ROS), and `moqros-bridge` (both in one node). Also a colcon (`ament_cargo`) package. |
| `examples/image_source` | Sample node that plays a video file (via ffmpeg) or a PNG/JPEG directory as an Image topic. |
| `examples/sim` | `teleop_sim`, a tiny simulated robot (rclpy, numpy) for the [teleop demo](#teleop-demo-docker). |
| `examples/web` | Browser viewer (React, TypeScript, `@moq/watch`) for several video and data broadcasts. See [Web viewer](#web-viewer). |
| `docker/` | Dockerfile (ROS 2 Jazzy + Rust, moq-relay) and the end-to-end `compose.yml`. |

## End-to-end sample (Docker)

You only need Docker:

```sh
docker compose -f docker/compose.yml up --build
```

On first start, the one-shot `samples` service generates a test video (`samples/sample.mp4`) and a PNG
sequence (`samples/frames/`) with ffmpeg inside the container. Files that already exist are kept. To stream
your own footage, put a video file or a directory of PNG/JPEG frames in `samples/` and pass `SAMPLE=<name>`
(`SAMPLE=frames` plays the PNG sequence instead of the video).

Open <http://localhost:8080> in a Chromium-based browser or Firefox. Both support WebTransport. The page
connects to `http://localhost:4443/anon` and shows a panel for every broadcast the relay announces: the two
videos `sample/image_raw` and `sample/frames/image_raw`, and the latest messages of the data broadcasts
`chatter` and `sample/pose`. The [hosted viewer](#web-viewer) connects there too, so it works with the
sample without the `web` service.

![The web viewer showing the two cameras and the two data topics of the Docker sample](https://github.com/yuma-m/moqros/raw/main/images/viewer.jpg)

`docker/compose.yml` describes each service. Besides the viewer, `moqros-sub` republishes the four
broadcasts on `/moqros/image`, `/moqros/frames`, `/moqros/chatter`, and `/moqros/pose`.

The relay allows anyone who can reach it to watch and publish. Read [Security](#security) before you run the
sample on a machine others can reach.

The ROS services share one network and IPC namespace, as processes on one robot would. This lets Fast DDS
use shared memory for the 2.7 MB frames. The source publishes with reliable QoS, and `moqros-pub` runs with
`--reliable`. With best-effort QoS, large images are split into many UDP fragments, and losing any one of them
drops the whole frame. In testing that cut 30 fps down to 10–20 fps. On a real robot, match the camera
driver's QoS. Both bridges log their message rate every 10 s (`message rate`).

To check the round trip on the ROS side:

```sh
docker compose -f docker/compose.yml exec moqros-sub bash -c \
  'source /opt/ros/jazzy/setup.bash && ROS_DOMAIN_ID=42 ros2 topic hz /moqros/image'
```

If UDP port forwarding doesn't work in your Docker setup, run the relay on the host instead
(`cargo install moq-relay && moq-relay docker/relay.toml`).

## Teleop demo (Docker)

The other direction too: drive a simulated robot from the browser while watching its camera.

```sh
docker compose -f docker/teleop.compose.yml up --build
```

Open <http://localhost:8080/?teleop=robot/cmd_vel>. The `teleop` parameter adds a pad to the viewer that
publishes `geometry_msgs/msg/Twist` commands as a JSON data broadcast. Hold WASD, the arrow keys, or the pad's
buttons to drive, and press Space to stop. Only the page in use (visible and focused) publishes, so with several
tabs or windows open, the one you're using drives. The demo uses the same ports as the sample above, so run one at a time.

| Service | What |
| --- | --- |
| `sim` | `teleop_sim`: a differential-drive robot in a small grid world. It reads `/cmd_vel`, and publishes a first-person camera (`/camera/image_raw`), a top-down map (`/map/image_raw`), and `/odom`. It stops when no command arrives for 0.5 s, e.g. when the browser tab closes. |
| `bridge` | One `moqros-bridge` node for both directions: the two images as the video broadcasts `robot/camera` and `robot/map`, `/odom` as the JSON broadcast `robot/odom`, and the browser's `robot/cmd_vel` republished on `/cmd_vel`. |
| `relay`, `web` | The same as in the sample above. |

To watch the commands arrive on the ROS side:

```sh
docker compose -f docker/teleop.compose.yml exec bridge bash -c \
  'source /opt/ros/jazzy/setup.bash && ROS_DOMAIN_ID=42 ros2 topic echo /cmd_vel'
```

The relay lets anyone who reaches it publish, so anyone who can reach port 4443 can drive this robot. Read
[Security](#security) before you connect a real one.

## Installing the bridge nodes

`moqros-pub`, `moqros-sub`, and `moqros-bridge` need a ROS 2 environment, libvpx, pkg-config, and libclang at build time
(see [Development](#development)).

With cargo, from [crates.io](https://crates.io/crates/moqros-cli):

```sh
source /opt/ros/jazzy/setup.bash
IDL_PACKAGE_FILTER="std_msgs;sensor_msgs;geometry_msgs;nav_msgs;rosgraph_msgs" cargo install moqros-cli
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

### Bridge behavior

- **QoS:** both bridges default to the best-effort `sensor_data` profile. Pass `--reliable` to match a
  reliable publisher, which is recommended for large uncompressed images.
- **Latency:** if the encoder falls behind, `moqros-pub` drops all but the newest pending image, and
  `moqros-sub` always jumps to the newest group.
- **Reconnects:** the relay connection redials automatically, and `moqros-sub` resubscribes when a broadcast
  comes back. It also resubscribes when the relay retracts a broadcast or routes it to another publisher of the
  same name, since the relay doesn't end subscriptions then. It stamps republished images with the local receive time.
- **Shutdown:** on SIGINT or SIGTERM (`docker stop`, systemd), both bridges close the relay session before
  exiting, so the relay ends their broadcasts right away and a restarted `moqros-pub` can announce them again.
- **Codec:** `moqros-pub --codec vp8|h264`. H.264 needs the `h264` feature and Cisco's OpenH264 binary:

  ```sh
  export OPENH264_LIBRARY=$(./scripts/fetch_openh264.sh)   # downloads from ciscobinary.openh264.org
  cargo run -p moqros-cli --features h264 --bin moqros-pub -- --codec h264 ...
  ```

### ROS arguments and parameters

Both bridges accept the standard `--ros-args`: remap the node name (`-r __node:=…`) and namespace
(`-r __ns:=…`), and set ROS parameters with `-p` or `--params-file`. Each command-line flag can also be
set as a parameter of the same name in snake_case (`--keyframe-interval-ms` becomes `keyframe_interval_ms`).
A flag on the command line wins over a parameter, and a parameter wins over environment variables and
defaults. Bool flags also take a value (`--reliable=false`). Parameters are read once at startup, and
parameters that match no flag are ignored with a warning. In a parameter file, key a namespaced node by
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

### Several topics

One `moqros-pub` can publish several topics, and one `moqros-sub` can republish several broadcasts. Repeat
`--topic` or `--broadcast`, and add options after the name, separated by commas:

```sh
moqros-pub --topic /camera/image_raw --topic /imu,format=json --topic /scan,format=cdr,mode=stream
moqros-sub --broadcast camera/image_raw,topic=/remote/image --broadcast imu --broadcast scan,topic=/remote/scan
```

| Bridge | Options | Defaults |
| --- | --- | --- |
| `moqros-pub --topic TOPIC,…` | `format`, `type`, `mode`, `broadcast` | `--format` and `--mode` for every topic. The broadcast name is the topic without its leading slash. |
| `moqros-sub --broadcast NAME,…` | `topic`, `type`, `frame_id` | `--frame-id` for every broadcast. The topic is `/moqros/<broadcast>` (`/moqros/image` for a lone broadcast). |

`moqros-bridge` does both in one node, e.g. for a robot that streams its camera and takes velocity commands.
It takes `--topic` like `moqros-pub` and `--broadcast` like `moqros-sub`, with the same options. The flags
that name a single topic or broadcast (`--type`, and the other side's name) are left out; use the options instead:

```sh
moqros-bridge --topic /camera/image_raw --topic /odom,format=json --broadcast cmd_vel,topic=/cmd_vel
```

Each topic becomes a broadcast of its own. `--type` and `--broadcast` of `moqros-pub`, and `--topic` and
`--type` of `moqros-sub`, name a single topic, so they only work with one `--topic` or `--broadcast`. Video
topics start at once. A data topic without a `type` starts when it appears in the ROS graph. `moqros-sub`
waits for all its broadcasts at once, and starts each one when it's announced. It skips a broadcast whose
topic it can't create (e.g. a message type this build has no bindings for), and goes on with the rest.
Otherwise, each bridge exits when any of its topics fails.

### Other message types

`moqros-pub --format json|cdr` publishes a topic of any message type. The default, `--format video`, is for
`sensor_msgs/msg/Image`.

```sh
moqros-pub --topic /imu --format json                  # the type is looked up in the ROS graph
moqros-pub --topic /scan,format=cdr,type=sensor_msgs/msg/LaserScan,mode=stream
moqros-sub --broadcast imu --topic /moqros/imu         # format and type come from the catalog
```

- **`json`**: each message is one JSON document, in r2r's mapping of the message fields. Any hang consumer can
  read it, including a browser; the [web viewer](#web-viewer) shows the latest message. Large arrays (point
  clouds, images) get big and slow as JSON, so use `cdr` for those.
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
  `std_msgs`, `sensor_msgs`, `geometry_msgs`, `nav_msgs`, and `rosgraph_msgs`).

`moqros-sub` handles both kinds of broadcast: it republishes video as `rgb8` images, and data tracks as their
message type. It fails if a broadcast comes back carrying something different from before.

## Web viewer

`examples/web` is a React and TypeScript app built on [`@moq/watch`](https://www.npmjs.com/package/@moq/watch).
It shows every broadcast the relay announces side by side: a video panel for each image broadcast, and the
latest message of each data broadcast. Its view is kept in the URL, so it can be bookmarked or shared. See
[its README](examples/web/README.md) for the query parameters, custom data renderers, and development.

The hosted copy at <https://yuma-m.github.io/moqros/> connects to `http://localhost:4443/anon` by default;
set the Relay URL field or the `url` parameter for another relay. Since it's served over HTTPS, it can only
reach an `http://` relay on `localhost`. Elsewhere, give the relay a real certificate and use `https://`.

## Cloudflare MoQ relay

The bridges and the viewer also work with [Cloudflare's MoQ relays](https://developers.cloudflare.com/moq/),
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

## Library usage

The library is published on [crates.io](https://crates.io/crates/moqros) as `moqros`:

```toml
[dependencies]
moqros = { version = "0.4", features = ["ros"] }   # omit `ros` for the ROS-independent core
```

```rust
use moqros::ros::{QosProfile, spawn_ros_to_moq};

// `node` is an r2r node that you spin.
let publisher = moqros::Publisher::connect(&moqros::ClientConfig::new("http://localhost:4443/anon".parse()?))?;
let broadcast = publisher.create_image_broadcast("camera", Default::default())?;
let bridge = spawn_ros_to_moq(&mut node, "/camera/image_raw", QosProfile::sensor_data(), broadcast)?;
```

See [docs.rs](https://docs.rs/moqros) for the API, data broadcasts, use without ROS, the codec features
(VP8 by default, H.264 through Cisco's OpenH264 binary), and the format on the wire.

## Security

The Docker sample is meant for one machine. Before you expose a relay beyond localhost, keep these in mind:

- **Anyone who reaches the relay can watch, and publish.** `docker/relay.toml` grants anonymous publish *and*
  subscribe under `anon/` (`[auth] public = "anon/**"`). With port 4443 reachable, anyone who knows the
  address can watch every broadcast, and can publish broadcasts of their own. Broadcast names aren't secret:
  subscribers can list what's announced under a path.
- **Published broadcasts can reach ROS.** `moqros-sub` republishes whatever arrives on its broadcasts as ROS
  messages. On an open relay, someone else can publish under the same name and inject messages into your
  ROS graph, e.g. `geometry_msgs/msg/Twist` on a command topic.
- **Decoded pictures are capped.** A few bytes of bitstream can declare a huge picture, so `moqros-sub`
  rejects pictures above 8K UHD (7680×4320 pixels) instead of allocating them, and resubscribes. In the
  library, raise or lower the cap with `ImageSubscriber::set_max_pixels` or `Decoder::set_max_pixels`.
- **The compose file listens on all interfaces.** Its `ports:` entries bind 4443 (relay) and 8080 (viewer)
  on every host interface, and on Linux Docker's port publishing bypasses firewalls such as ufw. To keep the
  sample local, bind to loopback (`"127.0.0.1:4443:4443/udp"`, `"127.0.0.1:4443:4443/tcp"`,
  `"127.0.0.1:8080:80"`).
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
viewer's relay URL (the Relay URL field or the `url` query parameter of `examples/web`). A token's `root` is
the path it's valid under, and its publish and subscribe patterns say what it may do there. The viewer keeps the
token out of its address bar, so links copied from it don't carry the token.

The bridges accept a token too (`--url "https://relay.example.com/robot?jwt=…"`), but prefer certificates:
a URL ends up in shell history and process lists, and the bridges reuse it on every reconnect without asking
for a new token, so once it expires they can't reconnect. They do hide the token (`jwt=REDACTED`, or a
JWT-shaped path segment as `/REDACTED`) when they log the URL.

## Development

The core library builds and tests natively on any OS. ROS isn't required for it, but the default `vp8`
feature needs libvpx, pkg-config, and libclang (bindgen generates the libvpx bindings):

```sh
# Debian/Ubuntu
sudo apt install libvpx-dev pkg-config libclang-dev
# or macOS (libclang comes with the Xcode CLI tools)
# brew install libvpx pkgconf

cargo test -p moqros
# H.264 tests (skipped unless OPENH264_LIBRARY is set):
# OPENH264_LIBRARY=$(./scripts/fetch_openh264.sh) cargo test -p moqros --features h264

# Integration test through a real relay:
cargo install moq-relay
MOQROS_RELAY_BIN=$(which moq-relay) cargo test -p moqros --test relay
```

The `ros` feature, `moqros-cli`, and `examples/image_source` need a sourced ROS 2 environment and libclang,
because r2r generates its bindings at build time. Use the Docker `build` stage, or on a ROS machine:

```sh
source /opt/ros/jazzy/setup.bash
IDL_PACKAGE_FILTER="std_msgs;sensor_msgs;geometry_msgs;nav_msgs;rosgraph_msgs" cargo build --workspace
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Dependencies have their own licenses. libvpx is BSD-3-Clause, and the `env-libvpx-sys` bindings are MPL-2.0.
Cisco's OpenH264 binary is covered by [Cisco's binary license](https://www.openh264.org/BINARY_LICENSE.txt).
