let port = null;
let nextId = 1;

const pendingHostRequests = new Map();

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
      const resolver = pendingHostRequests.get(msg.id);
      if (resolver) {
        pendingHostRequests.delete(msg.id);
        const payload = msg.payload_json ? JSON.parse(msg.payload_json) : null;
        resolver({ ...msg, payload });
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

  return new Promise((resolve, reject) => {
    pendingHostRequests.set(id, resolve);

    try {
      port.postMessage(frame);
    } catch (err) {
      pendingHostRequests.delete(id);
      reject(err);
      return;
    }

    setTimeout(() => {
      if (pendingHostRequests.has(id)) {
        pendingHostRequests.delete(id);
        reject(new Error("Native host timeout"));
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

chrome.action.onClicked.addListener(() => {
  connectNative();
  sendRequestToHost("host_info", { source: "extension_action_click" })
    .then((resp) => {
      console.log("Response from host:", resp);
    })
    .catch((err) => {
      console.error("Failed to call host:", err);
    });
});

connectNative();
setInterval(sendHeartbeatEvent, 15000);
