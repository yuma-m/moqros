"""Bridge a ROS 2 image topic to a MoQ relay and back.

    ros2 launch moqros-cli moqros.launch.py url:=http://localhost:4443/anon namespace:=camera

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


def launch_nodes(context):
    url = LaunchConfiguration("url")
    namespace = LaunchConfiguration("namespace").perform(context)
    broadcast = broadcast_name(namespace)
    return [
        Node(
            package="moqros-cli",
            executable="moqros-pub",
            output="screen",
            namespace=namespace,
            parameters=[{"url": url, "topic": "image_raw", "broadcast": broadcast, "bitrate": 2_000_000}],
        ),
        Node(
            package="moqros-cli",
            executable="moqros-sub",
            output="screen",
            namespace=namespace,
            parameters=[{"url": url, "broadcast": broadcast, "topic": "image_moq"}],
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
            OpaqueFunction(function=launch_nodes),
        ]
    )
