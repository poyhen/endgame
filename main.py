from pyrogram import Client, filters

from config import allowed_user_ids, api_hash, api_id, super_users, url_pattern
from download import download_and_upload

if not api_id or not isinstance(api_id, (int, str)):
    raise ValueError("API ID must be a non-empty integer or string")
if not api_hash or not isinstance(api_hash, str):
    raise ValueError("API Hash must be a non-empty string")
if not allowed_user_ids:
    raise ValueError(
        "ALLOWED_USER_IDS environment variable must be set with at least one user ID"
    )

app = Client(
    "userbot",
    api_id=int(api_id) if isinstance(api_id, str) else api_id,
    api_hash=str(api_hash),
)


@app.on_message(
    filters.private
    & filters.user(allowed_user_ids)
    & filters.text
    & ~filters.command("h")
)
async def handle_message(client, message):
    """Handle incoming messages from allowed users."""
    urls = url_pattern.findall(message.text)

    if urls:
        await download_and_upload(client, message, urls[0])
    else:
        pass


@app.on_message(
    filters.command("insta") & filters.private & filters.user(allowed_user_ids)
)
async def handle_insta_command(client, message):
    """Allow authorized users to refresh Instagram cookies from chat."""
    authorized_users = super_users or allowed_user_ids
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


@app.on_message(filters.command("h") & filters.private & filters.user(allowed_user_ids))
async def handle_h_command(client, message):
    """Handle the /h command to check if the bot is alive."""
    await message.reply("alive", quote=True)


print("Userbot is running...")

app.run()
