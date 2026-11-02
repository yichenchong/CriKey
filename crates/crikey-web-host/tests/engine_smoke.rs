#![cfg(all(feature = "engine", target_os = "linux"))]

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_native_protocol::message::{
    CloseSurface, Envelope, Gone, Navigate, OpenSurface, Payload, RawInput, Resize, WebFrame, WebGoneCode,
    WebInputCode, WebLoadCode, WebStorageMode,
};
use crikey_native_protocol::{frame, Endpoint, Message, ProtocolError};

const SURFACE_ID: u64 = 7;
static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        self.stop();
    }
}

struct Host {
    child: ChildGuard,
    stream: UnixStream,
    reader: frame::FrameReader,
    socket: PathBuf,
    private_dir: PathBuf,
}

impl Host {
    fn start() -> Self {
        let socket = PathBuf::from(format!(
            "/tmp/crikey-web-smoke-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let private_dir = socket.with_extension("files");
        std::fs::create_dir(&private_dir).unwrap();
        std::fs::create_dir(private_dir.join("Downloads")).unwrap();
        std::fs::write(
            private_dir.join("user-dirs.dirs"),
            format!("XDG_DOWNLOAD_DIR=\"{}/Downloads\"\n", private_dir.display()),
        )
        .unwrap();
        let listener = UnixListener::bind(&socket).expect("bind private host endpoint");
        listener.set_nonblocking(true).unwrap();
        let mut child = ChildGuard {
            child: Some(
                Command::new(env!("CARGO_BIN_EXE_crikey-web-host"))
                    .arg("--endpoint")
                    .arg(Endpoint::UnixSocket(socket.clone()).to_spec())
                    .stdin(Stdio::null())
                    .env("XDG_CONFIG_HOME", &private_dir)
                    .env("HOME", &private_dir)
                    .spawn()
                    .expect("start real WPE host"),
            ),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "host connection deadline expired");
                    assert!(
                        child.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                        "host exited before connecting"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("host connection failed: {error}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        Self {
            child,
            stream,
            reader: frame::FrameReader::new(),
            socket,
            private_dir,
        }
    }

    fn send(&mut self, payload: Payload) {
        let envelope = Envelope {
            payload: Some(payload),
            ..Envelope::default()
        };
        frame::write_frame(&mut self.stream, &envelope.encode()).expect("send host command");
    }

    fn open(&mut self, url: String) {
        self.send(Payload::OpenSurface(OpenSurface {
            surface_id: SURFACE_ID,
            pixel_width: 320,
            pixel_height: 200,
            url,
            storage_mode: WebStorageMode::Ephemeral,
            ..OpenSurface::default()
        }));
    }

    fn receive(&mut self, deadline: Instant) -> Option<Payload> {
        while Instant::now() < deadline {
            let mut bytes = Vec::new();
            match self.reader.read_frame(&mut self.stream, &mut bytes) {
                Ok(()) => return Some(Envelope::decode(&bytes).unwrap().payload.expect("host payload")),
                Err(ProtocolError::Timeout) => {}
                Err(error) => panic!("host output failed: {error}"),
            }
        }
        None
    }

    fn painted(&mut self, size: (u32, u32), rgba: [u8; 4]) -> WebFrame {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut last_frame = None;
        let mut last_load = None;
        loop {
            match self.receive(deadline) {
                Some(Payload::WebFrame(frame)) => {
                    assert_eq!(frame.surface_id, SURFACE_ID);
                    frame.validate().expect("real WPE frame satisfies wire bounds");
                    let offset =
                        ((frame.pixel_height / 2 * frame.pixel_width + frame.pixel_width / 2) * 4) as usize;
                    last_frame = Some((
                        frame.pixel_width,
                        frame.pixel_height,
                        frame.generation,
                        <[u8; 4]>::try_from(&frame.rgba8[offset..offset + 4]).unwrap(),
                    ));
                    if (frame.pixel_width, frame.pixel_height) == size
                        && frame.rgba8[offset..offset + 4] == rgba
                    {
                        return frame;
                    }
                }
                Some(Payload::LoadState(state)) => {
                    assert_ne!(
                        state.state,
                        WebLoadCode::Failed,
                        "unexpected load failure: {}",
                        state.failure
                    );
                    last_load = Some(state);
                }
                Some(Payload::Gone(gone)) => {
                    panic!("surface ended before expected paint: {}", gone.detail);
                }
                None => panic!(
                    "host paint deadline expired: expected {size:?} center {rgba:02x?}; \
                     last frame (width, height, generation, center RGBA)={last_frame:?}; \
                     last load={last_load:?}"
                ),
                Some(_) => {}
            }
        }
    }

    fn terminal(&mut self) -> Gone {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Payload::Gone(gone) = self
                .receive(deadline)
                .expect("host terminal Gone deadline expired")
            {
                assert_eq!(gone.surface_id, SURFACE_ID);
                return gone;
            }
        }
    }

    fn close(&mut self) {
        self.send(Payload::CloseSurface(CloseSurface {
            surface_id: SURFACE_ID,
            reason: "engine smoke complete".to_owned(),
            ..CloseSurface::default()
        }));
        assert_eq!(self.terminal().reason, WebGoneCode::Closed);
        assert!(self.child.wait_bounded(Duration::from_secs(5)).success());
        assert!(
            matches!(
                self.reader.read_frame(&mut self.stream, &mut Vec::new()),
                Err(ProtocolError::Closed)
            ),
            "terminal Gone must be followed by transport EOF"
        );
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        self.child.stop();
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.private_dir);
    }
}

struct HttpFixture {
    url: String,
    stop: Arc<AtomicBool>,
    requests: mpsc::Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl HttpFixture {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let (request_tx, requests) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut held = Vec::new();
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("accept HTTP fixture request: {error}"),
                };
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut request = Vec::new();
                while request.len() < 4096 && !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let mut bytes = [0; 512];
                    match stream.read(&mut bytes) {
                        Ok(0) | Err(_) => break,
                        Ok(count) => request.extend_from_slice(&bytes[..count]),
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let _ = request_tx.send(path.clone());
                if path == "/slow" {
                    // Keep this navigation pending while another is requested.
                    held.push(stream);
                    continue;
                }
                // Dynamic fixtures paint `body` over an opaque root. Give it
                // the full viewport even when its content is empty, so the
                // center pixel observes script-driven state changes.
                let body = match path.as_str() {
                    "/second" => "<!doctype html><style>html,body{margin:0;background:#654321}</style><title>Second</title>",
                    "/download-test" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<a id=d href='/download' download>download</a><script>d.click();",
                        "setTimeout(()=>document.body.style.background='#123456',1000)</script>"
                    ),
                    "/download" => "attachment contents",
                    "/wheel" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<script>let n=0;addEventListener('wheel',e=>{n++;",
                        "let ok=e.deltaX===0&&(n===1?Math.abs(e.deltaY)>1:Math.abs(e.deltaY)<1);",
                        "document.body.style.background=ok?(n===1?'#123456':'#654321'):'#ff0000';",
                        "e.preventDefault()},{passive:false})</script>"
                    ),
                    "/policy" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<script>let escaped=false;addEventListener('message',()=>escaped=true);",
                        "setTimeout(()=>document.body.style.background=escaped?'#ff0000':'#123456',1000)</script>",
                        "<iframe hidden src=\"data:text/html,<script>parent.postMessage('escaped','*')</script>\"></iframe>"
                    ),
                    _ => "<!doctype html><style>html,body{margin:0;background:#123456}</style><title>CriKey smoke</title>",
                };
                let disposition = if path == "/download" {
                    "Content-Disposition: attachment; filename=unapproved.txt\r\n"
                } else {
                    ""
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n{disposition}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                // Cancellation is an expected action in the navigation test.
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            url,
            stop,
            requests,
            thread: Some(thread),
        }
    }

    fn requested(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .expect("HTTP fixture request deadline expired");
            if self
                .requests
                .recv_timeout(remaining)
                .expect("engine requests fixture")
                == expected
            {
                return;
            }
        }
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            if let Err(error) = thread.join() {
                if !thread::panicking() {
                    std::panic::resume_unwind(error);
                }
            }
        }
    }
}

#[test]
fn real_document_pixels_resize_and_superseded_navigation_work() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/", fixture.url));
    let first = host.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    host.send(Payload::Resize(Resize {
        surface_id: SURFACE_ID,
        pixel_width: 240,
        pixel_height: 160,
        ..Resize::default()
    }));
    let resized = host.painted((240, 160), [0x12, 0x34, 0x56, 0xff]);
    assert!(resized.generation > first.generation);
    host.send(Payload::Navigate(Navigate {
        surface_id: SURFACE_ID,
        url: format!("{}/slow", fixture.url),
        ..Navigate::default()
    }));
    fixture.requested("/slow");
    host.send(Payload::Navigate(Navigate {
        surface_id: SURFACE_ID,
        url: format!("{}/second", fixture.url),
        ..Navigate::default()
    }));
    host.painted((240, 160), [0x65, 0x43, 0x21, 0xff]);
    host.close();
}

#[test]
fn page_initiated_non_http_frame_navigation_is_blocked() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/policy", fixture.url));
    host.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    host.close();
}

#[test]
fn partial_protocol_frame_does_not_block_supervised_termination() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/", fixture.url));
    host.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    host.stream.write_all(&[0, 0]).unwrap();
    let pid = host.child.child.as_ref().unwrap().id();
    assert!(Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .unwrap()
        .success());
    assert_eq!(host.terminal().reason, WebGoneCode::Closed);
    assert!(host.child.wait_bounded(Duration::from_secs(5)).success());
}

#[test]
fn disconnected_launcher_unblocks_reader_and_writer_shutdown() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/", fixture.url));
    // One 320x200 frame exceeds a typical Unix socket buffer. Stop reading so
    // shutdown exercises a blocked output writer, not just an idle worker.
    fixture.requested("/");
    thread::sleep(Duration::from_millis(300));
    host.stream.shutdown(Shutdown::Both).unwrap();
    let _ = host.child.wait_bounded(Duration::from_secs(5));
}

#[test]
fn page_downloads_cannot_write_an_unapproved_destination() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/download-test", fixture.url));
    fixture.requested("/download");
    host.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    assert!(
        !host.private_dir.join("Downloads/unapproved.txt").exists(),
        "an ephemeral surface must not save an unauthorised attachment"
    );
    host.close();
}

#[test]
fn line_wheel_and_precise_pixel_scroll_reach_the_engine_distinctly() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/wheel", fixture.url));
    host.painted((320, 200), [0x11, 0x11, 0x11, 0xff]);
    for (precise, delta_y, expected) in [
        (false, 1.0, [0x12, 0x34, 0x56, 0xff]),
        (true, 0.5, [0x65, 0x43, 0x21, 0xff]),
    ] {
        host.send(Payload::RawInput(RawInput {
            surface_id: SURFACE_ID,
            kind: WebInputCode::Scroll,
            x: 20.0,
            y: 20.0,
            delta_y,
            precise,
            ..RawInput::default()
        }));
        host.painted((320, 200), expected);
    }
    host.close();
}
