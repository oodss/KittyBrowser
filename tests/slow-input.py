"""Run after cargo build --release. Exercise input backpressure with a real Chrome renderer."""

import fcntl
import json
import os
from pathlib import Path
import pty
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


KB = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / "target/release/kb"
PAGE = """<!doctype html><style>button{position:fixed;inset:0;width:100%;height:100%}</style>
<button>Target</button><script>
let events = [];
function record(e) { events.push([e.type, e.deltaY || e.key || 0, e.isTrusted]); }
for (const type of ['mousedown','mouseup','click','keydown','keyup']) addEventListener(type, record, true);
addEventListener('wheel', e => {
  record(e);
  e.preventDefault();
  const end = performance.now() + 85;
  while (performance.now() < end) {}
}, {passive:false});
</script>"""


def check(folder):
    page = folder / "wheel.html"
    page.write_text(PAGE)
    env = dict(os.environ, HOME=str(folder), KB_SOCKET=str(folder / "ctl"), KB_FORCE="1", KB_TRANSFER="png")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 31, 80, 640, 496))
    process = subprocess.Popen([KB, page.as_uri()], env=env, stdin=slave, stdout=slave)
    os.close(slave)

    def drain():
        try:
            while os.read(master, 65536):
                pass
        except OSError:
            pass  # Linux PTYs report EIO at EOF.

    threading.Thread(target=drain, daemon=True).start()

    def evaluate(script):
        return subprocess.check_output([KB, "ctl", "eval", script], env=env, text=True,
                                       stderr=subprocess.DEVNULL, timeout=10).strip()

    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                if evaluate("document.querySelector('button')?.textContent") == "Target":
                    break
            except subprocess.CalledProcessError:
                pass
            time.sleep(.05)
        else:
            raise AssertionError("test page never became ready")

        # Change pointer coordinates as on a trackpad so fixed-target batching
        # alone cannot protect Chrome. Reverse direction, then click and type.
        for i in range(78):
            code = 65 if i < 58 else 64
            os.write(master, f"\x1b[<{code};{25 + i % 3};14M".encode())
            time.sleep(.012)
        os.write(master, b"\x1b[<0;27;14M\x1b[<0;27;14mx")
        time.sleep(2)
        events = json.loads(evaluate("JSON.stringify(events)"))
        types = [kind for kind, _, _ in events]
        assert all(trusted for _, _, trusted in events)
        assert [t for t in types if t != "wheel"] == ["mousedown", "mouseup", "click", "keydown", "keyup"], types
        assert any(kind == "wheel" and dy < 0 for kind, dy, _ in events), "the reversal never reached the page"
        # Only requests already in Chrome's three wheel slots may finish here.
        after = types[types.index("mousedown") + 1:].count("wheel")
        assert after <= 3, f"{after} queued wheel events ran after the click"
        print(f"PASS reversal and trusted click/key order, {after} wheel events after mouse down")
    finally:
        try:
            if process.poll() is None:
                subprocess.run([KB, "ctl", "quit"], env=env, capture_output=True, timeout=5)
                process.wait(timeout=5)
        finally:
            os.close(master)
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="kb-slow-input-") as folder:
        check(Path(folder))
