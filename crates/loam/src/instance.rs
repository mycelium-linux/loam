//! Multi-instance command execution over one [`CommandRunner`].
//!
//! One engine, component cache and plugin registry are shared. Each instance
//! gets its own `Store`, host state and limits, so two commands cannot observe
//! each other's WASI, resources or plugin state. This is the execution layer a
//! supervisor can build on; it does not add a daemon or a control socket.

use std::{
    any::Any,
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Result, bail};

use crate::command::{CommandEntry, CommandRunner, RunLimits, RunOutput, RunRequest};

/// One command instance, with a stable host-chosen identity.
#[derive(Debug, Clone)]
pub struct InstanceSpec {
    pub id: String,
    pub entry: CommandEntry,
    pub request: RunRequest,
    /// Host limits for this instance. Defaults to the package's command limits.
    pub limits: RunLimits,
}

impl InstanceSpec {
    pub fn new(id: impl Into<String>, entry: CommandEntry, request: RunRequest) -> Self {
        let limits = RunLimits::from_entry(&entry);
        Self {
            id: id.into(),
            entry,
            request,
            limits,
        }
    }

    /// Override the package limits for this instance.
    #[must_use]
    pub fn with_limits(mut self, limits: RunLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// The result of one instance. A failure is kept with its id so one bad Spore
/// does not erase the results of the others.
#[derive(Debug)]
pub struct InstanceResult {
    pub id: String,
    pub result: Result<RunOutput>,
}

/// Runs a bounded number of command instances at once over a shared runner.
pub struct InstanceRunner {
    runner: CommandRunner,
    max_parallel: usize,
}

impl InstanceRunner {
    /// `max_parallel` is the number of live `Store`s this runner will create at
    /// once. Zero is refused rather than interpreted as “unbounded”.
    pub fn new(runner: CommandRunner, max_parallel: usize) -> Result<Self> {
        if max_parallel == 0 {
            bail!("instance concurrency must be greater than zero");
        }
        Ok(Self {
            runner,
            max_parallel,
        })
    }

    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    /// Run every instance, returning results in the same order as `specs`.
    ///
    /// Instance ids must be non-empty and unique. Limits are validated before
    /// any wasm is compiled, so a bad request cannot consume the worker pool.
    pub fn run(&self, specs: Vec<InstanceSpec>) -> Result<Vec<InstanceResult>> {
        validate(&specs)?;
        Ok(self.dispatch(&specs, |spec| {
            self.runner
                .run_with_limits(&spec.entry, &spec.request, spec.limits)
        }))
    }

    fn dispatch<F>(&self, specs: &[InstanceSpec], run: F) -> Vec<InstanceResult>
    where
        F: Fn(&InstanceSpec) -> Result<RunOutput> + Sync,
    {
        if specs.is_empty() {
            return Vec::new();
        }

        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Option<InstanceResult>>> =
            Mutex::new((0..specs.len()).map(|_| None).collect());
        let workers = self.max_parallel.min(specs.len());

        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(spec) = specs.get(index) else {
                            break;
                        };
                        let result = catch_unwind(AssertUnwindSafe(|| run(spec)))
                            .unwrap_or_else(|panic| Err(anyhow::anyhow!(panic_text(panic))));
                        let mut results = results.lock().expect("instance result lock poisoned");
                        results[index] = Some(InstanceResult {
                            id: spec.id.clone(),
                            result,
                        });
                    }
                });
            }
        });

        results
            .into_inner()
            .expect("instance result lock poisoned")
            .into_iter()
            .map(|result| result.expect("every instance scheduled exactly once"))
            .collect()
    }
}

fn validate(specs: &[InstanceSpec]) -> Result<()> {
    let mut ids = BTreeSet::new();
    for spec in specs {
        if spec.id.trim().is_empty() {
            bail!("instance id must not be empty");
        }
        if !ids.insert(spec.id.as_str()) {
            bail!("duplicate instance id {:?}", spec.id);
        }
        if spec.limits.timeout_ms == 0 {
            bail!("instance {:?} has a zero timeout", spec.id);
        }
        if spec.limits.max_memory_bytes == 0 {
            bail!("instance {:?} has a zero memory limit", spec.id);
        }
    }
    Ok(())
}

fn panic_text(payload: Box<dyn Any + Send>) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("instance worker panicked: {detail}")
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    use super::*;

    fn entry(name: &str) -> CommandEntry {
        CommandEntry {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            summary: "test".to_string(),
            package: PathBuf::from("/tmp/test"),
            requires: Vec::new(),
            module: PathBuf::from("/tmp/test/module.wasm"),
            timeout_ms: 1_000,
            max_stdout_bytes: 1024,
            max_stderr_bytes: 1024,
            max_memory_bytes: 1024 * 1024,
        }
    }

    fn spec(id: &str) -> InstanceSpec {
        InstanceSpec::new(
            id,
            entry(id),
            RunRequest {
                args: Vec::new(),
                stdin: Vec::new(),
                env: Vec::new(),
                cwd: None,
            },
        )
    }

    #[test]
    fn results_keep_spec_order() {
        let runner = InstanceRunner::new(CommandRunner::new(None, None).unwrap(), 2).unwrap();
        let specs = vec![spec("one"), spec("two"), spec("three")];
        let results = runner.dispatch(&specs, |spec| {
            Ok(RunOutput {
                code: spec.id.len() as i32,
                stdout: spec.id.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        });
        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        assert_eq!(results[0].result.as_ref().unwrap().stdout, b"one");
    }

    #[test]
    fn dispatches_up_to_the_parallel_limit() {
        let runner = InstanceRunner::new(CommandRunner::new(None, None).unwrap(), 2).unwrap();
        let specs = vec![spec("one"), spec("two")];
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let results = std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                runner.dispatch(&specs, |_spec| {
                    started_tx.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                    Ok(RunOutput {
                        code: 0,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                })
            });

            let first = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            let second = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            // Release both workers even on failure; the assertion below is the
            // test result, not a deadlock.
            let _ = release_tx.send(());
            let _ = release_tx.send(());
            worker.join().unwrap();
            (first, second)
        });

        assert!(results.0, "first worker did not start");
        assert!(results.1, "second worker did not run concurrently");
    }

    #[test]
    fn duplicate_ids_are_refused_before_running() {
        let runner = InstanceRunner::new(CommandRunner::new(None, None).unwrap(), 2).unwrap();
        let error = runner.run(vec![spec("same"), spec("same")]).unwrap_err();
        assert!(error.to_string().contains("duplicate instance id"));
    }

    #[test]
    fn zero_concurrency_is_refused() {
        let error = match InstanceRunner::new(CommandRunner::new(None, None).unwrap(), 0) {
            Ok(_) => panic!("zero concurrency should be refused"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn bad_limits_are_refused_before_running() {
        let runner = InstanceRunner::new(CommandRunner::new(None, None).unwrap(), 1).unwrap();
        let mut spec = spec("bad");
        spec.limits.timeout_ms = 0;
        let error = runner.run(vec![spec]).unwrap_err();
        assert!(error.to_string().contains("zero timeout"));
    }
}
