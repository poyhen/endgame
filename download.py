import os
import subprocess

from utils import (
    generate_random_filename,
    extract_thumbnail,
    get_video_duration,
    get_video_dimensions,
)


async def download_and_upload(client, message, url):
    """Download the video from the URL and upload it with a thumbnail."""
    try:
        random_filename = generate_random_filename(".%(ext)s")
        output_template = f"{random_filename}"
        cookies_file = "cookies.txt"
        user_agent = (
            "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0"
        )

        result = subprocess.run(
            [
                "yt-dlp",
                "-o",
                output_template,
                "--cookies",
                cookies_file,
                "--user-agent",
                user_agent,
                "-f",
                "bestvideo[vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best",
                url,
            ],
            capture_output=True,
            text=True,
        )

        if result.returncode == 0:
            downloaded_files = [
                f for f in os.listdir() if f.startswith(random_filename.split(".")[0])
            ]
            if not downloaded_files:
                await message.reply("failed to find the downloaded file")
                return

            downloaded_file = downloaded_files[0]

            if "youtube.com" in url or "youtu.be" in url:
                try:
                    mp4_filename = generate_random_filename(".mp4")
                    convert_result = subprocess.run(
                        [
                            "ffmpeg",
                            "-y",
                            "-i",
                            downloaded_file,
                            "-c",
                            "copy",
                            mp4_filename,
                        ],
                        capture_output=True,
                        text=True,
                    )

                    if convert_result.returncode != 0:
                        await message.reply(
                            f"failed to convert to mp4. error: {convert_result.stderr}"
                        )
                        return

                    downloaded_file = mp4_filename

                except Exception as e:
                    await message.reply(
                        f"an error occurred during conversion: {str(e)}"
                    )
                    return

            try:
                thumbnail_filename = extract_thumbnail(downloaded_file)
            except Exception as e:
                await message.reply(
                    f"an error occurred while extracting thumbnail: {str(e)}"
                )
                return

            try:
                video_duration = get_video_duration(downloaded_file)
            except Exception as e:
                await message.reply(
                    f"an error occurred while getting video duration: {str(e)}"
                )
                return

            try:
                width, height = get_video_dimensions(downloaded_file)
            except Exception as e:
                await message.reply(
                    f"an error occurred while getting video dimensions: {str(e)}"
                )
                return

            await client.send_video(
                message.chat.id,
                downloaded_file,
                supports_streaming=True,
                thumb=thumbnail_filename,
                duration=video_duration,
                width=width,
                height=height,
            )

            os.remove(downloaded_file)
            os.remove(thumbnail_filename)
        else:
            await message.reply(f"failed to download the video. error: {result.stderr}")
    except Exception as e:
        await message.reply(f"an error occurred: {str(e)}")
