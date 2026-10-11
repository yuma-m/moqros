"""Bridge a ROS 2 image topic to a MoQ relay and back.

    ros2 launch moqros-cli moqros.launch.py url:=http://localhost:4443/anon namespace:=camera

To authenticate with a client certificate (mTLS), pass its PEM files:

    ros2 launch moqros-cli moqros.launch.py url:=https://relay.example.com/robot \
        tls_cert:=/etc/moqros/robot.pem tls_key:=/etc/moqros/robot.key

Run one instance per camera with distinct namespaces; the broadcast name is
`<namespace>/image_raw` (without a leading slash), so they don't collide on a
shared relay.
"""

from launch import LaunchDescription
from launch.actions import DeclareLaunchArgument, OpaqueFunction
from launch.substitutions import EnvironmentVariable, LaunchConfiguration
from launch_ros.actions import Node


def broadcast_name(namespace):
    """`/front` or `front/` -> `front/image_raw`; an empty namespace -> `image_raw`."""
    return "/".join(part for part in (namespace.strip("/"), "image_raw") if part)


def relay_parameters(context):
    """The relay URL, and the client certificate when one is given."""
    params = {"url": LaunchConfiguration("url")}
    for name in ("tls_cert", "tls_key"):
        value = LaunchConfiguration(name).perform(context)
        if value:
            params[name] = value
    return params


def launch_nodes(context):
    relay = relay_parameters(context)
    namespace = LaunchConfiguration("namespace").perform(context)
    broadcast = broadcast_name(namespace)
    return [
        Node(
            package="moqros-cli",
            executable="moqros-pub",
            output="screen",
            namespace=namespace,
            parameters=[{**relay, "topic": "image_raw", "broadcast": broadcast, "bitrate": 2_000_000}],
        ),
        Node(
            package="moqros-cli",
            executable="moqros-sub",
            output="screen",
            namespace=namespace,
            parameters=[{**relay, "broadcast": broadcast, "topic": "image_moq"}],
        ),
    ]


def generate_launch_description():
    return LaunchDescription(
        [
            DeclareLaunchArgument(
                "url",
                default_value=EnvironmentVariable("MOQROS_URL", default_value="http://localhost:4443/anon"),
            ),
            DeclareLaunchArgument("namespace", default_value="camera"),
            DeclareLaunchArgument(
                "tls_cert",
                default_value=EnvironmentVariable("MOQROS_TLS_CERT", default_value=""),
                description="PEM client certificate for mTLS; set it with tls_key",
            ),
            DeclareLaunchArgument(
                "tls_key",
                default_value=EnvironmentVariable("MOQROS_TLS_KEY", default_value=""),
                description="PEM private key of tls_cert",
            ),
            OpaqueFunction(function=launch_nodes),
        ]
    )
