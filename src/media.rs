//! Local video preparation. Fixed argument lists are passed directly to child
//! processes; uploaded names never become command text or filesystem paths.
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use tokio::{process::Command, time::timeout};
use uuid::Uuid;

use crate::error::ApiError;

const MAX_UPLOAD: u64 = 100 * 1024 * 1024;
const MAX_SEGMENT: u64 = 50 * 1024 * 1024;
const SEGMENT_MS: i64 = 60_000;
const MAX_DURATION_MS: i64 = 7_200_000;
static VISUAL_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

pub struct SampledFrame {
    pub offset_ms: i64,
    pub data_url: String,
}

/// Read multiple moments from an existing published segment. These JPEGs are
/// request-scoped processing files, removed even on error; OSS remains the
/// source of the video and no orphaned durable derivative is created.
pub async fn sample_frames(
    root: &Path,
    key: &str,
    offset_ms: i64,
    duration_ms: i64,
) -> Result<Vec<SampledFrame>, ApiError> {
    validate_key(key)?;
    if offset_ms < 0 || duration_ms <= 0 || duration_ms > SEGMENT_MS + 1000 {
        return Err(ApiError::bad_request("无效的视频画面范围"));
    }
    let _permit = VISUAL_WORKERS
        .acquire()
        .await
        .map_err(|_| ApiError::internal())?;
    let mut generated = GeneratedFiles::default();
    let mut result = Vec::new();
    let mut previous = None;
    for fraction in [1, 3, 6, 9, 11] {
        let position = (duration_ms * fraction / 12).min((duration_ms - 200).max(0));
        if previous == Some(position) {
            continue;
        }
        previous = Some(position);
        let path = root.join(format!("{}.jpg", Uuid::new_v4()));
        generated.paths.push(path.clone());
        extract_frame(&root.join(key), &path, offset_ms + position).await?;
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|_| ApiError::unavailable("视频取帧失败"))?;
        if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
            return Err(ApiError::unavailable("视频画面为空或过大"));
        }
        result.push(SampledFrame {
            offset_ms: position,
            data_url: format!("data:image/jpeg;base64,{}", STANDARD.encode(&bytes)),
        });
    }
    Ok(result)
}

#[derive(Clone, Debug)]
pub struct MediaSegment {
    pub storage_key: String,
    pub frame_storage_key: Option<String>,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Prepare original video segments for native visual embeddings and a frame
/// for a separately verified vision-capable answer model. No audio transcript
/// is implied by either operation.
#[cfg(test)]
pub async fn prepare_video(
    root: &std::path::Path,
    storage_key: &str,
) -> Result<Vec<MediaSegment>, ApiError> {
    prepare_video_range(root, storage_key, None).await
}

pub async fn prepare_video_range(
    root: &std::path::Path,
    storage_key: &str,
    range: Option<(i64, i64)>,
) -> Result<Vec<MediaSegment>, ApiError> {
    validate_key(storage_key)?;

    let input = root.join(storage_key);
    let metadata = tokio::fs::metadata(&input)
        .await
        .map_err(|_| ApiError::not_found("视频原件不存在"))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_UPLOAD {
        return Err(ApiError::bad_request("视频原件为空或超过100MB上传限制"));
    }
    let duration = probe_duration(&input, "video").await?;
    if duration > MAX_DURATION_MS {
        return Err(ApiError::bad_request(
            "视频超过两小时，请按课程内容拆分后上传",
        ));
    }
    let (range_start, range_end) = range.unwrap_or((0, duration));
    if range_start < 0 || range_end <= range_start || range_end > duration + 100 {
        return Err(ApiError::bad_request("片段时间范围超出原视频时长"));
    }
    let range_end = range_end.min(duration);
    let mut generated = GeneratedFiles::default();
    let mut result = Vec::new();
    let reuse_original = range_start == 0
        && range_end == duration
        && duration <= SEGMENT_MS
        && metadata.len() <= MAX_SEGMENT;
    let mut start = range_start;
    while start < range_end {
        let end = (start + SEGMENT_MS).min(range_end);
        let segment_key = if reuse_original {
            storage_key.to_owned()
        } else {
            let key = format!("{}.mp4", Uuid::new_v4());
            let output = root.join(&key);
            generated.paths.push(output.clone());
            transcode(&input, &output, start, end - start).await?;
            let size = tokio::fs::metadata(&output)
                .await
                .map_err(|_| ApiError::unavailable("视频分段未产生输出，请重试"))?
                .len();
            if size == 0 || size > MAX_SEGMENT {
                return Err(ApiError::unavailable(
                    "视频分段超过模型50MB限制，请降低原视频码率后重试",
                ));
            }
            key
        };
        let frame_key = format!("{}.jpg", Uuid::new_v4());
        let frame_path = root.join(&frame_key);
        generated.paths.push(frame_path.clone());
        // The midpoint is relative to this extracted segment, not the source.
        extract_frame(&root.join(&segment_key), &frame_path, (end - start) / 2).await?;
        let size = tokio::fs::metadata(&frame_path)
            .await
            .map_err(|_| ApiError::unavailable("视频画面提取失败，请重试"))?
            .len();
        if size == 0 || size > 10 * 1024 * 1024 {
            return Err(ApiError::unavailable("视频画面输出为空或过大，请重试"));
        }
        result.push(MediaSegment {
            storage_key: segment_key,
            frame_storage_key: Some(frame_key),
            start_ms: start,
            end_ms: end,
        });
        start = end;
    }
    generated.retained = true;
    Ok(result)
}

fn validate_key(key: &str) -> Result<(), ApiError> {
    // Ingestion generates UUID.ext keys. Enforce the same boundary even if
    // this public helper is later called from another job handler.
    let Some((stem, extension)) = key.rsplit_once('.') else {
        return Err(ApiError::bad_request("无效的视频存储键"));
    };
    if Uuid::parse_str(stem).is_err()
        || !matches!(extension, "mp4" | "mov" | "avi")
        || key.contains(['/', '\\'])
    {
        return Err(ApiError::bad_request("无效的视频存储键"));
    }
    Ok(())
}

#[derive(Default)]
struct GeneratedFiles {
    paths: Vec<PathBuf>,
    retained: bool,
}
impl Drop for GeneratedFiles {
    fn drop(&mut self) {
        if !self.retained {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[derive(Deserialize)]
struct Probe {
    format: Option<ProbeFormat>,
    streams: Option<Vec<ProbeStream>>,
}
#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}
#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
}

/// Read the original file's duration, never a teacher-supplied time range.
pub async fn probe_duration(input: &Path, kind: &str) -> Result<i64, ApiError> {
    let mut command = Command::new("ffprobe");
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type",
            "-of",
            "json",
        ])
        .arg(input);
    let output = run(command, "ffprobe", Duration::from_secs(30)).await?;
    let probe: Probe = serde_json::from_slice(&output)
        .map_err(|_| ApiError::bad_request("无法解析视频元数据，请检查文件内容"))?;
    if !probe
        .streams
        .unwrap_or_default()
        .iter()
        .any(|stream| stream.codec_type.as_deref() == Some(kind))
    {
        return Err(ApiError::bad_request("该文件不包含所需的音视频轨道"));
    }
    let seconds = probe
        .format
        .and_then(|f| f.duration)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 86_400.0)
        .ok_or_else(|| ApiError::bad_request("无法确定有效的媒体时长"))?;
    Ok((seconds * 1000.0).ceil() as i64)
}

async fn transcode(input: &Path, output: &Path, start: i64, duration: i64) -> Result<(), ApiError> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
        ])
        .arg(seconds(start))
        .args(["-threads", "1", "-filter_threads", "1", "-i"])
        .arg(input)
        .args(["-t"])
        .arg(seconds(duration))
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a?",
            "-c:v",
            "libx264",
            "-threads",
            "1",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-maxrate",
            "2M",
            "-bufsize",
            "4M",
            "-vf",
            "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "96k",
            "-movflags",
            "+faststart",
        ])
        .arg(output);
    run(command, "ffmpeg", Duration::from_secs(180)).await?;
    Ok(())
}

async fn extract_frame(input: &Path, output: &Path, middle: i64) -> Result<(), ApiError> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-ss",
        ])
        .arg(seconds(middle))
        .args(["-threads", "1", "-filter_threads", "1", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-threads",
            "1",
            "-vf",
            "scale=1280:720:force_original_aspect_ratio=decrease",
            "-q:v",
            "3",
            "-update",
            "1",
        ])
        .arg(output);
    run(command, "ffmpeg", Duration::from_secs(60)).await?;
    Ok(())
}

fn seconds(milliseconds: i64) -> String {
    format!("{}.{:03}", milliseconds / 1000, milliseconds % 1000)
}

async fn run(mut command: Command, tool: &str, deadline: Duration) -> Result<Vec<u8>, ApiError> {
    command.kill_on_drop(true);
    let output = timeout(deadline, command.output())
        .await
        .map_err(|_| ApiError::unavailable(format!("{tool} 处理超时，可重试该任务")))?
        .map_err(|_| ApiError::unavailable(format!("{tool} 不可用，请安装媒体解析工具后重试")))?;
    if !output.status.success() {
        // ffmpeg diagnostics can contain absolute paths or input bytes. Keep
        // them out of public API errors and application logs.
        return Err(ApiError::unavailable(format!(
            "{tool} 无法处理该媒体，请检查格式或重试"
        )));
    }
    if output.stdout.len() > 64 * 1024 {
        return Err(ApiError::unavailable("媒体元数据响应过大"));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_validation_prevents_path_traversal() {
        assert!(validate_key("../file.mp4").is_err());
        assert!(validate_key("C:\\file.mp4").is_err());
        assert!(validate_key(&format!("{}.mp4", Uuid::new_v4())).is_ok());
    }
    #[test]
    fn command_seconds_are_locale_independent() {
        assert_eq!(seconds(61_125), "61.125");
    }

    #[tokio::test]
    #[ignore = "requires local ffmpeg; verifies distinct moments and request-file cleanup"]
    async fn video_sampling_covers_multiple_moments_and_cleans_processing_files() {
        let directory = std::env::temp_dir().join(format!("kb-frames-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await.unwrap();
        let key = format!("{}.mp4", Uuid::new_v4());
        let input = directory.join(&key);
        let mut command = Command::new("ffmpeg");
        command
            .args([
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=4",
                "-t",
                "8",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&input);
        run(command, "ffmpeg", Duration::from_secs(30))
            .await
            .unwrap();
        let frames = sample_frames(&directory, &key, 0, 8000).await.unwrap();
        assert_eq!(frames.len(), 5);
        assert!(frames[0].offset_ms < 1000 && frames[4].offset_ms > 7000);
        assert_ne!(frames[0].data_url, frames[4].data_url);
        assert!(frames
            .iter()
            .all(|f| f.data_url.starts_with("data:image/jpeg;base64,")));
        let mut remaining = tokio::fs::read_dir(&directory).await.unwrap();
        assert_eq!(remaining.next_entry().await.unwrap().unwrap().path(), input);
        assert!(remaining.next_entry().await.unwrap().is_none());
        // Error cleanup also removes files created before a failed frame read.
        assert!(sample_frames(&directory, &key, 9000, 1000).await.is_err());
        let mut remaining = tokio::fs::read_dir(&directory).await.unwrap();
        remaining.next_entry().await.unwrap().unwrap();
        assert!(remaining.next_entry().await.unwrap().is_none());
        tokio::fs::remove_file(input).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires local ffmpeg and ffprobe; generates synthetic videos only"]
    async fn real_ffmpeg_generates_frames_and_rebased_segments() {
        let root = crate::ingest::uploads_dir();
        tokio::fs::create_dir_all(&root).await.unwrap();
        let mut cleanup = GeneratedFiles::default();
        for duration in [1, 61] {
            let key = format!("{}.mp4", Uuid::new_v4());
            let input = root.join(&key);
            cleanup.paths.push(input.clone());
            let mut command = Command::new("ffmpeg");
            command
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red:s=256x256:r=4",
                    "-t",
                ])
                .arg(duration.to_string())
                .args([
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-movflags",
                    "+faststart",
                ])
                .arg(&input);
            run(command, "ffmpeg", Duration::from_secs(60))
                .await
                .unwrap();
            let segments = prepare_video(&root, &key).await.unwrap();
            assert_eq!(segments.len(), if duration == 1 { 1 } else { 2 });
            for segment in &segments {
                let frame = root.join(segment.frame_storage_key.as_ref().unwrap());
                cleanup.paths.push(frame.clone());
                assert!(tokio::fs::read(frame)
                    .await
                    .unwrap()
                    .starts_with(&[0xff, 0xd8]));
                if segment.storage_key != key {
                    cleanup.paths.push(root.join(&segment.storage_key));
                }
            }
            if duration == 1 {
                assert_eq!(segments[0].storage_key, key);
                assert_eq!(segments[0].start_ms, 0);
                assert!((1000..=1001).contains(&segments[0].end_ms));
            } else {
                assert_ne!(segments[0].storage_key, key);
                assert_eq!(segments[0].end_ms, 60_000);
                assert_eq!(segments[1].start_ms, 60_000);
                let local_duration = probe_duration(&root.join(&segments[1].storage_key), "video")
                    .await
                    .unwrap();
                assert!((900..=1500).contains(&local_duration));
                assert!(local_duration < segments[1].start_ms);
                let selected = prepare_video_range(&root, &key, Some((10_500, 14_250)))
                    .await
                    .unwrap();
                assert_eq!(selected.len(), 1);
                assert_eq!((selected[0].start_ms, selected[0].end_ms), (10_500, 14_250));
                assert_ne!(selected[0].storage_key, key);
                cleanup.paths.push(root.join(&selected[0].storage_key));
                cleanup
                    .paths
                    .push(root.join(selected[0].frame_storage_key.as_ref().unwrap()));
                let selected_duration =
                    probe_duration(&root.join(&selected[0].storage_key), "video")
                        .await
                        .unwrap();
                assert!((3500..=4100).contains(&selected_duration));
                assert!(prepare_video_range(&root, &key, Some((60_000, 63_000)))
                    .await
                    .is_err());
            }
        }
    }
}
