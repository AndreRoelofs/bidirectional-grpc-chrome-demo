use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

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

type PendingMap = Arc<Mutex<HashMap<u64, (oneshot::Sender<Frame>, Instant)>>>;

#[derive(Default)]
struct Stats {
    total_sent: AtomicU64,
    total_received: AtomicU64,
    total_errors: AtomicU64,
    total_timeouts: AtomicU64,
}

impl Stats {
    fn report(&self) {
        let sent = self.total_sent.load(Ordering::Relaxed);
        let received = self.total_received.load(Ordering::Relaxed);
        let errors = self.total_errors.load(Ordering::Relaxed);
        let timeouts = self.total_timeouts.load(Ordering::Relaxed);
        println!("\n=== STRESS TEST STATISTICS ===");
        println!("Total requests sent: {}", sent);
        println!("Total responses received: {}", received);
        println!("Total errors: {}", errors);
        println!("Total timeouts: {}", timeouts);
        println!(
            "Success rate: {:.2}%",
            if sent > 0 {
                (received as f64 / sent as f64) * 100.0
            } else {
                0.0
            }
        );
        println!("==============================\n");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("bgc-client: Starting");
    println!("bgc-client: Connecting to gRPC server at http://[::1]:50051");

    let mut client = BridgeClient::connect(format!("http://[::1]:50051")).await?;
    println!("bgc-client: Connected to gRPC server");

    let (tx_out, rx_out) = mpsc::channel::<Frame>(256);
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
    let stats = Arc::new(Stats::default());

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

    let stats_in = stats.clone();
    tokio::spawn(async move {
        println!("bgc-client: Started inbound frame handler task");
        while let Some(result) = inbound.next().await {
            match result {
                Ok(frame) => {
                    println!(
                        "bgc-client: Received frame: kind={} id={} action={} event={}",
                        frame.kind, frame.id, frame.action, frame.event
                    );
                    handle_incoming_frame(frame, &pending_in, &tx_out_in, &stats_in).await;
                }
                Err(e) => {
                    eprintln!("bgc-client: Error receiving frame: {e:?}");
                    break;
                }
            }
        }
        println!("bgc-client: Inbound frame handler task ended");
    });

    // Stress test: send concurrent requests
    println!("\n=== STARTING STRESS TEST ===");
    println!("Sending 100 concurrent requests in 5 batches of 20...\n");

    let stats_main = stats.clone();

    // Spawn a task to periodically report stats
    let stats_reporter = stats.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            stats_reporter.report();
        }
    });

    // Send requests in batches to test concurrent handling
    for batch in 0..5 {
        println!("--- Batch {} ---", batch + 1);

        let mut handles = vec![];

        for i in 0..20 {
            let tx_out_clone = tx_out.clone();
            let pending_clone = pending.clone();
            let stats_clone = stats_main.clone();

            let handle = tokio::spawn(async move {
                let action = if i % 3 == 0 {
                    "get_active_tab"
                } else if i % 3 == 1 {
                    "host_info"
                } else {
                    "test_action"
                };

                let payload = serde_json::json!({
                    "batch": batch,
                    "request": i,
                    "timestamp": std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis()
                });

                stats_clone.total_sent.fetch_add(1, Ordering::Relaxed);

                match send_request(action, payload, &tx_out_clone, &pending_clone, &stats_clone)
                    .await
                {
                    Ok(resp_frame) => {
                        println!(
                            "✓ Batch {} Request {}: Got response id={} ok={} (action={})",
                            batch, i, resp_frame.id, resp_frame.ok, action
                        );
                    }
                    Err(e) => {
                        eprintln!("✗ Batch {} Request {}: Error: {e:?}", batch, i);
                        stats_clone.total_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all requests in this batch to complete
        for handle in handles {
            let _ = handle.await;
        }

        println!("Batch {} completed\n", batch + 1);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    println!("=== STRESS TEST COMPLETED ===\n");
    stats_main.report();

    // Continue with periodic requests to keep connection alive
    println!("Continuing with periodic requests every 5 seconds...");
    loop {
        let payload = Value::Null;
        stats_main.total_sent.fetch_add(1, Ordering::Relaxed);

        match send_request("get_active_tab", payload, &tx_out, &pending, &stats_main).await {
            Ok(resp_frame) => {
                println!(
                    "Periodic request: Got response id={} ok={} payload_json={}",
                    resp_frame.id, resp_frame.ok, resp_frame.payload_json
                );
            }
            Err(e) => {
                eprintln!("Periodic request error: {e:?}");
                stats_main.total_errors.fetch_add(1, Ordering::Relaxed);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn handle_incoming_frame(
    frame: Frame,
    pending: &PendingMap,
    tx_out: &mpsc::Sender<Frame>,
    stats: &Arc<Stats>,
) {
    match frame.kind.as_str() {
        "response" => {
            println!("bgc-client: Processing response frame id={}", frame.id);
            if let Some((tx, start_time)) = pending.lock().unwrap().remove(&frame.id) {
                let elapsed = start_time.elapsed();
                println!(
                    "bgc-client: Matched response to pending request id={} (took {}ms)",
                    frame.id,
                    elapsed.as_millis()
                );
                stats.total_received.fetch_add(1, Ordering::Relaxed);
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
    stats: &Arc<Stats>,
) -> anyhow::Result<Frame> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    let start_time = Instant::now();

    pending.lock().unwrap().insert(id, (tx, start_time));
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

    // Set a timeout for the response
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(resp)) => {
            println!("bgc-client: Received response for request id={}", id);
            Ok(resp)
        }
        Ok(Err(_)) => {
            pending.lock().unwrap().remove(&id);
            stats.total_timeouts.fetch_add(1, Ordering::Relaxed);
            Err(anyhow::anyhow!(
                "Response channel closed for request id={}",
                id
            ))
        }
        Err(_) => {
            pending.lock().unwrap().remove(&id);
            stats.total_timeouts.fetch_add(1, Ordering::Relaxed);
            Err(anyhow::anyhow!(
                "Timeout waiting for response to request id={}",
                id
            ))
        }
    }
}
