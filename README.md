# Video Optimizer (Rust)

A fast, multithreaded CLI utility built in Rust that recursively scans a directory for video files, verifies metadata tags, and optimizes them using `ffmpeg`. It skips re-encoding files that have already been tagged, preserves original files when encoding yields no size reduction, and displays real-time progress indicators.

---

## Features

- **Parallel Tag Inspection:** Leverages `rayon` to rapidly scan directories and inspect file metadata using `ffprobe`.
- **Idempotent Processing:** Tags processed files with metadata (default `comment=optimized_by_vault`) to avoid redundant re-encoding.
- **Size-Reduction Guard:** If an encoded file turns out larger than the original, the operation is skipped and metadata is updated in place via stream-copying without quality degradation.
- **Real-time Progress:** Parses FFmpeg's `-progress` stream directly via `out_time_us` to provide accurate terminal progress bars and percentages.
- **Configurable Encoders:** Full control over video codec, audio codec, CRF quality target, preset, and bitrates via CLI arguments or environment variables.

---

## Prerequisites

Ensure the following tools are installed and available in your system's `$PATH`:

- **Rust toolchain** (Cargo & rustc 1.70+)
- **FFmpeg** (`ffmpeg` executable)
- **FFprobe** (`ffprobe` executable)

---

## Installation & Building

### 1. Clone the Repository
```bash
git clone <repository-url>
cd video_optimizer

### 2. Build the Release Binary
```Bash
cargo make build