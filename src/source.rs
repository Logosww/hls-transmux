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
/// and spawns `concurrency` background workers that prefetch up to
/// `concurrency * 3` segments ahead of the transmuxer's sequential
/// consumption. Memory is bounded: at most `concurrency * 3` segment bodies
/// are held in flight or ready at any time (matching the backpressure of
/// `semaphore(N) in-flight + channel(2N) buffered = 3N`). When the
/// transmuxer races ahead of prefetch, the consumer self-builds a slot and
/// spawns a one-shot fetch (no redundant direct-fetch competing for
/// bandwidth). This is transparent to the transmuxer — it still calls
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
    /// Cancel signal held by `ReqwestSource` (NOT by `PrefetchState`) so it
    /// drops when `ReqwestSource` drops, even if workers still hold
    /// `Arc<PrefetchState>` references. Without this split, workers would
    /// hold the only `Arc<PrefetchState>` refs forever (deadlock) since
    /// `cancel_tx` would never drop.
    cancel_tx: std::sync::OnceLock<tokio::sync::watch::Sender<bool>>,
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
            cancel_tx: std::sync::OnceLock::new(),
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
            cancel_tx: std::sync::OnceLock::new(),
        }
    }

    /// Creates a new `ReqwestSource` with a custom `reqwest::Client` and
    /// sequential (non-concurrent) downloads.
    pub fn with_client(http: reqwest::Client) -> Self {
        Self {
            http,
            concurrency: 1,
            state: std::sync::OnceLock::new(),
            cancel_tx: std::sync::OnceLock::new(),
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
            cancel_tx: std::sync::OnceLock::new(),
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
            cancel_tx: std::sync::OnceLock::new(),
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
                    // Fast path: prefetch slot exists for this (url, range).
                    // Wait for it to become Ready, take the bytes, evict.
                    if let Some(state_ref) = self.state.get() {
                        let state = std::sync::Arc::clone(state_ref);
                        let key = (url.clone(), range.copied());
                        // Try the cache first — MutexGuard is dropped before
                        // the await boundary (MutexGuard is !Send).
                        let cached_slot = {
                            state.slots.lock().unwrap().get(&key).cloned()
                        };
                        if let Some(slot) = cached_slot {
                            return read_from_slot(state, &key, &slot).await;
                        }
                        // Slow path: consumer raced ahead of prefetch (no
                        // slot exists yet). Use the Entry API to atomically
                        // either grab a worker-spawned InFlight slot (raced
                        // with a worker between our cache miss and the lock)
                        // or self-build one with `_buffer_permit = None` and
                        // spawn a one-shot fetch. This avoids redundant
                        // direct-fetch bandwidth competing with workers.
                        let slot = {
                            let mut slots = state.slots.lock().unwrap();
                            match slots.entry(key.clone()) {
                                std::collections::hash_map::Entry::Occupied(e) => {
                                    // A worker inserted between our cache miss
                                    // and this lock acquisition — use its slot.
                                    e.get().clone()
                                }
                                std::collections::hash_map::Entry::Vacant(v) => {
                                    // Self-build a slot. Permit is None so it
                                    // doesn't count against `buffer_sem`.
                                    let slot = std::sync::Arc::new(Slot {
                                        state: tokio::sync::Mutex::new(SlotState::InFlight),
                                        notify: tokio::sync::Notify::new(),
                                        _buffer_permit: None,
                                    });
                                    v.insert(std::sync::Arc::clone(&slot));
                                    // Spawn one-shot fetch using the cloned
                                    // http client — `state` is not captured.
                                    let http = state.http.clone();
                                    let fetch_url = url.clone();
                                    let fetch_range = range.copied();
                                    let fetch_slot = std::sync::Arc::clone(&slot);
                                    tokio::spawn(async move {
                                        let result = fetch_bytes_with_range(
                                            &http,
                                            &fetch_url,
                                            fetch_range.as_ref(),
                                        )
                                        .await;
                                        let mut s = fetch_slot.state.lock().await;
                                        match result {
                                            Ok(bytes) => {
                                                *s = SlotState::Ready(
                                                    std::sync::Arc::new(bytes),
                                                );
                                            }
                                            Err(e) => {
                                                *s = SlotState::Failed(e.to_string());
                                            }
                                        }
                                        drop(s);
                                        fetch_slot.notify.notify_waiters();
                                    });
                                    slot
                                }
                            }
                        };
                        return read_from_slot(state, &key, &slot).await;
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
// Design: "worker + buffer_sem(3N) + consumer self-built slot" — N workers
// pop targets from a shared queue and fetch concurrently; a
// `Semaphore(concurrency * 3)` bounds total outstanding slots (InFlight +
// Ready-unconsumed) to 3N, matching v1 `semaphore(N) + channel(2N) = 3N`
// backpressure. Each worker stores its `OwnedSemaphorePermit` inside the
// slot it creates; the permit is released when the consumer drops the slot
// after reading.
//
// When the consumer (transmuxer) races ahead of prefetch — i.e. calls
// `read_bytes` for a target no worker has reached yet — it self-builds a
// slot with `_buffer_permit = None` and spawns a one-shot fetch. Workers
// popping that target later see an existing slot and skip it (releasing
// their freshly-acquired buffer_permit). This mirrors v1's "consumer waits
// on `rx.recv()` when channel is empty" semantics and avoids redundant
// downloads competing for bandwidth.
//
// Why not BTreeMap by index? The Source trait API is URL-based, not
// index-based. The transmuxer already consumes segments in playlist order,
// so the Source only needs point lookups by URL — no ordered reassembly.
// ---------------------------------------------------------------------------

#[cfg(feature = "default-source")]
type SlotKey = (Url, Option<ByteRange>);

#[cfg(feature = "default-source")]
struct PrefetchState {
    /// URL+range → slot. `std::sync::Mutex` because critical sections are
    /// tiny (insert / lookup / evict) and never await.
    slots: std::sync::Mutex<HashMap<SlotKey, std::sync::Arc<Slot>>>,
    /// Ordered queue of (Url, Option<ByteRange>) targets to prefetch.
    /// Workers pop from the front; consumers never touch this except for
    /// the consumer-self-built-slot fast path, which uses `slots` not
    /// `targets`.
    targets: std::sync::Mutex<std::collections::VecDeque<SlotKey>>,
    /// Shared HTTP client (clone is cheap — internally `Arc`).
    http: reqwest::Client,
    /// Buffer semaphore: bounds total outstanding slots (InFlight +
    /// Ready-unconsumed) to `concurrency * 3`. Workers acquire_owned() a
    /// permit before fetching; the permit is stored in the slot and
    /// released when the consumer drops the slot. This matches v1's
    /// `semaphore(N) in-flight + channel(2N) buffered = 3N` backpressure.
    buffer_sem: std::sync::Arc<tokio::sync::Semaphore>,
}

#[cfg(feature = "default-source")]
struct Slot {
    /// `tokio::sync::Mutex` because the consumer awaits state transitions.
    state: tokio::sync::Mutex<SlotState>,
    /// Efficient zero-poll wakeup for consumers waiting on InFlight → Ready.
    notify: tokio::sync::Notify,
    /// Worker-created slots hold `Some(permit)` so the buffer semaphore
    /// releases when the consumer drops the slot. Consumer-self-built
    /// slots hold `None` — they don't count against `buffer_sem` (at most
    /// one such slot exists per pending target).
    _buffer_permit: Option<tokio::sync::OwnedSemaphorePermit>,
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
        //
        // `cancel_tx` is held by `self` (NOT by `PrefetchState`) so that
        // dropping `ReqwestSource` drops the Sender — workers see
        // `cancel_rx.changed()` return Err and exit. If `cancel_tx` lived
        // inside `PrefetchState`, workers would hold the only `Arc` refs
        // forever (deadlock: PrefetchState won't drop until workers exit,
        // but workers won't exit until cancel_tx drops).
        let cancel_tx = self.cancel_tx.get_or_init(|| tokio::sync::watch::channel(false).0);
        let cancel_rx = cancel_tx.subscribe();
        let state = self.state.get_or_init(|| {
            std::sync::Arc::new(PrefetchState {
                slots: std::sync::Mutex::new(HashMap::new()),
                targets: std::sync::Mutex::new(targets),
                http: self.http.clone(),
                buffer_sem: std::sync::Arc::new(tokio::sync::Semaphore::new(
                    self.concurrency * 3,
                )),
            })
        });

        // Spawn N workers. Each worker loops independently: pop target
        // → acquire buffer_permit → fetch → store slot → continue.
        // Workers stop when targets is empty OR cancel_tx is dropped
        // (i.e. ReqwestSource is dropped).
        for _ in 0..self.concurrency {
            tokio::spawn(prefetch_worker(
                std::sync::Arc::clone(state),
                cancel_rx.clone(),
            ));
        }
    }
}

/// Worker: pops targets in order, acquires a buffer permit (backpressure),
/// fetches the segment, stores the result in a slot. Runs concurrently
/// with other workers and with consumers. Exits when the target queue is
/// empty or the cancel signal fires.
#[cfg(feature = "default-source")]
async fn prefetch_worker(
    state: std::sync::Arc<PrefetchState>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        // Fast cancel check: if cancel_tx was dropped (returns Err) or
        // the flag flipped to true, stop the worker.
        if *cancel_rx.borrow() {
            return;
        }
        // Pop the next target. Empty queue → prefetch complete → exit.
        let target = state.targets.lock().unwrap().pop_front();
        let Some((url, range)) = target else {
            return;
        };

        // Acquire a buffer permit before fetching. This blocks when total
        // outstanding slots reach `concurrency * 3`, providing backpressure
        // until the consumer frees a slot. Race against cancel so we don't
        // hang forever if the source is dropped while waiting.
        let buffer_permit = tokio::select! {
            biased;
            _ = cancel_rx.changed() => return,
            permit = state.buffer_sem.clone().acquire_owned() => {
                match permit {
                    Ok(p) => p,
                    Err(_) => return, // Semaphore closed (shouldn't happen)
                }
            }
        };

        // Race: if the consumer already self-built a slot for this target
        // (it caught up to prefetch front), skip — drop the buffer_permit
        // to release the semaphore slot. Use Entry API for atomic insert.
        let slot = {
            let mut slots = state.slots.lock().unwrap();
            match slots.entry((url.clone(), range)) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    // Consumer got here first. Drop buffer_permit and
                    // continue to next target.
                    drop(buffer_permit);
                    continue;
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    let slot = std::sync::Arc::new(Slot {
                        state: tokio::sync::Mutex::new(SlotState::InFlight),
                        notify: tokio::sync::Notify::new(),
                        _buffer_permit: Some(buffer_permit),
                    });
                    v.insert(std::sync::Arc::clone(&slot));
                    slot
                }
            }
        };

        // Fetch the bytes. Don't hold any locks across the await.
        let http = state.http.clone();
        let result = fetch_bytes_with_range(&http, &url, range.as_ref()).await;

        // Store the result and wake any waiting consumers.
        let mut s = slot.state.lock().await;
        match result {
            Ok(bytes) => *s = SlotState::Ready(std::sync::Arc::new(bytes)),
            Err(e) => *s = SlotState::Failed(e.to_string()),
        }
        drop(s);
        slot.notify.notify_waiters();
        // Loop back to pop the next target — don't wait for the consumer!
    }
}

/// Consumer side: wait for the slot to become Ready (or Failed), extract
/// the bytes, and evict the slot. Eviction drops the `Arc<Slot>`; if this
/// is the last strong reference, the slot's `_buffer_permit` is dropped,
/// releasing the buffer semaphore and unblocking a waiting worker.
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
                // Evict the slot. The Arc<Slot> returned by `remove` will
                // drop at end of scope, releasing the buffer_permit (if
                // any) and freeing memory.
                state.slots.lock().unwrap().remove(key);
                return Ok(bytes);
            }
            SlotState::Failed(msg) => {
                let msg = msg.clone();
                drop(s);
                // Evict even on failure — the worker consumed a permit but
                // didn't produce consumable bytes. Releasing the permit
                // lets another worker fetch the next target.
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
