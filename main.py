from pyrogram import Client
from config import api_id, api_hash, bot_token, url_pattern, bot_owner
from download import download_and_upload
from utils import check_user_access
from pyrogram import filters

app = Client("bot", api_id=api_id, api_hash=api_hash, bot_token=bot_token)


@app.on_message(filters.command("start"))
async def start(client, message):
    """Handle the /start command."""
    user_id = message.from_user.id
    if check_user_access(user_id):
        await message.reply(
            "send a link. sometimes cookies can expire (eg. instagram) it can take a while for me to refresh them, please be patient. also this bot is in alpha stage, don't expect it to be flawless"
        )
    else:
        if bot_owner is None:
            blacklist = "your user id needs to be whitelisted. please contact an admin"
        else:
            blacklist = (
                f"your user id needs to be whitelisted. please contact @{bot_owner}"
            )
        await message.reply(blacklist)


@app.on_message(filters.text & ~filters.command("start") & ~filters.command("h"))
async def handle_message(client, message):
    """Handle incoming messages."""
    user_id = message.from_user.id
    if not check_user_access(user_id):
        if bot_owner is None:
            blocklist = "you are not allowed to use this bot. contact an admin"
        else:
            blocklist = f"you are not allowed to use this bot. contact @{bot_owner}"
        await message.reply(blocklist)
        return

    urls = url_pattern.findall(message.text)

    if urls:
        await download_and_upload(client, message, urls[0])
    else:
        await message.reply("no valid link found in the message")


@app.on_message(filters.command("h"))
async def handle_h_command(client, message):
    """Handle the /h command to check if the bot is alive."""
    await message.reply("alive")


print("Bot is running...")

app.run()
