use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::jobs::JobProgress;
use crate::media::command;

pub async fn read_transcode(
    reader: impl AsyncRead + Unpin,
    progress: JobProgress,
    duration_micros: u64,
) -> Vec<u8> {
    read_lines(
        reader,
        move |line| transcode_percent(line, duration_micros),
        move |bucket| progress.transcoding(Some(bucket)),
    )
    .await
}

pub async fn read_download(reader: impl AsyncRead + Unpin, progress: JobProgress) -> Vec<u8> {
    read_lines(reader, download_percent, move |bucket| {
        progress.download_progress(bucket)
    })
    .await
}

async fn read_lines(
    reader: impl AsyncRead + Unpin,
    mut parse: impl FnMut(&[u8]) -> Option<u8>,
    mut report: impl FnMut(u8),
) -> Vec<u8> {
    let mut reader = BufReader::new(reader);
    let mut output = Vec::with_capacity(command::OUTPUT_LIMIT);
    let mut line = Vec::new();
    let mut last_bucket = 0u8;
    loop {
        line.clear();
        let Ok(read) = reader.read_until(b'\n', &mut line).await else {
            break;
        };
        if read == 0 {
            break;
        }
        output.extend_from_slice(&line);
        trim_output(&mut output);

        if let Some(percent) = parse(&line) {
            let bucket = percent / 5 * 5;
            if bucket >= last_bucket.saturating_add(5) {
                last_bucket = bucket;
                report(bucket);
            }
        }
    }
    if output.len() > command::OUTPUT_LIMIT {
        output.drain(..output.len() - command::OUTPUT_LIMIT);
    }
    output
}

fn trim_output(output: &mut Vec<u8>) {
    if output.len() > command::OUTPUT_LIMIT * 2 {
        output.drain(..output.len() - command::OUTPUT_LIMIT);
    }
}

fn download_percent(line: &[u8]) -> Option<u8> {
    let value = String::from_utf8_lossy(line)
        .trim()
        .strip_prefix("download:")?
        .trim()
        .trim_end_matches('%')
        .trim()
        .parse::<f64>()
        .ok()?;
    Some(value.clamp(0.0, 100.0) as u8)
}

fn transcode_percent(line: &[u8], duration_micros: u64) -> Option<u8> {
    if duration_micros == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(line);
    let elapsed = text
        .trim()
        .strip_prefix("out_time_us=")
        .or_else(|| text.trim().strip_prefix("out_time_ms="))?
        .parse::<u64>()
        .ok()?;
    Some(((elapsed as u128 * 100) / duration_micros as u128).min(99) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ffmpeg_progress_safely() {
        assert_eq!(
            transcode_percent(b"out_time_us=25000000\n", 100_000_000),
            Some(25)
        );
        assert_eq!(
            transcode_percent(b"out_time_ms=200000000\n", 100_000_000),
            Some(99)
        );
        assert_eq!(transcode_percent(b"progress=continue\n", 100_000_000), None);
        assert_eq!(transcode_percent(b"out_time_us=1\n", 0), None);
    }

    #[test]
    fn parses_yt_dlp_progress_safely() {
        assert_eq!(download_percent(b"download:  42.7%\n"), Some(42));
        assert_eq!(download_percent(b"download:101.0%\n"), Some(100));
        assert_eq!(download_percent(b"[download] destination\n"), None);
    }
}
