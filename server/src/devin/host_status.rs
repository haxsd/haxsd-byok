//! Read-only status wrapper for the explicit Devin host patch boundary.

use std::path::Path;

use crate::Result;

use super::host_patch::{inspect, PatchStatus};

pub fn status(path: &Path) -> Result<PatchStatus> {
    inspect(path)
}
