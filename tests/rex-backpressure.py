"""Run after cargo build --release. An optional binary path compares another build."""

import fcntl
import os
from pathlib import Path
import pty
import queue
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


KB = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / "target/release/kb"
END = b"\x1b[?2026l"
QUERY = b"\x1b[5n"
PAGE = """<!doctype html><style>body{background:#345}
.panel{height:275px;width:450px;animation:change .7s infinite alternate linear}
@keyframes change{from{background:#135}to{background:#fed}}</style><div class=panel></div>"""


def check(folder):
    page = folder / "panel.html"
    page.write_text(PAGE)
    # Supply the Rex environment without looking up or starting a real session.
    rex = folder / "rex"
    rex.write_text("#!/bin/sh\nexit 1\n")
    rex.chmod(0o700)
    env = dict(os.environ, HOME=str(folder), PATH=f"{folder}:{os.environ['PATH']}",
               KB_SOCKET=str(folder / "ctl.sock"), KB_TRANSFER="pty", KB_FORCE="1",
               REX_SESSION="test", REX_BLOCK="test")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 41, 100, 800, 656))
    process = subprocess.Popen([KB, page.as_uri()], env=env, stdin=slave, stdout=slave)
    os.close(slave)

    # A fast PTY reader hides the slow display from write_all. Only the
    # ordered status reply tells kb when this terminal has consumed the image.
    batches = queue.Queue()
    first_frame = threading.Event()
    question = threading.Event()
    received = presented = 0

    def read_pty():
        nonlocal received
        buf = bytearray()
        try:
            while chunk := os.read(master, 65536):
                buf.extend(chunk)
                while buf:
                    if buf.startswith(QUERY):
                        batches.put("query")
                        del buf[:len(QUERY)]
                        continue
                    end = buf.find(END)
                    if end < 0:
                        break
                    batch = buf[:end]
                    del buf[:end + len(END)]
                    if b"\x1b_Ga=T" in batch or b"\x1b_Ga=p" in batch:
                        received += 1
                        batches.put("image")
                    if b"Quit KittyBrowser?" in batch:
                        batches.put("question")
        except OSError:
            pass  # The PTY can return EIO when kb exits.

    def display():
        nonlocal presented
        while item := batches.get():
            if item == "image":
                time.sleep(0.125)
                presented += 1
                first_frame.set()
            elif item == "query":
                os.write(master, b"\x1b[0n")
            elif item == "question":
                question.set()

    reader = threading.Thread(target=read_pty, daemon=True)
    consumer = threading.Thread(target=display, daemon=True)
    reader.start()
    consumer.start()
    try:
        assert first_frame.wait(10), "no visible page"
        time.sleep(4)
        backlog = received - presented
        started = time.monotonic()
        os.write(master, b"\x1b[<0;100;1M\x1b[<0;100;1m")
        assert question.wait(30), "the close question never reached the display"
        print(f"{backlog} queued images at click, visible question after {1000 * (time.monotonic() - started):.0f} ms")
        os.write(master, b"y")
        assert process.wait(timeout=5) == 0
        assert presented >= 4, "the animation stopped updating"
        assert backlog <= 1, "stale page images accumulated ahead of the toolbar"
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        reader.join(timeout=1)
        batches.put(None)
        consumer.join(timeout=1)
        os.close(master)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as folder:
        check(Path(folder))
