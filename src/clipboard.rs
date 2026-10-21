use std::sync::mpsc as std_mpsc;
use std::thread;

use tokio::sync::oneshot;

struct ClipboardRequest {
    text: String,
    reply: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone)]
pub struct ClipboardHandle {
    sender: std_mpsc::Sender<ClipboardRequest>,
}

impl ClipboardHandle {
    pub async fn copy(&self, text: String) -> Result<(), String> {
        let (reply, result) = oneshot::channel();

        if let Err(error) = self.sender.send(ClipboardRequest { text, reply }) {
            return Err(format!("clipboard worker unavailable: {error}"));
        }

        match result.await {
            Ok(result) => result,
            Err(error) => Err(format!("clipboard worker stopped: {error}")),
        }
    }
}

pub struct Clipboard {
    sender: std_mpsc::Sender<ClipboardRequest>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Clipboard {
    pub fn start() -> Self {
        let (sender, receiver) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("zooport-clipboard".to_owned())
            .spawn(move || clipboard_loop(receiver))
            .ok();

        Self { sender, thread }
    }

    pub fn handle(&self) -> ClipboardHandle {
        ClipboardHandle {
            sender: self.sender.clone(),
        }
    }

    pub fn shutdown(mut self) {
        drop(self.sender);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn clipboard_loop(requests: std_mpsc::Receiver<ClipboardRequest>) {
    let mut clipboard: Option<Result<arboard::Clipboard, String>> = None;

    while let Ok(request) = requests.recv() {
        let result = match clipboard.get_or_insert_with(|| {
            arboard::Clipboard::new().map_err(|error| format!("clipboard unavailable: {error}"))
        }) {
            Ok(clipboard) => clipboard
                .set_text(request.text.as_str())
                .map_err(|error| format!("clipboard write failed: {error}")),
            Err(error) => Err(error.clone()),
        };

        let _ = request.reply.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_a_closed_worker_channel() {
        let (sender, receiver) = std_mpsc::channel();
        drop(receiver);
        let handle = ClipboardHandle { sender };

        let error = handle
            .copy("hello".to_owned())
            .await
            .expect_err("copy should fail");

        assert!(error.contains("clipboard worker unavailable"));
    }
}
