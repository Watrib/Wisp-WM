//! One bounded, blocking I/O worker; the compositor only receives complete requests.
//! Slow clients can delay other IPC clients, never the input/render loop.
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use smithay::reexports::calloop::channel::{self, Channel};

use crate::protocol::{Action, MAX_REQUEST_BYTES, Request, Response};

pub enum Message {
    Request {
        request: Request,
        reply: SyncSender<Response>,
    },
    /// Sent only after the worker has attempted to write quit's acknowledgement.
    QuitAcknowledged,
}

pub struct IpcServer {
    path: PathBuf,
}

impl IpcServer {
    pub fn start(wayland_socket: &str) -> Result<(Self, Channel<Message>)> {
        let runtime = PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?,
        );
        ensure!(
            runtime.is_absolute() && runtime.is_dir(),
            "XDG_RUNTIME_DIR must be an existing absolute directory"
        );
        let path = runtime.join(format!("wisp-{wayland_socket}.sock"));
        // Remove a stale socket from a previous run so this instance can restart.
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding IPC socket {}", path.display()))?;
        let guard = Self { path };
        std::fs::set_permissions(&guard.path, std::fs::Permissions::from_mode(0o600))?;
        let (sender, channel) = channel::channel();
        std::thread::Builder::new()
            .name("wisp-ipc".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut quit = false;
                    let response = (|| -> Result<Response> {
                        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
                        stream.set_write_timeout(Some(Duration::from_millis(500)))?;
                        let request = read_request(&mut stream)?;
                        quit = matches!(
                            &request,
                            Request::Dispatch {
                                action: Action::Quit {}
                            }
                        );
                        let (reply, receiver) = mpsc::sync_channel(1);
                        sender
                            .send(Message::Request { request, reply })
                            .context("compositor stopped")?;
                        Ok(receiver.recv_timeout(Duration::from_secs(2))?)
                    })()
                    .unwrap_or_else(Response::failure);
                    let _ = write_response(&mut stream, &response);
                    // Begin graceful shutdown only after writing the response. Even
                    // an empty session must not exit before wispctl receives its reply.
                    if quit && response.ok {
                        let _ = sender.send(Message::QuitAcknowledged);
                        break;
                    }
                }
            })?;
        Ok((guard, channel))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "IPC read deadline exceeded");
        stream.set_read_timeout(Some(remaining))?;
        let mut buffer = [0; 1024];
        let count = stream.read(&mut buffer)?;
        ensure!(count != 0, "IPC requests must end with a newline");
        let end = buffer[..count]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| i + 1);
        bytes.extend_from_slice(&buffer[..end.unwrap_or(count)]);
        ensure!(
            bytes.len() as u64 <= MAX_REQUEST_BYTES,
            "IPC request exceeds {MAX_REQUEST_BYTES} bytes"
        );
        if end.is_some() {
            return serde_json::from_slice(&bytes).context("invalid IPC request");
        }
    }
}

fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

pub fn send(path: &Path, request: &Request) -> Result<Response> {
    let mut stream =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let bytes = serde_json::to_vec(request)?;
    ensure!(
        (bytes.len() as u64) < MAX_REQUEST_BYTES,
        "IPC request is too large"
    );
    stream.write_all(&bytes)?;
    stream.write_all(b"\n")?;
    let mut response = Vec::new();
    BufReader::new(stream)
        .take(4 * 1024 * 1024)
        .read_until(b'\n', &mut response)?;
    ensure!(response.last() == Some(&b'\n'), "incomplete IPC response");
    serde_json::from_slice(&response).context("invalid IPC response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;

    #[test]
    fn accepts_only_complete_bounded_messages() {
        for (input, valid) in [
            (b"{\"command\":\"status\"}\n".as_slice(), true),
            (b"{}", false),
            (b"not json\n", false),
        ] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            a.write_all(input).unwrap();
            a.shutdown(Shutdown::Write).unwrap();
            assert_eq!(read_request(&mut b).is_ok(), valid);
        }
        let (mut a, mut b) = UnixStream::pair().unwrap();
        a.write_all(&vec![b'x'; MAX_REQUEST_BYTES as usize + 1])
            .unwrap();
        assert!(read_request(&mut b).is_err());
    }
}
