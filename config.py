import os
from dataclasses import dataclass, field

def require(name: str) -> str:
    value = os.getenv(name)
    if value is None:
       sys.exit(f"Missing required environment variable: {name}")
    return value


def optional(name: str, default: str = "") -> str:
    return os.getenv(name, default)

@dataclass(frozen=True)
class Config:
    API_ID: str
    API_HASH: str
    ALLOWED_USERS: list[int] = field(default_factory=list)
    SUPER_USERS: list[int] = field(default_factory=list)

    @classmethod
    def from_env(cls) -> "Config":
        allowed_users = optional("ALLOWED_USERS", "").split(",")
        super_users = optional("SUPERUSERS", "").split(",")

        return cls(
            API_ID=require("API_ID"),
            API_HASH=require("API_HASH"),
            ALLOWED_USERS=list(map(int, allowed_users)),
            SUPER_USERS=list(map(int, super_users)),
        )

config = Config()