import os
import re

api_id = os.getenv("API_ID")
api_hash = os.getenv("API_HASH")
bot_token = os.getenv("BOT_TOKEN")
bot_owner = os.getenv("BOT_OWNER")
allowed_user_ids = list(map(int, os.getenv("ALLOWED_USER_IDS").split(",")))

url_pattern = re.compile(
    r"http[s]?://(?:[a-zA-Z]|[0-9]|[$-_@.&+]|[!*\\(\\),]|(?:%[0-9a-fA-F][0-9a-fA-F]))+"
)
