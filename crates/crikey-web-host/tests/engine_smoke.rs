#![cfg(all(feature = "engine", target_os = "linux"))]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crikey_native_protocol::message::{
    CloseSurface, Envelope, Gone, ImeEvent, Navigate, OpenSurface, Payload, RawInput, Resize, WebFrame,
    WebGoneCode, WebImeCode, WebInputCode, WebLoadCode, WebStorageMode, WEB_MODIFIER_CONTROL,
    WEB_MODIFIER_SHIFT,
};
use crikey_native_protocol::{frame, Message, ProtocolError};

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
        Self::start_with_storage(None)
    }

    fn start_with_storage(storage_dir: Option<&Path>) -> Self {
        let socket = PathBuf::from(format!(
            "/tmp/crikey-web-smoke-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        Self::start_at(socket, storage_dir)
    }

    fn start_at(socket: PathBuf, storage_dir: Option<&Path>) -> Self {
        // Keep browser profile/config paths UTF-8 even when exercising a
        // non-UTF-8 transport endpoint.
        let private_dir = PathBuf::from(format!(
            "/tmp/crikey-web-smoke-{}-{}.files",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&private_dir).unwrap();
        std::fs::create_dir(private_dir.join("Downloads")).unwrap();
        std::fs::write(
            private_dir.join("user-dirs.dirs"),
            format!("XDG_DOWNLOAD_DIR=\"{}/Downloads\"\n", private_dir.display()),
        )
        .unwrap();
        let listener = UnixListener::bind(&socket).expect("bind private host endpoint");
        listener.set_nonblocking(true).unwrap();
        let mut endpoint = OsString::from("unix:");
        endpoint.push(socket.as_os_str());
        let mut child = ChildGuard {
            child: Some(
                Command::new(env!("CARGO_BIN_EXE_crikey-web-host"))
                    .arg("--endpoint")
                    .arg(endpoint)
                    .arg("--storage-dir")
                    .arg(storage_dir.map_or_else(|| private_dir.join("profile"), Path::to_path_buf))
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
        self.open_with_storage(url, WebStorageMode::Ephemeral);
    }

    fn open_with_storage(&mut self, url: String, storage_mode: WebStorageMode) {
        self.send(Payload::OpenSurface(OpenSurface {
            surface_id: SURFACE_ID,
            pixel_width: 320,
            pixel_height: 200,
            url,
            storage_mode,
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
                if path == "/failure" {
                    // Closing without an HTTP response is a genuine network
                    // load failure, unlike an HTTP error status document.
                    continue;
                }
                // Dynamic fixtures paint `body` over an opaque root. Give it
                // the full viewport even when its content is empty, so the
                // center pixel observes script-driven state changes.
                let body = match path.as_str() {
                    "/second" => "<!doctype html><style>html,body{margin:0;background:#654321}</style><title>Second</title>",
                    // Let Content-Disposition start the download only after
                    // the server has received /download. A `download` attribute
                    // lets WebKit cancel before any HTTP request is dispatched.
                    "/download-test" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<a id=d href='/download'>download</a><script>d.click();",
                        "setTimeout(()=>document.body.style.background='#123456',1000)</script>"
                    ),
                    "/download" => "attachment contents",
                    "/unsupported" => "not a supported document",
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
                    "/storage/set" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<script>localStorage.setItem('persisted','yes');",
                        "setTimeout(()=>{let ok=document.cookie.includes('persisted=yes')",
                        "&&document.cookie.includes('session_only=yes');",
                        "document.body.style.background=ok?'#123456':'#ff0000'},1000)</script>"
                    ),
                    "/storage/read" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<script>addEventListener('load',()=>{let ok=document.cookie.includes('persisted=yes')",
                        "&&!document.cookie.includes('session_only=')&&localStorage.getItem('persisted')==='yes';",
                        "document.body.style.background=ok?'#123456':'#ff0000'})</script>"
                    ),
                    "/storage/empty" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}</style>",
                        "<script>addEventListener('load',()=>document.body.style.background=",
                        "document.cookie===''&&localStorage.length===0?'#123456':'#ff0000')</script>"
                    ),
                    "/input" => concat!(
                        "<!doctype html><style>html,body{margin:0;min-height:100vh;background:#111111}",
                        "input{position:absolute;left:0;top:0;width:100px;height:40px}</style><input id=i>",
                        "<script>let stage=0;const paint=(ok,next,color)=>{",
                        "stage=ok?next:-1;document.body.style.background=ok?color:'#ff0000'};",
                        "i.addEventListener('mousedown',e=>paint(stage===0&&e.button===0&&e.buttons===1",
                        "&&e.clientX===20&&e.clientY===20&&e.ctrlKey&&e.shiftKey,1,'#123456'));",
                        "i.addEventListener('mousemove',e=>{if(stage===1)paint(e.buttons===1",
                        "&&e.clientX===30&&e.clientY===25&&e.ctrlKey&&e.shiftKey,2,'#234567')});",
                        "i.addEventListener('mouseup',e=>paint(stage===2&&e.button===0&&e.buttons===0",
                        "&&e.ctrlKey&&e.shiftKey,3,'#345678'));",
                        "i.addEventListener('keydown',e=>{paint(stage===3&&e.key==='A'&&e.ctrlKey",
                        "&&e.shiftKey&&!e.altKey&&!e.metaKey&&!e.repeat,4,'#456789');e.preventDefault()});",
                        "i.addEventListener('keyup',e=>paint(stage===4&&e.key==='A'&&e.ctrlKey",
                        "&&e.shiftKey,5,'#56789a'));",
                        "addEventListener('blur',()=>{if(stage===5)paint(!document.hasFocus(),6,'#6789ab')});",
                        "addEventListener('focus',()=>{if(stage===6)paint(document.hasFocus(),7,'#789abc')})</script>"
                    ),
                    "/ime" => concat!(
                        "<!doctype html><meta charset=utf-8>",
                        "<style>html,body{margin:0;min-height:100vh;background:#222222}</style><input id=i>",
                        "<script>i.addEventListener('input',()=>requestAnimationFrame(()=>{",
                        "if(i.value==='😀中')document.body.style.background=i.selectionStart===3?'#123456':'#ff0000';",
                        "else if(i.value==='確定')document.body.style.background='#654321'}));",
                        "i.focus();requestAnimationFrame(()=>document.body.style.background='#111111')</script>"
                    ),
                    _ => "<!doctype html><style>html,body{margin:0;background:#123456}</style><title>CriKey smoke</title>",
                };
                let disposition = if path == "/download" {
                    "Content-Disposition: attachment; filename=unapproved.txt\r\n"
                } else {
                    ""
                };
                let cookies = if path == "/storage/set" {
                    "Set-Cookie: persisted=yes; Max-Age=86400; Path=/\r\n\
                     Set-Cookie: session_only=yes; Path=/\r\n"
                } else {
                    ""
                };
                let content_type = if path == "/unsupported" {
                    "application/x-crikey-unsupported"
                } else {
                    "text/html; charset=utf-8"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n{disposition}{cookies}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                // Cancellation is an expected action in the navigation test.
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
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
    // Stop reading briefly while the engine may produce frames, then disconnect.
    // This checks bounded shutdown; the request and delay do not establish that
    // the output writer reached a blocked socket send.
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

#[test]
fn pointer_drag_key_modifiers_and_focus_reach_the_document() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/input", fixture.url));
    host.painted((320, 200), [0x11, 0x11, 0x11, 0xff]);
    let modifiers = WEB_MODIFIER_CONTROL | WEB_MODIFIER_SHIFT;
    for (kind, x, y, expected) in [
        (WebInputCode::PointerMove, 20.0, 20.0, None),
        (
            WebInputCode::PointerDown,
            20.0,
            20.0,
            Some([0x12, 0x34, 0x56, 0xff]),
        ),
        (
            WebInputCode::PointerMove,
            30.0,
            25.0,
            Some([0x23, 0x45, 0x67, 0xff]),
        ),
        (
            WebInputCode::PointerUp,
            30.0,
            25.0,
            Some([0x34, 0x56, 0x78, 0xff]),
        ),
    ] {
        host.send(Payload::RawInput(RawInput {
            surface_id: SURFACE_ID,
            kind,
            timestamp_ms: 1000,
            x,
            y,
            button: 1,
            modifiers,
            ..RawInput::default()
        }));
        if let Some(expected) = expected {
            host.painted((320, 200), expected);
        }
    }
    // The pointer press must focus the input: no script focuses it for us.
    for (kind, expected) in [
        (WebInputCode::KeyDown, [0x45, 0x67, 0x89, 0xff]),
        (WebInputCode::KeyUp, [0x56, 0x78, 0x9a, 0xff]),
    ] {
        host.send(Payload::RawInput(RawInput {
            surface_id: SURFACE_ID,
            kind,
            timestamp_ms: 1100,
            keysym: u32::from(b'A'),
            hardware_keycode: 38,
            modifiers,
            ..RawInput::default()
        }));
        host.painted((320, 200), expected);
    }
    for (kind, expected) in [
        (WebInputCode::FocusOut, [0x67, 0x89, 0xab, 0xff]),
        (WebInputCode::FocusIn, [0x78, 0x9a, 0xbc, 0xff]),
    ] {
        host.send(Payload::RawInput(RawInput {
            surface_id: SURFACE_ID,
            kind,
            timestamp_ms: 1200,
            ..RawInput::default()
        }));
        host.painted((320, 200), expected);
    }
    host.close();
}

#[test]
fn persistent_cookies_and_local_storage_survive_reopen_but_not_profile_boundaries() {
    let fixture = HttpFixture::start();
    let mut original = Host::start();
    let profile = original.private_dir.join("profile");
    original.open_with_storage(format!("{}/storage/set", fixture.url), WebStorageMode::Persistent);
    original.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    original.close();

    let mut reopened = Host::start_with_storage(Some(&profile));
    reopened.open_with_storage(
        format!("{}/storage/read", fixture.url),
        WebStorageMode::Persistent,
    );
    reopened.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    reopened.close();

    // The same configured directory must be ignored for an ephemeral request.
    let mut ephemeral = Host::start_with_storage(Some(&profile));
    ephemeral.open(format!("{}/storage/empty", fixture.url));
    ephemeral.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    ephemeral.close();

    // A different plugin profile on the same origin must not inherit the data.
    let mut isolated = Host::start();
    isolated.open_with_storage(
        format!("{}/storage/empty", fixture.url),
        WebStorageMode::Persistent,
    );
    isolated.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    isolated.close();
}

#[test]
fn ephemeral_cookie_and_local_storage_state_is_discarded_on_close() {
    let fixture = HttpFixture::start();
    let mut original = Host::start();
    let profile = original.private_dir.join("profile");
    original.open(format!("{}/storage/set", fixture.url));
    original.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    original.close();
    assert!(
        !profile.exists(),
        "an ephemeral surface must not create its configured profile"
    );

    let mut reopened = Host::start_with_storage(Some(&profile));
    reopened.open(format!("{}/storage/empty", fixture.url));
    reopened.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    reopened.close();
}

#[test]
fn supplementary_preedit_uses_utf16_engine_offsets_and_commits_expected_text() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/ime", fixture.url));
    host.painted((320, 200), [0x11, 0x11, 0x11, 0xff]);
    host.send(Payload::ImeEvent(ImeEvent {
        surface_id: SURFACE_ID,
        kind: WebImeCode::Preedit,
        text: "😀中".to_owned(),
        has_cursor: true,
        cursor_begin_chars: 2,
        cursor_end_chars: 2,
        ..ImeEvent::default()
    }));
    host.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    host.send(Payload::ImeEvent(ImeEvent {
        surface_id: SURFACE_ID,
        kind: WebImeCode::Commit,
        text: "確定".to_owned(),
        ..ImeEvent::default()
    }));
    host.painted((320, 200), [0x65, 0x43, 0x21, 0xff]);
    host.close();
}

#[test]
fn malformed_open_urls_are_protocol_failures_before_engine_startup() {
    for url in [
        "https://user@/".to_owned(),
        "https://:443/".to_owned(),
        "https://[]/".to_owned(),
        format!("https://example.test/{}", "a".repeat(8192)),
    ] {
        let mut host = Host::start();
        host.open(url);
        assert_eq!(host.terminal().reason, WebGoneCode::ProtocolViolation);
        assert!(!host.child.wait_bounded(Duration::from_secs(5)).success());
        assert!(matches!(
            host.reader.read_frame(&mut host.stream, &mut Vec::new()),
            Err(ProtocolError::Closed)
        ));
    }
}

#[test]
fn non_utf8_unix_endpoint_carries_real_document_frames_and_normal_close() {
    let fixture = HttpFixture::start();
    let mut bytes = format!(
        "/tmp/crikey-web-endpoint-{}-{}-",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    )
    .into_bytes();
    bytes.extend_from_slice(b"\xff.sock");
    let socket = PathBuf::from(OsString::from_vec(bytes));
    let mut host = Host::start_at(socket, None);
    host.open(format!("{}/", fixture.url));
    host.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    host.close();
}

#[test]
fn non_utf8_profile_is_refused_for_persistence_but_unused_for_ephemeral_storage() {
    let fixture = HttpFixture::start();
    let mut bytes = format!(
        "/tmp/crikey-web-encoding-{}-{}-",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    )
    .into_bytes();
    bytes.push(0xff);
    let profile = PathBuf::from(OsString::from_vec(bytes));
    let mut ephemeral = Host::start_with_storage(Some(&profile));
    ephemeral.open(format!("{}/", fixture.url));
    ephemeral.painted((320, 200), [0x12, 0x34, 0x56, 0xff]);
    ephemeral.close();

    let mut persistent = Host::start_with_storage(Some(&profile));
    persistent.open_with_storage(format!("{}/", fixture.url), WebStorageMode::Persistent);
    let gone = persistent.terminal();
    assert_eq!(gone.reason, WebGoneCode::StartupFailed);
    assert!(gone.detail.contains("UTF-8"), "{}", gone.detail);
    assert!(!persistent.child.wait_bounded(Duration::from_secs(5)).success());
    assert!(
        !profile.exists(),
        "an unusable persistent profile must not be created"
    );
    assert!(matches!(
        persistent
            .reader
            .read_frame(&mut persistent.stream, &mut Vec::new()),
        Err(ProtocolError::Closed)
    ));
}

#[test]
fn unsupported_document_response_preserves_surface_for_next_navigation() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    host.open(format!("{}/unsupported", fixture.url));
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut finished = false;
    let mut last_load = None;
    let mut last_frame = None;
    let mut load_states = Vec::new();
    while !finished {
        match host.receive(deadline).unwrap_or_else(|| {
            panic!(
                "unsupported document deadline: finished={finished}, \
                 load sequence={load_states:?}, last load={last_load:?}, \
                 last frame={last_frame:?}, requests={:?}",
                fixture.requests.try_iter().collect::<Vec<_>>()
            )
        }) {
            Payload::LoadState(state) => {
                assert_ne!(state.state, WebLoadCode::Failed, "{}", state.failure);
                // An ignored MIME response need not commit its pending URI.
                // Wait for its native completion, then prove the view is usable
                // by navigating to and painting an actual supported document.
                finished |= state.state == WebLoadCode::Finished;
                load_states.push(state.state);
                last_load = Some(state);
            }
            Payload::WebFrame(frame) => {
                frame.validate().unwrap();
                last_frame = Some((frame.pixel_width, frame.pixel_height, frame.generation));
            }
            Payload::Gone(gone) => panic!("unsupported response closed its surface: {}", gone.detail),
            _ => {}
        }
    }
    fixture.requested("/unsupported");
    host.send(Payload::Navigate(Navigate {
        surface_id: SURFACE_ID,
        url: format!("{}/second", fixture.url),
        ..Navigate::default()
    }));
    host.painted((320, 200), [0x65, 0x43, 0x21, 0xff]);
    host.close();
}

#[test]
fn genuine_load_failure_reports_diagnostic_and_stops_without_launcher_disconnect() {
    let fixture = HttpFixture::start();
    let mut host = Host::start();
    let url = format!("{}/failure", fixture.url);
    host.open(url.clone());
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match host
            .receive(deadline)
            .expect("native load failure deadline expired")
        {
            Payload::LoadState(state) if state.state == WebLoadCode::Failed => {
                assert_eq!(state.surface_id, SURFACE_ID);
                assert_eq!(state.url, url);
                assert!(
                    !state.failure.is_empty(),
                    "native failure must retain its diagnostic"
                );
                break;
            }
            Payload::Gone(gone) => panic!("load failure lost its diagnostic: {gone:?}"),
            _ => {}
        }
    }
    fixture.requested("/failure");
    // Keep the launcher endpoint connected: termination belongs to the fatal
    // load boundary, not a later CloseSurface or transport-disconnect event.
    assert!(!host.child.wait_bounded(Duration::from_secs(5)).success());
    assert!(matches!(
        host.reader.read_frame(&mut host.stream, &mut Vec::new()),
        Err(ProtocolError::Closed)
    ));
}
