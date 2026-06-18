use regex::Regex;

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub allowed_user_ids: Vec<i64>,
    pub super_users: Vec<i64>,
    pub url_pattern: Regex,
}

fn parse_id_list(value: &str) -> Result<Vec<i64>, String> {
    value
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<i64>()
                .map_err(|e| format!("invalid user id {s:?}: {e}"))
        })
        .collect()
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let api_id = std::env::var("API_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("API ID must be a non-empty integer or string"))?
            .parse::<i32>()
            .map_err(|_| anyhow::anyhow!("API_ID must be an integer"))?;

        let api_hash = std::env::var("API_HASH")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow::anyhow!("API Hash must be a non-empty string"))?;

        let allowed_user_ids =
            parse_id_list(&std::env::var("ALLOWED_USER_IDS").unwrap_or_default())
                .map_err(|e| anyhow::anyhow!("ALLOWED_USER_IDS: {e}"))?;
        if allowed_user_ids.is_empty() {
            anyhow::bail!(
                "ALLOWED_USER_IDS environment variable must be set with at least one user ID"
            );
        }

        let super_users = parse_id_list(&std::env::var("SUPERUSERS").unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("SUPERUSERS: {e}"))?;

        // NOTE: `$-_` is an ASCII range (0x24..=0x5F) that includes `/`, `:`, `?`, `=`, etc.
        // This mirrors the original Python regex `[$-_@.&+]` exactly (the `-` is NOT escaped).
        let url_pattern =
            Regex::new(r"https?://(?:[$-_@.&+!*(),a-zA-Z0-9]|(?:%[0-9a-fA-F]{2}))+")?;

        Ok(Self {
            api_id,
            api_hash,
            allowed_user_ids,
            super_users,
            url_pattern,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern() -> Regex {
        Regex::new(r"https?://(?:[$-_@.&+!*(),a-zA-Z0-9]|(?:%[0-9a-fA-F]{2}))+").unwrap()
    }

    #[test]
    fn captures_full_url_with_path() {
        let re = pattern();
        let msg = "https://x.com/musa1907_/status/2067571186532270261";
        let m = re.find(msg).expect("should match");
        assert_eq!(m.as_str(), msg, "must include the path after the host");
    }

    #[test]
    fn captures_query_chars() {
        let re = pattern();
        let url = "https://www.youtube.com/watch?v=abc-123_456&t=10s";
        let m = re.find(url).expect("should match");
        assert_eq!(m.as_str(), url);
    }
}
