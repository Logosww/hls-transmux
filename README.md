# hls-transmux

一个轻量级的 Rust HLS → MP4 transmuxer。读取 HLS playlist（本地文件或 HTTP/HTTPS），把底层的 MPEG-TS 或 fMP4/CMAF 分片解封装后直接重封装为单个 MP4，**不解码、不编码、不转码**。

核心 HLS / TS / ISOBMFF 逻辑全部自研，仅依赖少量基础异步与 HTTP 库。

## 特性

**输入**
- HLS media playlist 与 master playlist（显式 variant 索引选择）
- 本地文件路径与 HTTP/HTTPS 源（异步 API）
- 分片格式：MPEG-TS 与 fMP4 / CMAF（`#EXT-X-MAP`）
- `#EXT-X-BYTERANGE`（分片与 init segment 均支持）

**Codec**
- 视频：H.264 / AVC、H.265 / HEVC
- 音频：AAC-LC

**输出**（[`OutputFormat`]）

| 变体 | 输出布局 | pipeline | 峰值内存 | 中断可播放 |
| --- | --- | --- | --- | --- |
| `Mp4`（默认） | `ftyp` + `moov` + `mdat` | batch（全部 demux 到内存再 mux） | 高 | 否 |
| `FragmentedMp4` | `ftyp` + `moov` + 每 segment `moof` + `mdat` | streaming（逐 segment 写盘） | 低 | 是（fMP4） |
| `StreamingMp4` | `ftyp` + `moov` + `mdat` | streaming fMP4 → defrag | 低 | 是（temp 文件为 fMP4） |

`StreamingMp4` 输出与 `Mp4` 完全一致，但用流式 fMP4 pipeline（写临时 fMP4 文件）+ 末端 defrag，峰值内存更低，长输入更友好。临时文件 `<output>.partial.<ext>` 是合法可播放的 fMP4，中断后可直接播放已下载部分。

## 安装

```toml
[dependencies]
hls-transmux = "0.1"
```

默认启用 `default-source` feature（内置 reqwest-backed HTTP 客户端）。若要完全移除 reqwest 依赖、自行实现 HTTP 读取：

```toml
[dependencies]
hls-transmux = { version = "0.1", default-features = false }
```

可选启用 `ffmpeg-finalize` feature，在 `StreamingMp4` finalization 阶段用 ffmpeg（via `ffmpeg-next`）做 remux，替代自研 defrag 路径。需要系统安装 FFmpeg 8 共享库 + pkg-config：

```toml
[dependencies]
hls-transmux = { version = "0.1", features = ["ffmpeg-finalize"] }
```

## 自定义 Source

本 crate 只专注 transmux 能力，资源读取（playlist 文本 + segment 字节）通过 [`Source`] trait 抽象。内置 [`ReqwestSource`] 作为默认实现，调用方可以替换为自行实现：

```rust
use std::path::PathBuf;
use std::sync::Arc;
use hls_transmux::{
    ByteRange, HlsInput, OutputFormat, Source, SourceLocation,
    TextResource, TransmuxOptions, VariantSelection, transmux_hls_to_mp4_async,
};

#[derive(Debug)]
struct MySource;

impl Source for MySource {
    fn read_text<'a>(
        &'a self,
        location: &'a SourceLocation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = hls_transmux::Result<TextResource>> + Send + 'a>> {
        Box::pin(async move {
            // 自行实现：从 location 读取文本，返回最终 location（处理 redirect 等）
            todo!()
        })
    }

    fn read_bytes<'a>(
        &'a self,
        location: &'a SourceLocation,
        range: Option<&'a ByteRange>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = hls_transmux::Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move {
            // 自行实现：从 location 读取字节，按 range 切片
            todo!()
        })
    }
}

# async fn run() -> hls_transmux::Result<()> {
let report = transmux_hls_to_mp4_async(
    HlsInput::custom(
        Arc::new(MySource),
        SourceLocation::File(PathBuf::from("playlist.m3u8")),
    ),
    "output.mp4",
    TransmuxOptions::default(),
).await?;
# Ok(())
# }
```

## 快速开始

### 本地 VOD playlist → 标准 MP4

```rust
use hls_transmux::{
    HlsInput, TransmuxOptions, transmux_hls_to_mp4_async,
};

async fn run() -> hls_transmux::Result<()> {
    let report = transmux_hls_to_mp4_async(
        HlsInput::Path("playlist.m3u8".into()),
        "output.mp4",
        TransmuxOptions::default(),
    )
    .await?;
    println!(
        "写入 {} 字节，处理 {} 个分片",
        report.bytes_written, report.segment_count
    );
    Ok(())
}
```

### HTTP master playlist → 分片 MP4

```rust
use hls_transmux::{
    HlsInput, OutputFormat, TransmuxOptions, VariantSelection,
    transmux_hls_to_mp4_async,
};

async fn run() -> hls_transmux::Result<()> {
    let report = transmux_hls_to_mp4_async(
        HlsInput::Url("https://example.com/master.m3u8".to_string()),
        "output.fmp4",
        TransmuxOptions {
            variant: Some(VariantSelection::Index(0)),
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}
```

### HTTP master playlist → 流式标准 MP4（低内存）

```rust
use hls_transmux::{
    HlsInput, OutputFormat, TransmuxOptions, VariantSelection,
    transmux_hls_to_mp4_async,
};

async fn run() -> hls_transmux::Result<()> {
    let report = transmux_hls_to_mp4_async(
        HlsInput::Url("https://example.com/master.m3u8".to_string()),
        "output.mp4",
        TransmuxOptions {
            variant: Some(VariantSelection::Index(0)),
            output_format: OutputFormat::StreamingMp4,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}
```

### 流式标准 MP4 + ffmpeg finalization（需 `ffmpeg-finalize` feature）

```rust
use hls_transmux::{
    FinalizeBackend, HlsInput, OutputFormat, TransmuxOptions, VariantSelection,
    transmux_hls_to_mp4_async,
};

async fn run() -> hls_transmux::Result<()> {
    let report = transmux_hls_to_mp4_async(
        HlsInput::Url("https://example.com/master.m3u8".to_string()),
        "output.mp4",
        TransmuxOptions {
            variant: Some(VariantSelection::Index(0)),
            output_format: OutputFormat::StreamingMp4,
            finalize_backend: FinalizeBackend::Ffmpeg,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}
```

需要阻塞调用时，用 tokio runtime 包一层即可：

```rust
let report = tokio::runtime::Runtime::new()
    .unwrap()
    .block_on(transmux_hls_to_mp4_async(
        HlsInput::Path("playlist.m3u8".into()),
        "output.mp4",
        TransmuxOptions::default(),
    ))
    .unwrap();
```

## API 一览

| 名称 | 说明 |
| --- | --- |
| [`transmux_hls_to_mp4_async`] | 唯一入口，支持本地/HTTP/自定义 Source、master playlist、byterange、fMP4 输入与三种输出格式 |
| [`HlsInput`] | 输入源（`Path` / `Url` / `Custom`） |
| [`Source`] / [`SourceLocation`] / [`TextResource`] / [`ByteRange`] | 自定义资源读取的 trait 与配套类型 |
| [`ReqwestSource`] | 内置 reqwest-backed `Source` 实现（`default-source` feature） |
| [`TransmuxOptions`] | 选项：`variant`、`output_format`、`finalize_backend` |
| [`OutputFormat`] | `Mp4`（默认）/ `FragmentedMp4` / `StreamingMp4` |
| [`FinalizeBackend`] | `StreamingMp4` 的 finalization 后端：`Native`（默认，自研 defrag）/ `Ffmpeg`（需 `ffmpeg-finalize` feature） |
| [`VariantSelection`] | master playlist 的 variant 选择（目前仅 `Index`） |
| [`TransmuxReport`] | 返回值：segment 数、track 信息、duration、写入字节数 |
| [`Error`] / [`Result`] | 结构化错误，区分 I/O、HTTP、非法输入、不支持特性、bitstream、muxing |

完整文档：`cargo doc --open`。

## 暂不支持

以下场景会返回结构化的 `Error::Unsupported`：

- 加密：AES-128 / SAMPLE-AE
- Live playlist、`#EXT-X-DISCONTINUITY`
- Alternate audio group、多视频 / 多音频 track
- 非 AVC / HEVC / AAC-LC 的 codec（如 MP3、AC-3、E-AC-3、AV1）
- 输出超过 4 GiB（非分片 MP4 受 32-bit 偏移限制）

## 设计说明

- 内部时间戳统一保留 PTS / DTS，TS 使用 90 kHz 时钟，输出以首个 DTS 归零。
- TS 与 fMP4 demuxer 共用同一份 `DemuxOutput` 结构，AVC / HEVC 共用 Annex B start code 扫描。
- 分片 MP4 的 `trun` `data_offset` 在写入前预计算，避免回填。
- `StreamingMp4` 的临时文件用 `.partial.<ext>` 命名，扩展名仍是 `.mp4`，中断时是可直接播放的 fMP4。
- 仅做 remux，不引入高层 m3u8 / TS / MP4 parser-muxer 依赖。

## License

MIT OR Apache-2.0。
