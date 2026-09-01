//! Test helpers shared by more than one capability.
//!
//! A capability that needs a fixture only it uses keeps it at home: `notifications` has its own
//! `test_support` for building a body span, and that is the right place for it. This file is for
//! the ones a second caller turned up for, which so far is exactly [`p2p_pair`], used by `tray`
//! and by `mpris`. It moved here rather than growing a second copy or being reached into across
//! capabilities.

use tokio::net::UnixStream;

/// A connected pair of p2p zbus connections, no bus daemon involved, with one addition: the server
/// side gets a short `method_timeout`. `tray::registry::register_item`'s real code path calls out
/// from this side to a peer that registers no object server handler for some calls, and with
/// zbus's default timeout each would hang until it lapses instead of erroring quickly, several
/// sequentially.
///
/// Binding a proxy makes no call at all, so a test that only needs a `Proxy` to exist can use this
/// and never provide a peer that answers.
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
