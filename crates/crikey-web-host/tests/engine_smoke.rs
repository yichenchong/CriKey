#![cfg(all(feature = "engine", target_os = "linux"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crikey_native_protocol::message::{
    CloseSurface, Envelope, OpenSurface, Payload, WebGoneCode, WebStorageMode,
};
use crikey_native_protocol::transport::Listener;
use crikey_native_protocol::{Endpoint, ProtocolError};

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn wait_bounded(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut().expect("child is still owned");
            if let Some(status) = child.try_wait().expect("query web host status") {
                self.child = None;
                return status;
            }
            if Instant::now() >= deadline {
                let mut child = self.child.take().expect("child is still owned");
                let _ = child.kill();
                let status = child.wait().expect("reap timed-out web host");
                panic!("web host did not exit within {timeout:?}; killed with {status}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn a_real_wpe_surface_paints_local_html_and_closes() {
    let http = TcpListener::bind(("127.0.0.1", 0)).expect("bind local HTTP fixture");
    let address = http.local_addr().expect("fixture address");
    let server = thread::spawn(move || {
        let (mut stream, _) = http.accept().expect("WPE requests the fixture");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).expect("read fixture request");
        let body = b"<!doctype html><meta charset=utf-8><style>html,body{margin:0;background:#123456}</style><title>CriKey smoke</title>painted";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write fixture headers");
        stream.write_all(body).expect("write fixture body");
    });

    let socket = PathBuf::from(format!(
        "/tmp/crikey-web-host-engine-smoke-{}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&socket);
    let endpoint = Endpoint::UnixSocket(socket.clone());
    let listener = Listener::bind(&endpoint).expect("bind private web-host endpoint");
    let mut child = ChildGuard::new(
        Command::new(env!("CARGO_BIN_EXE_crikey-web-host"))
            .arg("--endpoint")
            .arg(endpoint.to_spec())
            .stdin(Stdio::null())
            .spawn()
            .expect("start real WPE host"),
    );

    let mut transport = listener
        .accept(Some(Duration::from_secs(10)))
        .expect("host connects to launcher endpoint");
    transport
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set smoke-test deadline");
    let surface_id = 7;
    transport
        .send(&Envelope {
            payload: Some(Payload::OpenSurface(OpenSurface {
                surface_id,
                pixel_width: 320,
                pixel_height: 200,
                url: format!("http://{address}/"),
                storage_mode: WebStorageMode::Ephemeral,
                ..OpenSurface::default()
            })),
            ..Envelope::default()
        })
        .expect("open real surface");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut painted = false;
    while Instant::now() < deadline {
        let envelope = transport.recv().expect("host output before deadline");
        match envelope.payload {
            Some(Payload::WebFrame(frame)) => {
                assert_eq!(frame.surface_id, surface_id);
                frame.validate().expect("real WPE frame satisfies wire bounds");
                assert_eq!((frame.pixel_width, frame.pixel_height), (320, 200));
                painted = true;
                break;
            }
            Some(Payload::Gone(gone)) => panic!("WPE surface ended before painting: {}", gone.detail),
            _ => {}
        }
    }
    assert!(painted, "the real WPE surface must export at least one frame");

    transport
        .send(&Envelope {
            payload: Some(Payload::CloseSurface(CloseSurface {
                surface_id,
                reason: "engine smoke complete".to_owned(),
                ..CloseSurface::default()
            })),
            ..Envelope::default()
        })
        .expect("close real surface");
    transport
        .set_read_timeout(Some(Duration::from_millis(250)))
        .expect("set shutdown drain deadline");
    let terminal_deadline = Instant::now() + Duration::from_secs(5);
    let gone = loop {
        assert!(
            Instant::now() < terminal_deadline,
            "web host did not emit terminal Gone within the shutdown deadline"
        );
        match transport.recv() {
            Ok(Envelope {
                payload: Some(Payload::Gone(gone)),
                ..
            }) => break gone,
            Ok(_) | Err(ProtocolError::Timeout) => {}
            Err(error) => panic!("web host transport ended before terminal Gone: {error}"),
        }
    };
    assert_eq!(gone.reason, WebGoneCode::Closed);
    assert_eq!(gone.surface_id, surface_id);

    let status = child.wait_bounded(Duration::from_secs(5));
    assert!(
        status.success(),
        "web host exits cleanly after CloseSurface: {status}"
    );
    server.join().expect("HTTP fixture completes");
    let _ = std::fs::remove_file(socket);
}
