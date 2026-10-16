# SPDX-License-Identifier: GPL-3.0-or-later
import http.server, json, sys
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_GET(self):
        if self.path=="/hello.txt":
            self.send_response(200); self.send_header("Content-Type","text/plain"); self.end_headers(); self.wfile.write(b"hello\n")
        elif self.path=="/redir":
            self.send_response(302); self.send_header("Location","/hello.txt"); self.end_headers()
        else:
            self.send_response(404); self.end_headers(); self.wfile.write(b"nope")
    def do_POST(self):
        b=self.rfile.read(int(self.headers["Content-Length"]))
        self.send_response(200); self.send_header("Content-Type","application/json"); self.end_headers(); self.wfile.write(json.dumps({"got":json.loads(b),"ct":self.headers["Content-Type"]}).encode())
http.server.HTTPServer(("127.0.0.1",int(sys.argv[1])),H).serve_forever()
