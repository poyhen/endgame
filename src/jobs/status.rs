use std::time::Duration;

use crate::jobs::progress::JobPhase;

pub fn render(id: u64, phase: &JobPhase, failure: Option<&str>) -> String {
    match phase {
        JobPhase::Queued => format!("Job #{id} queued."),
        JobPhase::Downloading {
            percent: Some(percent),
        } => format!("Job #{id} is downloading: {percent}%."),
        JobPhase::Downloading { percent: None } => format!("Job #{id} is downloading."),
        JobPhase::Inspecting => format!("Job #{id} is inspecting media."),
        JobPhase::Transcoding {
            percent: Some(percent),
        } => format!("Job #{id} is transcoding compatible media: {percent}%."),
        JobPhase::Transcoding { percent: None } => {
            format!("Job #{id} is transcoding compatible media.")
        }
        JobPhase::Thumbnailing => format!("Job #{id} is preparing a thumbnail."),
        JobPhase::Uploading {
            current,
            total,
            bytes,
        } => render_uploading(id, *current, *total, *bytes),
        JobPhase::Finalizing => format!("Job #{id} is finalizing delivery."),
        JobPhase::Cancelling => format!("Job #{id} is cancelling…"),
        JobPhase::Cancelled => format!("Job #{id} was cancelled."),
        JobPhase::Completed => match failure {
            Some(reason) => format!("Job #{id} completed with warnings: {reason}"),
            None => format!("Job #{id} completed."),
        },
        JobPhase::Failed => match failure {
            Some(reason) => format!("Job #{id} failed: {reason}"),
            None => format!("Job #{id} failed."),
        },
    }
}

pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

pub(crate) fn render_uploading(
    id: u64,
    current: usize,
    total: usize,
    bytes: Option<(u64, u64)>,
) -> String {
    let Some((uploaded, size)) = bytes else {
        return format!("Job #{id} is uploading item {current}/{total}.");
    };
    let percentage = if size == 0 {
        100
    } else {
        ((uploaded as u128 * 100) / size as u128) as u64
    };
    format!(
        "Job #{id} is uploading item {current}/{total}: {percentage}% • {} left.",
        format_bytes(size.saturating_sub(uploaded))
    )
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let precision = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{value:.precision$} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_status_and_units_are_readable() {
        assert_eq!(
            render_uploading(15, 1, 1, Some((800, 2_000))),
            "Job #15 is uploading item 1/1: 40% • 1.17 KiB left."
        );
        assert_eq!(format_bytes(1024), "1.00 KiB");
    }

    #[test]
    fn durations_are_human_readable() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_duration(Duration::from_secs(7_500)), "2h 5m");
    }
}
