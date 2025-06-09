import os
import asyncio
import mimetypes  # For checking file type
import shutil  # For removing directories

from utils import (
    generate_random_filename,
    extract_thumbnail,
    get_video_duration,
    get_video_dimensions,
)

# Define a base download directory for gallery-dl
GALLERY_DL_DOWNLOAD_PATH = "gallery_dl_downloads"


async def download_and_upload(client, message, url):
    """Download media from the URL using yt-dlp for videos and gallery-dl for others."""

    # Common variables
    cookies_file = "cookies.txt"
    user_agent = (
        "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0"
    )
    h264_format_selector = "bestvideo[ext=mp4][vcodec=h264]+bestaudio[ext=m4a]/best[ext=mp4][vcodec=h264]/best"

    # Paths for cleanup
    # For yt-dlp, it will be a direct file path. For gallery-dl, a directory.
    path_to_clean = None
    is_yt_dlp_download = False  # Flag to know if path_to_clean is a file or dir
    thumbnail_filename = None

    # Determine which tool to use
    # Prioritize yt-dlp for known video sources that benefit from its specific handling
    use_yt_dlp = False
    if (
        "youtube.com/" in url
        or "youtu.be/" in url
        or "instagram.com/reels/" in url
        or "instagram.com/tv/" in url
        or "tiktok.com/" in url
    ):
        use_yt_dlp = True
    # For all other URLs, including instagram.com/p/, use gallery-dl.
    # gallery-dl is better suited for /p/ links which can be images, videos, or albums.
    # The existing gallery-dl processing logic will handle the downloaded content type.

    try:
        cmd = []
        download_items_from_dir = (
            False  # True if gallery-dl is used and we need to scan a dir
        )
        single_downloaded_file = None  # Path to file if yt-dlp is used

        if use_yt_dlp:
            is_yt_dlp_download = True
            random_filename_base = (
                generate_random_filename()
            )  # Base name without extension
            # yt-dlp will append .%(ext)s, so we'll find it later
            output_template = f"{random_filename_base}.%(ext)s"
            path_to_clean = (
                random_filename_base  # We'll search for files starting with this
            )

            cmd = [
                "yt-dlp",
                "-o",
                output_template,
                "--cookies",
                cookies_file,
                "--user-agent",
                user_agent,
                "-f",
                h264_format_selector,
                url,
            ]
            print(f"Using yt-dlp for URL: {url}")
        else:
            # Use gallery-dl for other URLs (e.g., image galleries)
            download_instance_path = os.path.join(
                GALLERY_DL_DOWNLOAD_PATH, generate_random_filename()
            )
            os.makedirs(download_instance_path, exist_ok=True)
            path_to_clean = download_instance_path
            download_items_from_dir = True

            cmd = [
                "gallery-dl",
                "--cookies",
                cookies_file,
                "--user-agent",
                user_agent,
                "-D",
                download_instance_path,
                url,
            ]
            print(f"Using gallery-dl for URL: {url}")

        process = await asyncio.create_subprocess_exec(
            *cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await process.communicate()
        result = {
            "returncode": process.returncode,
            "stdout": stdout.decode("utf-8") if stdout else "",
            "stderr": stderr.decode("utf-8") if stderr else "",
        }

        downloader_name = "yt-dlp" if use_yt_dlp else "gallery-dl"

        if result.get("returncode") == 0:
            downloaded_media_paths = []
            if use_yt_dlp:
                # Find the file downloaded by yt-dlp
                # It should start with random_filename_base
                found_files = [
                    f for f in os.listdir(".") if f.startswith(random_filename_base)
                ]
                if found_files:
                    single_downloaded_file = found_files[0]
                    downloaded_media_paths.append(single_downloaded_file)
                    path_to_clean = (
                        single_downloaded_file  # Actual file to clean for yt-dlp
                    )
                else:
                    await message.reply(
                        f"{downloader_name} finished, but the downloaded file was not found (expected prefix: {random_filename_base})."
                    )
                    return
            elif download_items_from_dir:  # gallery-dl
                for root, _, files in os.walk(
                    path_to_clean
                ):  # path_to_clean is download_instance_path here
                    for file in files:
                        downloaded_media_paths.append(os.path.join(root, file))

                if not downloaded_media_paths:
                    await message.reply(
                        f"{downloader_name} downloaded successfully, but no files found in {path_to_clean}. Output: {result.get('stdout')}"
                    )
                    return

                # Sort gallery-dl items to prefer videos/images
                downloaded_media_paths.sort(
                    key=lambda x: (
                        not x.lower().endswith((".mp4", ".mkv", ".webm", ".mov")),
                        not x.lower().endswith(
                            (".jpg", ".jpeg", ".png", ".gif", ".webp")
                        ),
                        x,
                    )
                )

            if not downloaded_media_paths:
                await message.reply(
                    f"{downloader_name} did not produce any downloadable files."
                )
                return

            processed_successfully = False
            # Process the first valid media item found
            # For yt-dlp, there should only be one. For gallery-dl, we take the best match.
            item_path_to_process = downloaded_media_paths[0]

            # Ensure the file exists before processing
            if not os.path.exists(item_path_to_process):
                await message.reply(
                    f"Downloaded file {item_path_to_process} seems to have disappeared before processing."
                )
                return

            mime_type, _ = mimetypes.guess_type(item_path_to_process)
            is_video = mime_type and mime_type.startswith("video")
            is_image = mime_type and mime_type.startswith("image")

            if is_video:
                try:
                    # yt-dlp with H.264 format selector should produce compatible mp4.
                    # If ffmpeg conversion is ever needed again for yt-dlp outputs:
                    # if use_yt_dlp and not item_path_to_process.lower().endswith(".mp4"):
                    #     # ... (ffmpeg conversion logic, update item_path_to_process and path_to_clean if original is removed)

                    thumbnail_filename = await extract_thumbnail(item_path_to_process)
                    video_duration = await get_video_duration(item_path_to_process)
                    width, height = await get_video_dimensions(item_path_to_process)

                    await client.send_video(
                        message.chat.id,
                        item_path_to_process,
                        supports_streaming=True,
                        thumb=thumbnail_filename,
                        duration=video_duration,
                        width=width,
                        height=height,
                    )
                    processed_successfully = True
                except Exception as e:
                    await message.reply(
                        f"Error processing video {os.path.basename(item_path_to_process)}: {str(e)}"
                    )

            elif is_image:
                try:
                    await client.send_photo(
                        message.chat.id,
                        photo=item_path_to_process,
                    )
                    processed_successfully = True
                except Exception as e:
                    await message.reply(
                        f"Error sending image {os.path.basename(item_path_to_process)}: {str(e)}"
                    )

            else:  # Not identified as video or image
                if (
                    downloaded_media_paths
                ):  # If gallery-dl downloaded something non-media first
                    await message.reply(
                        f"Downloaded a file ({os.path.basename(item_path_to_process)}) that is not a recognized video or image (MIME: {mime_type})."
                    )
                # If yt-dlp downloaded something non-media (unlikely with format selection)
                # This path should ideally not be hit if format selection works.

            if not processed_successfully and downloaded_media_paths:
                await message.reply(
                    f"{downloader_name} downloaded content, but could not process or send any recognized media file."
                )

        else:  # Download failed
            error_message = result.get("stderr", "Unknown error")
            if not error_message and result.get(
                "stdout"
            ):  # Sometimes errors go to stdout
                error_message = result.get("stdout")
            await message.reply(
                f"{downloader_name} failed to download. Error: {error_message}"
            )

    except Exception as e:
        await message.reply(f"An unexpected error occurred: {str(e)}")
    finally:
        if thumbnail_filename and os.path.exists(thumbnail_filename):
            try:
                os.remove(thumbnail_filename)
            except OSError as e:
                print(f"Error removing thumbnail file {thumbnail_filename}: {e}")

        if path_to_clean and os.path.exists(path_to_clean):
            try:
                if is_yt_dlp_download and os.path.isfile(
                    path_to_clean
                ):  # yt-dlp downloaded a file
                    os.remove(path_to_clean)
                    print(f"Cleaned up yt-dlp file: {path_to_clean}")
                elif not is_yt_dlp_download and os.path.isdir(
                    path_to_clean
                ):  # gallery-dl downloaded to a dir
                    shutil.rmtree(path_to_clean)
                    print(f"Cleaned up gallery-dl directory: {path_to_clean}")
                # Edge case: if yt-dlp was supposed to run but path_to_clean is still the base prefix
                # and no file was actually created and assigned to path_to_clean.
                elif is_yt_dlp_download and not os.path.isfile(path_to_clean):
                    # Try to find files starting with the base prefix again, in case of early exit
                    found_files_for_cleanup = [
                        f for f in os.listdir(".") if f.startswith(path_to_clean)
                    ]
                    for f_clean in found_files_for_cleanup:
                        try:
                            os.remove(f_clean)
                            print(f"Cleaned up orphaned yt-dlp file: {f_clean}")
                        except OSError as e_clean:
                            print(
                                f"Error cleaning up orphaned yt-dlp file {f_clean}: {e_clean}"
                            )

            except OSError as e:
                print(f"Error during cleanup of {path_to_clean}: {e}")
