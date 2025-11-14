let port = null;
let nextId = 1;

const pendingHostRequests = new Map();

// Statistics tracking
const stats = {
  totalSent: 0,
  totalReceived: 0,
  totalErrors: 0,
  totalTimeouts: 0,

  report() {
    console.log("\n=== CHROME EXTENSION STRESS TEST STATISTICS ===");
    console.log(`Total requests sent: ${this.totalSent}`);
    console.log(`Total responses received: ${this.totalReceived}`);
    console.log(`Total errors: ${this.totalErrors}`);
    console.log(`Total timeouts: ${this.totalTimeouts}`);
    const successRate =
      this.totalSent > 0
        ? ((this.totalReceived / this.totalSent) * 100).toFixed(2)
        : 0;
    console.log(`Success rate: ${successRate}%`);
    console.log("===============================================\n");
  },
};

function connectNative() {
  if (port) return;

  port = chrome.runtime.connectNative("com.example.rust_native_demo");

  port.onMessage.addListener(onNativeMessage);
  port.onDisconnect.addListener(() => {
    console.warn("Native host disconnected");
    port = null;
  });
}

// Parse Frame from host and fan out
function onNativeMessage(msg) {
  // msg is a Frame:
  // { kind, id, action, event, payload_json, ok }

  switch (msg.kind) {
    case "response": {
      const pendingData = pendingHostRequests.get(msg.id);
      if (pendingData) {
        pendingHostRequests.delete(msg.id);
        const elapsed = Date.now() - pendingData.startTime;
        console.log(`Response received for id=${msg.id} (took ${elapsed}ms)`);
        stats.totalReceived++;

        const payload = msg.payload_json ? JSON.parse(msg.payload_json) : null;
        pendingData.resolve({ ...msg, payload });
      } else {
        console.warn("Unexpected response from host", msg);
      }
      break;
    }
    case "request": {
      handleHostRequest(msg);
      break;
    }
    case "event": {
      handleHostEvent(msg);
      break;
    }
    default:
      console.warn("Unknown message kind from host", msg);
  }
}

// Extension → host request using Frame
function sendRequestToHost(action, payload) {
  if (!port) connectNative();
  const id = nextId++;

  const frame = {
    kind: "request",
    id,
    action,
    event: "",
    payload_json: JSON.stringify(payload ?? null),
    ok: true,
  };

  stats.totalSent++;

  return new Promise((resolve, reject) => {
    const startTime = Date.now();
    pendingHostRequests.set(id, { resolve, reject, startTime });

    try {
      port.postMessage(frame);
    } catch (err) {
      pendingHostRequests.delete(id);
      stats.totalErrors++;
      reject(err);
      return;
    }

    setTimeout(() => {
      if (pendingHostRequests.has(id)) {
        pendingHostRequests.delete(id);
        stats.totalTimeouts++;
        reject(new Error(`Native host timeout for request id=${id}`));
      }
    }, 10000);
  });
}

// Host-initiated request → extension
function handleHostRequest(msg) {
  const { id, action, payload_json } = msg;
  const payload = payload_json ? JSON.parse(payload_json) : null;

  if (action === "get_active_tab") {
    chrome.tabs.query({ active: true, lastFocusedWindow: true }, (tabs) => {
      const tab = tabs[0] || null;

      const respPayload = tab
        ? { id: tab.id, title: tab.title, url: tab.url }
        : { id: null };

      const respFrame = {
        kind: "response",
        id,
        action: "",
        event: "",
        payload_json: JSON.stringify(respPayload),
        ok: true,
      };

      port.postMessage(respFrame);
    });
    return;
  }

  const respFrame = {
    kind: "response",
    id,
    action: "",
    event: "",
    payload_json: JSON.stringify({
      error: "unknown_action",
      action,
      received: payload,
    }),
    ok: false,
  };

  port.postMessage(respFrame);
}

// Unsolicited events from host
function handleHostEvent(msg) {
  const payload = msg.payload_json ? JSON.parse(msg.payload_json) : null;
  console.log("Event from host:", msg.event, payload);
}

// Extension → host unsolicited event Frame
function sendHeartbeatEvent() {
  if (!port) connectNative();
  const frame = {
    kind: "event",
    id: 0,
    action: "",
    event: "heartbeat",
    payload_json: JSON.stringify({ timestamp: Date.now() }),
    ok: true,
  };
  port.postMessage(frame);
}

// Stress test function
async function runStressTest() {
  console.log("\n=== STARTING CHROME EXTENSION STRESS TEST ===");
  console.log("Sending 50 concurrent requests in 5 batches of 10...\n");

  for (let batch = 0; batch < 5; batch++) {
    console.log(`--- Batch ${batch + 1} ---`);

    const promises = [];

    for (let i = 0; i < 10; i++) {
      const action =
        i % 3 === 0
          ? "get_active_tab"
          : i % 3 === 1
          ? "host_info"
          : "test_action";
      const payload = {
        batch,
        request: i,
        timestamp: Date.now(),
      };

      const promise = sendRequestToHost(action, payload)
        .then((resp) => {
          console.log(
            `✓ Batch ${batch} Request ${i}: Success (id=${resp.id}, action=${action})`
          );
        })
        .catch((err) => {
          console.error(`✗ Batch ${batch} Request ${i}: Error:`, err.message);
          stats.totalErrors++;
        });

      promises.push(promise);
    }

    await Promise.all(promises);
    console.log(`Batch ${batch + 1} completed\n`);

    // Brief pause between batches
    await new Promise((resolve) => setTimeout(resolve, 500));
  }

  console.log("=== STRESS TEST COMPLETED ===");
  stats.report();
}

chrome.action.onClicked.addListener(() => {
  connectNative();

  // Run the stress test
  runStressTest().then(() => {
    console.log("Stress test finished, continuing with normal operation...");
  });
});

// Automatically run stress test on startup after a brief delay
setTimeout(() => {
  console.log("Running automatic stress test on startup...");
  runStressTest().then(() => {
    console.log("Automatic stress test completed");
  });
}, 2000);

// Report stats periodically
setInterval(() => {
  if (stats.totalSent > 0) {
    stats.report();
  }
}, 10000);

connectNative();
setInterval(sendHeartbeatEvent, 15000);
