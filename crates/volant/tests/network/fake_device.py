"""A fake network device for the network connection tests.

`fake_device.py ssh` serves a command line over SSH (password `fake-pw`), with a prompt, a
configuration mode and a running configuration; `fake_device.py http` serves RESTCONF over HTTP.
Each prints the port it listens on, on one line, then serves until it is killed. Nothing here
leaves the loopback interface.
"""

import json
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PASSWORD = "fake-pw"


def serve_ssh():
    import paramiko

    key = paramiko.RSAKey.generate(1024)
    running = ["hostname fake0"]

    class Server(paramiko.ServerInterface):
        def check_auth_password(self, username, password):
            if password == PASSWORD:
                return paramiko.AUTH_SUCCESSFUL
            return paramiko.AUTH_FAILED

        def get_allowed_auths(self, username):
            return "password"

        def check_channel_request(self, kind, chanid):
            if kind == "session":
                return paramiko.OPEN_SUCCEEDED
            return paramiko.OPEN_FAILED_ADMINISTRATIVELY_PROHIBITED

        def check_channel_pty_request(self, *args):
            return True

        def check_channel_shell_request(self, channel):
            return True

    def answer(line, config):
        if config:
            if line == "end":
                return "", False
            if line not in running:
                running.append(line)
            return "", True
        if line == "configure terminal":
            return "", True
        if line in ("", "terminal length 0", "terminal width 512"):
            return "", False
        if line == "show version":
            return "Fake OS 1.0\r\n", False
        if line == "show running-config":
            return "\r\n".join(running) + "\r\n", False
        return "% Error: unknown command '%s'\r\n" % line, False

    def shell(channel):
        config = False
        channel.send(b"\r\nfake# ")
        pending = b""
        while True:
            data = channel.recv(4096)
            if not data:
                return
            pending += data
            while b"\r" in pending or b"\n" in pending:
                cut = min(i for i in (pending.find(b"\r"), pending.find(b"\n")) if i >= 0)
                line = pending[:cut].decode().strip()
                pending = pending[cut + 1:].lstrip(b"\r\n")
                output, config = answer(line, config)
                prompt = "fake(config)# " if config else "fake# "
                channel.send(("%s\r\n%s%s" % (line, output, prompt)).encode())

    def client(conn):
        transport = paramiko.Transport(conn)
        transport.add_server_key(key)
        transport.start_server(server=Server())
        while transport.is_active():
            channel = transport.accept(1)
            if channel is not None:
                threading.Thread(target=shell, args=(channel,), daemon=True).start()

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(8)
    print(listener.getsockname()[1], flush=True)
    while True:
        conn, _ = listener.accept()
        threading.Thread(target=client, args=(conn,), daemon=True).start()


def serve_http():
    store = {}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def key(self):
            return self.path.split("/data/", 1)[-1]

        def reply(self, code, body=None):
            data = json.dumps(body).encode() if body is not None else b""
            self.send_response(code)
            self.send_header("Content-Type", "application/yang-data+json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.key() in store:
                self.reply(200, store[self.key()])
            else:
                self.reply(404, {"errors": {"error": [{
                    "error-tag": "invalid-value",
                    "error-message": "uri keypath not found",
                }]}})

        def write(self):
            length = int(self.headers.get("Content-Length") or 0)
            store[self.key()] = json.loads(self.rfile.read(length) or b"{}")
            self.reply(204)

        do_PUT = do_PATCH = do_POST = write

        def do_DELETE(self):
            store.pop(self.key(), None)
            self.reply(204)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    print(server.server_address[1], flush=True)
    server.serve_forever()


if __name__ == "__main__":
    {"ssh": serve_ssh, "http": serve_http}[sys.argv[1]]()
