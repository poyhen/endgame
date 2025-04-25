import asyncio

# Remove unused import
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


async def extract_thumbnail(video_file):
    """Extract a thumbnail from the video and return the thumbnail filename."""
    try:
        thumbnail_filename = generate_random_filename(".jpg")
        process_extract = await asyncio.create_subprocess_exec(
            "ffmpeg",
            "-y",
            "-i",
            video_file,
            "-ss",
            "00:00:01.000",
            "-vframes",
            "1",
            thumbnail_filename,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_extract, stderr_extract = await process_extract.communicate()
        extract_result = {
            "returncode": process_extract.returncode,
            "stderr": stderr_extract.decode("utf-8") if stderr_extract else "",
        }

        if extract_result.get("returncode") != 0:  # Use dict get method for safe access
            raise Exception(
                f"failed to extract thumbnail. error: {extract_result.get('stderr', '')}"  # Use dict get method for safe access
            )

        process_resize = await asyncio.create_subprocess_exec(
            "ffmpeg",
            "-y",
            "-i",
            thumbnail_filename,
            "-vf",
            "scale=320:-1",
            thumbnail_filename,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_resize, stderr_resize = await process_resize.communicate()
        resize_result = {
            "returncode": process_resize.returncode,
            "stderr": stderr_resize.decode("utf-8") if stderr_resize else "",
        }

        if resize_result.get("returncode") != 0:  # Use dict get method for safe access
            raise Exception(
                f"failed to resize thumbnail. error: {resize_result.get('stderr', '')}"  # Use dict get method for safe access
            )

        return thumbnail_filename

    except Exception as e:
        raise e


async def get_video_duration(video_file):
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
        process_duration = await asyncio.create_subprocess_exec(
            *ffprobe_command,  # Unpack the list
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_duration, stderr_duration = await process_duration.communicate()
        duration_result = {
            "returncode": process_duration.returncode,
            "stdout": stdout_duration.decode("utf-8") if stdout_duration else "",
            "stderr": stderr_duration.decode("utf-8") if stderr_duration else "",
        }

        if (
            duration_result.get("returncode") != 0
        ):  # Use dict get method for safe access
            raise Exception(
                f"failed to get video duration. error: {duration_result.get('stderr', '')}"  # Use dict get method for safe access
            )

        duration = float(
            duration_result.get("stdout", "").strip()
        )  # Use dict get method for safe access
        return int(duration)
    except Exception as e:
        raise e


async def get_video_dimensions(video_file):
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
        process_dimensions = await asyncio.create_subprocess_exec(
            *ffprobe_command,  # Unpack the list
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout_dimensions, stderr_dimensions = await process_dimensions.communicate()
        dimensions_result = {
            "returncode": process_dimensions.returncode,
            "stdout": stdout_dimensions.decode("utf-8") if stdout_dimensions else "",
            "stderr": stderr_dimensions.decode("utf-8") if stderr_dimensions else "",
        }

        if (
            dimensions_result.get("returncode") != 0
        ):  # Use dict get method for safe access
            raise Exception(
                f"failed to get video dimensions. error: {dimensions_result.get('stderr', '')}"  # Use dict get method for safe access
            )

        width, height = map(
            int, dimensions_result.get("stdout", "").strip().split("x")
        )  # Use dict get method for safe access
        return width, height
    except Exception as e:
        raise e
