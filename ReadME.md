# Secure Chat

A terminal-based end-to-end encrypted chat app built with Rust and ratatui. Messages are encrypted before they leave your machine. The relay server in the middle only ever sees ciphertext.

**Crypto stack:** Ed25519 identity keys, X25519 ephemeral key exchange, HKDF-SHA256 key derivation, ChaCha20-Poly1305 message encryption, symmetric ratchet for per-message forward secrecy.

---

## Requirements

- Rust (stable) — install from https://rustup.rs
- Python 3 — for the relay server (standard library only, no pip installs needed)

---

## Quick start

**1. Clone and build**

```bash
git clone https://github.com/aquinoonell/ratatui-
cd ratatui-
cargo build --release
```

**2. Start the relay server**

The relay can run on any machine reachable by both clients. It just routes encrypted messages and cannot read any content.

```bash
python3 relay_server.py
```

It listens on `0.0.0.0:5000` by default. Requires Python 3 only — no external dependencies (uses `socket`, `threading`, `datetime` from the standard library).

On startup you should see:

```
[HH:MM:SS] Relay server starting on 0.0.0.0:5000
[HH:MM:SS] NOTE: This relay routes encrypted traffic only.
[HH:MM:SS]       ENC payloads are ciphertext -- relay cannot read message content.
[HH:MM:SS] ------------------------------------------------------------
[HH:MM:SS] Listening for connections...
```

If port 5000 is already in use (e.g. restarting after a crash), free it first:

```bash
fuser -k 5000/tcp; python3 relay_server.py
```

> `fuser` may not be installed by default on minimal Linux systems
> (Debian/Ubuntu: `apt install psmisc`). On macOS, use
> `lsof -ti:5000 | xargs kill -9` instead, since `fuser` isn't available there.

To stop the relay, either `Ctrl+C` the foreground process, or from another terminal:

```bash
fuser -k 5000/tcp
```

Or find and kill it manually:

```bash
ps aux | grep relay_server.py
kill <PID>
```

**3. Set the relay address**

Open `src/main.rs` and update this line near the top to match where your relay is running:

```rust
const RELAY_HOST: &str = "192.168.1.160";
```

Then rebuild:

```bash
cargo build --release
```

**4. Run the client**

```bash
cargo run
```

Or run the release binary directly:

```bash
./target/release/ratatui-
```

Navigate to the Chat tab from the main menu.

---

## Setting up a session

Open two terminals, both running the client pointed at the same relay.

Terminal 1:
```
REGISTER alice
```

Terminal 2:
```
REGISTER bob
```

Terminal 1:
```
SECURE bob
```

That is it. The app handles the full key exchange automatically. Once the title bar shows `chatting with: bob` just type and press Enter to send encrypted messages.

---

## Commands

| Command | What it does |
|---|---|
| `REGISTER name` | Load or generate your identity key and connect to the relay |
| `SECURE peer` | Run the handshake and set up an encrypted session |
| `MSG peer text` | Send an encrypted message (or just type when a session is active) |
| `LIST` | See who is online |
| `QUIT` | Disconnect |

---

## How it works

When you run `SECURE peer` the app generates a fresh X25519 keypair for that session, signs it with your Ed25519 identity key, and sends both to the other person. They verify the signature, send theirs back, and both sides derive the same session keys from the X25519 exchange. After that the ephemeral keys are destroyed. Every message then gets its own key via a one-way ratchet so past messages stay safe even if current state is compromised.

The relay sees who is talking to whom but never sees message content. Its logs look like:

```
[22:14:35] MSG  alice -> bob  |  type=ENC  |  ciphertext=aGVsbG8gdGhl...  [RELAY CANNOT READ]
```

---

## Key storage

Your identity keypair is saved at `~/.config/timetracker/keys/username.json` on first run and reloaded every time after. Peer keys you have seen before are stored at `~/.config/timetracker/known_users.json` using Trust On First Use. Any key change on reconnect triggers a warning and kills the handshake.

To start fresh just delete your key file and the app generates a new one.

---

## Project structure

```
src/
  main.rs       TUI, TCP client, chat state machine, all commands
  crypto.rs     Identity keys, handshake, session keys, ratchet
relay_server.py Python TCP relay with structured logging
Cargo.toml      Dependencies
```
