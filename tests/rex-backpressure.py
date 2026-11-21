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
    resumed = threading.Event()
    question = threading.Event()
    received = presented = peak = 0

    def read_pty():
        nonlocal received, peak
        buf = bytearray()
        try:
            while chunk := os.read(master, 65536):
                buf.extend(chunk)
                while buf:
                    if buf.startswith(b"\x1b_Ga=q,i="):
                        end = buf.find(b"\x1b\\", 2)
                        if end < 0:
                            break
                        probe = bytes(buf[len(b"\x1b_Ga=q,i="):end]).split(b",", 1)[0]
                        batches.put(probe)
                        del buf[:end + 2]
                        continue
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
                        peak = max(peak, received - presented)
                        batches.put("image")
                    if b"Quit KittyBrowser?" in batch:
                        batches.put("question")
        except OSError:
            pass  # The PTY can return EIO when kb exits.

    def display():
        nonlocal presented
        dropped = False
        while item := batches.get():
            if item == "image":
                # After receipts are working, stall longer than the old
                # one-second timeout. No more images should collect behind it.
                time.sleep(2.4 if presented == 3 else 0.125)
                presented += 1
                first_frame.set()
                if presented >= 6:
                    resumed.set()
            elif isinstance(item, bytes):
                if presented == 5 and not dropped:
                    dropped = True
                    continue
                os.write(master, b"\x1b_Gi=" + item + b";OK\x1b\\")
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
        assert resumed.wait(10), "animation did not recover after the stalled terminal"
        backlog = received - presented
        started = time.monotonic()
        os.write(master, b"\x1b[<0;100;1M\x1b[<0;100;1m")
        assert question.wait(30), "the close question never reached the display"
        print(f"{backlog} queued images at click, visible question after {1000 * (time.monotonic() - started):.0f} ms")
        os.write(master, b"y")
        assert process.wait(timeout=5) == 0
        assert presented >= 4, "the animation stopped updating"
        assert peak <= 1, f"{peak} page images accumulated ahead of the toolbar"
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
