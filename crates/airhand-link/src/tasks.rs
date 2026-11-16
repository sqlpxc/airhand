//! Owned async task: dropping the owner cancels the task, including on error,
//! timeout and parent cancellation. A bare JoinHandle would detach instead.
use std::ops::{Deref, DerefMut};
use tokio::task::JoinHandle;

pub struct OwnedTask<T>(pub JoinHandle<T>);
impl<T> Drop for OwnedTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl<T> Deref for OwnedTask<T> {
    type Target = JoinHandle<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T> DerefMut for OwnedTask<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// 等一组 future 都完成，有一个出错就马上返回它的错误（其余的随之丢掉）。个数运行时才知道（画面有几路），
/// `tokio::try_join!` 用不了；为它不引入 futures
pub(crate) async fn try_join_all<F, E>(futures: Vec<F>) -> Result<(), E>
where
    F: Future<Output = Result<(), E>>,
{
    use std::task::Poll;
    let mut futures: Vec<Option<std::pin::Pin<Box<F>>>> =
        futures.into_iter().map(|f| Some(Box::pin(f))).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for slot in &mut futures {
            let Some(f) = slot else { continue };
            match f.as_mut().poll(cx) {
                Poll::Ready(Ok(())) => *slot = None,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => pending = true,
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn try_join_all_waits_for_all_or_the_first_error() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let slow = async move {
            let _ = rx.await;
            Ok::<(), &str>(())
        };
        let mut all = Box::pin(try_join_all(vec![
            Box::pin(slow) as std::pin::Pin<Box<dyn Future<Output = _>>>,
            Box::pin(async { Ok(()) }),
        ]));
        assert!(futures_poll_once(&mut all).await.is_none());
        tx.send(()).unwrap();
        assert_eq!(all.await, Ok(()));
        let failed = try_join_all(vec![
            Box::pin(std::future::pending()) as std::pin::Pin<Box<dyn Future<Output = _>>>,
            Box::pin(async { Err("broken") }),
        ]);
        assert_eq!(failed.await, Err("broken"));
        assert_eq!(
            try_join_all::<std::future::Ready<Result<(), ()>>, ()>(vec![]).await,
            Ok(())
        );
    }

    /// 轮询一次：完成了是 `Some`
    async fn futures_poll_once<F: Future + Unpin>(f: &mut F) -> Option<F::Output> {
        std::future::poll_fn(|cx| {
            std::task::Poll::Ready(match std::pin::Pin::new(&mut *f).poll(cx) {
                std::task::Poll::Ready(v) => Some(v),
                std::task::Poll::Pending => None,
            })
        })
        .await
    }

    #[tokio::test]
    async fn timeout_cancels_instead_of_detaching() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut task = OwnedTask(tokio::spawn(async move {
            let _tx = tx;
            std::future::pending::<()>().await;
        }));
        let _ = tokio::time::timeout(std::time::Duration::from_millis(1), &mut task.0).await;
        drop(task);
        assert!(rx.await.is_err());
    }
}
