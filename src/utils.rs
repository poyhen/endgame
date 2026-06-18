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

    let (ok, _, stderr) = run(Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(video_file)
        .arg("-ss")
        .arg("00:00:01.000")
        .arg("-vframes")
        .arg("1")
        .arg(&thumbnail_filename))
    .await?;
    if !ok {
        anyhow::bail!("failed to extract thumbnail. error: {stderr}");
    }

    let (ok, _, stderr) = run(Command::new("ffmpeg")
        .arg("-y")
        .arg("-i")
        .arg(&thumbnail_filename)
        .arg("-vf")
        .arg("scale=320:-1")
        .arg(&thumbnail_filename))
    .await?;
    if !ok {
        anyhow::bail!("failed to resize thumbnail. error: {stderr}");
    }

    Ok(thumbnail_filename)
}

pub async fn get_video_duration(video_file: &Path) -> anyhow::Result<i64> {
    let (ok, stdout, stderr) = run(Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(video_file))
    .await?;
    if !ok {
        anyhow::bail!("failed to get video duration. error: {stderr}");
    }
    let duration: f64 = stdout
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("unable to parse duration from output: {stdout} ({e})"))?;
    Ok(duration as i64)
}

pub async fn get_video_dimensions(video_file: &Path) -> anyhow::Result<(i32, i32)> {
    let (ok, stdout, stderr) = run(Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=width,height")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(video_file))
    .await?;
    if !ok {
        anyhow::bail!("failed to get video dimensions. error: {stderr}");
    }

    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() != 2 {
        anyhow::bail!(
            "expected two lines for width and height, got {} lines",
            lines.len()
        );
    }
    let width: i32 = lines[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("unable to parse dimensions from output: {stdout}"))?;
    let height: i32 = lines[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("unable to parse dimensions from output: {stdout}"))?;
    Ok((width, height))
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
