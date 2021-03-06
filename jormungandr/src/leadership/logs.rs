use futures03::future::poll_fn;
pub use jormungandr_lib::interfaces::LeadershipLogStatus;
use jormungandr_lib::interfaces::{LeadershipLog, LeadershipLogId};
use std::{sync::Arc, time::Duration};
use tokio02::{sync::RwLock, time};

/// all leadership logs, allow for following up on the different entity
/// of the blockchain
#[derive(Clone)]
pub struct Logs(Arc<RwLock<internal::Logs>>);

/// leadership log handle. will allow to update the status of the log
/// without having to hold the [`Logs`]
///
/// [`Logs`]: ./struct.Logs.html
#[derive(Clone)]
pub struct LeadershipLogHandle {
    internal_id: LeadershipLogId,
    logs: Logs,
}

impl LeadershipLogHandle {
    /// make a leadership event as triggered.
    ///
    /// This should be called when the leadership event has started.
    ///
    /// # panic
    ///
    /// on non-release build, this function will panic if the log was already
    /// marked as awaken.
    ///
    pub async fn mark_wake(&self) {
        self.logs.mark_wake(self.internal_id).await
    }

    pub async fn set_status(&self, status: LeadershipLogStatus) {
        self.logs.set_status(self.internal_id, status).await
    }

    /// make a leadership event as finished.
    ///
    /// This should be called when the leadership event has finished its
    /// scheduled action.
    ///
    /// # panic
    ///
    /// on non-release build, this function will panic if the log was already
    /// marked as finished.
    ///
    pub async fn mark_finished(&self) {
        self.logs.mark_finished(self.internal_id).await
    }
}

impl Logs {
    /// create a Leadership Logs. This will make sure we delete from time to time
    /// some of the logs that are not necessary.
    ///
    /// the `ttl` can be any sensible value the user will see appropriate. The log will
    /// live at least its scheduled time + `ttl`.
    ///
    /// On changes, the log's TTL will be reset to this `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Logs(Arc::new(RwLock::new(internal::Logs::new(ttl))))
    }

    pub async fn insert(&self, log: LeadershipLog) -> Result<LeadershipLogHandle, ()> {
        let logs = self.clone();
        let id = logs.0.write().await.insert(log);
        Ok(LeadershipLogHandle {
            internal_id: id,
            logs: logs,
        })
    }

    async fn mark_wake(&self, leadership_log_id: LeadershipLogId) {
        let inner = self.0.clone();
        inner.write().await.mark_wake(&leadership_log_id.into());
    }

    async fn set_status(&self, leadership_log_id: LeadershipLogId, status: LeadershipLogStatus) {
        let inner = self.0.clone();
        inner
            .write()
            .await
            .set_status(&leadership_log_id.into(), status);
    }

    async fn mark_finished(&self, leadership_log_id: LeadershipLogId) {
        let inner = self.0.clone();
        inner.write().await.mark_finished(&leadership_log_id.into());
    }

    pub async fn poll_purge(&mut self) -> Result<(), time::Error> {
        let inner = self.0.clone();
        let mut guard = inner.write().await;
        poll_fn(move |mut cx| guard.poll_purge(&mut cx)).await
    }

    pub async fn logs(&self) -> Vec<LeadershipLog> {
        let inner = self.0.clone();
        let guard = inner.read().await;
        guard.logs().cloned().collect()
    }
}

pub(super) mod internal {
    use super::{LeadershipLog, LeadershipLogId, LeadershipLogStatus};
    use futures03::{
        task::{Context, Poll},
        Stream,
    };
    use std::{
        collections::HashMap,
        pin::Pin,
        time::{Duration, Instant},
    };
    use tokio02::time::{self, delay_queue, DelayQueue, Instant as TokioInstant};

    pub struct Logs {
        entries: HashMap<LeadershipLogId, (LeadershipLog, delay_queue::Key)>,
        expirations: Pin<Box<DelayQueue<LeadershipLogId>>>,
        ttl: Duration,
    }

    impl Logs {
        pub fn new(ttl: Duration) -> Self {
            Logs {
                entries: HashMap::new(),
                expirations: Box::pin(DelayQueue::new()),
                ttl,
            }
        }

        pub fn insert(&mut self, log: LeadershipLog) -> LeadershipLogId {
            let id = log.leadership_log_id();

            let now = std::time::SystemTime::now();
            let minimal_duration = if &now < log.scheduled_at_time().as_ref() {
                log.scheduled_at_time()
                    .as_ref()
                    .duration_since(now)
                    .unwrap()
            } else {
                Duration::from_secs(0)
            };
            let ttl = minimal_duration.checked_add(self.ttl).unwrap_or(self.ttl);

            let delay = self.expirations.insert(id.clone(), ttl);

            self.entries.insert(id, (log, delay));
            id
        }

        pub fn mark_wake(&mut self, leadership_log_id: &LeadershipLogId) {
            if let Some((ref mut log, ref key)) = self.entries.get_mut(leadership_log_id) {
                log.mark_wake();

                self.expirations
                    .reset_at(key, TokioInstant::from_std(Instant::now() + self.ttl));
            } else {
                unimplemented!()
            }
        }

        pub fn set_status(
            &mut self,
            leadership_log_id: &LeadershipLogId,
            status: LeadershipLogStatus,
        ) {
            if let Some((ref mut log, ref key)) = self.entries.get_mut(leadership_log_id) {
                log.set_status(status);

                self.expirations
                    .reset_at(key, TokioInstant::from_std(Instant::now() + self.ttl));
            } else {
                unimplemented!()
            }
        }

        pub fn mark_finished(&mut self, leadership_log_id: &LeadershipLogId) {
            if let Some((ref mut log, ref key)) = self.entries.get_mut(leadership_log_id) {
                log.mark_finished();

                self.expirations
                    .reset_at(key, TokioInstant::from_std(Instant::now() + self.ttl));
            } else {
                unimplemented!()
            }
        }

        pub fn poll_purge(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), time::Error>> {
            loop {
                match self.expirations.as_mut().poll_next(cx) {
                    Poll::Ready(Some(Ok(entry))) => {
                        self.entries.remove(entry.get_ref());
                    }
                    Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                    Poll::Ready(None) => return Poll::Ready(Ok(())),

                    // Here Pending means there are still items in the DelayQueue but
                    // they are not expired. We don't want this function to wait for these
                    // ones to expired. We only cared about removing the expired ones.
                    Poll::Pending => return Poll::Ready(Ok(())),
                }
            }
        }

        pub fn logs<'a>(&'a self) -> impl Iterator<Item = &'a LeadershipLog> {
            self.entries.values().map(|(v, _)| v)
        }
    }
}
