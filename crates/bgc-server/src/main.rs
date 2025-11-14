use std::net::ToSocketAddrs;
use std::pin::Pin;

use anyhow::Context;
use futures::Stream;
use std::fs::File;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};

use tracing::info;
#[allow(unused_imports)]
use tracing_subscriber::prelude::*;
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt,
};

mod bridge;

use bridge::{
    Frame,
    bridge_server::{Bridge, BridgeServer},
};

// gRPC service: bridge between gRPC stream and Chrome native messaging
#[derive(Clone)]
struct BridgeService {
    // Frames going to Chrome (native writer)
    chrome_tx: mpsc::UnboundedSender<Frame>,
    // Frames coming from Chrome (broadcast to all gRPC clients)
    chrome_from_tx: broadcast::Sender<Frame>,
}

#[tonic::async_trait]
impl Bridge for BridgeService {
    type OpenStream = Pin<Box<dyn Stream<Item = Result<Frame, Status>> + Send + 'static>>;

    async fn open(
        &self,
        request: Request<tonic::Streaming<Frame>>,
    ) -> Result<Response<Self::OpenStream>, Status> {
        let mut inbound = request.into_inner();

        // Client-specific outbound stream
        let (tx_to_client, rx_to_client) = mpsc::channel::<Result<Frame, Status>>(32);

        // Subscribe to Chrome → host broadcast
        let mut chrome_from_rx = self.chrome_from_tx.subscribe();
        let chrome_tx = self.chrome_tx.clone();

        // Task: client → host → Chrome
        tokio::spawn(async move {
            info!("bgc-server: gRPC client connected, starting forward task (client → Chrome)");
            loop {
                match inbound.message().await {
                    Ok(Some(frame)) => {
                        info!(
                            "bgc-server: Forwarding frame from gRPC client to Chrome: kind={} id={} action={}",
                            frame.kind, frame.id, frame.action
                        );
                        if let Err(e) = chrome_tx.send(frame) {
                            info!("bgc-server: Error forwarding frame to Chrome: {e:?}");
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("bgc-server: gRPC client stream ended");
                        break;
                    }
                    Err(e) => {
                        info!("bgc-server: gRPC client stream error: {e:?}");
                        break;
                    }
                }
            }
            info!("bgc-server: Forward task ended (client → Chrome)");
        });

        // Task: Chrome → host → client
        tokio::spawn(async move {
            info!("bgc-server: Starting broadcast task (Chrome → gRPC client)");
            loop {
                match chrome_from_rx.recv().await {
                    Ok(frame) => {
                        info!(
                            "bgc-server: Broadcasting frame from Chrome to gRPC client: kind={} id={} action={}",
                            frame.kind, frame.id, frame.action
                        );
                        if tx_to_client.send(Ok(frame)).await.is_err() {
                            info!("bgc-server: gRPC client disconnected");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        info!("bgc-server: gRPC client lagged behind by {n} frames");
                    }
                    Err(e) => {
                        info!("bgc-server: Broadcast receive error for client: {e:?}");
                        break;
                    }
                }
            }
            info!("bgc-server: Broadcast task ended (Chrome → gRPC client)");
        });

        let out_stream = ReceiverStream::new(rx_to_client);
        Ok(Response::new(Box::pin(out_stream) as Self::OpenStream))
    }
}

// Native messaging framing helpers: 4-byte LE length + JSON (Frame)

async fn read_framed<R>(reader: &mut R) -> anyhow::Result<Option<Frame>>
where
    R: AsyncReadExt + Unpin,
{
    let mut len_buf = [0u8; 4];

    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(None);
        }
        Err(e) => return Err(e).context("reading message length"),
    }

    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .await
        .context("reading message body")?;

    let frame: Frame = serde_json::from_slice(&buf).context("parsing Frame from JSON")?;

    Ok(Some(frame))
}

async fn write_framed<W>(writer: &mut W, frame: &Frame) -> anyhow::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let json = serde_json::to_vec(frame).context("serializing Frame to JSON")?;
    let len = json.len() as u32;

    writer
        .write_all(&len.to_le_bytes())
        .await
        .context("writing length")?;
    writer.write_all(&json).await.context("writing body")?;
    writer.flush().await.context("flushing stdout")?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into()) // anything not listed → WARN
        .parse_lossy("bgc_=trace,hyper=off,tokio=off"); // keep yours, silence deps

    // Write only to file
    fmt()
        .with_env_filter(filter.clone())
        .with_writer(File::create("bgc-native-messaging.log")?)
        .init();

    info!("bgc-server: Starting native messaging host");
    info!("bgc-server: Waiting for Chrome connection on stdin/stdout");
    info!("bgc-server: gRPC server will listen on [::1]:50051");

    // Frames host → Chrome
    let (chrome_tx, mut chrome_rx) = mpsc::unbounded_channel::<Frame>();
    // Frames Chrome → host (broadcast to all gRPC clients)
    let (chrome_from_tx, _) = broadcast::channel::<Frame>(1024);

    // Shutdown signal: when stdin closes, we need to exit
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Native messaging writer: host → Chrome
    let writer_handle = tokio::spawn(async move {
        let mut stdout = io::stdout();
        info!("bgc-server: Native messaging writer task started");
        while let Some(frame) = chrome_rx.recv().await {
            info!(
                "bgc-server: Writing frame to Chrome: kind={} id={} action={}",
                frame.kind, frame.id, frame.action
            );
            if let Err(e) = write_framed(&mut stdout, &frame).await {
                info!("bgc-server: Native host write error: {e:?}");
                break;
            }
        }
        info!("bgc-server: Native messaging writer task ended");
    });

    // Native messaging reader: Chrome → host
    let reader_handle = {
        let chrome_from_tx = chrome_from_tx.clone();
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            let mut stdin = io::stdin();
            info!(
                "bgc-server: Native messaging reader task started, waiting for input from Chrome"
            );
            loop {
                match read_framed(&mut stdin).await {
                    Ok(Some(frame)) => {
                        info!(
                            "bgc-server: Received frame from Chrome: kind={} id={} action={} event={}",
                            frame.kind, frame.id, frame.action, frame.event
                        );
                        if chrome_from_tx.send(frame).is_err() {
                            info!("bgc-server: Failed to broadcast frame (no receivers)");
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("bgc-server: EOF from Chrome, connection closed");
                        break;
                    }
                    Err(e) => {
                        info!("bgc-server: Native host read error: {e:?}");
                        break;
                    }
                }
            }
            info!("bgc-server: Native messaging reader task ended");
            // Signal shutdown when stdin closes
            let _ = shutdown_tx.send(()).await;
        })
    };

    // gRPC server
    let svc = BridgeService {
        chrome_tx,
        chrome_from_tx,
    };

    let grpc_handle = tokio::spawn(async move {
        info!("bgc-server: Starting gRPC server on [::1]:50051");
        let addr = format!("[::1]:50051")
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap();

        if let Err(e) = Server::builder()
            .add_service(BridgeServer::new(svc))
            .serve(addr)
            .await
        {
            info!("bgc-server: gRPC server error: {e:?}");
        }
        info!("bgc-server: gRPC server ended");
    });

    // Wait for shutdown signal (when Chrome disconnects)
    tokio::select! {
        _ = shutdown_rx.recv() => {
            info!("bgc-server: Received shutdown signal, exiting");
        }
        _ = reader_handle => {
            info!("bgc-server: Reader task ended, exiting");
        }
        _ = writer_handle => {
            info!("bgc-server: Writer task ended, exiting");
        }
    }

    info!("bgc-server: Shutting down");
    grpc_handle.abort();

    Ok(())
}
