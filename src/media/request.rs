use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct DownloadLimits {
    pub max_upload_bytes: u64,
    pub command_timeout: Duration,
    pub upload_timeout: Duration,
    pub job_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadRequest {
    pub url: String,
    pub mode: DownloadMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadMode {
    Video { max_height: Option<u32> },
    Audio,
}

impl DownloadRequest {
    pub fn video(url: String, max_height: Option<u32>) -> Self {
        Self {
            url,
            mode: DownloadMode::Video { max_height },
        }
    }

    pub fn audio(url: String) -> Self {
        Self {
            url,
            mode: DownloadMode::Audio,
        }
    }
}

pub enum DownloadOutcome {
    Completed,
    Failed,
    Cancelled,
}
