//! Test helpers shared across capabilities.
//!
//! Capability-specific fixtures stay local; `notifications` builds its body span in its own
//! `test_support`. This file currently holds [`p2p_pair`], rather than duplicating or reaching
//! across modules.
//!
//! `pub(crate)` rather than `pub(super)`, because `polkit` is a D-Bus test outside `capabilities`
//! and needs the same pair. A second copy there is what this file exists to prevent.

use tokio::net::UnixStream;

/// Connected p2p zbus connections without a bus daemon, returned as `(server, client)`. `serve`
/// installs the client's interfaces; pass `Ok` when the client answers nothing, which is most tests
/// here, since binding a proxy makes no call unless it asks for `CacheProperties::Yes` -- that one
/// fetches the cache inside `ProxyBuilder::build` and does need an answering peer.
///
/// Installing them on the builder rather than through `client.object_server().at(..)` is the whole
/// point. `Connection::object_server()` starts the dispatch task without waiting for it to
/// subscribe, and the socket reader is running by then, so a call arriving in that window is
/// broadcast to no receiver and dropped -- not delayed, dropped, which no timeout can rescue.
/// `Builder::build` is the ordering that holds: it adds the interfaces, starts dispatch, awaits its
/// started event, and only then calls `init_socket_reader`, with zbus's own comment there saying
/// the reader would otherwise lose early messages. That race cost `tray`'s two answering tests
/// their first call about one loaded `just check` in five, and a throwaway warm-up call papered
/// over it in one of them for a while.
///
/// Both sides get a `method_timeout` only as a backstop, because zbus's default is `None` and a
/// call nobody answers then waits for ever, wedging the suite instead of failing it. Nothing here
/// should come near five seconds.
pub(crate) async fn p2p_pair() -> (zbus::Connection, zbus::Connection) {
    p2p_pair_serving(Ok).await
}

/// [`p2p_pair`] with stub interfaces installed on the client, which is where its doc explains why
/// they have to go in here rather than through `object_server().at(..)` afterwards.
pub(crate) async fn p2p_pair_serving<F>(serve: F) -> (zbus::Connection, zbus::Connection)
where
    F: FnOnce(zbus::connection::Builder<'static>) -> zbus::Result<zbus::connection::Builder<'static>>,
{
    let (a, b) = UnixStream::pair().expect("failed to create a unix socket pair");
    let guid = zbus::Guid::generate();
    let backstop = std::time::Duration::from_secs(5);
    let server_builder = zbus::connection::Builder::unix_stream(a)
        .server(guid)
        .expect("p2p server builder setup")
        .p2p()
        .method_timeout(backstop);
    let client_builder = serve(zbus::connection::Builder::unix_stream(b).p2p().method_timeout(backstop))
        .expect("failed to install the client's interfaces");
    tokio::try_join!(server_builder.build(), client_builder.build()).expect("p2p handshake")
}
