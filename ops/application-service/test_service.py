"""One-request socket-activation fixture, not an installed product service."""
import hmac
from http.server import BaseHTTPRequestHandler, HTTPServer
import os
from pathlib import Path
import socket


def main():
    assert os.environ['LISTEN_FDS'] == '1'
    assert int(os.environ['LISTEN_PID']) == os.getpid()
    listener = socket.socket(fileno=3)
    assert listener.getsockname()[0] == '127.0.0.1'
    token = (Path(os.environ['CREDENTIALS_DIRECTORY']) / 'upstream-token').read_bytes()
    assert len(token) == 64

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            supplied = self.headers.get('x-duck-upstream-token', '').encode()
            if not hmac.compare_digest(supplied, token):
                self.send_error(403)
                return
            if not self.headers.get('x-duck-caller-account'):
                self.send_error(401)
                return
            self.send_response(200)
            self.end_headers()

        def log_message(self, *args):
            pass

    server = HTTPServer(listener.getsockname(), Handler, bind_and_activate=False)
    server.socket.close()
    server.socket = listener
    server.handle_request()
    server.server_close()


if __name__ == '__main__':
    main()
