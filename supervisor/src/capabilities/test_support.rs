//! Test helpers shared across capabilities.
//!
//! Capability-specific fixtures stay local; `notifications` builds its body span in its own
//! `test_support`. This file currently holds [`p2p_pair`], shared by `tray` and `mpris`, rather
//! than duplicating or reaching across modules.

use tokio::net::UnixStream;

/// Connected p2p zbus connections without a bus daemon. The server has a short `method_timeout`:
/// `tray::registry::register_item` calls a peer with no handlers, and zbus's default timeout would
/// let several sequential calls hang.
///
/// Binding a proxy makes no call, so tests needing only a `Proxy` need no answering peer. A test
/// that drives `register_item` is not one of those: its `Status` probe is a real call, and the
/// peer answers it only on a multi-thread runtime.
pub(super) async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
    let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
    let guid = zbus::Guid::generate();
    let server_builder = zbus::connection::Builder::unix_stream(a)
        .server(guid)
        .expect("p2p server builder setup")
        .p2p()
        .method_timeout(std::time::Duration::from_millis(200));
    let client_builder = zbus::connection::Builder::unix_stream(b).p2p();
    tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
}
