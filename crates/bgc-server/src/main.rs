use std::pin::Pin;

use anyhow::Context;
use futures::Stream;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, transport::Server};

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
            loop {
                match inbound.message().await {
                    Ok(frame) => {
                        if let Err(e) = chrome_tx.send(frame.unwrap()) {
                            eprintln!("bridge: error forwarding frame to Chrome: {e:?}");
                            break;
                        }
                    }
                    Err(e) => {
                        eprintln!("bridge: client stream error: {e:?}");
                        break;
                    }
                }
            }
        });

        // Task: Chrome → host → client
        tokio::spawn(async move {
            loop {
                match chrome_from_rx.recv().await {
                    Ok(frame) => {
                        if tx_to_client.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("bridge: client lagged behind by {n} frames");
                    }
                    Err(e) => {
                        eprintln!("bridge: broadcast receive error for client: {e:?}");
                        break;
                    }
                }
            }
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
    // Frames host → Chrome
    let (chrome_tx, mut chrome_rx) = mpsc::unbounded_channel::<Frame>();
    // Frames Chrome → host (broadcast to all gRPC clients)
    let (chrome_from_tx, _) = broadcast::channel::<Frame>(1024);

    // Native messaging writer: host → Chrome
    tokio::spawn(async move {
        let mut stdout = io::stdout();
        while let Some(frame) = chrome_rx.recv().await {
            if let Err(e) = write_framed(&mut stdout, &frame).await {
                eprintln!("native host write error: {e:?}");
                break;
            }
        }
    });

    // Native messaging reader: Chrome → host
    {
        let chrome_from_tx = chrome_from_tx.clone();
        tokio::spawn(async move {
            let mut stdin = io::stdin();
            loop {
                match read_framed(&mut stdin).await {
                    Ok(Some(frame)) => {
                        if chrome_from_tx.send(frame).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        // EOF from Chrome
                        break;
                    }
                    Err(e) => {
                        eprintln!("native host read error: {e:?}");
                        break;
                    }
                }
            }
        });
    }

    // gRPC server
    let svc = BridgeService {
        chrome_tx,
        chrome_from_tx,
    };

    let addr = "[::1]:50051".parse().unwrap();

    Server::builder()
        .add_service(BridgeServer::new(svc))
        .serve(addr)
        .await
        .context("running gRPC bridge server")
}
