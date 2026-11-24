//! Crawllab Spider -> `ramz` Visual Capture -> Reverie Data Plane Visual Stores.
//!
//! Bridges `cerebral-work/spider` (HTTP + Chrome/Obscura CDP visual page capture) with
//! Reverie's `ramz` visual archival pipeline:
//! - Compresses raw captured HTML with `zstd` (level 3)
//! - Downscales captured screenshots / diagram crops proportionally (`<= 1280px`, Lanczos3)
//!   and encodes them to `image/webp` content-addressed by `BLAKE3`
//! - Persists deterministic local `ramz` bundles (`raw.html.zst`, `article.md`, `assets/<blake3>.webp`, `manifest.json`)
//! - Streams captured pages + `image/webp` visuals directly into Reverie's visual data planes:
//!   - Internal/staff daemon visual store: `POST /v1/archives/ingest` (`reveried` / `reverie-pg`)
//!   - Cloudflare Edge visual store: `POST /v1/sources` (`api.usereverie.dev` -> `TenantMemoryDO` + R2 + Workers AI Vision)

use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use chrono::Utc;
use image::imageops::FilterType;
use image::{GenericImageView, ImageFormat};
use serde::{Deserialize, Serialize};
use spider::configuration::{
    CaptureScreenshotFormat, CaptureScreenshotParams, ScreenShotConfig, ScreenshotParams, Viewport,
    WaitForDelay,
};
use spider::website::Website;
use url::Url;

/// Maximum pixel width for `ramz` WebP visual captures (matches `reverie-archive::MAX_SIDECAR_WIDTH`).
pub const MAX_VISUAL_WIDTH: u32 = 1280;

/// Maximum pixel height per vertical WebP tile (keeps every capture well below WebP's 16,383px limit
/// and readable for VLM vision models like Llama 3.2 11B Vision).
pub const MAX_VISUAL_TILE_HEIGHT: u32 = 4096;

/// Maximum vertical tiles retained per ultra-tall page capture.
pub const MAX_TILES_PER_CAPTURE: usize = 4;

/// Classification of a spider visual capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VisualCaptureKind {
    #[default]
    PageScreenshot,
    ElementCapture,
    InlineImage,
}

impl VisualCaptureKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PageScreenshot => "page_screenshot",
            Self::ElementCapture => "element_capture",
            Self::InlineImage => "inline_image",
        }
    }
}

/// Content-addressed `image/webp` visual artifact ready for storage and Reverie ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RamzVisualAsset {
    pub blake3_hash: String,
    pub source_url: String,
    pub alt: String,
    pub kind: VisualCaptureKind,
    pub content_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_len: usize,
    #[serde(skip_serializing)]
    pub webp_bytes: Vec<u8>,
}

/// Complete `ramz` visual capture bundle for one crawled web page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RamzCapturedPage {
    pub url: String,
    pub final_url: String,
    pub title: String,
    pub markdown: String,
    pub project: String,
    pub spider_name: String,
    pub crawl_id: String,
    pub tags: Vec<String>,
    pub blake3_digest: String,
    pub raw_html_bytes: usize,
    pub raw_html_zstd_bytes: usize,
    pub captured_at: String,
    pub visuals: Vec<RamzVisualAsset>,
    #[serde(skip_serializing)]
    pub raw_html: Vec<u8>,
    #[serde(skip_serializing)]
    pub raw_html_zstd: Vec<u8>,
}

/// Target Reverie visual data plane sink mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReverieSinkMode {
    None,
    Daemon,
    Edge,
    Both,
}

/// Receipt emitted after pushing a `RamzCapturedPage` to Reverie data planes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IngestReceipt {
    pub url: String,
    pub blake3_digest: String,
    pub visual_count: usize,
    pub local_bundle_dir: Option<String>,
    pub daemon_archive_id: Option<String>,
    pub daemon_status: Option<u16>,
    pub edge_source_id: Option<String>,
    pub edge_status: Option<u16>,
}

/// Decode raw image bytes (`PNG`, `JPEG`, `GIF`, `WebP`), reject `<= 2x2` tracking pixels,
/// downscale proportionally when `width > MAX_VISUAL_WIDTH`, and encode to one or more
/// vertical `image/webp` tiles (`<= MAX_VISUAL_TILE_HEIGHT` each, preventing WebP 16,383px overflow).
pub fn transcode_to_ramz_webp_tiles(
    raw_bytes: &[u8],
    source_url: &str,
    alt: &str,
    kind: VisualCaptureKind,
) -> Result<Vec<RamzVisualAsset>> {
    let dyn_img = image::load_from_memory(raw_bytes)
        .with_context(|| format!("decode visual capture for {source_url}"))?;
    let (orig_w, orig_h) = dyn_img.dimensions();
    if orig_w <= 2 && orig_h <= 2 {
        bail!("rejected {orig_w}x{orig_h} tracking pixel from {source_url}");
    }
    let resized = if orig_w > MAX_VISUAL_WIDTH {
        dyn_img.resize(MAX_VISUAL_WIDTH, u32::MAX, FilterType::Lanczos3)
    } else {
        dyn_img
    };
    let (width, total_height) = resized.dimensions();

    if total_height <= MAX_VISUAL_TILE_HEIGHT {
        let mut cursor = Cursor::new(Vec::new());
        resized
            .write_to(&mut cursor, ImageFormat::WebP)
            .with_context(|| format!("encode WebP visual capture for {source_url}"))?;
        let webp_bytes = cursor.into_inner();
        let blake3_hash = blake3::hash(&webp_bytes).to_hex().to_string();
        return Ok(vec![RamzVisualAsset {
            blake3_hash,
            source_url: source_url.to_string(),
            alt: alt.to_string(),
            kind,
            content_type: "image/webp".to_string(),
            width,
            height: total_height,
            byte_len: webp_bytes.len(),
            webp_bytes,
        }]);
    }

    let tile_count = total_height
        .div_ceil(MAX_VISUAL_TILE_HEIGHT)
        .min(MAX_TILES_PER_CAPTURE as u32) as usize;
    let mut tiles = Vec::with_capacity(tile_count);
    for idx in 0..tile_count {
        let y = (idx as u32) * MAX_VISUAL_TILE_HEIGHT;
        let h = (total_height - y).min(MAX_VISUAL_TILE_HEIGHT);
        let tile = resized.crop_imm(0, y, width, h);
        let mut cursor = Cursor::new(Vec::new());
        tile.write_to(&mut cursor, ImageFormat::WebP)
            .with_context(|| format!("encode WebP tile {idx} for {source_url}"))?;
        let webp_bytes = cursor.into_inner();
        let blake3_hash = blake3::hash(&webp_bytes).to_hex().to_string();
        tiles.push(RamzVisualAsset {
            blake3_hash,
            source_url: format!("{source_url}#tile-{idx}"),
            alt: format!("{alt} [tile {}/{tile_count}]", idx + 1),
            kind,
            content_type: "image/webp".to_string(),
            width,
            height: h,
            byte_len: webp_bytes.len(),
            webp_bytes,
        });
    }
    Ok(tiles)
}

/// Convenience wrapper returning the first WebP tile.
pub fn transcode_to_ramz_webp(
    raw_bytes: &[u8],
    source_url: &str,
    alt: &str,
    kind: VisualCaptureKind,
) -> Result<RamzVisualAsset> {
    transcode_to_ramz_webp_tiles(raw_bytes, source_url, alt, kind)?
        .into_iter()
        .next()
        .context("empty visual tile output")
}

/// Compress raw HTML bytes with `zstd` level 3 (matches `reverie-archive`).
pub fn compress_html_zstd(raw_html: &[u8]) -> Result<Vec<u8>> {
    zstd::encode_all(Cursor::new(raw_html), 3).context("zstd compress raw HTML")
}

/// Extract a clean page title from HTML `<title>` or fallback to URL path.
#[must_use]
pub fn extract_html_title(html: &str, fallback_url: &str) -> String {
    let lower = html.to_ascii_lowercase();
    if let Some(start) = lower.find("<title") {
        if let Some(gt) = lower[start..].find('>') {
            let content_start = start + gt + 1;
            if let Some(end) = lower[content_start..].find("</title>") {
                let raw = html[content_start..content_start + end].trim();
                let clean = raw
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if !clean.is_empty() {
                    return clean;
                }
            }
        }
    }
    fallback_url.to_string()
}

/// Strip `<script>` / `<style>` blocks and tags into compact plain Markdown fallback
/// when a caller does not supply pre-extracted Markdown.
#[must_use]
pub fn html_to_compact_markdown(html: &str, title: &str, url: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut skip_tag: Option<&'static str> = None;
    let bytes = html.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let rest = &html[i..];
            let lower_prefix: String = rest
                .chars()
                .take(16)
                .collect::<String>()
                .to_ascii_lowercase();
            if skip_tag.is_none() {
                if lower_prefix.starts_with("<script") {
                    skip_tag = Some("</script>");
                } else if lower_prefix.starts_with("<style") {
                    skip_tag = Some("</style>");
                }
            }
            if let Some(end_marker) = skip_tag {
                if lower_prefix.starts_with(end_marker) {
                    skip_tag = None;
                }
            }
            in_tag = true;
            if lower_prefix.starts_with("<p")
                || lower_prefix.starts_with("<h1")
                || lower_prefix.starts_with("<h2")
                || lower_prefix.starts_with("<h3")
                || lower_prefix.starts_with("<h4")
                || lower_prefix.starts_with("<li")
                || lower_prefix.starts_with("<tr")
                || lower_prefix.starts_with("<br")
            {
                out.push('\n');
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'>' {
            in_tag = false;
            i += 1;
            continue;
        }
        if !in_tag && skip_tag.is_none() {
            if let Some(ch) = html[i..].chars().next() {
                out.push(ch);
                i += ch.len_utf8();
                continue;
            }
        }
        i += 1;
    }

    let cleaned_lines: Vec<String> = out
        .lines()
        .map(|l| {
            l.replace("&nbsp;", " ")
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|l| !l.is_empty())
        .collect();

    format!(
        "# {title}\n\nSource: `{url}`\n\n{}",
        cleaned_lines.join("\n\n")
    )
}

/// Build a deterministic `RamzCapturedPage` from spider-captured HTML + visual screenshot bytes.
#[allow(clippy::too_many_arguments)]
pub fn build_ramz_captured_page(
    url: &str,
    final_url: Option<&str>,
    title_override: Option<&str>,
    raw_html: Vec<u8>,
    markdown_override: Option<String>,
    raw_visuals: Vec<(Vec<u8>, String, String, VisualCaptureKind)>,
    project: &str,
    spider_name: &str,
    crawl_id: &str,
    tags: Vec<String>,
) -> Result<RamzCapturedPage> {
    let parsed = Url::parse(url.trim()).with_context(|| format!("invalid URL: {url}"))?;
    let final_str = final_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(parsed.as_str())
        .to_string();
    let html_lossy = String::from_utf8_lossy(&raw_html);
    let title = title_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| extract_html_title(&html_lossy, &final_str));
    let markdown = markdown_override
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| html_to_compact_markdown(&html_lossy, &title, &final_str));

    let raw_html_zstd = if raw_html.is_empty() {
        Vec::new()
    } else {
        compress_html_zstd(&raw_html)?
    };

    let mut visuals = Vec::with_capacity(raw_visuals.len());
    for (bytes, src_url, alt, kind) in raw_visuals {
        if bytes.is_empty() {
            continue;
        }
        match transcode_to_ramz_webp_tiles(&bytes, &src_url, &alt, kind) {
            Ok(mut assets) => visuals.append(&mut assets),
            Err(err) => eprintln!("[ramz-sink] skipping visual {src_url}: {err}"),
        }
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(&raw_html);
    hasher.update(b"\0crawllab-ramz-v1\0");
    hasher.update(final_str.as_bytes());
    for v in &visuals {
        hasher.update(b"\0asset\0");
        hasher.update(v.blake3_hash.as_bytes());
    }
    let blake3_digest = hasher.finalize().to_hex().to_string();

    Ok(RamzCapturedPage {
        url: parsed.to_string(),
        final_url: final_str,
        title,
        markdown,
        project: project.to_string(),
        spider_name: spider_name.to_string(),
        crawl_id: crawl_id.to_string(),
        tags,
        blake3_digest,
        raw_html_bytes: raw_html.len(),
        raw_html_zstd_bytes: raw_html_zstd.len(),
        captured_at: Utc::now().to_rfc3339(),
        visuals,
        raw_html,
        raw_html_zstd,
    })
}

/// Write a `RamzCapturedPage` to `<out_dir>/<blake3>/` (`raw.html.zst`, `article.md`,
/// `assets/<hash>.webp`, `manifest.json`) and append one record to `<out_dir>/ramz-manifest.jsonl`.
pub fn write_local_ramz_bundle(out_dir: &Path, page: &RamzCapturedPage) -> Result<PathBuf> {
    let bundle_dir = out_dir.join(&page.blake3_digest);
    let assets_dir = bundle_dir.join("assets");
    std::fs::create_dir_all(&assets_dir)?;

    if !page.raw_html_zstd.is_empty() {
        std::fs::write(bundle_dir.join("raw.html.zst"), &page.raw_html_zstd)?;
    }

    let mut md = page.markdown.clone();
    if !page.visuals.is_empty() {
        md.push_str("\n\n## Captured Visuals\n");
        for v in &page.visuals {
            md.push_str(&format!(
                "\n![{}](assets/{}.webp)\n",
                v.alt.replace(']', "\\]"),
                v.blake3_hash
            ));
            std::fs::write(
                assets_dir.join(format!("{}.webp", v.blake3_hash)),
                &v.webp_bytes,
            )?;
        }
    }
    std::fs::write(bundle_dir.join("article.md"), md.as_bytes())?;

    let manifest_json = serde_json::to_string_pretty(page)?;
    std::fs::write(bundle_dir.join("manifest.json"), &manifest_json)?;

    use std::io::Write as _;
    let mut jsonl = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_dir.join("ramz-manifest.jsonl"))?;
    writeln!(jsonl, "{}", serde_json::to_string(page)?)?;

    Ok(bundle_dir)
}

/// HTTP ingestion client targeting Reverie's internal daemon (`/v1/archives/ingest`)
/// and/or Cloudflare Edge visual data store (`/v1/sources`).
#[derive(Debug, Clone)]
pub struct ReverieVisualSink {
    client: reqwest::Client,
    pub mode: ReverieSinkMode,
    pub daemon_url: String,
    pub edge_url: String,
    pub bearer_token: Option<String>,
}

impl ReverieVisualSink {
    pub fn new(
        mode: ReverieSinkMode,
        daemon_url: impl Into<String>,
        edge_url: impl Into<String>,
        bearer_token: Option<String>,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(45))
            .build()?;
        Ok(Self {
            client,
            mode,
            daemon_url: daemon_url.into().trim_end_matches('/').to_string(),
            edge_url: edge_url.into().trim_end_matches('/').to_string(),
            bearer_token,
        })
    }

    /// Push one `RamzCapturedPage` to the configured Reverie visual data plane(s).
    pub async fn ingest(&self, page: &RamzCapturedPage) -> Result<IngestReceipt> {
        let mut receipt = IngestReceipt {
            url: page.url.clone(),
            blake3_digest: page.blake3_digest.clone(),
            visual_count: page.visuals.len(),
            ..Default::default()
        };

        if matches!(self.mode, ReverieSinkMode::Daemon | ReverieSinkMode::Both) {
            let visuals_json: Vec<serde_json::Value> = page
                .visuals
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "data_base64": base64::engine::general_purpose::STANDARD.encode(&v.webp_bytes),
                        "source_url": v.source_url,
                        "alt": v.alt,
                        "kind": v.kind.as_str(),
                    })
                })
                .collect();

            let payload = serde_json::json!({
                "url": page.url,
                "final_url": page.final_url,
                "title": page.title,
                "raw_html_base64": (!page.raw_html.is_empty()).then(|| {
                    base64::engine::general_purpose::STANDARD.encode(&page.raw_html)
                }),
                "markdown": page.markdown,
                "project": page.project,
                "tags": page.tags,
                "spider_name": page.spider_name,
                "crawl_id": page.crawl_id,
                "visuals": visuals_json,
            });

            let mut req = self
                .client
                .post(format!("{}/v1/archives/ingest", self.daemon_url))
                .json(&payload);
            if let Some(ref tok) = self.bearer_token {
                req = req.bearer_auth(tok);
            }
            let resp = req.send().await.context("POST /v1/archives/ingest")?;
            let status = resp.status().as_u16();
            receipt.daemon_status = Some(status);
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            if status >= 400 {
                bail!("reverie daemon ingest failed (HTTP {status}): {body}");
            }
            receipt.daemon_archive_id = body
                .get("id")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
        }

        if matches!(self.mode, ReverieSinkMode::Edge | ReverieSinkMode::Both) {
            let images_json: Vec<serde_json::Value> = page
                .visuals
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "data_base64": base64::engine::general_purpose::STANDARD.encode(&v.webp_bytes),
                        "mime": "image/webp",
                        "alt": v.alt,
                    })
                })
                .collect();

            let payload = serde_json::json!({
                "url": page.final_url,
                "title": page.title,
                "markdown": page.markdown,
                "project": page.project,
                "images": images_json,
            });

            let mut req = self
                .client
                .post(format!("{}/v1/sources", self.edge_url))
                .json(&payload);
            if let Some(ref tok) = self.bearer_token {
                req = req.bearer_auth(tok);
            }
            let resp = req.send().await.context("POST /v1/sources")?;
            let status = resp.status().as_u16();
            receipt.edge_status = Some(status);
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            if status >= 400 {
                bail!("reverie edge visual ingest failed (HTTP {status}): {body}");
            }
            receipt.edge_source_id = body
                .pointer("/source/id")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned);
        }

        Ok(receipt)
    }
}

/// Crawl one URL (or subtree up to `limit`) with `cerebral-work/spider` using a remote
/// Chrome/Obscura CDP endpoint, capturing rendered HTML and a `ramz` visual screenshot per page.
#[allow(clippy::too_many_arguments)]
pub async fn crawl_with_spider_cdp(
    seed_url: &str,
    chrome_cdp_url: &str,
    limit: u32,
    depth: usize,
    full_page: bool,
    project: &str,
    spider_name: &str,
    crawl_id: &str,
    tags: &[String],
) -> Result<Vec<RamzCapturedPage>> {
    let mut website = Website::new(seed_url);
    let shot_cfg = ScreenShotConfig::new(
        ScreenshotParams {
            cdp_params: CaptureScreenshotParams {
                format: Some(CaptureScreenshotFormat::Png),
                ..Default::default()
            },
            full_page: Some(full_page),
            omit_background: Some(false),
        },
        true,
        false,
        None,
    );
    website
        .with_chrome_connection(Some(chrome_cdp_url.to_string()))
        .with_viewport(Some(Viewport::new(1400, 900)))
        .with_wait_for_delay(Some(WaitForDelay::new(Some(
            std::time::Duration::from_millis(750),
        ))))
        .with_screenshot(Some(shot_cfg))
        .with_limit(limit)
        .with_depth(depth)
        .with_respect_robots_txt(true);
    let mut website = website
        .build()
        .map_err(|e| anyhow::anyhow!("build spider website: {e:?}"))?;

    let mut rx = website.subscribe(16);
    let project_owned = project.to_string();
    let spider_owned = spider_name.to_string();
    let crawl_owned = crawl_id.to_string();
    let tags_owned = tags.to_vec();

    let collector = tokio::spawn(async move {
        let mut captured = Vec::new();
        while let Ok(page) = rx.recv().await {
            let page_url = page.get_url().to_string();
            let final_url = page.get_url_final().to_string();
            let html = page.get_html();
            let png_bytes = match page.screenshot_bytes.clone() {
                Some(b) if !b.is_empty() => b,
                _ => {
                    page.screenshot(
                        full_page,
                        false,
                        CaptureScreenshotFormat::Png,
                        None,
                        Option::<PathBuf>::None,
                        None,
                    )
                    .await
                }
            };

            let title = extract_html_title(&html, &final_url);
            let mut visuals = Vec::new();
            if !png_bytes.is_empty() {
                visuals.push((
                    png_bytes,
                    format!("capture://spider-cdp/{}", captured.len()),
                    format!("{title} — rendered page visual capture"),
                    VisualCaptureKind::PageScreenshot,
                ));
            }
            if let Ok(bundle) = build_ramz_captured_page(
                &page_url,
                Some(&final_url),
                Some(&title),
                html.into_bytes(),
                None,
                visuals,
                &project_owned,
                &spider_owned,
                &crawl_owned,
                tags_owned.clone(),
            ) {
                captured.push(bundle);
            }
        }
        captured
    });

    website.crawl().await;
    website.unsubscribe();
    let pages = collector.await.context("join spider collector")?;
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn sample_png(w: u32, h: u32) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([(x % 250) as u8, (y % 200) as u8, 140, 255])
        });
        let mut cur = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cur, ImageFormat::Png)
            .unwrap();
        cur.into_inner()
    }

    #[test]
    fn transcode_downscales_wide_screenshot_to_1280_webp_and_writes_bundle() {
        let png = sample_png(2000, 1000);
        let page = build_ramz_captured_page(
            "https://arm.jonpalmisc.com/latest_aarch64/abs.html",
            None,
            None,
            b"<html><head><title>ABS | Arm</title></head><body><h1>ABS</h1><p>Absolute value</p></body></html>".to_vec(),
            None,
            vec![(
                png,
                "capture://spider-cdp/0".to_string(),
                "ABS bitfield screenshot".to_string(),
                VisualCaptureKind::PageScreenshot,
            )],
            "aarch64",
            "crawllab-arm",
            "crawl-unit-1",
            vec!["isa:aarch64".to_string()],
        )
        .unwrap();

        assert_eq!(page.title, "ABS | Arm");
        assert_eq!(page.visuals.len(), 1);
        assert_eq!(page.visuals[0].width, 1280);
        assert_eq!(page.visuals[0].height, 640);
        assert_eq!(&page.visuals[0].webp_bytes[0..4], b"RIFF");
        assert_eq!(&page.visuals[0].webp_bytes[8..12], b"WEBP");
        assert!(page.raw_html_zstd_bytes > 0);

        let dir = tempfile::tempdir().unwrap();
        let bundle_path = write_local_ramz_bundle(dir.path(), &page).unwrap();
        assert!(bundle_path.join("raw.html.zst").exists());
        assert!(bundle_path.join("article.md").exists());
        assert!(bundle_path.join("manifest.json").exists());
        assert!(bundle_path
            .join(format!("assets/{}.webp", page.visuals[0].blake3_hash))
            .exists());
        assert!(dir.path().join("ramz-manifest.jsonl").exists());
    }

    #[tokio::test]
    async fn reverie_visual_sink_posts_to_both_daemon_and_edge_visual_stores() {
        let png = sample_png(640, 360);
        let page = build_ramz_captured_page(
            "https://arm.jonpalmisc.com/latest_sysreg/AArch64-actlr_el1.html",
            None,
            Some("ACTLR_EL1"),
            b"<html><body><h1>ACTLR_EL1</h1></body></html>".to_vec(),
            Some("# ACTLR_EL1\n\nAuxiliary Control Register.".to_string()),
            vec![(
                png,
                "capture://spider-cdp/0".to_string(),
                "ACTLR_EL1 bit layout".to_string(),
                VisualCaptureKind::PageScreenshot,
            )],
            "aarch64",
            "crawllab-arm",
            "crawl-unit-2",
            vec!["sysreg:aarch64".to_string()],
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let mut seen_paths = Vec::new();
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let n = sock.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("").to_string();
                seen_paths.push(first_line.clone());
                let body = if first_line.contains("/v1/archives/ingest") {
                    r#"{"id":"arc-test-1"}"#
                } else {
                    r#"{"source":{"id":"src_test_1"},"visuals":[],"deduplicated":false}"#
                };
                let resp = format!(
                    "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
            seen_paths
        });

        let base = format!("http://{addr}");
        let sink = ReverieVisualSink::new(ReverieSinkMode::Both, &base, &base, None).unwrap();
        let receipt = sink.ingest(&page).await.unwrap();
        assert_eq!(receipt.daemon_status, Some(201));
        assert_eq!(receipt.daemon_archive_id.as_deref(), Some("arc-test-1"));
        assert_eq!(receipt.edge_status, Some(201));
        assert_eq!(receipt.edge_source_id.as_deref(), Some("src_test_1"));

        let paths = server.await.unwrap();
        assert!(paths
            .iter()
            .any(|l| l.starts_with("POST /v1/archives/ingest ")));
        assert!(paths.iter().any(|l| l.starts_with("POST /v1/sources ")));
    }
}
