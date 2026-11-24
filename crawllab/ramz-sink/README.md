# `crawllab/ramz-sink`

Integrates `cerebral-work/spider` (Crawllab estate fork) with Reverie's `ramz` visual capture pipeline and Reverie visual data stores.

## Pipeline

1. **Crawl & Visual Capture (`cerebral-work/spider` + Chrome/Obscura CDP):**
   Drives `spider::website::Website` over a remote CDP browser (`--chrome-cdp`), fetching rendered post-JS HTML and capturing page screenshots (`CaptureScreenshotFormat::Png`).
2. **`ramz` Visual Packaging:**
   - Compresses raw HTML with `zstd` level 3 (`raw.html.zst`).
   - Downscales visual captures proportionally (`<= 1280px`, Lanczos3), transcodes to `image/webp`, and content-addresses every asset with `BLAKE3` (`assets/<blake3>.webp`).
   - Persists deterministic local bundles under `<out>/<blake3>/` + `ramz-manifest.jsonl`.
3. **Reverie Visual Data Plane Ingestion:**
   - `--sink daemon`: `POST <daemon>/v1/archives/ingest` (`reveried` `:7437` -> `web_archives` / `web_archives_local`, `ArchiveBlobStore` WebP assets, and searchable `reference` observations).
   - `--sink edge`: `POST <edge>/v1/sources` (`api.usereverie.dev` -> `TenantMemoryDO` `sources` + `observation_visuals`, Cloudflare R2 `SOURCES_BUCKET`, Workers AI Llama 3.2 11B Vision caption/OCR, and Vectorize).
   - `--sink both`: dual-writes both visual data planes.

## Usage

```bash
# Build (host throttling: -j 2)
cargo build -j 2 --release

# Crawl with Obscura/Chromium CDP and ingest into internal reveried + local ramz archive
REVERIE_API_TOKEN="..." ./target/release/crawllab-ramz-capture \
  --url https://arm.jonpalmisc.com/latest_aarch64/index.html \
  --chrome-cdp ws://127.0.0.1:9222/devtools/browser/<id> \
  --out /home/ctodie/research/corpora/aarch64-arm-ref-ramz \
  --project aarch64 \
  --spider-name crawllab-arm \
  --sink daemon \
  --reverie-daemon-url http://127.0.0.1:7437
```
