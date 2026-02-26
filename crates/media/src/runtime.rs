use anyhow::Result;
use glib::{MainContext, MainLoop};
use gstreamer as gst;
use std::{sync::Arc, thread};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone)]
pub struct GstRuntime {
    inner: Arc<GstInner>,
}

struct GstInner {
    tx: mpsc::UnboundedSender<Job>,
    _thread: Arc<thread::JoinHandle<()>>,
}

impl GstRuntime {
    pub fn init() -> Result<Self> {
        gst::init()?;

        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();

        let handle = thread::spawn(move || {
            let ctx = MainContext::new();
            let _guard = ctx.acquire().expect("Failed to acquire GLib context");

            let main_loop = MainLoop::new(Some(&ctx), false);

            let main_loop_clone = main_loop.clone();
            // Drain job queue on the GLib thread
            ctx.spawn_local(async move {
                while let Some(job) = rx.recv().await {
                    job();
                }
                // if channel closes, quit loop
                main_loop_clone.quit();
            });

            main_loop.run();
        });

        Ok(Self {
            inner: Arc::new(GstInner {
                tx,
                _thread: Arc::new(handle),
            }),
        })
    }

    /// Execute `f` on the GLib/GStreamer thread and await the result.
    pub async fn exec<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (done_tx, done_rx) = oneshot::channel::<Result<R>>();
        let job: Job = Box::new(move || {
            let res = f();
            let _ = done_tx.send(res);
        });

        self.inner
            .tx
            .send(job)
            .map_err(|_| anyhow::anyhow!("GstRuntime queue closed"))?;

        done_rx
            .await
            .map_err(|_| anyhow::anyhow!("GstRuntime queue cancelled"))?
    }

    /// Fire-and-forget scheduling on GLib thread.
    pub fn spawn<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        self.inner
            .tx
            .send(Box::new(f))
            .map_err(|_| anyhow::anyhow!("GstRuntime queue closed"))
    }
}
