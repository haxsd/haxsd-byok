//! Builds the top-level server router.

use crate::{
    cursor::transport::TransportRegistry, local_app::CursorHarness, network::NetworkClients, Result,
};

pub fn router(
    registry: TransportRegistry,
    clients: NetworkClients,
    harness: CursorHarness,
) -> Result<axum::Router> {
    super::cursor::router(registry, clients, harness)
}
