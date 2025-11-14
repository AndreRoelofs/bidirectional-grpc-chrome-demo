use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Context;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;
use tonic::transport::Channel;

pub mod bridge {
    tonic::include_proto!("bridge");
}

use bridge::Frame;
use bridge::bridge_client::BridgeClient;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Frame>>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let channel = Channel::from_static("http://127.0.0.1:50051")
        .connect()
        .await
        .unwrap();
    let mut client = BridgeClient::new(channel);

    let (tx_out, rx_out) = mpsc::channel::<Frame>(32);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    let outbound = ReceiverStream::new(rx_out);
    let response = client
        .open(Request::new(outbound))
        .await
        .context("opening gRPC stream")
        .unwrap();
    let mut inbound = response.into_inner();

    let pending_in = pending.clone();
    let tx_out_in = tx_out.clone();

    tokio::spawn(async move {
        while let Some(result) = inbound.next().await {
            match result {
                Ok(frame) => {
                    handle_incoming_frame(frame, &pending_in, &tx_out_in).await;
                }
                Err(e) => {
                    eprintln!("client: error receiving frame: {e:?}");
                    break;
                }
            }
        }
    });

    // Example: periodically request active tab via Chrome
    loop {
        let payload = Value::Null;
        match send_request("get_active_tab", payload, &tx_out, &pending).await {
            Ok(resp_frame) => {
                println!(
                    "client: got response id={} ok={} payload_json={}",
                    resp_frame.id, resp_frame.ok, resp_frame.payload_json
                );
            }
            Err(e) => {
                eprintln!("client: request error: {e:?}");
                break;
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    Ok(())
}

async fn handle_incoming_frame(frame: Frame, pending: &PendingMap, tx_out: &mpsc::Sender<Frame>) {
    match frame.kind.as_str() {
        "response" => {
            if let Some(tx) = pending.lock().unwrap().remove(&frame.id) {
                let _ = tx.send(frame);
            } else {
                eprintln!(
                    "client: unexpected response id={} payload_json={}",
                    frame.id, frame.payload_json
                );
            }
        }
        "event" => {
            println!(
                "client: unsolicited event from Chrome: event={} payload_json={}",
                frame.event, frame.payload_json
            );
        }
        "request" => {
            println!(
                "client: request from Chrome: id={} action={} payload_json={}",
                frame.id, frame.action, frame.payload_json
            );

            let response_payload = serde_json::json!({
                "from": "rust-native-client",
                "echo": frame.payload_json,
            });

            let resp = Frame {
                kind: "response".to_string(),
                id: frame.id,
                action: String::new(),
                event: String::new(),
                payload_json: response_payload.to_string(),
                ok: true,
            };

            if let Err(e) = tx_out.send(resp).await {
                eprintln!("client: failed sending response: {e:?}");
            }
        }
        other => {
            eprintln!("client: unknown frame kind={other}");
        }
    }
}

async fn send_request(
    action: &str,
    payload: Value,
    tx_out: &mpsc::Sender<Frame>,
    pending: &PendingMap,
) -> anyhow::Result<Frame> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();

    pending.lock().unwrap().insert(id, tx);

    let frame = Frame {
        kind: "request".to_string(),
        id,
        action: action.to_string(),
        event: String::new(),
        payload_json: payload.to_string(),
        ok: true,
    };

    tx_out
        .send(frame)
        .await
        .context("sending request frame failed")?;

    let resp = rx.await.context("oneshot receive failed")?;
    Ok(resp)
}
