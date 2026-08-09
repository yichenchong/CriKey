//! Scratch experiment (not a test): records which `WindowEvent`s winit's X11
//! backend actually emits for a real compose sequence typed through XTEST,
//! against whatever input method the ambient `XMODIFIERS` / fallback selects.
//!
//! Run under a private Xvfb with `/tmp/xtype` typing at the same display.

use std::time::{Duration, Instant};

use winit::{
    application::ApplicationHandler,
    event::{Ime, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

struct Probe {
    window: Option<Window>,
    deadline: Instant,
    ime_events: usize,
}

impl ApplicationHandler for Probe {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let window = event_loop
            .create_window(Window::default_attributes().with_visible(true))
            .expect("window");
        window.set_ime_allowed(true);
        window.focus_window();
        println!("PROBE: window created, ime allowed, focus requested");
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::Ime(ime) => {
                self.ime_events += 1;
                let described = match ime {
                    Ime::Enabled => "Enabled".to_owned(),
                    Ime::Preedit(text, range) => format!("Preedit(text={text:?}, range={range:?})"),
                    Ime::Commit(text) => format!("Commit({text:?})"),
                    Ime::Disabled => "Disabled".to_owned(),
                };
                println!("PROBE: Ime::{described}");
            }
            WindowEvent::KeyboardInput {
                event, is_synthetic, ..
            } => {
                println!(
                    "PROBE: KeyboardInput state={:?} logical={:?} text={:?} synthetic={is_synthetic}",
                    event.state, event.logical_key, event.text
                );
            }
            WindowEvent::Focused(focused) => println!("PROBE: Focused({focused})"),
            _ => {}
        }
        if Instant::now() >= self.deadline {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if Instant::now() >= self.deadline {
            println!("PROBE: finished, {} Ime event(s) observed", self.ime_events);
            event_loop.exit();
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
        }
    }
}

fn main() {
    println!(
        "PROBE: XMODIFIERS={:?} LC_CTYPE={:?}",
        std::env::var("XMODIFIERS"),
        std::env::var("LC_CTYPE")
    );
    let event_loop = EventLoop::new().expect("event loop");
    let mut probe = Probe {
        window: None,
        deadline: Instant::now() + Duration::from_secs(6),
        ime_events: 0,
    };
    event_loop.run_app(&mut probe).expect("run");
    println!("PROBE: exited with {} Ime event(s)", probe.ime_events);
}
