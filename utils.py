import subprocess
import random
import string

from config import allowed_user_ids


def check_user_access(user_id):
    return user_id in allowed_user_ids


def generate_random_filename(extension=""):
    """Generate a random filename with a given extension."""
    return (
        "".join(random.choices(string.ascii_letters + string.digits, k=10))
        + "frrr"
        + extension
    )


def extract_thumbnail(video_file):
    """Extract a thumbnail from the video and return the thumbnail filename."""
    try:
        thumbnail_filename = generate_random_filename(".jpg")
        extract_result = subprocess.run(
            [
                "ffmpeg",
                "-y",
                "-i",
                video_file,
                "-ss",
                "00:00:01.000",
                "-vframes",
                "1",
                thumbnail_filename,
            ],
            capture_output=True,
            text=True,
        )

        if extract_result.returncode != 0:
            raise Exception(
                f"failed to extract thumbnail. error: {extract_result.stderr}"
            )

        resize_result = subprocess.run(
            [
                "ffmpeg",
                "-y",
                "-i",
                thumbnail_filename,
                "-vf",
                "scale=320:-1",
                thumbnail_filename,
            ],
            capture_output=True,
            text=True,
        )

        if resize_result.returncode != 0:
            raise Exception(
                f"failed to resize thumbnail. error: {resize_result.stderr}"
            )

        return thumbnail_filename

    except Exception as e:
        raise e


def get_video_duration(video_file):
    """Get the duration of the video in seconds."""
    try:
        ffprobe_command = [
            "ffprobe",
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            video_file,
        ]
        duration_result = subprocess.run(
            ffprobe_command, capture_output=True, text=True
        )

        if duration_result.returncode != 0:
            raise Exception(
                f"failed to get video duration. error: {duration_result.stderr}"
            )

        duration = float(duration_result.stdout.strip())
        return int(duration)
    except Exception as e:
        raise e


def get_video_dimensions(video_file):
    """Get the width and height of the video."""
    try:
        ffprobe_command = [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=s=x:p=0",
            video_file,
        ]
        dimensions_result = subprocess.run(
            ffprobe_command, capture_output=True, text=True
        )

        if dimensions_result.returncode != 0:
            raise Exception(
                f"failed to get video dimensions. error: {dimensions_result.stderr}"
            )

        width, height = map(int, dimensions_result.stdout.strip().split("x"))
        return width, height
    except Exception as e:
        raise e
