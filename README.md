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

可选启用 `serde` feature，为 `TransmuxResumeState` 派生 `Serialize`/`Deserialize`，便于 app 直接持久化续传 checkpoint：

```toml
[dependencies]
hls-transmux = { version = "0.1", features = ["serde"] }
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

## 并发下载

`ReqwestSource` 默认串行下载分片。通过 [`ReqwestSource::with_concurrency`] 启用有界并发预取（opt-in），让内置 HTTP 客户端在 transmuxer 顺序消费之前并发拉取最多 `concurrency` 个分片：

```rust
use std::sync::Arc;
use hls_transmux::{
    HlsInput, OutputFormat, ReqwestSource, SourceLocation,
    TransmuxOptions, VariantSelection, transmux_hls_to_mp4_async,
};

# async fn run() -> hls_transmux::Result<()> {
let source = Arc::new(ReqwestSource::with_concurrency(8));
let location = SourceLocation::Url(
    url::Url::parse("https://example.com/media.m3u8").unwrap()
);
let report = transmux_hls_to_mp4_async(
    HlsInput::custom(source, location),
    "output.fmp4",
    TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        ..Default::default()
    },
).await?;
# Ok(())
# }
```

**内存有界**：mpsc channel 作为计数信号量，outstanding（InFlight + Ready-unconsumed）分片数 ≤ `concurrency`。消费侧（`read_bytes`）取出字节后归还 token，coordinator 才会 spawn 下一个 fetch。与 `StreamingMp4` 低内存 pipeline 协同友好。

**触发条件**：
- `concurrency > 1`
- 输入为 HTTP/HTTPS URL（本地文件顺序读已足够快，不预取）
- `read_text` 返回 media playlist（master playlist 不预取 —— variant 尚未选定）

**透明性**：transmuxer 仍按 `segments[i]` 顺序调用 `read_bytes(url)`，并发预取对 transmux 逻辑完全透明 —— 字节可能已在 slot cache 中，也可能需要等 fetch 完成。`concurrency = 1` 走原串行路径，零开销。

`HlsInput::Url` / `HlsInput::Path` 不变（仍用 `ReqwestSource::new()`，串行）；并发用户通过 `HlsInput::custom` 显式传入 `ReqwestSource::with_concurrency(n)` 启用。

## 进度回调 / 取消 / 续传

`TransmuxOptions` 提供三个可选钩子，均默认 `None`（行为与不传时完全一致，不破坏现有调用方）：

- `on_progress`：逐分片进度回调
- `cancel`：协作取消令牌
- `resume`：断点续传 checkpoint

### 进度回调

每个分片处理完成后（demux + 写盘），crate 同步调用 `on_progress` 回调，报告当前进度与续传快照：

```rust
use std::sync::{Arc, Mutex};
use hls_transmux::{
    HlsInput, OutputFormat, TransmuxOptions, TransmuxProgress,
    transmux_hls_to_mp4_async,
};

# async fn run() -> hls_transmux::Result<()> {
let events: Arc<Mutex<Vec<TransmuxProgress>>> = Arc::new(Mutex::new(Vec::new()));
let events_cb = events.clone();

let report = transmux_hls_to_mp4_async(
    HlsInput::Path("playlist.m3u8".into()),
    "output.fmp4",
    TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        on_progress: Some(Arc::new(move |p: TransmuxProgress| {
            events_cb.lock().unwrap().push(p);
        })),
        ..Default::default()
    },
)
.await?;
# Ok(())
# }
```

`TransmuxProgress` 字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `total_segments` | `usize` | playlist 总分片数 |
| `completed_segments` | `usize` | 已完成分片数 |
| `downloaded_bytes` | `u64` | 累计已下载分片字节（不含 init segment） |
| `bytes_written` | `u64` | 已写盘字节（`Mp4` batch 路径恒为 0） |
| `current_segment_index` | `usize` | 刚完成的分片下标 |
| `resume` | `TransmuxResumeState` | 当前续传快照，app 应在每次回调时持久化 |

### 协作取消

`cancel` 在每个分片迭代开头检查；取消后返回 `Error::Cancelled`。`StreamingMp4` 路径下 `.partial.mp4` 保留（含已写 fragment，是可播放的 fMP4），可直接用于续传。

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use hls_transmux::{CancelToken, Error, HlsInput, OutputFormat, TransmuxOptions, transmux_hls_to_mp4_async};

#[derive(Debug, Default)]
struct MyCancelToken(Arc<AtomicBool>);

impl MyCancelToken {
    fn trigger(&self) { self.0.store(true, Ordering::SeqCst); }
}

impl CancelToken for MyCancelToken {
    fn is_cancelled(&self) -> bool { self.0.load(Ordering::SeqCst) }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

# async fn run() -> hls_transmux::Result<()> {
let token = Arc::new(MyCancelToken::default());
let opts = TransmuxOptions {
    output_format: OutputFormat::StreamingMp4,
    cancel: Some(token.clone()),
    ..Default::default()
};
let result = transmux_hls_to_mp4_async(
    HlsInput::Path("playlist.m3u8".into()),
    "output.mp4",
    opts,
).await;
// 取消后得到 Error::Cancelled，.partial.mp4 保留
assert!(matches!(result, Err(Error::Cancelled)));
# Ok(())
# }
```

`CancelToken` 是零依赖 trait，app 侧可包装 `tokio_util::sync::CancellationToken` 或任意取消原语。

### 断点续传

`resume` 让 crate 跳过 `segments[..completed_segments]`，以 append 模式打开已有输出文件继续写。app 负责在每次 `on_progress` 回调时持久化 `TransmuxResumeState` 快照，取消/崩溃后传回 crate 续传。

```rust
use hls_transmux::{
    HlsInput, OutputFormat, TransmuxOptions, TransmuxResumeState,
    transmux_hls_to_mp4_async,
};

# async fn run() -> hls_transmux::Result<()> {
// app 从持久化层读回上次保存的 checkpoint
let saved: TransmuxResumeState = load_from_db()?;

let report = transmux_hls_to_mp4_async(
    HlsInput::Path("playlist.m3u8".into()),
    "output.fmp4",           // 同一文件，crate 以 append 模式打开
    TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        resume: Some(saved),
        ..Default::default()
    },
)
.await?;
# Ok(())
# }
# fn load_from_db() -> hls_transmux::Result<TransmuxResumeState> { unimplemented!() }
```

`TransmuxResumeState` 4 字段：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `completed_segments` | `usize` | 已完成分片数；续传跳过 `segments[..completed_segments]` |
| `bytes_written` | `u64` | 输出文件当前字节偏移；crate 以 append 模式打开后从此偏移继续写 |
| `next_sequence` | `u32` | 下一个 fragment 的 mfhd sequence number |
| `global_base_dts_90k` | `u64` | 首包 DTS（90k 时钟域），所有 sample 时间线归零基准 |

**约束**：
- 仅 `StreamingMp4` / `FragmentedMp4` 支持续传；`Mp4` + `resume` 返回 `Error::InvalidInput`
- 续传时 crate 重新 demux `segments[0]` 重建 codec config（tracks 不进 checkpoint，跨版本更稳定）
- 续传完成时 crate 扫描已有 `.partial.mp4` 的 moof 重建历史 `tfra` entries，输出完整 `mfra` box（与首次完成的输出字节一致，仅 wall-clock 时间戳差异）

### `serde` feature

启用 `serde` feature 为 `TransmuxResumeState` 派生 `Serialize`/`Deserialize`，便于 app 直接持久化：

```toml
[dependencies]
hls-transmux = { version = "0.1", features = ["serde"] }
```

```rust
# #[cfg(feature = "serde")] {
# use hls_transmux::TransmuxResumeState;
let json = serde_json::to_string(&resume_state)?;
let restored: TransmuxResumeState = serde_json::from_str(&json)?;
# }
# fn serde_json<T>(_: T) -> Result<T, ()> { unimplemented!() }
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

`VariantSelection` 三策略：

| 变体 | 行为 |
| --- | --- |
| `Index(n)` | 显式指定零基索引（原行为） |
| `HighestBandwidth` | 选 `BANDWIDTH` 最高的 variant；`bandwidth=None` 视为 0 |
| `LowestBandwidth` | 选 `BANDWIDTH` 最低的 variant；`bandwidth=None` 视为 `u64::MAX` |

并列时（多个 variant 带宽相同）按 Rust `max_by_key` / `min_by_key` 语义返回最后一个匹配元素。

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
| [`TransmuxOptions`] | 选项：`variant`、`output_format`、`finalize_backend`、`on_progress`、`cancel`、`resume` |
| [`OutputFormat`] | `Mp4`（默认）/ `FragmentedMp4` / `StreamingMp4` |
| [`FinalizeBackend`] | `StreamingMp4` 的 finalization 后端：`Native`（默认，自研 defrag）/ `Ffmpeg`（需 `ffmpeg-finalize` feature） |
| [`TransmuxProgress`] | 进度事件：`total_segments`、`completed_segments`、`downloaded_bytes`、`bytes_written`、`resume` |
| [`CancelToken`] | 协作取消 trait：`is_cancelled` / `cancelled`（零依赖，app 自实现） |
| [`TransmuxResumeState`] | 续传 checkpoint：`completed_segments`、`bytes_written`、`next_sequence`、`global_base_dts_90k`（可选 `serde` derive） |
| [`VariantSelection`] | master playlist 的 variant 选择（`Index` / `HighestBandwidth` / `LowestBandwidth`） |
| [`TransmuxReport`] | 返回值：segment 数、track 信息、duration、写入字节数 |
| [`Error`] / [`Result`] | 结构化错误，区分 I/O、HTTP、非法输入、不支持特性、bitstream、muxing、取消 |

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
