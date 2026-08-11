//! Red-first tests for transport-independent native protocol I/O (spec 16.2).

use std::time::Duration;

use crikey_native_protocol::{message, transport, Endpoint, Message, ProtocolError};

fn envelope(sequence: u64) -> message::Envelope {
    message::Envelope {
        connection_id: 7,
        request_id: sequence,
        generation: sequence + 10,
        deadline_ms: 500,
        payload: Some(message::Payload::HealthCheck(message::HealthCheck {
            nonce: sequence,
            unknown: crikey_native_protocol::wire::UnknownFields::default(),
        })),
        unknown: crikey_native_protocol::wire::UnknownFields::default(),
    }
}

#[test]
fn pair_delivers_envelopes_both_directions_in_order() {
    let (mut left, mut right) = transport::pair();
    let left_first = envelope(1);
    let left_second = envelope(2);
    let right_reply = envelope(3);

    left.send(&left_first).expect("left send");
    left.send(&left_second).expect("left send");
    assert_eq!(
        right.recv().expect("right first receive").encode(),
        left_first.encode()
    );
    assert_eq!(
        right.recv().expect("right second receive").encode(),
        left_second.encode()
    );

    right.send(&right_reply).expect("right send");
    assert_eq!(left.recv().expect("left receive").encode(), right_reply.encode());
}

#[test]
fn closing_one_pair_side_closes_the_other_receiver() {
    let (mut left, mut right) = transport::pair();
    left.close();
    assert!(matches!(right.recv(), Err(ProtocolError::Closed)));
}

#[test]
fn listener_rejects_stdio_endpoint() {
    assert!(matches!(
        transport::Listener::bind(&Endpoint::Stdio),
        Err(ProtocolError::Malformed(_))
    ));
}

/// ADR-0017 rejected a shared-memory transport for v1 after measuring
/// ADR-0004's profiling gate, so no endpoint spelling may name one. A partial
/// implementation that teaches `Endpoint` about shared memory before the
/// transport exists would let the host advertise a capability it does not
/// have; this test makes that a compile-and-fail rather than a silent claim.
#[test]
fn endpoint_vocabulary_names_no_shared_memory_transport() {
    for spec in ["shm:/dev/shm/crikey", "shm:crikey", "shared:crikey", "shm"] {
        assert!(
            matches!(Endpoint::parse(spec), Err(ProtocolError::Malformed(_))),
            "{spec:?} must not parse: shared memory is rejected for v1 by ADR-0017"
        );
    }
}
#[test]
fn stdio_survives_read_timeout_request() {
    let mut connection = transport::stdio();
    assert!(!connection.supports_read_timeout());
    connection
        .set_read_timeout(Some(Duration::from_millis(1)))
        .expect("unsupported timeout must remain a no-op for stdio");
    connection
        .set_read_timeout(None)
        .expect("clearing an unsupported timeout must remain harmless");
}

/// A unique endpoint of whatever kind this platform actually uses for native
/// plugins, so the test below drives the real listener rather than a portable
/// stand-in for it.
#[cfg(unix)]
fn native_endpoint() -> Endpoint {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "crikey-accept-retry-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    Endpoint::UnixSocket(path)
}

#[cfg(windows)]
fn native_endpoint() -> Endpoint {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    Endpoint::NamedPipe(format!(
        "crikey-accept-retry-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(any(unix, windows))]
fn remove_endpoint(endpoint: &Endpoint) {
    if let Endpoint::UnixSocket(path) = endpoint {
        let _ = std::fs::remove_file(path);
    }
}

/// A timeout is a retryable answer, not the end of the listener.
///
/// `accept` is the host's only way to meet a plugin that is slow to start, and
/// the worker calls it once per startup attempt. A listener that gave up its
/// endpoint on the first expired deadline would turn "this plugin was slow
/// once" into "this endpoint can never accept anyone", and the caller would
/// report that as a plugin that never connected. The Windows named-pipe
/// listener used to do exactly that, taking its only handle out of the slot
/// before polling and dropping it on the way out; the Unix listener never has.
/// One test for both, because it is one contract of [`transport::Listener`].
#[cfg(any(unix, windows))]
#[test]
fn a_timed_out_accept_leaves_the_listener_able_to_accept_a_later_client() {
    let endpoint = native_endpoint();
    let listener = transport::Listener::bind(&endpoint).expect("bind the platform's native endpoint");

    assert!(
        matches!(
            listener.accept(Some(Duration::from_millis(50))),
            Err(ProtocolError::Timeout)
        ),
        "an accept with no client waiting must report Timeout"
    );

    let client_endpoint = endpoint.clone();
    let client = std::thread::spawn(move || {
        let mut connection = transport::connect(&client_endpoint, Some(Duration::from_secs(5)))
            .expect("a client must still be able to reach the endpoint after the accept timed out");
        connection.send(&envelope(41)).expect("client send");
    });

    let mut accepted = listener
        .accept(Some(Duration::from_secs(5)))
        .expect("the listener must still accept after a timeout, not report Closed");
    assert_eq!(
        accepted.recv().expect("server receive").encode(),
        envelope(41).encode(),
        "the connection accepted on the retry must be the real client's"
    );
    client.join().expect("client thread must finish");

    drop(accepted);
    remove_endpoint(&endpoint);
}

/// A client that has already gone is still a client whose bytes must arrive.
///
/// This is the shape of every plugin handshake: the child connects, writes,
/// and can be finished before the host's next poll comes round. On Windows a
/// non-blocking `ConnectNamedPipe` answers `ERROR_NO_DATA` for that case
/// rather than `ERROR_PIPE_CONNECTED`, and reading it as a failure loses the
/// handshake of every plugin quick enough to hit it. The data is buffered in
/// the pipe instance until the server disconnects, so the contract is that the
/// accept succeeds and the connection reads what was written before it closed,
/// then ends.
#[cfg(any(unix, windows))]
#[test]
fn a_client_that_sends_and_leaves_before_the_accept_is_still_read() {
    let endpoint = native_endpoint();
    let listener = transport::Listener::bind(&endpoint).expect("bind the platform's native endpoint");

    let client_endpoint = endpoint.clone();
    let client = std::thread::spawn(move || {
        let mut connection = transport::connect(&client_endpoint, Some(Duration::from_secs(5)))
            .expect("the client reaches the endpoint");
        connection.send(&envelope(7)).expect("client send");
        // Gone before the server ever calls accept.
        drop(connection);
    });
    client.join().expect("client thread must finish");

    let mut accepted = listener
        .accept(Some(Duration::from_secs(5)))
        .expect("a client that already closed is still an accepted connection");
    assert_eq!(
        accepted
            .recv()
            .expect("the buffered frame survives the client")
            .encode(),
        envelope(7).encode(),
        "what the client wrote before leaving must still be delivered"
    );

    drop(accepted);
    remove_endpoint(&endpoint);
}

/// A client that connects and leaves without saying anything must not consume
/// the endpoint.
///
/// A plugin that dies during startup does exactly this, and the launcher's
/// answer has to be "that plugin failed, try the next client", not "this
/// endpoint is finished". On Windows the instance has to be disconnected and
/// re-armed for that, and the first `ConnectNamedPipe` after the re-arm
/// reports success for the arming rather than for a client — so a listener
/// that mistook it for one would hand the caller a pipe with nobody on the
/// other end, and the real client behind it would never be seen.
#[cfg(any(unix, windows))]
#[test]
fn a_client_that_says_nothing_and_leaves_does_not_consume_the_endpoint() {
    let endpoint = native_endpoint();
    let listener = transport::Listener::bind(&endpoint).expect("bind the platform's native endpoint");

    let silent_endpoint = endpoint.clone();
    std::thread::spawn(move || {
        let connection = transport::connect(&silent_endpoint, Some(Duration::from_secs(5)))
            .expect("the silent client reaches the endpoint");
        drop(connection);
    })
    .join()
    .expect("the silent client finishes");

    let real_endpoint = endpoint.clone();
    let real_client = std::thread::spawn(move || {
        let mut connection = transport::connect(&real_endpoint, Some(Duration::from_secs(5)))
            .expect("the real client reaches the endpoint after a silent one");
        connection.send(&envelope(9)).expect("client send");
        // Held open until the server has read, so this half of the test is
        // about the re-arm and not about salvaging a departed client.
        std::thread::sleep(Duration::from_millis(200));
    });

    // The two transports dispose of a silent client differently and both are
    // correct: a Unix listener hands it over as an accepted connection that
    // immediately reports end of stream, while a single-instance Windows pipe
    // re-arms and never mentions it. What must hold on both is that the real
    // client behind it is still reachable, so accepts are repeated until a
    // frame arrives rather than assuming which disposal happened.
    let mut delivered = None;
    for _ in 0..4 {
        let mut accepted = listener
            .accept(Some(Duration::from_secs(5)))
            .expect("the endpoint must still accept after a client that said nothing");
        if let Ok(frame) = accepted.recv() {
            delivered = Some(frame.encode());
            break;
        }
    }
    assert_eq!(
        delivered,
        Some(envelope(9).encode()),
        "the real client behind a silent one must still be served"
    );
    real_client.join().expect("client thread must finish");

    remove_endpoint(&endpoint);
}

#[cfg(unix)]
mod unix_tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn socket_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "crikey-native-protocol-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn unix_socket_binds_connects_and_exchanges_both_ways() {
        let path = socket_path();
        let endpoint = Endpoint::UnixSocket(path.clone());
        let listener = transport::Listener::bind(&endpoint).expect("bind unix socket");
        assert_eq!(listener.endpoint(), &endpoint);

        let response = envelope(22);
        let response_wire = response.encode();
        let client_endpoint = endpoint.clone();
        let client = thread::spawn(move || {
            let mut connection = transport::connect(&client_endpoint, Some(Duration::from_secs(2)))
                .expect("connect unix socket");
            let request = envelope(11);
            connection.send(&request).expect("client send");
            let received = connection.recv().expect("client receive");
            assert_eq!(received.encode(), response_wire);
        });

        let mut accepted = listener
            .accept(Some(Duration::from_secs(2)))
            .expect("accept unix socket");
        let received = accepted.recv().expect("server receive");
        assert_eq!(received.encode(), envelope(11).encode());
        accepted.send(&response).expect("server send");
        client.join().expect("client thread must finish");

        drop(accepted);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unix_socket_read_timeout_returns_typed_timeout_without_data() {
        let path = socket_path();
        let endpoint = Endpoint::UnixSocket(path.clone());
        let listener = transport::Listener::bind(&endpoint).expect("bind unix socket");
        let (connected_tx, connected_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let client_endpoint = endpoint.clone();
        let client = thread::spawn(move || {
            let _connection = transport::connect(&client_endpoint, Some(Duration::from_secs(2)))
                .expect("connect unix socket");
            connected_tx.send(()).expect("announce connection");
            release_rx.recv().expect("release client");
        });

        connected_rx.recv().expect("client connected");
        let mut accepted = listener
            .accept(Some(Duration::from_secs(2)))
            .expect("accept unix socket");
        accepted
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set socket timeout");
        assert!(matches!(accepted.recv(), Err(ProtocolError::Timeout)));

        release_tx.send(()).expect("release client");
        client.join().expect("client thread must finish");
        drop(accepted);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(not(unix))]
#[test]
fn unix_socket_is_typed_unavailable_on_non_unix_targets() {
    let endpoint = Endpoint::UnixSocket(std::path::PathBuf::from("native.sock"));
    assert!(matches!(
        transport::Listener::bind(&endpoint),
        Err(ProtocolError::Malformed(message))
            if message == "unix sockets are not available on this platform"
    ));
    assert!(matches!(
        transport::connect(&endpoint, Some(Duration::from_millis(1))),
        Err(ProtocolError::Malformed(message))
            if message == "unix sockets are not available on this platform"
    ));
}
