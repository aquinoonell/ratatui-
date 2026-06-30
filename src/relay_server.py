#!/usr/bin/env python3
import socket
import threading
from datetime import datetime

HOST = "0.0.0.0"
PORT = 5000
clients = {}
clients_lock = threading.Lock()

def ts():
    return datetime.now().strftime("%H:%M:%S")

def log(msg: str):
    print(f"[{ts()}] {msg}", flush=True)

def broadcast_info(msg: str):
    with clients_lock:
        for sock in clients.values():
            try:
                sock.sendall(f"INFO {msg}\n".encode("utf-8"))
            except Exception:
                pass

def handle_client(conn: socket.socket, addr):
    username = None
    try:
        conn_file = conn.makefile("r", encoding="utf-8")
        conn.sendall(b"INFO Welcome to the relay server. Please register:\n")
        conn.sendall(b"INFO Use: REGISTER <username>\n")
        line = conn_file.readline()
        if not line:
            return
        line = line.strip()
        parts = line.split(maxsplit=1)
        if len(parts) != 2 or parts[0].upper() != "REGISTER":
            conn.sendall(b"ERROR First command must be: REGISTER <username>\n")
            return
        requested_name = parts[1].strip()
        if not requested_name:
            conn.sendall(b"ERROR Username cannot be empty.\n")
            return
        with clients_lock:
            if requested_name in clients:
                conn.sendall(b"ERROR Username already in use.\n")
                return
            clients[requested_name] = conn
            username = requested_name
            online = sorted(clients.keys())
        conn.sendall(f"INFO Registered as {username}\n".encode("utf-8"))
        log(f"REGISTER  {username} ({addr[0]})  |  online: {', '.join(online)}")
        broadcast_info(f"{username} has joined the chat.")
        for line in conn_file:
            line = line.strip()
            if not line:
                continue
            parts = line.split(maxsplit=2)
            cmd = parts[0].upper()
            if cmd == "LIST":
                with clients_lock:
                    names = " ".join(sorted(clients.keys()))
                conn.sendall(f"USERLIST {names}\n".encode("utf-8"))
                log(f"LIST      {username} requested user list")
            elif cmd == "MSG":
                if len(parts) < 3:
                    conn.sendall(b"ERROR Usage: MSG <recipient> <message>\n")
                    continue
                recipient = parts[1]
                payload = parts[2]
                payload_parts = payload.split(maxsplit=1)
                msg_type = payload_parts[0] if payload_parts else "?"
                content = payload_parts[1] if len(payload_parts) > 1 else ""
                if msg_type == "ENC":
                    preview = content[:32] + "..." if len(content) > 32 else content
                    log(f"MSG       {username} -> {recipient}  |  type=ENC  |  ciphertext={preview}  [RELAY CANNOT READ]")
                elif msg_type == "HELLO":
                    log(f"MSG       {username} -> {recipient}  |  type=HELLO (key exchange)")
                elif msg_type == "PUBKEY":
                    log(f"MSG       {username} -> {recipient}  |  type=PUBKEY (identity key)")
                else:
                    log(f"MSG       {username} -> {recipient}  |  type={msg_type}")
                with clients_lock:
                    target_sock = clients.get(recipient)
                if target_sock is None:
                    conn.sendall(f"ERROR No such user: {recipient}\n".encode("utf-8"))
                    log(f"ERROR     {username} -> {recipient}: user not found")
                    continue
                try:
                    target_sock.sendall(f"FROM {username} {payload}\n".encode("utf-8"))
                    conn.sendall(b"INFO Message sent.\n")
                except Exception:
                    conn.sendall(b"ERROR Failed to deliver message.\n")
            elif cmd == "QUIT":
                conn.sendall(b"INFO Goodbye.\n")
                break
            else:
                conn.sendall(b"ERROR Unknown command.\n")
    except Exception as e:
        log(f"ERROR     client {addr}: {e}")
    finally:
        if username is not None:
            with clients_lock:
                if clients.get(username) is conn:
                    del clients[username]
                online = sorted(clients.keys())
            log(f"DISCONNECT {username}  |  online: {', '.join(online) if online else 'none'}")
            broadcast_info(f"{username} has left the chat.")
        try:
            conn.close()
        except Exception:
            pass

def main():
    log(f"Relay server starting on {HOST}:{PORT}")
    log(f"NOTE: This relay routes encrypted traffic only.")
    log(f"      ENC payloads are ciphertext -- relay cannot read message content.")
    log("-" * 60)
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind((HOST, PORT))
        s.listen()
        log("Listening for connections...")
        while True:
            conn, addr = s.accept()
            log(f"CONNECT   {addr[0]}:{addr[1]}")
            t = threading.Thread(target=handle_client, args=(conn, addr), daemon=True)
            t.start()

if __name__ == "__main__":
    main()
