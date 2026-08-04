//! The worker pool that applies the environment-sharing rule (spec 15.6, 15.3).
//!
//! A worker is one live CPython process running one plugin's code. The pool's
//! single job is to decide when two plugins may share a process and when they
//! must not:
//!
//! * Two plugins that resolve to the SAME ([`Interpreter`], [`EnvironmentId`])
//!   share ONE worker. Their dependency closures are byte-for-byte identical
//!   (that is what an equal [`EnvironmentId`] means, spec 15.3), so a single
//!   interpreter can serve both without either seeing the other's imports.
//! * Two plugins with DIFFERENT ids get SEPARATE workers, hence separate
//!   processes. This is the mechanism — not a decoration — by which conflicting
//!   dependency versions coexist (acceptance 31.20): a single address space can
//!   hold one version of a module at a time, so two versions demand two
//!   processes.
//!
//! The interpreter is part of the key, not just the id: the same locked
//! closure built for two different CPython builds is two different runtimes and
//! must not share one process.

use crikey_package_manager::{EnvironmentId, ImportPath};

use crate::interpreter::Interpreter;
use crate::worker::{HostError, ModernWorker, WorkerOptions};

/// One pooled worker, tagged with the key that decides sharing.
///
/// The key is `(interpreter, environment, entrypoint, import_path)`: the
/// protocol has no per-call plugin routing, so a shared worker answers with the
/// code it was born with. Sharing across two plugins that differ in entrypoint
/// or import path (distinct sources, even under one env id) would serve one
/// plugin's results for the other's query (pinned decision 1). The interpreter
/// and env id alone are not enough.
#[derive(Debug)]
struct PooledWorker {
    interpreter: Interpreter,
    environment: EnvironmentId,
    entrypoint: String,
    import_path: ImportPath,
    worker: ModernWorker,
}

/// A set of live modern-plugin workers, sharing one per environment.
///
/// The pool owns every worker it spawns; dropping the pool drops the workers,
/// each of which reaps its own child (process group on Unix). Lookup is a
/// linear scan: a host runs a handful of environments, not thousands, so a map
/// would trade a real ordering guarantee (workers spawn in request order) for
/// no measurable gain.
#[derive(Debug, Default)]
pub struct WorkerPool {
    workers: Vec<PooledWorker>,
}

impl WorkerPool {
    pub fn new() -> WorkerPool {
        WorkerPool::default()
    }

    /// Returns the worker for `(interpreter, environment, entrypoint,
    /// import_path)`, spawning one if this is the first plugin to ask for that
    /// key.
    ///
    /// Two plugins with the same key share the returned worker; a distinct key
    /// spawns a separate process. A shared worker keeps the options it was born
    /// with, because a running process cannot retroactively adopt a different
    /// import path — so the key includes the entrypoint and import path, not the
    /// `(interpreter, environment)` pair alone (pinned decision 1): the protocol
    /// routes no plugin id per call, and a worker answers only as the plugin it
    /// loaded at spawn. `options` is consumed only when a new worker is spawned.
    pub fn worker_for(
        &mut self,
        interpreter: &Interpreter,
        environment: &EnvironmentId,
        options: WorkerOptions,
    ) -> Result<&mut ModernWorker, HostError> {
        if let Some(index) = self.workers.iter().position(|pooled| {
            &pooled.interpreter == interpreter
                && &pooled.environment == environment
                && pooled.entrypoint == options.entrypoint
                && pooled.import_path == options.import_path
        }) {
            if self.workers[index].worker.is_alive() {
                return Ok(&mut self.workers[index].worker);
            }
            // A failed call has already stopped and reaped this process. Do
            // not return a permanently dead worker to the next request.
            self.workers.swap_remove(index);
        }

        let entrypoint = options.entrypoint.clone();
        let import_path = options.import_path.clone();
        let worker = ModernWorker::spawn(interpreter, options)?;
        self.workers.push(PooledWorker {
            interpreter: interpreter.clone(),
            environment: environment.clone(),
            entrypoint,
            import_path,
            worker,
        });
        Ok(&mut self.workers.last_mut().expect("a worker was just pushed").worker)
    }

    /// Number of keyed worker entries, including a stopped worker until the next
    /// lookup replaces it.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }
}
