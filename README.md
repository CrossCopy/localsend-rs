# LocalSend Rust

A high-performance, cross-platform implementation of the [LocalSend](https://localsend.org) protocol (v2) in Rust. This project provides both a library and a feature-rich CLI for secure, local network file and text transfers.

## Features

- **Protocol Compatibility**: Full interoperability with official LocalSend clients (Android, iOS, Windows, macOS, Linux).
- **Automatic Discovery**: Multicast UDP discovery to find devices on your network instantly.
- **Direct Send**: Transfer files or text directly to an IP address for speed and reliability.
- **HTTPS Security**: TLS encryption for all transfers using protocol-compliant certificate fingerprinting.
- **Text Messages**: Support for sending and receiving instant text messages.
- **CLI Suite**: Intuitive commands for discovering, sending, and receiving.

## Installation

Ensure you have Rust and Cargo installed. Clone the repository and build from source:

```bash
git clone https://github.com/LocalSend/localsend-rs.git
cd localsend-rs
cargo build --release --features https
```

## Quick Start

### 1. Discover Devices

Scan the local network for available LocalSend instances:

```bash
cargo run --features https -- discover
```

### 2. Receive Files

Start the receiver server (HTTPS recommended for compatibility):

```bash
# Start receiving on the default port (53317)
cargo run --features https -- receive --https
```

### 3. Send Files

Send a file to a device by its alias or IP:

```bash
# Send by alias
cargo run --features https -- send "My Phone" ./photos/vacation.jpg

# Send by IP address (bypasses discovery)
cargo run --features https -- send 192.168.1.50 ./documents/report.pdf
```

### 4. Send Text Messages

You can send plain text instead of files by providing a string that isn't a file path:

```bash
cargo run --features https -- send "ROG16" "Hello from the Rust CLI!"
```

## CLI Usage

### `discover`

Find devices on the local network.

- `--timeout <SECS>`: Search duration (default: 10s).
- `--json`: Output discovered devices in JSON format.

### `receive`

Start a LocalSend server to accept incoming transfers.

- `--port <PORT>`: Custom port (default: 53317).
- `--https`: Enable TLS encryption (highly recommended).
- `--alias <NAME>`: Custom device name shown to others.

### `send`

Send data to another device.

- `<TARGET>`: Device alias, hostname, or IP address.
- `<FILES...>`: One or more file paths or text strings.
- `--pin <PIN>`: Optional PIN for protected transfers.

## Architecture

- `src/discovery`: Multicast UDP and HTTP-based discovery logic.
- `src/server`: Axum-based HTTP/HTTPS server for handling LocalSend API v2.
- `src/client`: Request-based client for initiating transfers.
- `src/protocol`: Core type definitions and protocol constants.
- `src/crypto`: Certificate generation and fingerprinting.

## Development Status

This project is under active development.

- [x] Discovery (Multicast)
- [x] Receiving (HTTPS/HTTP)
- [x] Sending Files
- [x] Sending Text
- [ ] Direct Download (v3 feature)
- [ ] User Transfer Approval UI (CLI currently auto-accepts metadata)

## License

MIT License - see [LICENSE](LICENSE) for details.
