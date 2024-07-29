from pyrogram import Client, filters
import re
import subprocess
import os
import random
import string
import json

# Fetch API credentials from environment variables
api_id = os.getenv('API_ID')
api_hash = os.getenv('API_HASH')
bot_token = os.getenv('BOT_TOKEN')

allowed_user_ids = list(map(int, os.getenv('ALLOWED_USER_IDS').split(',')))


# Initialize the Pyrogram Client
app = Client("bot", api_id=api_id, api_hash=api_hash, bot_token=bot_token)

# Regular expression to detect URLs
url_pattern = re.compile(r'http[s]?://(?:[a-zA-Z]|[0-9]|[$-_@.&+]|[!*\\(\\),]|(?:%[0-9a-fA-F][0-9a-fA-F]))+')

def generate_random_filename(extension=''):
    """Generate a random filename with a given extension."""
    return ''.join(random.choices(string.ascii_letters + string.digits, k=10)) + extension

def extract_thumbnail(video_file):
    """Extract a thumbnail from the video and return the thumbnail filename."""
    try:
        thumbnail_filename = generate_random_filename('.jpg')
        # Extract thumbnail
        extract_result = subprocess.run(
            ['ffmpeg', '-y', '-i', video_file, '-ss', '00:00:01.000', '-vframes', '1', thumbnail_filename],
            capture_output=True, text=True
        )

        if extract_result.returncode != 0:
            raise Exception(f'Failed to extract thumbnail. Error: {extract_result.stderr}')

        # Resize the thumbnail if needed
        resize_result = subprocess.run(
            ['ffmpeg', '-y', '-i', thumbnail_filename, '-vf', 'scale=320:240', thumbnail_filename],
            capture_output=True, text=True
        )

        if resize_result.returncode != 0:
            raise Exception(f'Failed to resize thumbnail. Error: {resize_result.stderr}')

        return thumbnail_filename

    except Exception as e:
        raise e

def get_video_duration(video_file):
    """Get the duration of the video in seconds."""
    try:
        ffprobe_command = [
            'ffprobe', '-v', 'error', '-show_entries',
            'format=duration', '-of',
            'default=noprint_wrappers=1:nokey=1', video_file
        ]
        duration_result = subprocess.run(ffprobe_command, capture_output=True, text=True)

        if duration_result.returncode != 0:
            raise Exception(f'Failed to get video duration. Error: {duration_result.stderr}')

        duration = float(duration_result.stdout.strip())
        return int(duration)
    except Exception as e:
        raise e

async def download_and_upload(client, message, url):
    """Download the video from the URL and upload it with a thumbnail."""
    try:
        # Generate a random filename for yt-dlp output
        random_filename = generate_random_filename('.%(ext)s')
        output_template = f'{random_filename}'

        # Run yt-dlp to download the video
        result = subprocess.run(['yt-dlp', '-o', output_template, url], capture_output=True, text=True)

        if result.returncode == 0:

            # Find the downloaded file
            downloaded_files = [f for f in os.listdir() if f.startswith(random_filename.split('.')[0])]
            if not downloaded_files:
                await message.reply('Failed to find the downloaded file.')
                return

            downloaded_file = downloaded_files[0]

            # Check if the URL is from YouTube
            if 'youtube.com' in url or 'youtu.be' in url:
                try:
                    # Convert the video to MP4 using fast copy
                    mp4_filename = generate_random_filename('.mp4')
                    convert_result = subprocess.run(
                        ['ffmpeg', '-y', '-i', downloaded_file, '-c', 'copy', mp4_filename],
                        capture_output=True, text=True
                    )

                    if convert_result.returncode != 0:
                        await message.reply(f'Failed to convert to MP4. Error: {convert_result.stderr}')
                        return

                    # Replace the downloaded file with the MP4 file
                    downloaded_file = mp4_filename

                except Exception as e:
                    await message.reply(f'An error occurred during conversion: {str(e)}')
                    return

            # Extract the thumbnail
            try:
                thumbnail_filename = extract_thumbnail(downloaded_file)
            except Exception as e:
                await message.reply(f'An error occurred while extracting thumbnail: {str(e)}')
                return

            # Get video duration
            try:
                video_duration = get_video_duration(downloaded_file)
            except Exception as e:
                await message.reply(f'An error occurred while getting video duration: {str(e)}')
                return

            # Upload the video file with thumbnail
            await client.send_video(
                message.chat.id,
                downloaded_file,
                supports_streaming=True,
                thumb=thumbnail_filename,
                duration=video_duration
            )

            # Optionally, delete the files after uploading
            os.remove(downloaded_file)
            os.remove(thumbnail_filename)
        else:
            await message.reply(f'Failed to download the video. Error: {result.stderr}')
    except Exception as e:
        await message.reply(f'An error occurred: {str(e)}')

def check_user_access(user_id):
    return user_id in allowed_user_ids

@app.on_message(filters.command("start"))
async def start(client, message):
    """Handle the /start command."""
    user_id = message.from_user.id
    if check_user_access(user_id):
        await message.reply('Send a link to get the video.')
    else:
        await message.reply('You are not allowed to use this application.')

@app.on_message(filters.text & ~filters.command("start"))
async def handle_message(client, message):
    """Handle incoming messages."""
    user_id = message.from_user.id
    if not check_user_access(user_id):
        await message.reply('You are not allowed to use this application.')
        return

    if url_pattern.search(message.text):
        await download_and_upload(client, message, message.text)
    else:
        await message.reply('This is not a link.')

print("Bot is running...")
app.run()
