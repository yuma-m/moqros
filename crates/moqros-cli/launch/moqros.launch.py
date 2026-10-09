"""Bridge a ROS 2 image topic to a MoQ relay and back.

    ros2 launch moqros-cli moqros.launch.py url:=http://localhost:4443/anon namespace:=camera

Run one instance per camera with distinct namespaces; the broadcast name is
`<namespace>/image_raw`, so they don't collide on a shared relay.
"""

from launch import LaunchDescription
from launch.actions import DeclareLaunchArgument
from launch.substitutions import EnvironmentVariable, LaunchConfiguration, TextSubstitution
from launch_ros.actions import Node


def generate_launch_description():
    url = LaunchConfiguration("url")
    namespace = LaunchConfiguration("namespace")
    broadcast = [namespace, TextSubstitution(text="/image_raw")]
    return LaunchDescription(
        [
            DeclareLaunchArgument(
                "url",
                default_value=EnvironmentVariable("MOQROS_URL", default_value="http://localhost:4443/anon"),
            ),
            DeclareLaunchArgument("namespace", default_value="camera"),
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
    )
