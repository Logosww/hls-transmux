use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use url::Url;

use crate::error::{Error, Result};

#[cfg(feature = "default-source")]
use std::collections::HashMap;

/// A byte sub-range within a larger resource, used for `#EXT-X-BYTERANGE`
/// segment reads and Range requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

/// A resolvable input location: either a local filesystem path or a URL.
///
/// Exposed publicly because it is part of the [`Source`] trait contract and
/// the [`HlsInput::Custom`] variant. The transmuxer resolves relative URIs
/// found in playlists against the location of the playlist they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceLocation {
    File(PathBuf),
    Url(Url),
}

impl SourceLocation {
    pub(crate) fn resolve(&self, uri: &str) -> Result<Self> {
        if uri.starts_with("http://") || uri.starts_with("https://") {
            return Ok(Self::Url(
                Url::parse(uri).map_err(|error| Error::invalid(error.to_string()))?,
            ));
        }

        match self {
            Self::File(path) => {
                let base = path.parent().unwrap_or_else(|| std::path::Path::new(""));
                Ok(Self::File(base.join(uri)))
            }
            Self::Url(url) => Ok(Self::Url(
                url.join(uri)
                    .map_err(|error| Error::invalid(error.to_string()))?,
            )),
        }
    }
}

/// A text resource (typically a playlist) returned by [`Source::read_text`].
#[derive(Debug, Clone)]
pub struct TextResource {
    pub content: String,
    pub location: SourceLocation,
}

/// Abstracts how the transmuxer reads playlists and segment bytes.
///
/// The crate ships a built-in reqwest-backed implementation (`ReqwestSource`,
/// available with the `default-source` cargo feature) used by default for
/// [`HlsInput::Path`] and [`HlsInput::Url`]. Callers that
/// want to plug in a different HTTP client, caching layer, proxy, retry policy,
/// or a fully offline source should implement this trait and pass it via
/// [`HlsInput::custom`].
///
/// The trait uses boxed futures (no `async-trait` dependency) so it is
/// object-safe and can be used as `Arc<dyn Source>`.
pub trait Source: Send + Sync + std::fmt::Debug {
    /// Reads the full text content at `location` (typically a `.m3u8`
    /// playlist). Implementations should follow HTTP redirects and return the
    /// final resolved location in [`TextResource::location`].
    fn read_text<'a>(
        &'a self,
        location: &'a SourceLocation,
    ) -> Pin<Box<dyn Future<Output = Result<TextResource>> + Send + 'a>>;

    /// Reads raw bytes at `location`, optionally restricted to `range`. For
    /// HTTP sources, `range` maps to a `Range: bytes=start-end` request; the
    /// implementation must verify the server actually returned partial content
    /// (status 206) and reject short reads. For local files, `range` is a
    /// simple slice.
    fn read_bytes<'a>(
        &'a self,
        location: &'a SourceLocation,
        range: Option<&'a ByteRange>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

/// Built-in `reqwest`-backed [`Source`] implementation. Available with the
/// `default-source` cargo feature (enabled by default).
///
/// Use [`ReqwestSource::new`] for a default client, or construct a custom
/// `reqwest::Client` (e.g. with proxies, custom TLS, retry policies) and pass
/// it to [`ReqwestSource::with_client`].
///
/// # Concurrent segment downloads
///
/// By default `ReqwestSource` downloads segments sequentially. To enable
/// bounded concurrent prefetch, use [`ReqwestSource::with_concurrency`] (or
/// [`ReqwestSource::with_client_and_concurrency`]) with `concurrency > 1`.
/// When enabled, the source detects media playlists returned by `read_text`
/// and spawns a background coordinator that prefetches up to `concurrency`
/// segments ahead of the transmuxer's sequential consumption. Memory is
/// bounded: at most `concurrency` segment bodies are held in flight or ready
/// at any time. This is transparent to the transmuxer — it still calls
/// `read_bytes(url)` sequentially, but the bytes may already be cached.
///
/// Local file inputs and master playlists are never prefetched.
#[cfg(feature = "default-source")]
pub struct ReqwestSource {
    http: reqwest::Client,
    concurrency: usize,
    /// Lazy prefetch state, initialized on the first `read_text` that returns
    /// a media playlist (when `concurrency > 1`). `OnceLock` is used because
    /// `read_text` may be called twice (master → variant) and only the variant
    /// (media) playlist should trigger prefetch.
    state: std::sync::OnceLock<std::sync::Arc<PrefetchState>>,
}

#[cfg(feature = "default-source")]
impl std::fmt::Debug for ReqwestSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestSource")
            .field("http", &self.http)
            .field("concurrency", &self.concurrency)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "default-source")]
impl Default for ReqwestSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "default-source")]
impl Clone for ReqwestSource {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            concurrency: self.concurrency,
            // Each clone gets its own (lazily-initialized) prefetch state.
            // Clones are independent — they do not share prefetch caches.
            state: std::sync::OnceLock::new(),
        }
    }
}

#[cfg(feature = "default-source")]
impl ReqwestSource {
    /// Creates a new `ReqwestSource` with a default `reqwest::Client` and
    /// sequential (non-concurrent) downloads.
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            concurrency: 1,
            state: std::sync::OnceLock::new(),
        }
    }

    /// Creates a new `ReqwestSource` with a custom `reqwest::Client` and
    /// sequential (non-concurrent) downloads.
    pub fn with_client(http: reqwest::Client) -> Self {
        Self {
            http,
            concurrency: 1,
            state: std::sync::OnceLock::new(),
        }
    }

    /// Creates a new `ReqwestSource` with a default `reqwest::Client` and
    /// concurrent segment prefetch enabled. `concurrency` is clamped to a
    /// minimum of 1; values ≤ 1 disable prefetch (equivalent to [`Self::new`]).
    pub fn with_concurrency(concurrency: usize) -> Self {
        Self {
            http: reqwest::Client::new(),
            concurrency: concurrency.max(1),
            state: std::sync::OnceLock::new(),
        }
    }

    /// Creates a new `ReqwestSource` with a custom `reqwest::Client` and
    /// concurrent segment prefetch enabled. `concurrency` is clamped to a
    /// minimum of 1; values ≤ 1 disable prefetch (equivalent to
    /// [`Self::with_client`]).
    pub fn with_client_and_concurrency(http: reqwest::Client, concurrency: usize) -> Self {
        Self {
            http,
            concurrency: concurrency.max(1),
            state: std::sync::OnceLock::new(),
        }
    }

    /// Returns the configured concurrency level (1 = sequential).
    pub fn concurrency(&self) -> usize {
        self.concurrency
    }
}

#[cfg(feature = "default-source")]
impl Source for ReqwestSource {
    fn read_text<'a>(
        &'a self,
        location: &'a SourceLocation,
    ) -> Pin<Box<dyn Future<Output = Result<TextResource>> + Send + 'a>> {
        Box::pin(async move {
            let resource = match location {
                SourceLocation::File(path) => {
                    let content = tokio::fs::read_to_string(path).await?;
                    TextResource {
                        content,
                        location: location.clone(),
                    }
                }
                SourceLocation::Url(url) => {
                    let response = self
                        .http
                        .get(url.clone())
                        .send()
                        .await
                        .map_err(|e| Error::Http(e.to_string()))?;
                    if !response.status().is_success() {
                        return Err(Error::Http(format!(
                            "GET {url} returned status {}",
                            response.status()
                        )));
                    }
                    let final_url = response.url().clone();
                    let content = response
                        .text()
                        .await
                        .map_err(|e| Error::Http(e.to_string()))?;
                    TextResource {
                        content,
                        location: SourceLocation::Url(final_url),
                    }
                }
            };
            // Try to start prefetching if this is a media playlist and
            // concurrency is enabled. No-op for master playlists, local
            // files, or concurrency == 1.
            self.try_start_prefetch(&resource);
            Ok(resource)
        })
    }

    fn read_bytes<'a>(
        &'a self,
        location: &'a SourceLocation,
        range: Option<&'a ByteRange>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(async move {
            match location {
                SourceLocation::File(path) => {
                    let bytes = tokio::fs::read(path).await?;
                    apply_range(bytes, range)
                }
                SourceLocation::Url(url) => {
                    // Check the prefetch cache first. If there's a slot for
                    // this (url, range), wait for it and return the bytes.
                    // The MutexGuard is dropped at the end of the block so it
                    // doesn't cross the await boundary (MutexGuard is !Send).
                    let cached = self.state.get().map(|state| {
                        let key = (url.clone(), range.copied());
                        let slot = { state.slots.lock().unwrap().get(&key).cloned() };
                        (state, key, slot)
                    });
                    if let Some((state, key, Some(slot))) = cached {
                        return read_from_slot(state.clone(), &key, &slot).await;
                    }
                    // Fall through: init segment, URL not in prefetch list,
                    // or concurrency == 1 (state never initialized).
                    fetch_bytes_with_range(&self.http, url, range).await
                }
            }
        })
    }
}

/// Input source for the async transmux entry point.
///
/// `Path` and `Url` use the built-in `ReqwestSource` (when the
/// `default-source` feature is enabled). Callers that want to provide their
/// own HTTP client / cache / proxy / offline source should use
/// [`HlsInput::custom`] (or the [`HlsInput::Custom`] variant directly).
#[derive(Debug, Clone)]
pub enum HlsInput {
    /// A local filesystem path to a `.m3u8` playlist.
    Path(PathBuf),
    /// An HTTP/HTTPS URL pointing at a playlist.
    Url(String),
    /// Custom [`Source`] with an explicit starting [`SourceLocation`]. The
    /// source is responsible for resolving both the root location and any
    /// relative URIs the transmuxer derives from it.
    Custom(Arc<dyn Source>, SourceLocation),
}

impl HlsInput {
    /// Builds a [`HlsInput::Custom`] from a source implementation and a
    /// starting location.
    pub fn custom(source: Arc<dyn Source>, location: SourceLocation) -> Self {
        Self::Custom(source, location)
    }

    /// Splits the input into a starting location and a [`Source`] instance.
    pub(crate) fn into_parts(self) -> Result<(SourceLocation, Arc<dyn Source>)> {
        match self {
            Self::Path(path) => {
                #[cfg(feature = "default-source")]
                {
                    Ok((SourceLocation::File(path), Arc::new(ReqwestSource::new())))
                }
                #[cfg(not(feature = "default-source"))]
                {
                    let _ = path;
                    Err(Error::unsupported(
                        "HlsInput::Path requires the `default-source` cargo feature; \
                         enable it or use HlsInput::custom with your own Source impl",
                    ))
                }
            }
            Self::Url(url) => {
                #[cfg(feature = "default-source")]
                {
                    let location = SourceLocation::Url(
                        Url::parse(&url).map_err(|error| Error::invalid(error.to_string()))?,
                    );
                    Ok((location, Arc::new(ReqwestSource::new())))
                }
                #[cfg(not(feature = "default-source"))]
                {
                    let _ = url;
                    Err(Error::unsupported(
                        "HlsInput::Url requires the `default-source` cargo feature; \
                         enable it or use HlsInput::custom with your own Source impl",
                    ))
                }
            }
            Self::Custom(source, location) => Ok((location, source)),
        }
    }
}

/// Internal adapter that wraps an [`Arc<dyn Source>`] so the rest of the
/// transmux pipeline can keep using a small, owned `&SourceReader` handle.
#[derive(Debug, Clone)]
pub(crate) struct SourceReader {
    source: Arc<dyn Source>,
}

impl SourceReader {
    pub(crate) fn new(source: Arc<dyn Source>) -> Self {
        Self { source }
    }

    pub(crate) async fn read_text(&self, location: &SourceLocation) -> Result<TextResource> {
        self.source.read_text(location).await
    }

    pub(crate) async fn read_bytes(
        &self,
        location: &SourceLocation,
        range: Option<&ByteRange>,
    ) -> Result<Vec<u8>> {
        self.source.read_bytes(location, range).await
    }
}

#[cfg(feature = "default-source")]
fn apply_range(bytes: Vec<u8>, range: Option<&ByteRange>) -> Result<Vec<u8>> {
    let Some(range) = range else {
        return Ok(bytes);
    };
    let start = usize::try_from(range.offset)
        .map_err(|_| Error::invalid("byterange offset exceeds usize"))?;
    let length = usize::try_from(range.length)
        .map_err(|_| Error::invalid("byterange length exceeds usize"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| Error::invalid("byterange overflows usize"))?;
    if end > bytes.len() {
        return Err(Error::invalid(
            "byterange extends past the end of the segment",
        ));
    }
    Ok(bytes[start..end].to_vec())
}

// ---------------------------------------------------------------------------
// Concurrent download infrastructure (only compiled with `default-source`).
//
// Design: "bounded slot cache" — a counting semaphore (mpsc channel) bounds
// the number of outstanding (in-flight + ready-but-unconsumed) segment
// downloads. The coordinator pops targets in playlist order and spawns a
// fetch task per target. Consumers (`read_bytes`) look up the slot by
// (Url, Option<ByteRange>), wait for it to become Ready via `Notify`, take
// the bytes, return the token, and evict the slot.
//
// Why not BTreeMap by index? The Source trait API is URL-based, not
// index-based. The transmuxer already consumes segments in playlist order,
// so the Source only needs point lookups by URL — no ordered reassembly.
// ---------------------------------------------------------------------------

#[cfg(feature = "default-source")]
type SlotKey = (Url, Option<ByteRange>);

#[cfg(feature = "default-source")]
struct PrefetchState {
    /// Consumer side of the counting semaphore. The coordinator holds the
    /// `Receiver` and `recv()`s one token per spawned fetch. Consumers
    /// `send(())` a token back after reading (or failing to read) a slot.
    tokens_tx: tokio::sync::mpsc::Sender<()>,
    /// URL+range → slot. `std::sync::Mutex` because critical sections are
    /// tiny (insert / lookup / evict) and never await.
    slots: std::sync::Mutex<HashMap<SlotKey, std::sync::Arc<Slot>>>,
    /// Ordered queue of (Url, Option<ByteRange>) targets to prefetch.
    /// The coordinator pops from the front; consumers never touch this.
    targets: std::sync::Mutex<std::collections::VecDeque<SlotKey>>,
    /// Shared HTTP client (clone is cheap — internally `Arc`).
    http: reqwest::Client,
}

#[cfg(feature = "default-source")]
struct Slot {
    /// `tokio::sync::Mutex` because the consumer awaits state transitions.
    state: tokio::sync::Mutex<SlotState>,
    /// Efficient zero-poll wakeup for consumers waiting on InFlight → Ready.
    notify: tokio::sync::Notify,
}

#[cfg(feature = "default-source")]
enum SlotState {
    InFlight,
    Ready(std::sync::Arc<Vec<u8>>),
    Failed(String),
}

#[cfg(feature = "default-source")]
impl ReqwestSource {
    /// Tries to start prefetching after `read_text` returns a text resource.
    /// No-op unless: concurrency > 1, the location is a URL, the content
    /// parses as a media playlist with at least one segment.
    fn try_start_prefetch(&self, resource: &TextResource) {
        if self.concurrency <= 1 {
            return;
        }
        let SourceLocation::Url(playlist_url) = &resource.location else {
            return; // Don't prefetch local files.
        };

        // Parse the playlist content to extract segment URIs. Only media
        // playlists trigger prefetch — master playlists are skipped because
        // the variant hasn't been selected yet.
        let Ok(crate::hls::HlsPlaylist::Media(media)) =
            crate::hls::parse_hls_playlist_content(None, &resource.content)
        else {
            return;
        };
        if media.segments.is_empty() {
            return;
        }

        // Resolve each segment URI against the playlist's final URL. This
        // produces the exact `Url` values the transmuxer will later request
        // via `read_bytes`, so the slot keys will match.
        let targets: std::collections::VecDeque<SlotKey> = media
            .segments
            .iter()
            .filter_map(|seg| {
                let seg_url = playlist_url.join(&seg.uri).ok()?;
                Some((seg_url, seg.byte_range))
            })
            .collect();
        if targets.is_empty() {
            return;
        }

        // Lazy-init the prefetch state. `get_or_init` only runs once; if
        // `read_text` was called before (e.g. master playlist), the earlier
        // call would have returned early and left `state` uninitialized.
        self.state.get_or_init(|| {
            let (tokens_tx, tokens_rx) = tokio::sync::mpsc::channel(self.concurrency);
            let state = std::sync::Arc::new(PrefetchState {
                tokens_tx: tokens_tx.clone(),
                slots: std::sync::Mutex::new(HashMap::new()),
                targets: std::sync::Mutex::new(targets),
                http: self.http.clone(),
            });

            // Prefill the semaphore with `concurrency` tokens. The
            // coordinator will `recv()` one token before each fetch, and
            // consumers will `send(())` one back after consuming a slot.
            // This bounds outstanding slots (InFlight + Ready-unconsumed)
            // to `concurrency`.
            for _ in 0..self.concurrency {
                let _ = tokens_tx.try_send(());
            }

            // Spawn the coordinator. It runs until the target queue is
            // empty or all tokens are dropped (on `ReqwestSource` drop,
            // the `Sender` clones are dropped, `recv()` returns `None`).
            let state_clone = std::sync::Arc::clone(&state);
            tokio::spawn(prefetch_coordinator(state_clone, tokens_rx));

            state
        });
    }
}

/// Coordinator: pops targets in order, waits for a token (backpressure),
/// spawns a fetch task per target. Runs concurrently with consumers.
#[cfg(feature = "default-source")]
async fn prefetch_coordinator(
    state: std::sync::Arc<PrefetchState>,
    mut tokens_rx: tokio::sync::mpsc::Receiver<()>,
) {
    loop {
        // Block until a token is available. Returns None when all Sender
        // clones are dropped (i.e. the ReqwestSource was dropped) — then
        // stop the coordinator.
        if tokens_rx.recv().await.is_none() {
            return;
        }

        // Pop the next target. If the queue is empty, the prefetch is
        // complete — return the token and stop.
        let target = state.targets.lock().unwrap().pop_front();
        let Some((url, range)) = target else {
            return;
        };

        // Register an InFlight slot before spawning the fetch, so the
        // consumer can find it immediately if `read_bytes` races ahead.
        let slot = std::sync::Arc::new(Slot {
            state: tokio::sync::Mutex::new(SlotState::InFlight),
            notify: tokio::sync::Notify::new(),
        });
        state
            .slots
            .lock()
            .unwrap()
            .insert((url.clone(), range), std::sync::Arc::clone(&slot));

        // Spawn the fetch task. It runs concurrently with other fetches
        // and with the coordinator (which loops back to recv() the next
        // token, blocking if `concurrency` slots are already outstanding).
        let http = state.http.clone();
        tokio::spawn(async move {
            let result = fetch_bytes_with_range(&http, &url, range.as_ref()).await;
            let mut s = slot.state.lock().await;
            match result {
                Ok(bytes) => *s = SlotState::Ready(std::sync::Arc::new(bytes)),
                Err(e) => *s = SlotState::Failed(e.to_string()),
            }
            drop(s);
            slot.notify.notify_waiters();
        });
    }
}

/// Consumer side: wait for the slot to become Ready (or Failed), extract
/// the bytes, return the token to the semaphore, and evict the slot.
#[cfg(feature = "default-source")]
async fn read_from_slot(
    state: std::sync::Arc<PrefetchState>,
    key: &SlotKey,
    slot: &std::sync::Arc<Slot>,
) -> Result<Vec<u8>> {
    loop {
        let s = slot.state.lock().await;
        match &*s {
            SlotState::InFlight => {
                // Drop the lock before awaiting, so the fetch task can
                // acquire it to store the result.
                drop(s);
                slot.notify.notified().await;
                continue;
            }
            SlotState::Ready(bytes) => {
                let bytes = (**bytes).clone();
                drop(s);
                // Return the token so the coordinator can spawn the next
                // fetch. Evict the slot to free memory.
                let _ = state.tokens_tx.try_send(());
                state.slots.lock().unwrap().remove(key);
                return Ok(bytes);
            }
            SlotState::Failed(msg) => {
                let msg = msg.clone();
                drop(s);
                // Return the token even on failure — the worker consumed
                // a token but didn't produce consumable bytes. Without
                // this, a Failed slot would permanently leak a token.
                let _ = state.tokens_tx.try_send(());
                state.slots.lock().unwrap().remove(key);
                return Err(Error::Http(msg));
            }
        }
    }
}

/// Fetches bytes from `url` with an optional Range request. Extracted from
/// the original `read_bytes` URL branch so it can be shared by the sequential
/// path (`read_bytes` fallthrough) and the concurrent fetch tasks.
#[cfg(feature = "default-source")]
async fn fetch_bytes_with_range(
    http: &reqwest::Client,
    url: &Url,
    range: Option<&ByteRange>,
) -> Result<Vec<u8>> {
    use reqwest::header::{CONTENT_RANGE, RANGE};

    let mut request = http.get(url.clone());
    if let Some(range) = range {
        let end = range
            .offset
            .checked_add(range.length)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| Error::invalid("HTTP byterange overflows u64"))?;
        request = request.header(RANGE, format!("bytes={}-{}", range.offset, end));
    }

    let response = request
        .send()
        .await
        .map_err(|e| Error::Http(e.to_string()))?;
    if let Some(range) = range {
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(Error::Http(format!(
                "Range request for {url} returned status {} instead of 206",
                response.status()
            )));
        }
        if response.headers().get(CONTENT_RANGE).is_none() {
            return Err(Error::Http(format!(
                "Range request for {url} did not include Content-Range"
            )));
        }
        if range.length == 0 {
            return Ok(Vec::new());
        }
    } else if !response.status().is_success() {
        return Err(Error::Http(format!(
            "GET {url} returned status {}",
            response.status()
        )));
    }

    Ok(response
        .bytes()
        .await
        .map_err(|e| Error::Http(e.to_string()))?
        .to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_file_relative_paths() {
        let base = SourceLocation::File(PathBuf::from("/tmp/hls/master.m3u8"));
        assert_eq!(
            base.resolve("media/playlist.m3u8").unwrap(),
            SourceLocation::File(PathBuf::from("/tmp/hls/media/playlist.m3u8"))
        );
    }

    #[test]
    fn resolves_url_relative_paths() {
        let base = SourceLocation::Url(Url::parse("https://example.test/hls/master.m3u8").unwrap());
        assert_eq!(
            base.resolve("../media/playlist.m3u8").unwrap(),
            SourceLocation::Url(Url::parse("https://example.test/media/playlist.m3u8").unwrap())
        );
    }

    #[test]
    #[cfg(feature = "default-source")]
    fn applies_local_byterange() {
        let bytes = apply_range(
            b"0123456789".to_vec(),
            Some(&ByteRange {
                offset: 3,
                length: 4,
            }),
        )
        .unwrap();
        assert_eq!(bytes, b"3456");
    }
}
