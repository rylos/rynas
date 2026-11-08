use crate::sync_helpers::*;
use maybe_async::*;
use smb_transport::{IoVec, SmbTransport};
use std::{sync::Arc, time::Duration};

use crate::msg_handler::IncomingMessage;

use super::base::ParallelWorker;

#[maybe_async(AFIT)]
#[allow(async_fn_in_trait)] // for maybe_async.
pub trait MultiWorkerBackend {
    type SendMessage;
    type AwaitingNotifier;
    type AwaitingWaiter;

    async fn start(
        transport: Box<dyn SmbTransport>,
        worker: Arc<ParallelWorker<Self>>,
        send_channel_recv: mpsc::Receiver<Self::SendMessage>,
    ) -> crate::Result<Arc<Self>>
    where
        Self: std::fmt::Debug + Sized,
        Self::AwaitingNotifier: std::fmt::Debug;
    async fn stop(&self) -> crate::Result<()>;

    fn wrap_msg_to_send(msg: IoVec) -> Self::SendMessage;
    fn make_notifier_awaiter_pair() -> (Self::AwaitingNotifier, Self::AwaitingWaiter);
    // TODO: Consider typing the tx/rx in the trait, like the notifier/awaiter.
    fn make_send_channel_pair() -> (
        mpsc::Sender<Self::SendMessage>,
        mpsc::Receiver<Self::SendMessage>,
    );

    /// rynas patch: `timeout` is a progress watchdog: the wait fails when no message
    /// at all came on the connection for that long (`activity`, see
    /// [`ParallelWorker::activity_ms`]) since it began, not when this one answer took longer
    /// (with many requests in flight on a slow link a single answer may wait its turn).
    async fn wait_on_waiter(
        waiter: Self::AwaitingWaiter,
        timeout: Duration,
        activity: &std::sync::atomic::AtomicU64,
    ) -> crate::Result<IncomingMessage>;
    fn send_notify(
        tx: Self::AwaitingNotifier,
        msg: crate::Result<IncomingMessage>,
    ) -> crate::Result<()>;
}
