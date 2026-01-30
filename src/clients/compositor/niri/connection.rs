/// Taken from the `niri_ipc` crate.
/// Only a relevant snippet has been extracted
/// to reduce compile times.
use crate::await_sync;
use std::env;
use std::io::Result;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// Re-export types from the crate
pub use niri_ipc::{Action, Event, Reply, Request, Response, Window, Workspace};

#[derive(Debug)]
pub struct Connection(UnixStream);

impl Connection {
    pub async fn connect() -> Result<Self> {
        let socket_path = env::var_os("NIRI_SOCKET").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "NIRI_SOCKET not found")
        })?;
        Self::connect_to(socket_path).await
    }

    pub async fn connect_to(path: impl AsRef<Path>) -> Result<Self> {
        let raw_stream = UnixStream::connect(path.as_ref()).await?;
        Ok(Self(raw_stream))
    }

    pub async fn send(
        &mut self,
        request: Request,
    ) -> Result<(Reply, impl FnMut() -> Result<Event> + '_)> {
        let Self(stream) = self;
        let mut buf = serde_json::to_string(&request)?;

        stream.write_all(buf.as_bytes()).await?;
        stream.shutdown().await?;

        buf.clear();
        let mut reader = BufReader::new(stream);
        reader.read_line(&mut buf).await?;
        let reply = serde_json::from_str(&buf)?;

        let events = move || {
            buf.clear();
            await_sync(async {
                reader.read_line(&mut buf).await.unwrap_or(0);
            });
            // Handle potential empty lines or connection closes gracefully
            if buf.trim().is_empty() {
                return Ok(Event::Other); // Treat as no-op
            }
            let event: Event = serde_json::from_str(&buf).unwrap_or(Event::Other);
            Ok(event)
        };
        Ok((reply, events))
    }
}
}
