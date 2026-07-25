//! Bundled [`crate::driver::ChatDriver`] implementations.

pub mod sim;

#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub mod anthropic;
#[cfg(feature = "http")]
#[cfg_attr(docsrs, doc(cfg(feature = "http")))]
pub mod openai;
