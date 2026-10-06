//! Shareable media from a verify recording (fix.md "Media in the PR and in Linear"): Playwright's
//! `webm` becomes an mp4 (H.264, no audio, ≤ 10 MB, for GitHub's inline player and Linear) and a
//! GIF (≤ 15 s, ≤ 960 px wide, ~10 fps, palette-optimized, ≤ 10 MB, for the PR's before/after
//! table). The server uploads them to GitHub and Linear; the runner only produces artifacts.
//!
//! The size steps are pure ([`mp4_plan`], [`gif_plan`]); [`first_fitting`] tries them in order
//! until one output fits. ffmpeg runs on the runner host against the exported outbox file (no
//! repo code is involved), with argv only.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use mage_core::domain::ArtifactKind;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::ports::browser::LocalArtifact;

/// GitHub Free's video limit and the image/GIF limit (fix.md).
pub const MEDIA_MAX_BYTES: u64 = 10_000_000;

/// `[runner.media]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// Off: verify runs upload only the webm.
    pub enabled: bool,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Size cap per output.
    pub max_bytes: u64,
    /// Per ffmpeg invocation.
    pub timeout_s: u64,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ffmpeg: PathBuf::from("ffmpeg"),
            ffprobe: PathBuf::from("ffprobe"),
            max_bytes: MEDIA_MAX_BYTES,
            timeout_s: 180,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// ffmpeg / ffprobe missing or not runnable.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// The input is not a decodable video, or ffmpeg failed.
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(String),
}

impl From<std::io::Error> for MediaError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// One mp4 attempt. The failure (or acceptance) is at the end of a recording, so trimming keeps
/// the last `keep_last` of the video.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mp4Step {
    pub keep_last: Option<Duration>,
    pub max_width: u32,
    pub quality: Mp4Quality,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mp4Quality {
    /// x264 constant rate factor.
    Crf(u8),
    /// Average bitrate in bits/s (with matching maxrate/bufsize).
    Bitrate(u64),
}

/// One GIF attempt: last `keep_last` (≤ 15 s), `width` ≤ 960, `fps`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GifStep {
    pub keep_last: Duration,
    pub width: u32,
    pub fps: u32,
}

const GIF_MAX: Duration = Duration::from_secs(15);

/// Bitrate that lands `seconds` of video under `max_bytes` with `share` headroom.
fn bitrate_for(max_bytes: u64, seconds: f64, share: f64) -> u64 {
    let seconds = seconds.max(1.0);
    ((max_bytes as f64 * 8.0 * share) / seconds) as u64
}

/// Size steps for the mp4, in order: full quality; trim to the last 60 s; then target bitrates on
/// the last 30 s at decreasing sizes. Trims longer than the recording are skipped.
pub fn mp4_plan(duration: Duration, max_bytes: u64) -> Vec<Mp4Step> {
    let mut steps = vec![Mp4Step { keep_last: None, max_width: 1280, quality: Mp4Quality::Crf(28) }];
    let trim = |d: Duration| (duration > d).then_some(d);
    if let Some(keep) = trim(Duration::from_secs(60)) {
        steps.push(Mp4Step { keep_last: Some(keep), max_width: 1280, quality: Mp4Quality::Crf(28) });
    }
    let thirty = trim(Duration::from_secs(30));
    let secs = thirty.unwrap_or(duration).as_secs_f64();
    steps.push(Mp4Step { keep_last: thirty, max_width: 1280, quality: Mp4Quality::Bitrate(bitrate_for(max_bytes, secs, 0.85)) });
    steps.push(Mp4Step { keep_last: thirty, max_width: 960, quality: Mp4Quality::Bitrate(bitrate_for(max_bytes, secs, 0.6)) });
    let fifteen = trim(Duration::from_secs(15));
    let secs = fifteen.unwrap_or(duration).as_secs_f64();
    steps.push(Mp4Step { keep_last: fifteen, max_width: 640, quality: Mp4Quality::Bitrate(bitrate_for(max_bytes, secs, 0.5)) });
    steps
}

/// Size steps for the GIF: the last ≤ 15 s at 960 px / 10 fps, then smaller and slower.
pub fn gif_plan(duration: Duration) -> Vec<GifStep> {
    let keep = duration.min(GIF_MAX);
    let short = duration.min(Duration::from_secs(10));
    vec![
        GifStep { keep_last: keep, width: 960, fps: 10 },
        GifStep { keep_last: keep, width: 720, fps: 10 },
        GifStep { keep_last: keep, width: 480, fps: 8 },
        GifStep { keep_last: short, width: 360, fps: 6 },
    ]
}

/// `-ss <start>` arguments keeping the last `keep` of `duration`.
fn seek_args(duration: Duration, keep: Option<Duration>) -> Vec<String> {
    match keep {
        Some(keep) if duration > keep => vec!["-ss".to_owned(), format!("{:.3}", (duration - keep).as_secs_f64())],
        _ => Vec::new(),
    }
}

fn s(v: &str) -> String {
    v.to_owned()
}

/// ffmpeg argv (without the binary) for one mp4 step.
pub fn mp4_args(input: &Path, output: &Path, duration: Duration, step: &Mp4Step) -> Vec<String> {
    let mut args = vec![s("-hide_banner"), s("-loglevel"), s("error"), s("-y")];
    args.extend(seek_args(duration, step.keep_last));
    args.extend([s("-i"), input.display().to_string()]);
    args.extend([
        s("-an"),
        s("-c:v"),
        s("libx264"),
        s("-preset"),
        s("veryfast"),
        s("-pix_fmt"),
        s("yuv420p"),
        s("-vf"),
        // Even dimensions are required by yuv420p.
        format!("scale='min({},iw)':-2", step.max_width),
    ]);
    match step.quality {
        Mp4Quality::Crf(crf) => args.extend([s("-crf"), crf.to_string()]),
        Mp4Quality::Bitrate(bps) => {
            args.extend([s("-b:v"), bps.to_string(), s("-maxrate"), bps.to_string(), s("-bufsize"), (bps * 2).to_string()]);
        }
    }
    args.extend([s("-movflags"), s("+faststart"), s("-f"), s("mp4"), output.display().to_string()]);
    args
}

/// ffmpeg argv (without the binary) for one GIF step: one pass with `palettegen`/`paletteuse`.
pub fn gif_args(input: &Path, output: &Path, duration: Duration, step: &GifStep) -> Vec<String> {
    let mut args = vec![s("-hide_banner"), s("-loglevel"), s("error"), s("-y")];
    args.extend(seek_args(duration, Some(step.keep_last)));
    args.extend([s("-t"), format!("{:.3}", step.keep_last.as_secs_f64())]);
    args.extend([s("-i"), input.display().to_string()]);
    args.extend([
        s("-an"),
        s("-filter_complex"),
        format!(
            "fps={},scale='min({},iw)':-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
            step.fps, step.width
        ),
        s("-loop"),
        s("0"),
        s("-f"),
        s("gif"),
        output.display().to_string(),
    ]);
    args
}

/// Tries `steps` in order; `attempt` produces the output for one step and returns its size.
/// Returns the index and size of the first step within `max_bytes`, or `None` if none fits.
pub async fn first_fitting<S, F, Fut>(steps: &[S], max_bytes: u64, mut attempt: F) -> Result<Option<(usize, u64)>, MediaError>
where
    F: FnMut(usize, &S) -> Fut,
    Fut: Future<Output = Result<u64, MediaError>>,
{
    for (i, step) in steps.iter().enumerate() {
        let size = attempt(i, step).await?;
        if size <= max_bytes {
            return Ok(Some((i, size)));
        }
    }
    Ok(None)
}

/// The derived outputs (each `None` when no step fit the size cap).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DerivedMedia {
    pub mp4: Option<LocalArtifact>,
    pub gif: Option<LocalArtifact>,
}

pub struct MediaEncoder {
    config: MediaConfig,
}

impl MediaEncoder {
    pub fn new(config: MediaConfig) -> Self {
        Self { config }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// mp4 `<label>.video.mp4` and GIF `<label>.preview.gif` next to `webm` in `outbox`.
    pub async fn derive(&self, webm: &Path, outbox: &Path, label: &str) -> Result<DerivedMedia, MediaError> {
        let duration = self.probe_duration(webm).await?;
        let mp4_path = outbox.join(format!("{label}.video.mp4"));
        let mp4_steps = mp4_plan(duration, self.config.max_bytes);
        let mp4 = first_fitting(&mp4_steps, self.config.max_bytes, |_, step| {
            let args = mp4_args(webm, &mp4_path, duration, step);
            let path = mp4_path.clone();
            async move {
                self.ffmpeg(args).await?;
                Ok(tokio::fs::metadata(&path).await?.len())
            }
        })
        .await?;
        let mp4 = match mp4 {
            Some(_) => Some(local(&mp4_path, ArtifactKind::Video, "video/mp4").await?),
            None => {
                remove_quietly(&mp4_path).await;
                None
            }
        };

        let gif_path = outbox.join(format!("{label}.preview.gif"));
        let gif_steps = gif_plan(duration);
        let gif = first_fitting(&gif_steps, self.config.max_bytes, |_, step| {
            let args = gif_args(webm, &gif_path, duration, step);
            let path = gif_path.clone();
            async move {
                self.ffmpeg(args).await?;
                Ok(tokio::fs::metadata(&path).await?.len())
            }
        })
        .await?;
        let gif = match gif {
            Some(_) => Some(local(&gif_path, ArtifactKind::Gif, "image/gif").await?),
            None => {
                remove_quietly(&gif_path).await;
                None
            }
        };
        Ok(DerivedMedia { mp4, gif })
    }

    async fn probe_duration(&self, input: &Path) -> Result<Duration, MediaError> {
        let args = vec![
            s("-v"),
            s("error"),
            s("-show_entries"),
            s("format=duration"),
            s("-of"),
            s("default=noprint_wrappers=1:nokey=1"),
            input.display().to_string(),
        ];
        let out = self.run(&self.config.ffprobe, args).await?;
        let text = String::from_utf8_lossy(&out);
        parse_duration(&text).ok_or_else(|| MediaError::Invalid(format!("ffprobe reported no duration: {:?}", text.trim())))
    }

    async fn ffmpeg(&self, args: Vec<String>) -> Result<(), MediaError> {
        self.run(&self.config.ffmpeg, args).await.map(|_| ())
    }

    /// Runs `bin args`, returning stdout; non-zero exit → `Invalid` with stderr.
    async fn run(&self, bin: &Path, args: Vec<String>) -> Result<Vec<u8>, MediaError> {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let child = cmd.spawn().map_err(|e| MediaError::Unavailable(format!("{}: {e}", bin.display())))?;
        let out = tokio::time::timeout(Duration::from_secs(self.config.timeout_s), child.wait_with_output())
            .await
            .map_err(|_| MediaError::Invalid(format!("{} timed out after {} s", bin.display(), self.config.timeout_s)))??;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let tail: String = stderr.chars().rev().take(1024).collect::<Vec<_>>().into_iter().rev().collect();
            return Err(MediaError::Invalid(format!("{} failed ({}): {}", bin.display(), out.status, tail.trim())));
        }
        Ok(out.stdout)
    }
}

/// `ffprobe … format=duration` output: seconds as a decimal (`N/A` for unknown).
pub fn parse_duration(text: &str) -> Option<Duration> {
    let secs: f64 = text.lines().map(str::trim).find(|l| !l.is_empty())?.parse().ok()?;
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs))
}

async fn local(path: &Path, kind: ArtifactKind, content_type: &str) -> Result<LocalArtifact, MediaError> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(LocalArtifact { kind, name, path: path.to_path_buf(), bytes, sha256: hasher.finalize().into(), content_type: content_type.to_owned() })
}

async fn remove_quietly(path: &Path) {
    let _ = tokio::fs::remove_file(path).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn mp4_plan_trims_only_when_the_recording_is_longer() {
        let short = mp4_plan(secs(12), MEDIA_MAX_BYTES);
        assert_eq!(short.len(), 4, "no 60 s trim for a 12 s recording");
        assert!(short.iter().all(|s| s.keep_last.is_none()));
        assert_eq!(short[0].quality, Mp4Quality::Crf(28));

        let long = mp4_plan(secs(240), MEDIA_MAX_BYTES);
        let keeps: Vec<Option<u64>> = long.iter().map(|s| s.keep_last.map(|d| d.as_secs())).collect();
        assert_eq!(keeps, [None, Some(60), Some(30), Some(30), Some(15)]);
        let widths: Vec<u32> = long.iter().map(|s| s.max_width).collect();
        assert_eq!(widths, [1280, 1280, 1280, 960, 640]);
        // Bitrate steps decrease and each fits the cap over its kept seconds.
        let rates: Vec<u64> = long
            .iter()
            .filter_map(|s| match s.quality {
                Mp4Quality::Bitrate(b) => Some(b),
                Mp4Quality::Crf(_) => None,
            })
            .collect();
        assert_eq!(rates.len(), 3);
        assert!(rates[0] > rates[1]);
        for step in &long {
            if let (Mp4Quality::Bitrate(bps), Some(keep)) = (step.quality, step.keep_last) {
                assert!(bps as f64 * keep.as_secs_f64() / 8.0 < MEDIA_MAX_BYTES as f64);
            }
        }
    }

    #[test]
    fn gif_plan_caps_length_width_and_fps() {
        let plan = gif_plan(secs(40));
        assert_eq!(plan[0], GifStep { keep_last: secs(15), width: 960, fps: 10 });
        assert!(plan.iter().all(|s| s.keep_last <= secs(15) && s.width <= 960 && s.fps <= 10));
        assert!(plan.windows(2).all(|w| w[1].width <= w[0].width));
        assert_eq!(gif_plan(secs(4))[0].keep_last, secs(4));
    }

    #[test]
    fn mp4_args_shape() {
        let step = Mp4Step { keep_last: Some(secs(30)), max_width: 960, quality: Mp4Quality::Bitrate(2_000_000) };
        let args = mp4_args(Path::new("/o/v.webm"), Path::new("/o/v.mp4"), secs(100), &step);
        let joined = args.join(" ");
        assert!(joined.contains("-ss 70.000 -i /o/v.webm"), "seek before input: {joined}");
        assert!(joined.contains("-an"));
        assert!(joined.contains("-c:v libx264"));
        assert!(joined.contains("-pix_fmt yuv420p"));
        assert!(joined.contains("scale='min(960,iw)':-2"));
        assert!(joined.contains("-b:v 2000000 -maxrate 2000000 -bufsize 4000000"));
        assert!(joined.ends_with("-movflags +faststart -f mp4 /o/v.mp4"));
        let full = mp4_args(Path::new("in"), Path::new("out"), secs(10), &Mp4Step { keep_last: None, max_width: 1280, quality: Mp4Quality::Crf(28) });
        assert!(!full.contains(&"-ss".to_owned()));
        assert!(full.join(" ").contains("-crf 28"));
    }

    #[test]
    fn gif_args_shape() {
        let args = gif_args(Path::new("in.webm"), Path::new("out.gif"), secs(20), &GifStep { keep_last: secs(15), width: 960, fps: 10 });
        let joined = args.join(" ");
        assert!(joined.contains("-ss 5.000 -t 15.000 -i in.webm"), "{joined}");
        assert!(joined.contains("fps=10,scale='min(960,iw)':-1:flags=lanczos"));
        assert!(joined.contains("palettegen"));
        assert!(joined.contains("paletteuse"));
        assert!(joined.ends_with("-loop 0 -f gif out.gif"));
    }

    #[tokio::test]
    async fn first_fitting_walks_steps_until_one_fits() {
        let sizes = [25_000_000u64, 14_000_000, 9_000_000, 1];
        let mut tried = Vec::new();
        let found = first_fitting(&sizes, MEDIA_MAX_BYTES, |i, size| {
            tried.push(i);
            let size = *size;
            async move { Ok(size) }
        })
        .await
        .unwrap();
        assert_eq!(found, Some((2, 9_000_000)));
        assert_eq!(tried, [0, 1, 2], "stops at the first fit");

        let none = first_fitting(&[11_000_000u64, 10_000_001], MEDIA_MAX_BYTES, |_, s| {
            let s = *s;
            async move { Ok(s) }
        })
        .await
        .unwrap();
        assert_eq!(none, None);
        // Exactly at the cap fits.
        let edge = first_fitting(&[MEDIA_MAX_BYTES], MEDIA_MAX_BYTES, |_, s| {
            let s = *s;
            async move { Ok(s) }
        })
        .await
        .unwrap();
        assert_eq!(edge, Some((0, MEDIA_MAX_BYTES)));
        let err = first_fitting(&[1u64], MEDIA_MAX_BYTES, |_, _| async { Err(MediaError::Invalid("boom".into())) }).await;
        assert_eq!(err, Err(MediaError::Invalid("boom".into())));
    }

    #[test]
    fn parse_duration_forms() {
        assert_eq!(parse_duration("12.480000\n"), Some(Duration::from_secs_f64(12.48)));
        assert_eq!(parse_duration("N/A\n"), None);
        assert_eq!(parse_duration("0.000000"), None);
        assert_eq!(parse_duration(""), None);
    }

    /// Real ffmpeg (opt-in: `MAGE_TEST_FFMPEG=1`, needs `ffmpeg` and `ffprobe` on PATH): a
    /// synthetic 20 s webm becomes an mp4 (H.264, no audio) and a GIF (≤ 15 s, ≤ 960 px), both
    /// under the cap.
    #[tokio::test]
    async fn real_ffmpeg_derives_mp4_and_gif() {
        if std::env::var("MAGE_TEST_FFMPEG").as_deref() != Ok("1") {
            eprintln!("skipped: set MAGE_TEST_FFMPEG=1 to run against a real ffmpeg");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let webm = dir.path().join("verify-r1.video.webm");
        let made = tokio::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=1280x800:rate=25:duration=20"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20", "-c:v", "libvpx", "-b:v", "1M", "-c:a", "libopus"])
            .arg(&webm)
            .status()
            .await
            .unwrap();
        assert!(made.success());
        let encoder = MediaEncoder::new(MediaConfig::default());
        let media = encoder.derive(&webm, dir.path(), "verify-r1").await.unwrap();
        let mp4 = media.mp4.expect("mp4 within 10 MB");
        assert_eq!(mp4.name, "verify-r1.video.mp4");
        assert_eq!(mp4.kind, ArtifactKind::Video);
        assert!(mp4.bytes <= MEDIA_MAX_BYTES);
        let gif = media.gif.expect("gif within 10 MB");
        assert_eq!(gif.kind, ArtifactKind::Gif);
        assert!(gif.bytes <= MEDIA_MAX_BYTES);

        let probe = |path: PathBuf, entries: &'static str| async move {
            let out = tokio::process::Command::new("ffprobe")
                .args(["-v", "error", "-show_entries", entries, "-of", "default=noprint_wrappers=1"])
                .arg(path)
                .output()
                .await
                .unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        let streams = probe(mp4.path.clone(), "stream=codec_name,codec_type").await;
        assert!(streams.contains("codec_name=h264"), "{streams}");
        assert!(!streams.contains("codec_type=audio"), "no audio: {streams}");
        let gif_info = probe(gif.path.clone(), "stream=width:format=duration").await;
        let width: u32 = gif_info.lines().find_map(|l| l.strip_prefix("width=")).unwrap().parse().unwrap();
        assert!(width <= 960);
        let dur: f64 = gif_info.lines().find_map(|l| l.strip_prefix("duration=")).unwrap().parse().unwrap();
        assert!(dur <= 15.5, "gif duration {dur}");
    }
}
