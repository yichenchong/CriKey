//! One writer serializes learned snapshots and selective resets off the event loop.
use crikey_app::{SearchService, SelectionHistorySnapshot, SelectionHistoryStore};
use crikey_core::{CommandRef, ItemId, PluginId};
use std::{
    sync::mpsc,
    thread::{self, JoinHandle},
};

pub(crate) enum Learning {
    Command(crikey_app::CommandLearning, u64),
    Ordinary(crikey_app::SelectionLearning, u64),
    ItemSelection(crikey_core::Item, u64),
    Host(CommandRef, u64),
}
impl Learning {
    pub fn publish(self, search: &mut SearchService) -> Result<(), &'static str> {
        match self {
            Self::Command(evidence, time) => {
                search.set_history_time(time);
                search.record_command_learning(evidence)
            }
            Self::Ordinary(evidence, time) => {
                search.set_history_time(time);
                search.record_selection_learning(evidence);
                Ok(())
            }
            Self::ItemSelection(item, time) => {
                search.set_history_time(time);
                search.record_item_learning(item);
                Ok(())
            }
            Self::Host(reference, time) => {
                search.set_history_time(time);
                search.record_host_command_success(&reference)
            }
        }
    }
}
enum Request {
    Save(SelectionHistorySnapshot),
    Reset {
        snapshot: SelectionHistorySnapshot,
        owner: PluginId,
        item: ItemId,
    },
}
pub(crate) struct Completion {
    pub reset: bool,
    pub result: Result<Option<SelectionHistorySnapshot>, String>,
}
pub(crate) struct HistoryWorker {
    sender: Option<mpsc::SyncSender<Request>>,
    receiver: mpsc::Receiver<Completion>,
    worker: Option<JoinHandle<()>>,
}
impl HistoryWorker {
    pub fn spawn(
        store: Option<SelectionHistoryStore>,
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self, String> {
        let (sender, requests) = mpsc::sync_channel(16);
        let (results, receiver) = mpsc::sync_channel(32);
        let worker = thread::Builder::new()
            .name("selection-history-writer".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    let (reset, result) = match request {
                        Request::Save(snapshot) => (
                            false,
                            store.as_ref().map_or(Ok(None), |store| {
                                store
                                    .save(&snapshot)
                                    .map(|()| None)
                                    .map_err(|_| "Selection history was not saved".to_owned())
                            }),
                        ),
                        Request::Reset {
                            snapshot,
                            owner,
                            item,
                        } => (
                            true,
                            store
                                .as_ref()
                                .ok_or_else(|| {
                                    "Persistent history is unavailable; ranking was not reset".to_owned()
                                })
                                .and_then(|store| {
                                    crikey_app::reset_ranking_snapshot(store, snapshot, &owner, &item)
                                        .map(Some)
                                        .map_err(|_| {
                                            "Ranking reset was not saved; learned state is unchanged"
                                                .to_owned()
                                        })
                                }),
                        ),
                    };
                    if results.send(Completion { reset, result }).is_err() {
                        break;
                    }
                    wake();
                }
            })
            .map_err(|_| "selection history writer could not start".to_owned())?;
        Ok(Self {
            sender: Some(sender),
            receiver,
            worker: Some(worker),
        })
    }
    pub fn save(&self, snapshot: SelectionHistorySnapshot) -> Result<(), String> {
        self.submit(Request::Save(snapshot))
    }
    pub fn reset(
        &self,
        snapshot: SelectionHistorySnapshot,
        owner: PluginId,
        item: ItemId,
    ) -> Result<(), String> {
        self.submit(Request::Reset {
            snapshot,
            owner,
            item,
        })
    }
    fn submit(&self, request: Request) -> Result<(), String> {
        self.sender
            .as_ref()
            .ok_or("Selection history writer is closed")?
            .try_send(request)
            .map_err(|_| "Selection history writer is busy; change was not queued".to_owned())
    }
    pub fn take(&self) -> Option<Completion> {
        self.receiver.try_recv().ok()
    }
}
impl Drop for HistoryWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            // Drain bounded acknowledgments while the disconnected request
            // queue finishes; joining first can deadlock a full result queue.
            while self.receiver.recv().is_ok() {}
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_frozen_item_credit_records_no_query_or_command_affinity() {
        let mut search = SearchService::new(crikey_app::App::new());
        let item = crikey_core::Item {
            stable_id: ItemId("actionless".into()),
            plugin_id: PluginId("typed.owner".into()),
            category: crikey_core::Category::Command,
            label: "Frozen admitted typed item".into(),
            description: String::new(),
            target: String::new(),
            search_terms: Vec::new(),
            icon_reference: None,
            argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            hit_policy: crikey_core::HitPolicy::Ignored,
            score_hint: 0,
            metadata: Default::default(),
            actions: Vec::new(),
        };
        Learning::ItemSelection(item.clone(), 7)
            .publish(&mut search)
            .unwrap();
        Learning::ItemSelection(item, 8).publish(&mut search).unwrap();
        let snapshot = search.selection_history_snapshot();
        assert_eq!(snapshot.selections.len(), 1);
        assert_eq!(snapshot.selections[0].plugin.0, "typed.owner");
        assert_eq!(snapshot.selections[0].item.0, "actionless");
        assert_eq!(snapshot.selections[0].frequency, 2);
        assert_eq!(snapshot.selections[0].last_selected_secs, Some(8));
        assert!(snapshot.query_affinities.is_empty());
        assert!(snapshot.commands.is_empty());
    }
}
