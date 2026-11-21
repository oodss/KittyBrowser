"""Run after cargo build --release. Exercises the pane UI against local Chrome."""

import fcntl
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import pty
import struct
import subprocess
import tempfile
import termios
import threading
import time


KB = Path(__file__).resolve().parents[1] / "target/release/kb"


class Site(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        if self.path == "/login":
            self.send_header("Set-Cookie", "session=tab-test; HttpOnly; Path=/")
        self.end_headers()
        logged_in = "yes" if "session=tab-test" in self.headers.get("Cookie", "") else "no"
        self.wfile.write(
            f"<title>{self.path}</title><style>body{{margin:0}}a{{display:block;height:45px}}</style>"
            f"<a href='/link'>Link</a><b id='login'>{logged_in}</b>".encode()
        )

    def log_message(self, *args):
        pass


def wait_for(predicate):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except subprocess.CalledProcessError:
            pass  # Chrome is starting or replacing the page's execution context.
        time.sleep(0.05)
    raise AssertionError(f"condition timed out at line {predicate.__code__.co_firstlineno}")


def check(folder, base):
    env = dict(os.environ, HOME=folder, KB_SOCKET=f"{folder}/control.sock", KB_FORCE="1", KB_TRANSFER="png")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 31, 80, 640, 496))
    process = subprocess.Popen([KB, "--scale", "1", base + "/login"], env=env, stdin=slave, stdout=slave)
    os.close(slave)

    def drain():
        try:
            while os.read(master, 65536):
                pass
        except OSError:
            pass  # PTYs can report EIO rather than EOF when the child exits.

    threading.Thread(target=drain, daemon=True).start()

    def ctl(*args):
        return subprocess.check_output([KB, "ctl", *args], env=env, text=True, stderr=subprocess.DEVNULL, timeout=5).strip()

    def page(path):
        wait_for(lambda: ctl("eval", "location.href") == base + path)

    def click(column, row, button=0):
        os.write(master, f"\x1b[<{button};{column};{row}M\x1b[<{button};{column};{row}m".encode())

    try:
        page("/login")
        ctl("open", base + "/first")
        page("/first")

        click(76, 1)  # Visible + on the single-tab toolbar.
        wait_for(lambda: ctl("eval", "location.href") == "about:blank")
        os.write(master, b"\x17")
        page("/first")

        # The URL and Enter arrive in the same PTY read as Ctrl+T.
        os.write(master, b"\x14\x1b[200~" + (base + "/second").encode() + b"\x1b[201~\r")
        page("/second")
        assert ctl("eval", "document.querySelector('#login').textContent") == "yes"
        ctl("open", base + "/third")
        page("/third")
        ctl("back")
        page("/second")
        click(6, 2)
        page("/first")
        ctl("back")
        page("/login")

        click(3, 4, button=1)  # Middle-click the page link below both toolbar rows.
        wait_for(lambda: "3 open, this is 1" in ctl("status"))
        page("/login")
        click(32, 2)
        page("/link")
        os.write(master, b"\x17")  # Ctrl+W
        page("/login")
        os.write(master, b"\x1b[116;6u")  # Ctrl+Shift+T
        page("/link")

        ctl("eval", "window.open('/popup','signin','width=450,height=400'), true")
        page("/popup")
        ctl("eval", "window.opener.loginComplete=true; setTimeout(()=>window.close(),20); true")
        page("/link")
        assert ctl("eval", "window.loginComplete") == "true"
    finally:
        try:
            ctl("quit")
            process.wait(timeout=5)
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)
            os.close(master)


if __name__ == "__main__":
    with ThreadingHTTPServer(("127.0.0.1", 0), Site) as server, tempfile.TemporaryDirectory() as folder:
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            check(folder, f"http://127.0.0.1:{server.server_port}")
        finally:
            server.shutdown()
    print("PASS shared login, immediate input, independent histories, background link, restore and popup")
