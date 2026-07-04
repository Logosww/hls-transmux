use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use url::Url;

use crate::error::{Error, Result};

/// A byte sub-range within a larger resource, used for `#EXT-X-BYTERANGE`
/// segment reads and Range requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// The crate ships a built-in reqwest-backed implementation ([`ReqwestSource`])
/// used by default for [`HlsInput::Path`] and [`HlsInput::Url`]. Callers that
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
#[cfg(feature = "default-source")]
#[derive(Debug, Clone)]
pub struct ReqwestSource {
    http: reqwest::Client,
}

#[cfg(feature = "default-source")]
impl Default for ReqwestSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "default-source")]
impl ReqwestSource {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    pub fn with_client(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[cfg(feature = "default-source")]
impl Source for ReqwestSource {
    fn read_text<'a>(
        &'a self,
        location: &'a SourceLocation,
    ) -> Pin<Box<dyn Future<Output = Result<TextResource>> + Send + 'a>> {
        Box::pin(async move {
            match location {
                SourceLocation::File(path) => {
                    let content = tokio::fs::read_to_string(path).await?;
                    Ok(TextResource {
                        content,
                        location: location.clone(),
                    })
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
                    Ok(TextResource {
                        content,
                        location: SourceLocation::Url(final_url),
                    })
                }
            }
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
                    apply_local_range(bytes, range)
                }
                SourceLocation::Url(url) => {
                    use reqwest::header::{CONTENT_RANGE, RANGE};

                    let mut request = self.http.get(url.clone());
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
            }
        })
    }
}

/// Input source for the async transmux entry point.
///
/// `Path` and `Url` use the built-in [`ReqwestSource`] (when the
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
fn apply_local_range(bytes: Vec<u8>, range: Option<&ByteRange>) -> Result<Vec<u8>> {
    let Some(range) = range else {
        return Ok(bytes);
    };
    let start = usize::try_from(range.offset)
        .map_err(|_| Error::invalid("local byterange offset exceeds usize"))?;
    let length = usize::try_from(range.length)
        .map_err(|_| Error::invalid("local byterange length exceeds usize"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| Error::invalid("local byterange overflows usize"))?;
    if end > bytes.len() {
        return Err(Error::invalid(
            "local byterange extends past the end of the segment",
        ));
    }
    Ok(bytes[start..end].to_vec())
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
        let bytes = apply_local_range(
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
