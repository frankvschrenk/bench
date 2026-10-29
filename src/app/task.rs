//! Background work for the window: run a future on the backend runtime and
//! pick its result up in a later frame.

use std::future::Future;
use std::sync::mpsc::{channel, Receiver};

use tokio::runtime::Runtime;

/// Result of a spawned future, picked up with [`drain`].
pub struct Pending<T>(Receiver<T>);

/// Run `fut` on the runtime and wake the window when it is done.
pub fn spawn<T, F>(runtime: &Runtime, ctx: &egui::Context, fut: F) -> Pending<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    runtime.spawn(async move {
        let _ = tx.send(fut.await);
        ctx.request_repaint();
    });
    Pending(rx)
}

/// Keep the pending results that are not done yet, hand the finished ones out.
pub fn drain<T>(pending: &mut Vec<Pending<T>>) -> Vec<T> {
    let mut done = Vec::new();
    pending.retain(|p| match p.0.try_recv() {
        Ok(value) => {
            done.push(value);
            false
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => true,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
    });
    done
}

/// Boxed future, for spawning one of several async calls from one place.
pub type BoxFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send>>;
