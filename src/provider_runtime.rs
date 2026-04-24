use std::future::Future;
use std::thread;

use futures::executor;
use orion_error::{ToStructError, UvsFrom};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::oneshot;
use wp_error::{KnowledgeReason, KnowledgeResult};

pub(crate) fn init_provider_runtime<T, Init>(
    provider: &'static str,
    init_thread_name: &'static str,
    worker_thread_name: &'static str,
    pool_size: u32,
    init: Init,
) -> KnowledgeResult<T>
where
    T: Send + 'static,
    Init: FnOnce(Runtime) -> KnowledgeResult<T> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    thread::Builder::new()
        .name(init_thread_name.to_string())
        .spawn(move || {
            let runtime = Builder::new_multi_thread()
                .worker_threads(pool_size as usize)
                .enable_all()
                .thread_name(worker_thread_name)
                .build()
                .map_err(|err| {
                    KnowledgeReason::from_conf()
                        .to_err()
                        .with_detail(format!("create {provider} tokio runtime failed: {err}"))
                });
            let result = runtime.and_then(init);
            let _ = tx.send(result);
        })
        .map_err(|err| {
            KnowledgeReason::from_conf()
                .to_err()
                .with_detail(format!("spawn {provider} init thread failed: {err}"))
        })?;

    executor::block_on(rx).map_err(|err| {
        KnowledgeReason::from_conf()
            .to_err()
            .with_detail(format!("receive {provider} init result failed: {err}"))
    })?
}

pub(crate) async fn run_task<T, F>(
    runtime: &Runtime,
    provider: &'static str,
    action: &str,
    fut: F,
) -> KnowledgeResult<T>
where
    T: Send + 'static,
    F: Future<Output = KnowledgeResult<T>> + Send + 'static,
{
    runtime
        .handle()
        .spawn(fut)
        .await
        .map_err(|err| join_err(provider, action, err))?
}

pub(crate) fn block_on_task<T, F>(
    runtime: &Runtime,
    provider: &'static str,
    action: &str,
    fut: F,
) -> KnowledgeResult<T>
where
    T: Send + 'static,
    F: Future<Output = KnowledgeResult<T>> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let task = runtime.handle().spawn(fut);
    runtime.handle().spawn(async move {
        let _ = tx.send(task.await);
    });
    executor::block_on(rx)
        .map_err(|err| recv_err(provider, action, err))?
        .map_err(|err| join_err(provider, action, err))?
}

fn recv_err(
    provider: &str,
    action: &str,
    err: tokio::sync::oneshot::error::RecvError,
) -> wp_error::KnowledgeError {
    KnowledgeReason::from_logic().to_err().with_detail(format!(
        "{provider} async task receive failed during {action}: {err}"
    ))
}

fn join_err(provider: &str, action: &str, err: tokio::task::JoinError) -> wp_error::KnowledgeError {
    KnowledgeReason::from_logic().to_err().with_detail(format!(
        "{provider} async task join failed during {action}: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime() -> Runtime {
        Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("build test runtime")
    }

    #[test]
    fn init_provider_runtime_returns_initialized_value() {
        let value = init_provider_runtime("test", "test-init", "test-worker", 1, |runtime| {
            drop(runtime);
            Ok::<_, wp_error::KnowledgeError>(7usize)
        })
        .expect("init provider runtime");
        assert_eq!(value, 7);
    }

    #[test]
    fn block_on_task_returns_future_result() {
        let runtime = test_runtime();
        let value = block_on_task(&runtime, "test", "query", async {
            Ok::<_, wp_error::KnowledgeError>(11usize)
        })
        .expect("block_on_task");
        assert_eq!(value, 11);
    }

    #[test]
    fn block_on_task_reports_join_error_on_panic() {
        let runtime = test_runtime();
        let err = block_on_task::<(), _>(&runtime, "test", "panic", async move {
            panic!("boom");
        })
        .expect_err("panic should become join error");
        let detail = err.to_string();
        assert!(detail.contains("test async task join failed during panic"));
    }

    #[test]
    fn run_task_reports_join_error_on_panic() {
        let provider_runtime = test_runtime();
        let caller_runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build caller runtime");
        let err = caller_runtime
            .block_on(run_task::<(), _>(
                &provider_runtime,
                "test",
                "panic",
                async move {
                    panic!("boom");
                },
            ))
            .expect_err("panic should become join error");
        let detail = err.to_string();
        assert!(detail.contains("test async task join failed during panic"));
    }
}
