use std::{
    marker::PhantomData,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Poll,
};

use futures::{future::LocalBoxFuture, stream::FuturesOrdered, Future, Stream};

pub struct Scope<'scope, 'env: 'scope, C: 'env> {
    /// Stores the set of futures that have been spawned.
    ///
    /// This is behind a mutex so that multiple concurrent actors can access it.
    /// A `RwLock` seems better, but `FuturesUnordered is not `Sync` in the case.
    /// But in fact it doesn't matter anyway, because all spawned futures execute
    /// CONCURRENTLY and hence there will be no contention.
    futures: Mutex<Option<Pin<Box<FuturesOrdered<LocalBoxFuture<'scope, ()>>>>>>,
    enqueued: Mutex<Vec<LocalBoxFuture<'scope, ()>>>,
    cancelled: Mutex<Option<C>>,
    phantom: PhantomData<&'scope &'env ()>,
}

impl<'scope, 'env, C> Scope<'scope, 'env, C> {
    /// Create a scope.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            futures: Mutex::new(Some(Box::pin(FuturesOrdered::new()))),
            enqueued: Default::default(),
            cancelled: Default::default(),
            phantom: Default::default(),
        })
    }

    /// Polls the jobs that were spawned thus far. Returns:
    ///
    /// * `Pending` if there are jobs that cannot complete
    /// * `Ready(Ok(()))` if all jobs are completed
    /// * `Ready(Err(c))` if the scope has been canceled
    ///
    /// Should not be invoked again once `Ready(Err(c))` is returned.
    ///
    /// It is ok to invoke it again after `Ready(Ok(()))` has been returned;
    /// if any new jobs have been spawned, they will execute.
    pub(crate) fn poll_jobs(&self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), C>> {
        let mut futures = self.futures.lock().unwrap();
        'outer: loop {
            // once we are cancelled, we do no more work.
            if let Some(c) = self.cancelled.lock().unwrap().take() {
                return Poll::Ready(Err(c));
            }

            futures
                .as_mut()
                .unwrap()
                .extend(self.enqueued.lock().unwrap().drain(..));

            while let Some(()) = ready!(futures.as_mut().unwrap().as_mut().poll_next(cx)) {
                // once we are cancelled, we do no more work.
                if self.cancelled.lock().unwrap().is_some() {
                    continue 'outer;
                }
            }

            if self.enqueued.lock().unwrap().is_empty() {
                return Poll::Ready(Ok(()));
            }
        }
    }

    /// Clear out all pending jobs. This is used when dropping the
    /// scope body to ensure that any possible references to `Scope`
    /// are removed before we drop it.
    ///
    /// # Unsafe contract
    ///
    /// Once this returns, there are no more pending tasks.
    pub(crate) fn clear(&self) {
        let _ = self
            .futures
            .lock()
            .unwrap()
            .insert(Box::pin(FuturesOrdered::new()));
        self.enqueued.lock().unwrap().clear();
    }

    /// Cancel the scope immediately -- all existing jobs will stop at their next await point
    /// and never wake up again. Anything on their stacks will be dropped.
    ///
    /// This returns a future that you can await, but it will never complete (because you will never be reawoken).
    pub fn cancel<T>(&'scope self, value: C) -> impl Future<Output = T> + 'scope
    where
        T: std::fmt::Debug + 'scope,
    {
        let mut lock = self.cancelled.lock().unwrap();
        if lock.is_none() {
            *lock = Some(value);
        }
        std::mem::drop(lock);

        // The code below will never run
        self.spawn(async { panic!() })
    }

    /// Spawn a job that will run concurrently with everything else in the scope.
    /// The job may access stack fields defined outside the scope.
    /// The scope will not terminate until this job completes or the scope is cancelled.
    pub fn spawn<T>(
        &'scope self,
        future: impl Future<Output = T> + 'scope,
    ) -> impl Future<Output = T> + 'scope
    where
        T: std::fmt::Debug + 'scope,
    {
        // Use a channel to communicate result from the *actual* future
        // (which lives in the futures-unordered) and the caller.
        // This is kind of crappy because, ideally, the caller expressing interest
        // in the result of the future would let it run, but that would require
        // more clever coding and I'm just trying to stand something up quickly
        // here. What will happen when caller expresses an interest in result
        // now is that caller will block which should (eventually) allow the
        // futures-unordered to be polled and make progress. Good enough.

        let (tx, rx) = async_channel::bounded(1);

        self.enqueued.lock().unwrap().push(Box::pin(async move {
            let v = future.await;
            let _ = tx.send(v).await;
        }));

        async move {
            match rx.recv().await {
                Ok(v) => v,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
    }

    /// Spawn a job that returns `Result`. If the task returns `Err`, the
    /// scope will be canceled as if [`Scope::cancel`] were called. If the job
    /// returns `Ok(v)`, then `v` is used as the result of the job.
    pub fn spawn_cancelling<T>(
        &'scope self,
        future: impl Future<Output = Result<T, C>> + 'scope,
    ) -> impl Future<Output = T> + 'scope
    where
        T: std::fmt::Debug + 'scope,
    {
        // Use a channel to communicate result from the *actual* future
        // (which lives in the futures-unordered) and the caller.
        // This is kind of crappy because, ideally, the caller expressing interest
        // in the result of the future would let it run, but that would require
        // more clever coding and I'm just trying to stand something up quickly
        // here. What will happen when caller expresses an interest in result
        // now is that caller will block which should (eventually) allow the
        // futures-unordered to be polled and make progress. Good enough.

        let (tx, rx) = async_channel::bounded(1);

        self.enqueued.lock().unwrap().push(Box::pin(async move {
            match future.await {
                Ok(v) => {
                    let _ = tx.send(v).await;
                }
                Err(e) => self.cancel(e).await,
            }
        }));

        async move {
            match rx.recv().await {
                Ok(v) => v,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
    }
}
