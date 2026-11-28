#!/usr/bin/env python3
"""
Automated offline QEMU serial-console E2E test for Mycel OS local llama.cpp ISO.

Boots the smoke ISO in QEMU+KVM with `-nic none` (zero network interfaces),
logs in as root, and verifies:
  1. Zero non-loopback network interfaces (`ip -o link show` has only `lo`)
  2. `/usr/local/bin/llama-server --version` exits 0 cleanly on Void musl with
     zero glibc dependencies
  3. `/var/service/llama-server` runit service is supervised (`run:`),
     `/var/lib/mycel/models` exists, and `/etc/mycel/config.toml` has
     `local_backend = "auto"` and `llama_cpp_url = "http://127.0.0.1:8080/v1"`
  4. Starts a local Python 3 stdlib HTTP server inside the offline guest on
     `127.0.0.1:8080` implementing `/health`, `/v1/models`, and
     `/v1/chat/completions` (supporting both `stream: false` JSON and
     `stream: true` SSE), restarts `mycel-runtime`, waits for
     `/run/mycel/runtime.sock`, and sends a live authenticated `Chat` IPC
     request (`provider: "auto"`) verifying `mycel-runtime` auto-discovered
     `llama-cpp` on `127.0.0.1:8080/v1` and returned the exact offline response.

Uses Python 3 standard library only.
"""

import argparse
import glob
import os
import pty
import re
import select
import shutil
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07")


def strip_ansi(text: str) -> str:
    return ANSI_ESCAPE_RE.sub("", text).replace("\r", "")


def find_newest_smoke_iso(repo_root: Path) -> Path:
    smoke_pattern = str(repo_root / "output" / "*smoke*.iso")
    candidates = [Path(p) for p in glob.glob(smoke_pattern) if os.path.isfile(p)]
    if not candidates:
        any_pattern = str(repo_root / "output" / "*.iso")
        candidates = [Path(p) for p in glob.glob(any_pattern) if os.path.isfile(p)]
    if not candidates:
        raise FileNotFoundError(f"No ISO files found in {repo_root / 'output'}")
    candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return candidates[0]


class OfflineQemuHarness:
    """Manages an offline (-nic none) QEMU process with serial console on PTY."""

    def __init__(self, iso_path: Path, memory: str, overall_timeout: float):
        self.iso_path = iso_path
        self.memory = memory
        self.overall_timeout = overall_timeout
        self.deadline = time.monotonic() + overall_timeout
        self.master_fd: int | None = None
        self.proc: subprocess.Popen | None = None
        self.used_sudo = False
        self.raw_chunks: list[str] = []
        self.clean_buffer = ""
        self.cmd_list: list[str] = []

    def remaining(self) -> float:
        return max(0.0, self.deadline - time.monotonic())

    def start(self) -> None:
        qemu_bin = shutil.which("qemu-system-x86_64")
        if not qemu_bin:
            raise RuntimeError(
                "qemu-system-x86_64 not found. Install with: sudo apt-get install -y qemu-system-x86"
            )

        qemu_args = [
            qemu_bin,
            "-nographic",
            "-serial",
            "mon:stdio",
            "-enable-kvm",
            "-m",
            self.memory,
            "-smp",
            "2",
            "-cdrom",
            str(self.iso_path),
            "-boot",
            "d",
            "-nic",
            "none",
        ]

        if os.access("/dev/kvm", os.R_OK | os.W_OK):
            self.cmd_list = qemu_args
        else:
            sudo_check = subprocess.run(
                ["sudo", "-n", "true"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            if sudo_check.returncode != 0:
                raise RuntimeError(
                    "/dev/kvm is not accessible by current user and passwordless sudo is unavailable"
                )
            self.used_sudo = True
            self.cmd_list = ["sudo", "-n", *qemu_args]

        master_fd, slave_fd = pty.openpty()
        self.master_fd = master_fd
        os.set_blocking(master_fd, False)

        self.proc = subprocess.Popen(
            self.cmd_list,
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            start_new_session=True,
            close_fds=True,
        )
        os.close(slave_fd)

    def _read_available(self, timeout: float = 0.2) -> str:
        if self.master_fd is None:
            return ""
        rlist, _, _ = select.select([self.master_fd], [], [], max(0.0, timeout))
        if not rlist:
            return ""
        chunks: list[bytes] = []
        while True:
            try:
                data = os.read(self.master_fd, 4096)
                if not data:
                    break
                chunks.append(data)
            except BlockingIOError:
                break
            except OSError:
                break
        if not chunks:
            return ""
        text = b"".join(chunks).decode("utf-8", errors="replace")
        self.raw_chunks.append(text)
        self.clean_buffer += strip_ansi(text)
        return text

    def send(self, data: str) -> None:
        if self.master_fd is None:
            raise RuntimeError("PTY is not open")
        payload = data.encode("utf-8")
        view = memoryview(payload)
        sent = 0
        while sent < len(payload):
            try:
                n = os.write(self.master_fd, view[sent:])
                sent += n
            except BlockingIOError:
                select.select([], [self.master_fd], [], 0.1)

    def wait_for_regex(
        self,
        pattern: re.Pattern[str],
        timeout: float,
        start_pos: int = 0,
        on_idle_bootloader: bool = False,
    ) -> re.Match[str]:
        end_time = min(self.deadline, time.monotonic() + timeout)
        last_enter_time = time.monotonic()
        enter_count = 0

        while time.monotonic() < end_time:
            if self.proc is not None and self.proc.poll() is not None:
                self._read_available(0.1)
                raise RuntimeError(
                    f"QEMU exited prematurely with code {self.proc.returncode}"
                )

            match = pattern.search(self.clean_buffer, start_pos)
            if match:
                return match

            if on_idle_bootloader and enter_count < 3:
                now = time.monotonic()
                tail = self.clean_buffer[-1024:]
                if (
                    now - last_enter_time >= 6.0
                    and (
                        "GRUB" in tail
                        or "ISOLINUX" in tail
                        or "SYSLINUX" in tail
                        or "Automatic boot" in tail
                        or "boot:" in tail.lower()
                    )
                ):
                    self.send("\r")
                    last_enter_time = now
                    enter_count += 1

            self._read_available(0.2)

        tail = self.clean_buffer[-2000:]
        raise TimeoutError(
            f"Timed out waiting for pattern {pattern.pattern!r}. Last serial output:\n{tail}"
        )

    def boot_and_login(self) -> None:
        print("[1/5] Waiting for Void Linux live serial login prompt (offline -nic none)...")
        login_re = re.compile(r"(?:^|\n)[^\n]*login:\s*$", re.MULTILINE)
        self.wait_for_regex(
            login_re,
            timeout=min(120.0, self.remaining()),
            on_idle_bootloader=True,
        )

        pos = len(self.clean_buffer)
        self.send("root\r")

        pw_or_prompt_re = re.compile(r"(?:Password:|#\s*$)", re.MULTILINE)
        m = self.wait_for_regex(pw_or_prompt_re, timeout=20.0, start_pos=pos)
        if "Password:" in m.group(0):
            pos = len(self.clean_buffer)
            self.send("voidlinux\r")
            prompt_re = re.compile(r"(?:^|\n)[^\n]*#\s*$", re.MULTILINE)
            self.wait_for_regex(prompt_re, timeout=20.0, start_pos=pos)

        print("      Logged in as root.")

    def run_cmd(self, command: str, timeout: float = 30.0) -> tuple[int, str]:
        token = uuid.uuid4().hex[:10]
        wrapped = (
            f"printf '\\n___MYCEL_BEGIN_%s___\\n' '{token}'; "
            f"( {command}; ); "
            f"printf '\\n___MYCEL_END_%s:%d___\\n' '{token}' \"$?\""
        )
        start_pos = len(self.clean_buffer)
        self.send(wrapped + "\r")

        begin_marker = f"___MYCEL_BEGIN_{token}___"
        end_re = re.compile(rf"___MYCEL_END_{token}:(\d+)___")

        end_match = self.wait_for_regex(
            end_re, timeout=min(timeout, self.remaining()), start_pos=start_pos
        )
        rc = int(end_match.group(1))

        segment = self.clean_buffer[start_pos : end_match.start()]
        begin_idx = segment.find(begin_marker)
        if begin_idx != -1:
            output = segment[begin_idx + len(begin_marker) :].strip()
        else:
            output = segment.strip()
        return rc, output

    def shutdown(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            try:
                self.send("\rpoweroff -f\r")
            except OSError:
                pass
            for _ in range(25):
                if self.proc.poll() is not None:
                    break
                self._read_available(0.2)

        if self.proc is not None and self.proc.poll() is None:
            pgid = self.proc.pid
            if self.used_sudo:
                subprocess.run(
                    ["sudo", "-n", "kill", "-TERM", f"-{pgid}"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            else:
                try:
                    os.killpg(pgid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                if self.used_sudo:
                    subprocess.run(
                        ["sudo", "-n", "kill", "-KILL", f"-{pgid}"],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                else:
                    try:
                        os.killpg(pgid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                self.proc.wait(timeout=5)

        if self.master_fd is not None:
            try:
                os.close(self.master_fd)
            except OSError:
                pass
            self.master_fd = None

    def full_transcript(self) -> str:
        return strip_ansi("".join(self.raw_chunks))


MOCK_LLAMA_SERVER_PY = r'''import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def _send_json(self, code, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/health"):
            self._send_json(200, {"status": "ok"})
        elif self.path.startswith("/v1/models"):
            self._send_json(
                200,
                {
                    "object": "list",
                    "data": [{"id": "offline-test-model", "object": "model"}],
                },
            )
        else:
            self._send_json(404, {"error": "not found"})

    def do_POST(self):
        if not self.path.startswith("/v1/chat/completions"):
            self._send_json(404, {"error": "not found"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length > 0 else b"{}"
        req = json.loads(raw.decode("utf-8"))
        if req.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Cache-Control", "no-cache")
            self.end_headers()
            chunk = {
                "id": "chatcmpl-offline",
                "object": "chat.completion.chunk",
                "choices": [
                    {
                        "index": 0,
                        "delta": {"role": "assistant", "content": "OFFLINE_LLAMACPP_OK"},
                        "finish_reason": "stop",
                    }
                ],
            }
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode("utf-8"))
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            self._send_json(
                200,
                {
                    "id": "chatcmpl-offline",
                    "object": "chat.completion",
                    "choices": [
                        {
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": "OFFLINE_LLAMACPP_OK",
                            },
                            "finish_reason": "stop",
                        }
                    ],
                },
            )


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", 8080), Handler)
    server.serve_forever()
'''


def run_offline_checks(harness: OfflineQemuHarness) -> list[tuple[str, str]]:
    results: list[tuple[str, str]] = []

    # 1. Offline network check (only lo)
    print("[2/5] Checking offline network isolation (only lo interface)...")
    rc, out = harness.run_cmd("ip -o link show", timeout=20.0)
    if rc != 0:
        raise AssertionError(f"ip -o link show failed (rc={rc}):\n{out}")
    if_names = []
    for line in out.splitlines():
        m = re.match(r"^\d+:\s+([^:@]+)", line.strip())
        if m:
            if_names.append(m.group(1))
    if if_names != ["lo"]:
        raise AssertionError(f"Expected only ['lo'] interface, got {if_names}:\n{out}")
    results.append(("Offline network isolation (lo only)", out))

    # 2. llama-server binary check on Void musl + zero glibc deps
    print("[3/5] Checking /usr/local/bin/llama-server on Void musl...")
    rc, out = harness.run_cmd(
        "/usr/local/bin/llama-server --version && (ldd /usr/local/bin/llama-server 2>&1 || true)",
        timeout=20.0,
    )
    if rc != 0 or "libc.so.6" in out:
        raise AssertionError(f"llama-server check failed (rc={rc}):\n{out}")
    results.append(("Native Void musl llama-server --version", out))

    # 3. Runit service, models dir, and /etc/mycel/config.toml
    print("[4/5] Checking runit llama-server service, models dir, and config.toml...")
    rc, out = harness.run_cmd(
        "for i in $(seq 1 20); do "
        "sv status /var/service/llama-server 2>&1 | grep -q '^run:' && break; "
        "sleep 1; "
        "done; "
        "sv status /var/service/llama-server && "
        "test -d /var/lib/mycel/models && "
        "grep -E '^(local_backend|llama_cpp_url)' /etc/mycel/config.toml",
        timeout=30.0,
    )
    if (
        rc != 0
        or "run:" not in out
        or 'local_backend = "auto"' not in out
        or 'llama_cpp_url = "http://127.0.0.1:8080/v1"' not in out
    ):
        raise AssertionError(f"Service/config check failed (rc={rc}):\n{out}")
    results.append(("Runit llama-server + models dir + config.toml", out))

    # 4. Local offline llama.cpp HTTP server + mycel-runtime auto-discovery + live Chat IPC
    print("[5/5] Testing offline llama-cpp auto-discovery & live Chat IPC...")
    server_b64_cmd = (
        "cat << 'EOF' > /tmp/mock_llama.py\n"
        + MOCK_LLAMA_SERVER_PY
        + "\nEOF\n"
        "python3 /tmp/mock_llama.py >/tmp/mock_llama.log 2>&1 &\n"
        "MOCK_PID=$!\n"
        "for i in $(seq 1 20); do "
        "python3 -c \"import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:8080/health', timeout=2).read().decode())\" 2>/dev/null | grep -q 'ok' && break; "
        "sleep 0.5; "
        "done\n"
        "sv restart /var/service/mycel-runtime\n"
        "for i in $(seq 1 30); do "
        "[ -S /run/mycel/runtime.sock ] && [ -f /run/mycel/runtime.token ] && break; "
        "sleep 0.5; "
        "done\n"
        "python3 - << 'PYEOF'\n"
        "import json, socket\n"
        "s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)\n"
        "s.settimeout(30)\n"
        "s.connect('/run/mycel/runtime.sock')\n"
        "f = s.makefile('r', encoding='utf-8')\n"
        "tok = open('/run/mycel/runtime.token', encoding='utf-8').read().strip()\n"
        "s.sendall((json.dumps({'type': 'Authenticate', 'token': tok}) + '\\n').encode())\n"
        "auth = json.loads(f.readline())\n"
        "assert auth.get('type') == 'Ok', auth\n"
        "s.sendall((json.dumps({'type': 'Chat', 'message': 'hello offline', 'provider': 'auto'}) + '\\n').encode())\n"
        "chunks = []\n"
        "final = None\n"
        "while True:\n"
        "    line = f.readline()\n"
        "    assert line, 'EOF waiting for Chat response'\n"
        "    msg = json.loads(line)\n"
        "    if msg.get('type') == 'ChatChunk':\n"
        "        chunks.append(msg.get('delta', msg.get('content', '')))\n"
        "        continue\n"
        "    final = msg\n"
        "    break\n"
        "assert final and final.get('type') == 'Chat', f'Unexpected final: {final}'\n"
        "resp_text = final.get('response', '')\n"
        "assert 'OFFLINE_LLAMACPP_OK' in resp_text, f'Missing expected text in {final}'\n"
        "print('CHAT_IPC_RESPONSE=' + json.dumps(final))\n"
        "PYEOF\n"
        "kill $MOCK_PID 2>/dev/null || true"
    )
    rc, out = harness.run_cmd(server_b64_cmd, timeout=60.0)
    if rc != 0 or "OFFLINE_LLAMACPP_OK" not in out:
        raise AssertionError(f"Offline llama-cpp E2E Chat IPC failed (rc={rc}):\n{out}")
    results.append(("Offline llama-cpp auto-discovery + Chat IPC", out))

    return results


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Boot Mycel OS ISO completely offline (-nic none) and verify local llama-cpp E2E."
    )
    parser.add_argument(
        "--iso",
        type=Path,
        default=None,
        help="Path to ISO image (default: newest output/*smoke*.iso)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=180.0,
        help="Overall timeout in seconds (default: 180)",
    )
    parser.add_argument(
        "--memory",
        default="2G",
        help="QEMU guest memory size (default: 2G)",
    )
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    iso_path = args.iso.resolve() if args.iso else find_newest_smoke_iso(repo_root)
    if not iso_path.is_file():
        print(f"ERROR: ISO file not found: {iso_path}", file=sys.stderr)
        return 1

    iso_size_mb = iso_path.stat().st_size / (1024 * 1024)
    print(f"ISO:     {iso_path} ({iso_size_mb:.1f} MiB)")
    print(f"Timeout: {args.timeout:.0f}s | Memory: {args.memory}")

    harness = OfflineQemuHarness(
        iso_path=iso_path,
        memory=args.memory,
        overall_timeout=args.timeout,
    )

    try:
        harness.start()
        print(f"QEMU:    {' '.join(harness.cmd_list)}")
        harness.boot_and_login()
        results = run_offline_checks(harness)
    except Exception as exc:
        print(f"\nOFFLINE LLM ISO TEST FAILED: {exc}", file=sys.stderr)
        transcript = harness.full_transcript()
        if transcript:
            print("\n--- Serial Transcript (tail 4000 chars) ---", file=sys.stderr)
            print(transcript[-4000:], file=sys.stderr)
            print("--- End Serial Transcript ---", file=sys.stderr)
        return 1
    finally:
        harness.shutdown()

    print("\n=== OFFLINE LLM ISO TEST SUMMARY ===")
    for name, detail in results:
        print(f"[PASS] {name}")
        for line in detail.splitlines():
            print(f"       {line}")
    print("\nALL OFFLINE LLM ISO CHECKS PASSED.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
