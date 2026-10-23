use anyhow::{Context, Result, anyhow};

type Job = Box<dyn FnOnce() + Send>;

/// Wasmtime's compiler needs more stack than macOS dispatch workers provide.
/// Keep compilation, parsing, and queries on reusable threads with known stacks.
#[derive(Clone)]
pub struct Workers(async_channel::Sender<Job>);

impl Workers {
    pub fn new() -> Result<Self> {
        let (sender, receiver) = async_channel::unbounded::<Job>();
        for index in 0..2 {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("tree-sitter-wasm-{index}"))
                .stack_size(8 * 1024 * 1024)
                .spawn(move || {
                    while let Ok(job) = receiver.recv_blocking() {
                        job();
                    }
                })
                .context("Could not start WASM worker thread")?;
        }
        Ok(Self(sender))
    }

    fn submit<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<async_channel::Receiver<Result<T>>> {
        let (sender, receiver) = async_channel::bounded(1);
        self.0
            .try_send(Box::new(move || {
                // Report a failed job without losing the worker for later jobs.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                    .map_err(|panic| {
                        let message = panic
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| panic.downcast_ref::<&str>().copied())
                            .unwrap_or("unknown panic");
                        anyhow!("WASM worker panicked: {message}")
                    })
                    .and_then(|result| result);
                let _ = sender.try_send(result);
            }))
            .map_err(|_| anyhow!("WASM workers are no longer available"))?;
        Ok(receiver)
    }

    pub async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.submit(work)?
            .recv()
            .await
            .context("WASM worker did not return a result")?
    }

    pub fn run_blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.submit(work)?
            .recv_blocking()
            .context("WASM worker did not return a result")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_return_values_errors_and_panics_without_losing_workers() {
        let workers = Workers::new().unwrap();
        assert_eq!(workers.run_blocking(|| Ok(42)).unwrap(), 42);
        assert_eq!(
            workers
                .run_blocking(|| Err::<(), _>(anyhow!("failed job")))
                .unwrap_err()
                .to_string(),
            "failed job"
        );
        for _ in 0..4 {
            assert!(
                workers
                    .run_blocking(|| -> Result<()> { panic!("failed worker") })
                    .unwrap_err()
                    .to_string()
                    .contains("failed worker")
            );
        }
        assert_eq!(
            futures_lite::future::block_on(workers.run(|| Ok(7))).unwrap(),
            7
        );
    }
}
