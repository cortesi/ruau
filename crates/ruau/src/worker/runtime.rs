use std::{collections::HashMap, rc::Rc, sync::mpsc as std_mpsc};

use tokio::{
    runtime::Builder,
    sync::{mpsc, oneshot},
    task::{AbortHandle, LocalSet, spawn_local},
};

use super::{
    LuauWorkerCancellation, LuauWorkerError, LuauWorkerResult, SetupFn, WorkerJob, WorkerValue,
};
use crate::{Compiler, Luau, LuauOptions, StdLib};

pub(super) struct WorkerRequest {
    pub(super) id: u64,
    pub(super) job: WorkerJob,
    pub(super) cancellation: LuauWorkerCancellation,
    pub(super) response: oneshot::Sender<LuauWorkerResult<WorkerValue>>,
}

pub(super) enum WorkerControl {
    Cancel(u64),
    Shutdown(oneshot::Sender<()>),
}

pub(super) struct WorkerInit {
    pub(super) std_libs: StdLib,
    pub(super) options: LuauOptions,
    pub(super) compiler: Option<Compiler>,
    pub(super) setup: Vec<SetupFn>,
    pub(super) request_rx: mpsc::Receiver<WorkerRequest>,
    pub(super) control_rx: mpsc::UnboundedReceiver<WorkerControl>,
    pub(super) init_tx: std_mpsc::Sender<LuauWorkerResult<()>>,
}

pub(super) fn run_worker_thread(init: WorkerInit) {
    let runtime = match Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            drop(
                init.init_tx
                    .send(Err(LuauWorkerError::Runtime(error.to_string()))),
            );
            return;
        }
    };

    let local = LocalSet::new();
    runtime.block_on(local.run_until(async move {
        let lua = match Luau::new_with(init.std_libs, init.options) {
            Ok(lua) => Rc::new(lua),
            Err(error) => {
                drop(init.init_tx.send(Err(error.into())));
                return;
            }
        };
        if let Some(compiler) = init.compiler {
            lua.set_compiler(compiler);
        }
        for setup in init.setup {
            if let Err(error) = setup(&lua) {
                drop(init.init_tx.send(Err(error.into())));
                return;
            }
        }
        drop(init.init_tx.send(Ok(())));
        run_worker_loop(lua, init.request_rx, init.control_rx).await;
    }));
}

async fn run_worker_loop(
    lua: Rc<Luau>,
    mut request_rx: mpsc::Receiver<WorkerRequest>,
    mut control_rx: mpsc::UnboundedReceiver<WorkerControl>,
) {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<u64>();
    let mut in_flight: HashMap<u64, AbortHandle> = HashMap::new();
    let mut shutdown: Option<oneshot::Sender<()>> = None;
    let mut accepting = true;

    loop {
        if !accepting && in_flight.is_empty() {
            if let Some(sender) = shutdown.take() {
                let _ignored = sender.send(());
            }
            break;
        }

        tokio::select! {
            Some(id) = done_rx.recv() => {
                in_flight.remove(&id);
            }
            Some(control) = control_rx.recv() => {
                match control {
                    WorkerControl::Cancel(id) => {
                        if let Some(handle) = in_flight.get(&id) {
                            handle.abort();
                        }
                    }
                    WorkerControl::Shutdown(sender) => {
                        accepting = false;
                        shutdown = Some(sender);
                        while let Ok(request) = request_rx.try_recv() {
                            spawn_or_cancel_request(
                                Rc::clone(&lua),
                                request,
                                &mut in_flight,
                                done_tx.clone(),
                            );
                        }
                    }
                }
            }
            request = request_rx.recv(), if accepting => {
                match request {
                    Some(request) => spawn_or_cancel_request(
                        Rc::clone(&lua),
                        request,
                        &mut in_flight,
                        done_tx.clone(),
                    ),
                    None => accepting = false,
                }
            }
            else => {
                accepting = false;
            }
        }
    }
}

fn spawn_or_cancel_request(
    lua: Rc<Luau>,
    request: WorkerRequest,
    in_flight: &mut HashMap<u64, AbortHandle>,
    done_tx: mpsc::UnboundedSender<u64>,
) {
    let WorkerRequest {
        id,
        job,
        cancellation,
        response,
    } = request;
    if cancellation.is_cancelled() {
        drop(response.send(Err(LuauWorkerError::Cancelled)));
        return;
    }

    let task = spawn_local(async move { job(&lua, cancellation).await });
    let abort_handle = task.abort_handle();
    in_flight.insert(id, abort_handle);
    spawn_local(async move {
        let result = match task.await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(LuauWorkerError::Cancelled),
            Err(error) => Err(LuauWorkerError::Panicked(error.to_string())),
        };
        drop(response.send(result));
        let _ignored = done_tx.send(id);
    });
}
