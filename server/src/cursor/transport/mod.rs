//! Owns request-ID-scoped upstream ordering and downstream transport.

mod handle;
mod inbox;
mod lifecycle;
mod output;
mod registry;
#[cfg(test)]
mod test_support;

pub use handle::*;
pub use inbox::*;
pub(crate) use lifecycle::*;
pub use output::*;
pub use registry::*;
#[cfg(test)]
pub(crate) use test_support::test_registry;
