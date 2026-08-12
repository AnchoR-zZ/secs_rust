//! Public, typed control-plane intentions accepted by an HSMS endpoint.
//!
//! Applications choose protocol operations but cannot construct control
//! headers, allocate System Bytes, or mutate selection state directly.

/// Typed control-plane intent accepted by the endpoint command API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlIntent {
    /// Initiate the HSMS Select handshake.
    Select,
    /// Initiate the HSMS Deselect handshake.
    Deselect,
    /// Probe the peer with a Linktest transaction.
    Linktest,
    /// Send `Separate.req` and terminate the current connection generation.
    Separate,
}
