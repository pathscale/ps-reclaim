use ps_reclaim::Domain;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// Controlled reader overlap for progress tests, NOT memory-ordering tests:
/// these channels deliberately introduce synchronization with the driver.
pub(crate) struct RemoteReader {
    cmd: Option<mpsc::Sender<bool>>,
    ack: mpsc::Receiver<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RemoteReader {
    pub(crate) fn spawn(domain: Arc<Domain>) -> Self {
        let (cmd, rx) = mpsc::channel::<bool>();
        let (tx, ack) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let mut held = None;
            while let Ok(pin) = rx.recv() {
                held = if pin { Some(domain.pin()) } else { None };
                if tx.send(()).is_err() {
                    break;
                }
            }
            drop(held);
        });
        Self {
            cmd: Some(cmd),
            ack,
            handle: Some(handle),
        }
    }

    pub(crate) fn pin(&self) {
        self.cmd.as_ref().unwrap().send(true).unwrap();
        self.ack
            .recv_timeout(Duration::from_secs(30))
            .expect("reader did not pin");
    }

    pub(crate) fn unpin(&self) {
        self.cmd.as_ref().unwrap().send(false).unwrap();
        self.ack
            .recv_timeout(Duration::from_secs(30))
            .expect("reader did not unpin");
    }
}

impl Drop for RemoteReader {
    fn drop(&mut self) {
        self.cmd.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
