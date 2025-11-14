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

pub mod bridge {
    tonic::include_proto!("bridge");
}

use bridge::Frame;
use bridge::bridge_client::BridgeClient;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Frame>>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("bgc-client: Starting");
    println!("bgc-client: Connecting to gRPC server at http://[::1]:50051");

    let mut client = BridgeClient::connect(format!("http://[::1]:50051")).await?;
    println!("bgc-client: Connected to gRPC server");

    let (tx_out, rx_out) = mpsc::channel::<Frame>(32);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    let outbound = ReceiverStream::new(rx_out);
    println!("bgc-client: Opening bidirectional stream");
    let response = client
        .open(Request::new(outbound))
        .await
        .context("opening gRPC stream")
        .unwrap();
    let mut inbound = response.into_inner();
    println!("bgc-client: Bidirectional stream opened successfully");

    let pending_in = pending.clone();
    let tx_out_in = tx_out.clone();

    tokio::spawn(async move {
        println!("bgc-client: Started inbound frame handler task");
        while let Some(result) = inbound.next().await {
            match result {
                Ok(frame) => {
                    println!(
                        "bgc-client: Received frame: kind={} id={} action={} event={}",
                        frame.kind, frame.id, frame.action, frame.event
                    );
                    handle_incoming_frame(frame, &pending_in, &tx_out_in).await;
                }
                Err(e) => {
                    eprintln!("bgc-client: Error receiving frame: {e:?}");
                    break;
                }
            }
        }
        println!("bgc-client: Inbound frame handler task ended");
    });

    // Example: periodically request active tab via Chrome
    println!("bgc-client: Starting request loop (every 3 seconds)");
    loop {
        let payload = Value::Null;
        println!("bgc-client: Sending get_active_tab request");
        match send_request("get_active_tab", payload, &tx_out, &pending).await {
            Ok(resp_frame) => {
                println!(
                    "bgc-client: Got response id={} ok={} payload_json={}",
                    resp_frame.id, resp_frame.ok, resp_frame.payload_json
                );
            }
            Err(e) => {
                eprintln!("bgc-client: Request error: {e:?}");
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
            println!("bgc-client: Processing response frame id={}", frame.id);
            if let Some(tx) = pending.lock().unwrap().remove(&frame.id) {
                println!(
                    "bgc-client: Matched response to pending request id={}",
                    frame.id
                );
                let _ = tx.send(frame);
            } else {
                eprintln!(
                    "bgc-client: Unexpected response id={} payload_json={}",
                    frame.id, frame.payload_json
                );
            }
        }
        "event" => {
            println!(
                "bgc-client: Unsolicited event from Chrome: event={} payload_json={}",
                frame.event, frame.payload_json
            );
        }
        "request" => {
            println!(
                "bgc-client: Request from Chrome: id={} action={} payload_json={}",
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

            println!(
                "bgc-client: Sending response to Chrome for request id={}",
                frame.id
            );
            if let Err(e) = tx_out.send(resp).await {
                eprintln!("bgc-client: Failed sending response: {e:?}");
            }
        }
        other => {
            eprintln!("bgc-client: Unknown frame kind={other}");
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
    println!("bgc-client: Created request id={} action={}", id, action);

    let frame = Frame {
        kind: "request".to_string(),
        id,
        action: action.to_string(),
        event: String::new(),
        payload_json: payload.to_string(),
        ok: true,
    };

    println!("bgc-client: Sending request frame to outbound channel");
    tx_out
        .send(frame)
        .await
        .context("sending request frame failed")?;

    println!("bgc-client: Waiting for response to request id={}", id);
    let resp = rx.await.context("oneshot receive failed")?;
    println!("bgc-client: Received response for request id={}", id);
    Ok(resp)
}
