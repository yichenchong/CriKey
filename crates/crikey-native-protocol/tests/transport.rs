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
