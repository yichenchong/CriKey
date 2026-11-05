//! Bounded engine-to-launcher output queue.
//!
//! This stays engine-independent so coalescing and shutdown behavior are
//! deterministic unit tests rather than properties that require WPE to run.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

use crikey_native_protocol::message::{Envelope, Payload, WebLoadCode};

const OUTPUT_CAPACITY: usize = 16;

#[derive(Debug)]
pub(crate) struct OutputBuffer {
    state: Mutex<OutputState>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct OutputState {
    queue: VecDeque<Envelope>,
    closed: bool,
    terminal: bool,
}

impl OutputBuffer {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(OutputState::default()),
            ready: Condvar::new(),
        }
    }

    /// Keeps only the newest pending frame and each surface's latest state.
    ///
    /// Caret and load updates replace older snapshots of the same kind, so a
    /// burst of redirects cannot evict the only pending caret or frame. A control
    /// message evicts a stale frame first and otherwise the oldest control
    /// update. A frame never displaces control traffic. Gone and failed loads
    /// are exactly-once terminal fences: they take priority over all pending
    /// output, can never be evicted, and reject every later callback. The
    /// launcher closes the page on either terminal notification.
    pub(crate) fn push(&self, envelope: Envelope) -> Option<Vec<u8>> {
        let is_frame = matches!(envelope.payload.as_ref(), Some(Payload::WebFrame(_)));
        let is_terminal = matches!(envelope.payload.as_ref(), Some(Payload::Gone(_)))
            || matches!(
                envelope.payload.as_ref(),
                Some(Payload::LoadState(load)) if load.state == WebLoadCode::Failed
            );
        let mut state = lock_unpoisoned(&self.state);
        if state.closed || state.terminal {
            return frame_pixels(envelope);
        }

        if is_terminal {
            state.terminal = true;
            let recycled = state.queue.iter_mut().find_map(take_frame_pixels);
            state.queue.clear();
            state.queue.push_back(envelope);
            drop(state);
            self.ready.notify_all();
            return recycled;
        }

        let mut recycled = None;
        if is_frame {
            if let Some(index) = frame_index(&state.queue) {
                recycled = state.queue.remove(index).and_then(frame_pixels);
            }
            if state.queue.len() < OUTPUT_CAPACITY {
                state.queue.push_back(envelope);
            } else {
                recycled = frame_pixels(envelope);
            }
        } else {
            if let Some(index) = state.queue.iter().position(|queued| replaces(&envelope, queued)) {
                state.queue.remove(index);
            }
            if state.queue.len() == OUTPUT_CAPACITY {
                let index = frame_index(&state.queue).unwrap_or(0);
                recycled = state.queue.remove(index).and_then(frame_pixels);
            }
            state.queue.push_back(envelope);
        }
        debug_assert!(state.queue.len() <= OUTPUT_CAPACITY);
        drop(state);
        self.ready.notify_one();
        recycled
    }

    pub(crate) fn pop(&self) -> Option<Envelope> {
        let mut state = lock_unpoisoned(&self.state);
        loop {
            if let Some(envelope) = state.queue.pop_front() {
                return Some(envelope);
            }
            if state.closed || state.terminal {
                return None;
            }
            state = match self.ready.wait(state) {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
    }

    pub(crate) fn close(&self) {
        lock_unpoisoned(&self.state).closed = true;
        self.ready.notify_all();
    }
}

fn frame_index(queue: &VecDeque<Envelope>) -> Option<usize> {
    queue
        .iter()
        .position(|queued| matches!(queued.payload.as_ref(), Some(Payload::WebFrame(_))))
}

fn replaces(new: &Envelope, queued: &Envelope) -> bool {
    match (new.payload.as_ref(), queued.payload.as_ref()) {
        (Some(Payload::CaretArea(new)), Some(Payload::CaretArea(old))) => new.surface_id == old.surface_id,
        (Some(Payload::LoadState(new)), Some(Payload::LoadState(old))) => new.surface_id == old.surface_id,
        _ => false,
    }
}

fn frame_pixels(mut envelope: Envelope) -> Option<Vec<u8>> {
    take_frame_pixels(&mut envelope)
}

pub(crate) fn take_frame_pixels(envelope: &mut Envelope) -> Option<Vec<u8>> {
    match envelope.payload.as_mut() {
        Some(Payload::WebFrame(frame)) => Some(std::mem::take(&mut frame.rgba8)),
        _ => None,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use crikey_native_protocol::message::{CaretArea, Gone, LoadState, WebFrame, WebGoneCode, WebLoadCode};

    use super::*;

    fn envelope(payload: Payload) -> Envelope {
        Envelope {
            payload: Some(payload),
            ..Default::default()
        }
    }

    fn frame(generation: u64, fill: u8) -> Envelope {
        envelope(Payload::WebFrame(WebFrame {
            surface_id: 1,
            generation,
            pixel_width: 1,
            pixel_height: 1,
            rgba8: vec![fill; 4],
            ..Default::default()
        }))
    }

    fn caret(surface_id: u64) -> Envelope {
        envelope(Payload::CaretArea(CaretArea {
            surface_id,
            ..Default::default()
        }))
    }

    #[test]
    fn failed_load_survives_a_replacement_navigation_and_later_callbacks() {
        let output = OutputBuffer::new();
        output.push(caret(1));
        output.push(frame(1, 6));
        let failed = envelope(Payload::LoadState(LoadState {
            surface_id: 1,
            state: WebLoadCode::Failed,
            url: "https://example.test/broken".to_owned(),
            failure: "controlled network failure".to_owned(),
            ..Default::default()
        }));
        output.push(failed.clone());

        // A second navigation can start on the engine thread while the writer
        // has not yet drained the genuine failure from the preceding load.
        for state in [WebLoadCode::Started, WebLoadCode::Finished] {
            output.push(envelope(Payload::LoadState(LoadState {
                surface_id: 1,
                state,
                url: "https://example.test/replacement".to_owned(),
                ..Default::default()
            })));
        }
        output.push(frame(2, 9));
        output.push(caret(1));
        output.close();

        assert_eq!(output.pop(), Some(failed));
        assert_eq!(output.pop(), None, "shutdown drains the fatal diagnostic");
    }

    #[test]
    fn failed_load_fences_shutdown_without_losing_its_specific_diagnostic() {
        let output = OutputBuffer::new();
        let failed = envelope(Payload::LoadState(LoadState {
            surface_id: 1,
            state: WebLoadCode::Failed,
            failure: "controlled network failure".to_owned(),
            ..Default::default()
        }));
        output.push(failed.clone());
        assert_eq!(output.push(frame(2, 9)), Some(vec![9; 4]));
        output.push(envelope(Payload::Gone(Gone {
            surface_id: 1,
            reason: WebGoneCode::Closed,
            detail: "later shutdown".to_owned(),
            ..Default::default()
        })));

        assert_eq!(output.pop(), Some(failed));
        assert_eq!(output.pop(), None, "the fatal diagnostic ends the stream");
    }

    #[test]
    fn replacement_frame_is_coalesced_at_the_newest_position() {
        let output = OutputBuffer::new();
        assert_eq!(output.push(frame(1, 1)), None);
        assert_eq!(output.push(caret(7)), None);
        assert_eq!(output.push(frame(2, 2)), Some(vec![1; 4]));

        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::CaretArea(CaretArea { surface_id: 7, .. }))
        ));
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::WebFrame(WebFrame { generation: 2, .. }))
        ));
    }

    #[test]
    fn frame_never_displaces_a_full_control_queue() {
        let output = OutputBuffer::new();
        for id in 1..=OUTPUT_CAPACITY {
            assert_eq!(output.push(caret(id as u64)), None);
        }
        assert_eq!(output.push(frame(1, 9)), Some(vec![9; 4]));
        assert_eq!(lock_unpoisoned(&output.state).queue.len(), OUTPUT_CAPACITY);
    }

    #[test]
    fn gone_takes_priority_and_fences_all_later_output() {
        let output = OutputBuffer::new();
        output.push(caret(1));
        output.push(frame(1, 6));
        output.push(caret(2));

        let recycled = output.push(envelope(Payload::Gone(Gone {
            surface_id: 1,
            reason: WebGoneCode::Closed,
            ..Default::default()
        })));
        assert_eq!(recycled, Some(vec![6; 4]));
        assert_eq!(output.push(frame(2, 9)), Some(vec![9; 4]));
        assert_eq!(output.push(caret(3)), None);
        assert_eq!(
            output.push(envelope(Payload::Gone(Gone {
                surface_id: 1,
                reason: WebGoneCode::Crashed,
                ..Default::default()
            }))),
            None
        );

        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::Gone(Gone {
                reason: WebGoneCode::Closed,
                ..
            }))
        ));
        assert!(lock_unpoisoned(&output.state).queue.is_empty());
        assert_eq!(output.pop(), None, "a drained terminal fence ends the stream");
    }

    #[test]
    fn close_drains_existing_messages_then_ends_and_recycles_late_frames() {
        let output = OutputBuffer::new();
        output.push(caret(4));
        output.close();
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::CaretArea(_))
        ));
        assert_eq!(output.pop(), None);
        assert_eq!(output.push(frame(1, 3)), Some(vec![3; 4]));
    }

    #[test]
    fn load_flood_preserves_the_latest_caret_and_pending_frame() {
        let output = OutputBuffer::new();
        output.push(envelope(Payload::CaretArea(CaretArea {
            surface_id: 1,
            x: 11.0,
            ..Default::default()
        })));
        output.push(frame(1, 7));
        for generation in 0..OUTPUT_CAPACITY * 2 {
            output.push(envelope(Payload::LoadState(LoadState {
                surface_id: 1,
                state: WebLoadCode::Redirected,
                url: format!("https://example.test/redirect/{generation}"),
                ..Default::default()
            })));
        }
        assert_eq!(lock_unpoisoned(&output.state).queue.len(), 3);
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::CaretArea(caret)) if caret.x == 11.0
        ));
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::WebFrame(_))
        ));
        let Some(Payload::LoadState(load)) = output.pop().unwrap().payload else {
            panic!("latest load snapshot");
        };
        assert_eq!(
            load.url,
            format!("https://example.test/redirect/{}", OUTPUT_CAPACITY * 2 - 1)
        );
    }

    #[test]
    fn replacement_state_moves_after_intervening_updates_and_is_surface_specific() {
        let output = OutputBuffer::new();
        output.push(caret(1));
        output.push(caret(2));
        output.push(frame(1, 7));
        output.push(envelope(Payload::CaretArea(CaretArea {
            surface_id: 1,
            x: 23.0,
            ..Default::default()
        })));
        assert_eq!(lock_unpoisoned(&output.state).queue.len(), 3);
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::CaretArea(CaretArea { surface_id: 2, .. }))
        ));
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::WebFrame(_))
        ));
        assert!(matches!(
            output.pop().unwrap().payload,
            Some(Payload::CaretArea(caret)) if caret.surface_id == 1 && caret.x == 23.0
        ));
    }
}
