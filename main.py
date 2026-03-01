from pyrogram import Client, filters

from config import config
from utils import URL_PATTERN
from download import download_and_upload


app = Client("userbot",api_id=int(config.API_ID),api_hash=config.API_HASH)


@app.on_message(
    filters.private
    & filters.user(config.ALLOWED_USERS)
    & filters.text
    & ~filters.command("h")
)
async def handle_message(client, message):
    """Handle incoming messages from allowed users."""
    urls = URL_PATTERN.findall(message.text)

    if urls:
        await download_and_upload(client, message, urls[0])
    else:
        pass


@app.on_message(
    filters.command("insta") 
    & filters.private 
    & filters.user(config.ALLOWED_USERS)
)
async def handle_insta_command(client, message):
    """Allow authorized users to refresh Instagram cookies from chat."""
    authorized_users = config.SUPER_USERS or config.ALLOWED_USERS
    if message.from_user.id not in authorized_users:
        await message.reply("You are not authorized to use this command.", quote=True)
        return

    parts = (message.text or "").split(maxsplit=1)
    if len(parts) < 2 or not parts[1].strip():
        await message.reply(
            "Please provide the cookie content after the command.",
            quote=True,
        )
        return

    try:
        with open("instacookies.txt", "w") as cookie_file:
            cookie_file.write(parts[1].strip())
    except OSError as exc:
        await message.reply(f"Failed to update cookies: {exc}", quote=True)
        return

    await message.reply("Instagram cookies updated successfully.", quote=True)


@app.on_message(
    filters.command("h")
    & filters.private 
    & filters.user(config.ALLOWED_USERS))
async def handle_h_command(client, message):
    """Handle the /h command to check if the bot is alive."""
    await message.reply("alive", quote=True)


print("Userbot is running...")

app.run()
