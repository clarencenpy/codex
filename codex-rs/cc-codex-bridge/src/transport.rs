//! Transports: newline-delimited JSON on stdio, and WebSocket (one JSON
//! message per text frame) over TCP or a Unix socket, which is how a Codex
//! client reaches a remote app-server.

use crate::server::Bridge;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::Value;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    Stdio,
    Tcp { host: String, port: u16 },
    Unix(PathBuf),
}

/// Parse `stdio://`, `ws://host:port` or `unix:///path/to.sock`.
pub fn parse_listen(spec: &str) -> anyhow::Result<Listen> {
    if spec == "stdio" || spec == "stdio://" {
        return Ok(Listen::Stdio);
    }
    if let Some(path) = spec.strip_prefix("unix://") {
        anyhow::ensure!(!path.is_empty(), "unix:// needs a socket path");
        return Ok(Listen::Unix(PathBuf::from(path)));
    }
    let Some(address) = spec.strip_prefix("ws://") else {
        anyhow::bail!(
            "unsupported listen address {spec}: use stdio://, ws://HOST:PORT or unix:///PATH"
        );
    };
    let address = address.trim_end_matches('/');
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("ws:// needs HOST:PORT"))?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    // Single user, single machine: never listen beyond this host.
    let loopback = host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    anyhow::ensure!(loopback, "refusing to listen on non-loopback host {host}");
    Ok(Listen::Tcp {
        host,
        port: port.parse()?,
    })
}

/// Serve one client on a pair of byte streams, as newline-delimited JSON.
/// Returns when the input ends.
pub async fn serve_lines<R, W>(bridge: Arc<Bridge>, input: R, mut output: W)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (outgoing, mut receiver) = mpsc::unbounded_channel::<Value>();
    let connection = bridge.connect(outgoing);
    let writer = tokio::spawn(async move {
        while let Some(message) = receiver.recv().await {
            let mut line = message.to_string();
            line.push('\n');
            if output.write_all(line.as_bytes()).await.is_err() || output.flush().await.is_err() {
                break;
            }
        }
    });
    let mut lines = BufReader::new(input).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Like Codex's own server, input that is not JSON is logged and
        // dropped: there is no error shape without a request id.
        match serde_json::from_str::<Value>(line) {
            Ok(message) => bridge.handle_message(connection, message).await,
            Err(err) => tracing::warn!("dropping unparseable input: {err}"),
        }
    }
    bridge.disconnect(connection);
    // Dropping the bridge's sender ends the writer once it has flushed.
    let _ = writer.await;
}

async fn serve_websocket<S>(bridge: Arc<Bridge>, stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = socket.split();
    let (outgoing, mut receiver) = mpsc::unbounded_channel::<Value>();
    let connection = bridge.connect(outgoing);
    loop {
        tokio::select! {
            message = receiver.recv() => {
                let Some(message) = message else { break };
                if sink.send(Message::Text(message.to_string().into())).await.is_err() {
                    break;
                }
            }
            frame = source.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Value>(text.as_str()) {
                        Ok(message) => bridge.handle_message(connection, message).await,
                        Err(err) => tracing::warn!("dropping unparseable frame: {err}"),
                    },
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    bridge.disconnect(connection);
}

/// Accept WebSocket clients until the listener fails.
pub async fn listen(bridge: Arc<Bridge>, listen: Listen) -> anyhow::Result<()> {
    match listen {
        Listen::Stdio => {
            serve_lines(bridge, tokio::io::stdin(), tokio::io::stdout()).await;
            Ok(())
        }
        Listen::Tcp { host, port } => {
            let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
            tracing::info!("listening on ws://{}", listener.local_addr()?);
            loop {
                let (stream, _) = listener.accept().await?;
                tokio::spawn(serve_websocket(bridge.clone(), stream));
            }
        }
        Listen::Unix(path) => {
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)?;
            tracing::info!("listening on unix://{}", path.display());
            loop {
                let (stream, _) = listener.accept().await?;
                tokio::spawn(serve_websocket(bridge.clone(), stream));
            }
        }
    }
}
