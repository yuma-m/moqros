"""Bridge a ROS 2 image topic to a MoQ relay and back.

    ros2 launch moqros-cli moqros.launch.py url:=http://localhost:4443/anon
"""

from launch import LaunchDescription
from launch.actions import DeclareLaunchArgument
from launch.substitutions import LaunchConfiguration
from launch_ros.actions import Node


def generate_launch_description():
    url = LaunchConfiguration("url")
    return LaunchDescription(
        [
            DeclareLaunchArgument("url", default_value="http://localhost:4443/anon"),
            DeclareLaunchArgument("namespace", default_value="camera"),
            Node(
                package="moqros-cli",
                executable="moqros-pub",
                namespace=LaunchConfiguration("namespace"),
                parameters=[{"url": url, "topic": "image_raw", "bitrate": 2_000_000}],
            ),
            Node(
                package="moqros-cli",
                executable="moqros-sub",
                namespace=LaunchConfiguration("namespace"),
                parameters=[{"url": url, "broadcast": "image_raw", "topic": "image_moq"}],
            ),
        ]
    )
