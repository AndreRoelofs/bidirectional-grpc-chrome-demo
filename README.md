# bidirectional-grpc-chrome-demo

A demonstration of bidirectional, multiplexed communication between a Chrome extension and a Rust native host using gRPC over Chrome's Native Messaging API.

## Architecture

- **Chrome Extension** (`extension/`): Browser-side UI and message handling
- **bgc-server** (`crates/bgc-server/`): Native host handling stdio communication with Chrome
- **bgc-client** (`crates/bgc-client/`): gRPC client for testing
- **Protocol** (`proto/bridge.proto`): gRPC service definition with bidirectional streaming

## Communication Flow

```
Chrome Extension <--[Native Messaging]--> bgc-server <--[gRPC]--> Internal Services
```

The [`bridge.proto`](proto/bridge.proto) defines [`Frame`](proto/bridge.proto:9) messages supporting:

- **Request/Response**: Correlated by ID for async operations
- **Events**: Server-to-client push notifications
- **JSON Payloads**: Flexible data serialization

## Setup

### Build the Native Host

```bash
cargo build --release
```

### Configure Chrome Native Messaging

1. Edit [`com.example.rust_native_demo.json`](com.example.rust_native_demo.json):

   - Update `path` to your `bgc-server` binary location
   - Replace `YOUR_EXTENSION_ID` with actual extension ID

2. Install manifest (Linux):
   ```bash
   mkdir -p ~/.config/google-chrome/NativeMessagingHosts
   cp com.example.rust_native_demo.json ~/.config/google-chrome/NativeMessagingHosts/
   ```

### Load Chrome Extension

1. Open `chrome://extensions/`
2. Enable "Developer mode"
3. Click "Load unpacked" and select the `extension/` directory

## Usage

The extension popup allows sending test messages to the native host. The native host processes requests via the gRPC bridge and streams responses back to Chrome.

## License

See [`LICENSE`](LICENSE)
