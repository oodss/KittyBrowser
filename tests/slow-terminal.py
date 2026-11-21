"""Run with python3 tests/slow-terminal.py after cargo build --release. Requires Chrome."""

import base64
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import time
import zlib


KB = Path(__file__).resolve().parents[1] / "target/release/kb"
PAGE = """<!doctype html><style>*{margin:0}</style><canvas width=640 height=400></canvas>
<script>
const ctx = document.querySelector('canvas').getContext('2d');
const image = ctx.createImageData(640, 400);
let seed = 17;
for (let i = 0; i < image.data.length; i += 4) {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    image.data.set([seed >>> 24, seed >>> 16 & 255, seed >>> 8 & 255, 255], i);
}
ctx.putImageData(image, 0, 0);
function paint(color) { ctx.fillStyle = color; ctx.fillRect(0, 0, 30, 30); }
paint('red');
window.gotKey = new Promise(resolve => addEventListener('keydown', e => resolve(e.key), {once: true}));
</script>"""


def read_until(fd, delimiter, buffer):
    deadline = time.monotonic() + 10
    while delimiter not in buffer:
        left = deadline - time.monotonic()
        if left <= 0 or not select.select([fd], [], [], left)[0]:
            raise TimeoutError(f"terminal did not send {delimiter!r}")
        chunk = os.read(fd, 65536)
        if not chunk:
            raise EOFError("terminal closed during a frame")
        buffer.extend(chunk)
    end = buffer.index(delimiter) + len(delimiter)
    result = bytes(buffer[:end])
    del buffer[:end]
    return result


def wait_for_pixel(fd, buffer, rgb):
    # For an 8-bit RGB(A) PNG, the neighbours of the first pixel are zero
    # for every PNG row filter. We only need its three colour bytes.
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        update = read_until(fd, b"\x1b[?2026l", buffer)
        chunks = re.findall(rb'\x1b_G[^;\x1b]*,?m=[01];([A-Za-z0-9+/=]+)\x1b\\', update)
        if not chunks:
            continue
        png = base64.b64decode(b"".join(chunks))
        assert png.startswith(b"\x89PNG\r\n\x1a\n") and png[24:26] in (b"\x08\x02", b"\x08\x06")
        compressed = bytearray()
        offset = 8
        while offset < len(png):
            length, kind = struct.unpack_from(">I4s", png, offset)
            if kind == b"IDAT":
                compressed.extend(png[offset + 8:offset + 8 + length])
            offset += length + 12
        if zlib.decompressobj().decompress(compressed, 4)[1:4] == rgb:
            return
    raise AssertionError(f"the last page colour {rgb!r} did not arrive")


def check(folder):
    page = folder / "page.html"
    page.write_text(PAGE)
    env = dict(os.environ, HOME=str(folder), KB_SOCKET=str(folder / "ctl.sock"), KB_FORCE="1", KB_TRANSFER="png")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 26, 80, 640, 416))
    process = subprocess.Popen([KB, page.as_uri()], env=env, stdin=slave, stdout=slave)
    os.close(slave)

    def evaluate(script):
        return subprocess.check_output([KB, "ctl", "eval", script], env=env, text=True, timeout=5).strip()

    try:
        buffer = bytearray()
        wait_for_pixel(master, buffer, bytes((255, 0, 0)))
        evaluate("paint('blue'), true")
        # Leave the large blue frame partly unread. Forwarding the key and
        # answering control requests must still work while the writer blocks.
        read_until(master, b"\x1b_Ga=T,f=100", buffer)
        os.write(master, b"j")
        assert evaluate("gotKey") == "j"
        for color in ["red", "blue"] * 6 + ["#16c813"]:
            evaluate(f"paint({json.dumps(color)}), new Promise(resolve => requestAnimationFrame(() => resolve(true)))")
        # Discard the rest of the interrupted batch before examining complete frames.
        read_until(master, b"\x1b[?2026l", buffer)
        wait_for_pixel(master, buffer, bytes((22, 200, 19)))
        # A click on the last tab asks for confirmation. Keep changing the
        # page while the question is up: it must remain a text-only update,
        # and cancelling must present the latest pixels even after paint stops.
        os.write(master, b"\x1b[<0;79;1M\x1b[<0;79;1m")
        read_until(master, b"Quit KittyBrowser?", buffer)
        read_until(master, b"\x1b[?2026l", buffer)
        evaluate("paint('blue'), new Promise(resolve => requestAnimationFrame(() => resolve(true)))")
        # Focus explicitly requests a base repaint, including on a static page.
        # A key redraws the question so we can inspect the resulting batch.
        os.write(master, b"\x1b[Ix")
        question = read_until(master, b"\x1b[?2026l", buffer)
        assert b"Quit KittyBrowser?" in question and b"\x1b_Ga=T" not in question
        os.write(master, b"n")
        wait_for_pixel(master, buffer, bytes((0, 0, 255)))
        os.write(master, b"\x1b[<0;79;1M\x1b[<0;79;1m")
        read_until(master, b"Quit KittyBrowser?", buffer)
        os.write(master, b"y")
        assert process.wait(timeout=5) == 0
    finally:
        os.close(master)
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="kb-slow-terminal-") as folder:
        check(Path(folder))
    print("PASS input during blocked output, latest page after draining, and responsive quit confirmation")
