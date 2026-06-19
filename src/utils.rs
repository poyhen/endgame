use std::path::Path;

use rand::seq::SliceRandom;
use tokio::process::Command;

const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

pub fn generate_random_filename(ext: &str) -> String {
    let mut rng = rand::thread_rng();
    let mut s: String = (0..10)
        .map(|_| *CHARS.choose(&mut rng).unwrap() as char)
        .collect();
    s.push_str("frrr");
    s.push_str(ext);
    s
}

async fn run(cmd: &mut Command) -> std::io::Result<(bool, String, String)> {
    let out = cmd.output().await?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

pub async fn extract_thumbnail(video_file: &Path) -> anyhow::Result<String> {
    let thumbnail_filename = generate_random_filename(".jpg");

    // Extract the frame and scale it in a single pass. The previous code did a
    // second ffmpeg pass that read from and wrote to the *same* file, which is
    // unreliable and would sometimes leave an empty/corrupt thumbnail (aborting
    // the whole upload as a result).
    let (ok, _, stderr) = run(Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(video_file)
        .arg("-ss")
        .arg("00:00:01.000")
        .arg("-vframes")
        .arg("1")
        .arg("-vf")
        .arg("scale=320:-1")
        .arg(&thumbnail_filename))
    .await?;
    if !ok {
        anyhow::bail!("failed to extract thumbnail. error: {stderr}");
    }

    Ok(thumbnail_filename)
}

/// Probes a video file, returning `(duration_secs, width, height)`.
///
/// This only fails when `ffprobe` itself rejects the file (i.e. it is not a
/// real/decodable video, e.g. an HTML error page gallery-dl saved with a `.mp4`
/// name). Unparseable metadata is tolerated and defaults to `0`, so a perfectly
/// valid video is never dropped just because `ffprobe` printed `N/A` for some
/// field or emitted the values in an unexpected order.
pub async fn probe_video(video_file: &Path) -> anyhow::Result<(i64, i32, i32)> {
    let (ok, stdout, stderr) = run(Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=width,height:format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1")
        .arg(video_file))
    .await?;
    if !ok {
        anyhow::bail!("ffprobe could not read video. error: {stderr}");
    }

    let mut duration = 0i64;
    let mut width = 0i32;
    let mut height = 0i32;
    for line in stdout.lines() {
        if let Some(v) = line.trim().strip_prefix("width=") {
            width = v.parse().unwrap_or(0);
        } else if let Some(v) = line.trim().strip_prefix("height=") {
            height = v.parse().unwrap_or(0);
        } else if let Some(v) = line.trim().strip_prefix("duration=") {
            duration = v.parse::<f64>().map(|f| f as i64).unwrap_or(0);
        }
    }

    Ok((duration, width, height))
}

/// Reads a cookie file and reformats it to be tab-delimited.
/// Handles cookies pasted with spaces instead of tabs.
pub fn clean_cookie_file(file_path: &str) {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut reformatted = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            reformatted.push_str(trimmed);
            reformatted.push('\n');
            continue;
        }

        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() >= 6 {
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    reformatted.push('\t');
                }
                reformatted.push_str(part);
            }
            reformatted.push('\n');
        }
    }

    if let Err(e) = std::fs::write(file_path, reformatted) {
        println!("Error cleaning cookie file {file_path}: {e}");
    }
}
