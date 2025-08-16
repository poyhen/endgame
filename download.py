import os
import asyncio
import mimetypes  # For checking file type
import shutil  # For removing directories

from utils import (
    generate_random_filename,
    extract_thumbnail,
    get_video_duration,
    get_video_dimensions,
    clean_cookie_file,
)

# Define a base download directory for gallery-dl
GALLERY_DL_DOWNLOAD_PATH = "gallery_dl_downloads"


async def download_and_upload(client, message, url):
    """Download media from the URL using yt-dlp for videos and gallery-dl for others."""

    # Common variables
    user_agent = (
        "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0"
    )
    # Default format selector, good for YouTube/Instagram as per old logic
    default_yt_dlp_format_selector = "bestvideo[vcodec!*=av01][vcodec!*=vp9][ext=mp4]+bestaudio[ext=m4a]/best[ext=mp4]/best"
    # Platform-specific format selectors
    PLATFORM_FORMAT_CONFIG = {
        "tiktok.com": "bestvideo[ext=mp4][vcodec=h264]+bestaudio[ext=m4a]/best[ext=mp4][vcodec=h264]/best"
        # Add other platform-specific selectors here if needed in the future
    }

    # Paths for cleanup
    # For yt-dlp, it will be a direct file path. For gallery-dl, a directory.
    path_to_clean = None
    is_yt_dlp_download = False  # Flag to know if path_to_clean is a file or dir

    # Determine which tool to use
    # Prioritize yt-dlp for known video sources that benefit from its specific handling
    use_yt_dlp = False
    if (
        "youtube.com/" in url
        or "youtu.be/" in url
        or "instagram.com/share/" in url
        or "instagram.com/reels/" in url
        or "instagram.com/tv/" in url
        or "x.com/i/broadcasts/" in url
        or "tiktok.com/" in url
    ):
        use_yt_dlp = True
    # For all other URLs, including instagram.com/p/, use gallery-dl.
    # gallery-dl is better suited for /p/ links which can be images, videos, or albums.
    # The existing gallery-dl processing logic will handle the downloaded content type.

    try:
        cookies_file = "cookies.txt"
        if "instagram.com/" in url and os.path.exists("instacookies.txt"):
            cookies_file = "instacookies.txt"
            clean_cookie_file(cookies_file)

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

            # Determine the format selector for yt-dlp
            chosen_format_selector = default_yt_dlp_format_selector
            for domain, fmt_selector in PLATFORM_FORMAT_CONFIG.items():
                if domain in url:
                    chosen_format_selector = fmt_selector
                    break

            cmd = [
                "yt-dlp",
                "-o",
                output_template,
                "--cookies",
                cookies_file,
                "--user-agent",
                user_agent,
                "-f",
                chosen_format_selector,
                url,
            ]
            user_info = f"User {message.from_user.id}"
            if message.from_user.username:
                user_info = (
                    f"User @{message.from_user.username} ({message.from_user.id})"
                )
            print(f"{user_info} | Using yt-dlp for URL: {url}")
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
            user_info = f"User {message.from_user.id}"
            if message.from_user.username:
                user_info = (
                    f"User @{message.from_user.username} ({message.from_user.id})"
                )
            print(f"{user_info} | Using gallery-dl for URL: {url}")

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

            any_item_processed_successfully = False
            items_sent_count = 0

            for item_path_to_process in downloaded_media_paths:
                # Ensure the file exists before processing
                if not os.path.exists(item_path_to_process):
                    if not use_yt_dlp:  # gallery-dl
                        await message.reply(
                            f"Skipping a downloaded file ({os.path.basename(item_path_to_process)}) as it seems to have disappeared before processing."
                        )
                        continue
                    else:  # yt-dlp: single file, if it's gone, it's a failure for this op
                        await message.reply(
                            f"Downloaded file {item_path_to_process} seems to have disappeared before processing."
                        )
                        return  # Exits download_and_upload, finally will run

                mime_type, _ = mimetypes.guess_type(item_path_to_process)
                is_video = mime_type and mime_type.startswith("video")
                is_image = mime_type and mime_type.startswith("image")

                current_item_processed_this_iteration = False
                local_thumbnail_filename = None  # For per-video thumbnail

                if is_video:
                    try:
                        local_thumbnail_filename = await extract_thumbnail(
                            item_path_to_process
                        )
                        video_duration = await get_video_duration(item_path_to_process)
                        width, height = await get_video_dimensions(item_path_to_process)

                        await client.send_video(
                            message.chat.id,
                            item_path_to_process,
                            supports_streaming=True,
                            thumb=local_thumbnail_filename,
                            duration=video_duration,
                            width=width,
                            height=height,
                        )
                        any_item_processed_successfully = True
                        current_item_processed_this_iteration = True
                        items_sent_count += 1
                    except Exception as e:
                        await message.reply(
                            f"Error processing video {os.path.basename(item_path_to_process)}: {str(e)}"
                        )
                    finally:
                        if local_thumbnail_filename and os.path.exists(
                            local_thumbnail_filename
                        ):
                            try:
                                os.remove(local_thumbnail_filename)
                            except OSError as e_thumb:
                                user_info = f"User {message.from_user.id}"
                                if message.from_user.username:
                                    user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                                print(
                                    f"{user_info} | Error removing thumbnail {local_thumbnail_filename} for {item_path_to_process}: {e_thumb}"
                                )
                elif is_image:
                    try:
                        await client.send_photo(
                            message.chat.id,
                            photo=item_path_to_process,
                        )
                        any_item_processed_successfully = True
                        current_item_processed_this_iteration = True
                        items_sent_count += 1
                    except Exception as e:
                        await message.reply(
                            f"Error sending image {os.path.basename(item_path_to_process)}: {str(e)}"
                        )

                if (
                    not current_item_processed_this_iteration
                ):  # Not video or image, or failed before sending
                    if (
                        not use_yt_dlp
                    ):  # gallery-dl might download non-media files (e.g. .txt)
                        user_info = f"User {message.from_user.id}"
                        if message.from_user.username:
                            user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                        print(
                            f"{user_info} | Skipping non-media file from gallery-dl: {os.path.basename(item_path_to_process)} (MIME: {mime_type})"
                        )
                    elif not (
                        is_video or is_image
                    ):  # yt-dlp downloaded something not explicitly video/image
                        await message.reply(
                            f"Downloaded a file ({os.path.basename(item_path_to_process)}) with {downloader_name} that is not a recognized video or image (MIME: {mime_type})."
                        )

            # After the loop, provide summary messages
            if not any_item_processed_successfully and downloaded_media_paths:
                # This covers cases where download happened but no items could be sent
                # (e.g., all files were non-media, or all processing attempts failed)
                user_info = f"User {message.from_user.id}"
                if message.from_user.username:
                    user_info = (
                        f"User @{message.from_user.username} ({message.from_user.id})"
                    )
                print(
                    f"{user_info} | {downloader_name} downloaded content, but could not process or send any recognized media file."
                )
            elif (
                download_items_from_dir and items_sent_count > 0
            ):  # gallery-dl processed some items
                total_items = len(downloaded_media_paths)
                # Filter out known non-media extensions for a more accurate "media item" count if desired,
                # but for now, len(downloaded_media_paths) is the count of all files gallery-dl fetched.
                # A more accurate message might count only actual media files attempted.
                # For simplicity, we'll use items_sent_count vs total files found in dir.
                if items_sent_count == total_items:
                    user_info = f"User {message.from_user.id}"
                    if message.from_user.username:
                        user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                    print(
                        f"{user_info} | Finished processing gallery. Sent all {items_sent_count} item(s)."
                    )
                else:
                    # This message implies some files in the gallery download might not have been sendable media
                    # or encountered errors.
                    user_info = f"User {message.from_user.id}"
                    if message.from_user.username:
                        user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                    print(
                        f"{user_info} | Finished processing gallery. Sent {items_sent_count} item(s) from {total_items} downloaded file(s)."
                    )
            # If yt-dlp, individual success (item sent) or failure (error message / 'not recognized media' message) is handled within the loop or by the 'not any_item_processed_successfully' condition.

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
        if path_to_clean and os.path.exists(path_to_clean):
            try:
                if is_yt_dlp_download and os.path.isfile(
                    path_to_clean
                ):  # yt-dlp downloaded a file
                    os.remove(path_to_clean)
                    user_info = f"User {message.from_user.id}"
                    if message.from_user.username:
                        user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                    print(f"{user_info} | Cleaned up yt-dlp file: {path_to_clean}")
                elif not is_yt_dlp_download and os.path.isdir(
                    path_to_clean
                ):  # gallery-dl downloaded to a dir
                    shutil.rmtree(path_to_clean)
                    user_info = f"User {message.from_user.id}"
                    if message.from_user.username:
                        user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                    print(
                        f"{user_info} | Cleaned up gallery-dl directory: {path_to_clean}"
                    )
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
                            user_info = f"User {message.from_user.id}"
                            if message.from_user.username:
                                user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                            print(
                                f"{user_info} | Cleaned up orphaned yt-dlp file: {f_clean}"
                            )
                        except OSError as e_clean:
                            user_info = f"User {message.from_user.id}"
                            if message.from_user.username:
                                user_info = f"User @{message.from_user.username} ({message.from_user.id})"
                            print(
                                f"{user_info} | Error cleaning up orphaned yt-dlp file {f_clean}: {e_clean}"
                            )

            except OSError as e:
                user_info = f"User {message.from_user.id}"
                if message.from_user.username:
                    user_info = (
                        f"User @{message.from_user.username} ({message.from_user.id})"
                    )
                print(f"{user_info} | Error during cleanup of {path_to_clean}: {e}")
