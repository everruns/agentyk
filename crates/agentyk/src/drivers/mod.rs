//! Bundled [`crate::driver::ChatDriver`] implementations.

pub mod sim;

/// Shared machinery for HTTP providers: sending, status and transport
/// classification, and SSE framing. A provider driver is wire mapping only.
#[cfg(feature = "http")]
pub(crate) mod http;

#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub mod anthropic;
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub mod openai;
