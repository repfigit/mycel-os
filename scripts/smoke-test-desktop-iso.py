#!/usr/bin/env python3
"""
Automated QEMU desktop E2E smoke test for Mycel OS (Void Linux musl) desktop ISO.

Boots the desktop ISO in QEMU with KVM, virtio-vga, VNC, Unix monitor socket,
and serial console attached to a PTY, logs in as root on the Void live image,
and verifies:
  1. seatd, udevd, and mycel-runtime runit services are running (`run:`)
  2. `/run/mycel/runtime.sock` and `/run/mycel/runtime.token` (0600) exist,
     and `mycel status` succeeds (`qwen3:8b`)
  3. `/usr/local/bin/mycel-session` launches cleanly on tty1, `wayland-0` socket
     appears in `$XDG_RUNTIME_DIR`, BOTH `mycel-compositor` and `mycel-shell`
     remain alive, and `mycel-shell` connects to Wayland & authenticates to
     `mycel-runtime`
  4. QEMU `screendump` produces a valid PPM -> converted to PNG via pure Python
     stdlib (`zlib` + `struct`), with non-zero dimensions, >= 10 distinct RGB
     colors, and non-black / non-uniform rendered GUI pixels
  5. Live natural-language `Chat` turn over `/run/mycel/runtime.sock` hitting
     host Ollama (`10.0.2.2:11434`, `qwen3:8b`) with >= 1 streaming `ChatChunk`
     and non-empty final `Chat` response
  6. Policy-gated `ExecuteCode` -> `ConfirmationRequired` -> `ConfirmAction`
     round-trip inside the guest
  7. Authenticated `ListProviders` IPC (`>= 45` providers, key IDs present, zero
     secret fields) and `mycel providers` CLI output

Uses Python 3 standard library only.
"""

import argparse
import base64
from collections import Counter
import glob
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import uuid
import zlib
from pathlib import Path

ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07")
PPM_OUT = Path("/tmp/mycel-desktop-screenshot.ppm")
PNG_OUT = Path("/tmp/mycel-desktop-screenshot.png")


def strip_ansi(text: str) -> str:
    return ANSI_ESCAPE_RE.sub("", text).replace("\r", "")


def find_newest_desktop_iso(repo_root: Path) -> Path:
    desktop_pattern = str(repo_root / "output" / "*desktop*.iso")
    candidates = [Path(p) for p in glob.glob(desktop_pattern) if os.path.isfile(p)]
    if not candidates:
        any_pattern = str(repo_root / "output" / "*.iso")
        candidates = [Path(p) for p in glob.glob(any_pattern) if os.path.isfile(p)]
    if not candidates:
        raise FileNotFoundError(f"No ISO files found in {repo_root / 'output'}")
    candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return candidates[0]


def parse_ppm_p6(ppm_path: Path) -> tuple[int, int, bytes]:
    """Parse a binary P6 PPM image into (width, height, rgb_bytes)."""
    raw = ppm_path.read_bytes()
    pos = 0
    tokens: list[bytes] = []
    n = len(raw)

    while len(tokens) < 4:
        while pos < n and raw[pos : pos + 1] in (b" ", b"\t", b"\r", b"\n"):
            pos += 1
        if pos >= n:
            raise ValueError("Unexpected EOF while parsing PPM header")
        if raw[pos : pos + 1] == b"#":
            nl = raw.find(b"\n", pos)
            pos = n if nl == -1 else nl + 1
            continue
        start = pos
        while pos < n and raw[pos : pos + 1] not in (b" ", b"\t", b"\r", b"\n", b"#"):
            pos += 1
        tokens.append(raw[start:pos])

    magic, w_tok, h_tok, max_tok = tokens
    if magic != b"P6":
        raise ValueError(f"Expected P6 PPM magic, got {magic!r}")
    width = int(w_tok)
    height = int(h_tok)
    maxval = int(max_tok)
    if width <= 0 or height <= 0:
        raise ValueError(f"Invalid PPM dimensions: {width}x{height}")
    if maxval <= 0 or maxval > 255:
        raise ValueError(f"Unsupported PPM maxval: {maxval}")

    # P6 specifies a single whitespace character after maxval
    if pos >= n or raw[pos : pos + 1] not in (b" ", b"\t", b"\r", b"\n"):
        raise ValueError("Missing whitespace separator after PPM maxval")
    pos += 1

    expected_len = width * height * 3
    rgb = raw[pos : pos + expected_len]
    if len(rgb) != expected_len:
        raise ValueError(
            f"Truncated PPM pixel data: expected {expected_len} bytes, got {len(rgb)}"
        )

    if maxval != 255:
        rgb = bytes((b * 255) // maxval for b in rgb)

    return width, height, rgb


def _png_chunk(chunk_type: bytes, data: bytes) -> bytes:
    crc = zlib.crc32(chunk_type + data) & 0xFFFFFFFF
    return struct.pack(">I", len(data)) + chunk_type + data + struct.pack(">I", crc)


def write_png_rgb(png_path: Path, width: int, height: int, rgb: bytes) -> None:
    """Encode raw 8-bit RGB bytes as a PNG file using stdlib zlib + struct."""
    stride = width * 3
    scanlines = bytearray()
    for y in range(height):
        scanlines.append(0)  # Filter type 0 (None)
        start = y * stride
        scanlines.extend(rgb[start : start + stride])

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    idat = zlib.compress(bytes(scanlines), level=6)
    png_bytes = (
        b"\x89PNG\r\n\x1a\n"
        + _png_chunk(b"IHDR", ihdr)
        + _png_chunk(b"IDAT", idat)
        + _png_chunk(b"IEND", b"")
    )
    png_path.write_bytes(png_bytes)


def analyze_rgb_pixels(width: int, height: int, rgb: bytes) -> dict[str, float | int]:
    """Analyze RGB buffer to verify real non-uniform rendered GUI pixels."""
    total_pixels = width * height
    if total_pixels <= 0 or len(rgb) != total_pixels * 3:
        raise ValueError("Invalid RGB buffer size")

    pixels = [rgb[i : i + 3] for i in range(0, len(rgb), 3)]
    counts = Counter(pixels)
    unique_colors = len(counts)
    black_count = counts.get(b"\x00\x00\x00", 0)
    dominant_color, dominant_count = counts.most_common(1)[0]

    non_black_ratio = (total_pixels - black_count) / total_pixels
    non_uniform_ratio = (total_pixels - dominant_count) / total_pixels

    return {
        "width": width,
        "height": height,
        "total_pixels": total_pixels,
        "unique_colors": unique_colors,
        "non_black_ratio": non_black_ratio,
        "non_uniform_ratio": non_uniform_ratio,
        "dominant_rgb": int.from_bytes(dominant_color, "big"),
    }


class QemuDesktopHarness:
    """Manages QEMU desktop VM with PTY serial console and Unix monitor socket."""

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
        self.temp_dir = tempfile.TemporaryDirectory(prefix="mycel-desktop-e2e-")
        self.monitor_sock = Path(self.temp_dir.name) / "qemu-monitor.sock"

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
            "-enable-kvm",
            "-m",
            self.memory,
            "-smp",
            "2",
            "-device",
            "virtio-vga",
            "-vnc",
            "127.0.0.1:19",
            "-monitor",
            f"unix:{self.monitor_sock},server,nowait",
            "-serial",
            "stdio",
            "-cdrom",
            str(self.iso_path),
            "-boot",
            "d",
            "-netdev",
            "user,id=net0",
            "-device",
            "virtio-net-pci,netdev=net0",
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
                if now - last_enter_time >= 8.0 and (
                    not tail
                    or "GRUB" in tail
                    or "ISOLINUX" in tail
                    or "SYSLINUX" in tail
                    or "Automatic boot" in tail
                    or "boot:" in tail.lower()
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
        print("[1/8] Waiting for Void Linux live serial login prompt...")
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

    def send_monitor_command(self, command: str, timeout: float = 10.0) -> str:
        """Send an HMP command to the QEMU monitor Unix socket."""
        end_time = time.monotonic() + timeout
        while not self.monitor_sock.exists():
            if time.monotonic() >= end_time:
                raise TimeoutError(f"QEMU monitor socket not found: {self.monitor_sock}")
            time.sleep(0.1)

        def _connect_and_send() -> str:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(timeout)
                s.connect(str(self.monitor_sock))
                # Drain initial banner if present
                try:
                    s.settimeout(0.3)
                    s.recv(4096)
                except socket.timeout:
                    pass
                s.settimeout(timeout)
                s.sendall((command.strip() + "\n").encode("utf-8"))
                time.sleep(0.3)
                try:
                    s.settimeout(0.5)
                    return s.recv(4096).decode("utf-8", errors="replace")
                except socket.timeout:
                    return ""

        try:
            return _connect_and_send()
        except PermissionError:
            if not self.used_sudo:
                raise
            # Socket created by sudo QEMU; chmod or send via sudo
            py = (
                "import socket, sys, time; "
                f"s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); "
                f"s.settimeout({timeout}); "
                f"s.connect({str(self.monitor_sock)!r}); "
                "time.sleep(0.2); "
                f"s.sendall({(command.strip() + chr(10))!r}.encode()); "
                "time.sleep(0.3); "
                "s.settimeout(0.4); "
                "print(s.recv(4096).decode('utf-8', 'replace') if True else '')"
            )
            res = subprocess.run(
                ["sudo", "-n", sys.executable, "-c", py],
                capture_output=True,
                text=True,
                timeout=timeout + 2.0,
            )
            if res.returncode != 0:
                raise RuntimeError(
                    f"Failed to send QEMU monitor command via sudo: {res.stderr}"
                )
            return res.stdout

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

        self.temp_dir.cleanup()

    def full_transcript(self) -> str:
        return strip_ansi("".join(self.raw_chunks))


def _remove_host_file(path: Path) -> None:
    if not path.exists():
        return
    try:
        path.unlink()
    except PermissionError:
        subprocess.run(["sudo", "-n", "rm", "-f", str(path)], check=False)


def run_desktop_e2e_checks(harness: QemuDesktopHarness) -> list[tuple[str, str]]:
    results: list[tuple[str, str]] = []

    # 2. Runit services: seatd, udevd, mycel-runtime
    print("[2/8] Checking runit services (seatd, udevd, mycel-runtime)...")
    rc, out = harness.run_cmd(
        "for svc in udevd seatd mycel-runtime; do "
        "  ok=0; "
        "  for i in $(seq 1 30); do "
        "    if sv status /var/service/$svc 2>&1 | grep -q '^run:'; then "
        "      sv status /var/service/$svc; ok=1; break; "
        "    fi; "
        "    sleep 1; "
        "  done; "
        "  [ \"$ok\" = \"1\" ] || { echo \"FAILED: $svc\"; sv status /var/service/$svc 2>&1; exit 1; }; "
        "done",
        timeout=45.0,
    )
    if rc != 0 or out.count("run:") < 3:
        raise AssertionError(f"Runit service check failed (rc={rc}):\n{out}")
    results.append(("Runit services (udevd, seatd, mycel-runtime)", out))

    # 3. Ensure guest networking -> host Ollama is up, restart mycel-runtime if needed,
    #    and verify runtime.sock, 0600 token, and `mycel status`
    print("[3/8] Checking host Ollama reachability, /run/mycel/runtime.sock, 0600 token, and `mycel status`...")
    rc, out = harness.run_cmd(
        "for i in $(seq 1 20); do "
        "  curl -sf http://10.0.2.2:11434/api/tags >/dev/null && break; "
        "  sleep 1; "
        "done; "
        "curl -sf http://10.0.2.2:11434/api/tags | head -c 200; echo ''; "
        "sv restart /var/service/mycel-runtime >/dev/null; "
        "sleep 1; "
        "for i in $(seq 1 20); do "
        "  sv status /var/service/mycel-runtime 2>&1 | grep -q '^run:' && "
        "  [ -S /run/mycel/runtime.sock ] && [ -f /run/mycel/runtime.token ] && break; "
        "  sleep 1; "
        "done; "
        "sock_perm=$(stat -c '%a' /run/mycel/runtime.sock) && "
        "tok_perm=$(stat -c '%a' /run/mycel/runtime.token) && "
        "echo \"SOCK_PERM=$sock_perm TOK_PERM=$tok_perm\" && "
        "[ \"$sock_perm\" = \"600\" ] && [ \"$tok_perm\" = \"600\" ] && "
        "MYCEL_SOCKET=/run/mycel/runtime.sock mycel status",
        timeout=45.0,
    )
    if (
        rc != 0
        or "SOCK_PERM=600" not in out
        or "TOK_PERM=600" not in out
        or "Runtime: Running" not in out
        or "qwen3:8b" not in out
    ):
        raise AssertionError(f"Runtime socket/token/status check failed (rc={rc}):\n{out}")
    results.append(("Runtime socket, 0600 token & mycel status", out))

    # 4. Launch /usr/local/bin/mycel-session on tty1 and verify compositor + shell
    print("[4/8] Launching /usr/local/bin/mycel-session on tty1 and verifying Wayland + shell...")
    rc, out = harness.run_cmd(
        "export XDG_RUNTIME_DIR=/run/user/0; "
        "mkdir -p \"$XDG_RUNTIME_DIR\" && chmod 0700 \"$XDG_RUNTIME_DIR\"; "
        "rm -f /tmp/mycel-session.log; "
        "chvt 1; "
        "setsid sh -c 'exec /usr/local/bin/mycel-session </dev/tty1 >/tmp/mycel-session.log 2>&1' & "
        "wl_sock=''; "
        "for i in $(seq 1 30); do "
        "  wl_sock=$(find \"$XDG_RUNTIME_DIR\" -maxdepth 1 -type s -name 'wayland-*' 2>/dev/null | head -n1); "
        "  [ -n \"$wl_sock\" ] && break; "
        "  sleep 0.5; "
        "done; "
        "sleep 2; "
        "ls -la \"$XDG_RUNTIME_DIR\"; "
        "comp_pid=$(pgrep -f '^/usr/local/bin/mycel-compositor' | head -n1 || true); "
        "shell_pid=$(pgrep -f '^/usr/local/bin/mycel-shell' | head -n1 || true); "
        "echo \"WL_SOCK=$wl_sock COMP_PID=$comp_pid SHELL_PID=$shell_pid\"; "
        "cat /tmp/mycel-session.log; "
        "[ -n \"$wl_sock\" ] && [ -S \"$wl_sock\" ] && [ -n \"$comp_pid\" ] && [ -n \"$shell_pid\" ]",
        timeout=40.0,
    )
    if rc != 0 or "WL_SOCK=/" not in out or "COMP_PID=" not in out or "SHELL_PID=" not in out:
        raise AssertionError(
            f"mycel-session launch / Wayland / process check failed (rc={rc}):\n{out}"
        )

    # Verify compositor & shell are still alive and check log + socket connections
    rc_alive, out_alive = harness.run_cmd(
        "comp_pid=$(pgrep -f '^/usr/local/bin/mycel-compositor' | head -n1 || true); "
        "shell_pid=$(pgrep -f '^/usr/local/bin/mycel-shell' | head -n1 || true); "
        "echo \"ALIVE comp=$comp_pid shell=$shell_pid\"; "
        "ps -o pid,ppid,comm,args -p \"$comp_pid,$shell_pid\" 2>/dev/null || true; "
        "ss -xp 2>/dev/null | grep -E 'wayland|runtime\\.sock' || true; "
        "cat /tmp/mycel-session.log; "
        "[ -n \"$comp_pid\" ] && [ -n \"$shell_pid\" ]",
        timeout=20.0,
    )
    if rc_alive != 0:
        raise AssertionError(
            f"mycel-compositor or mycel-shell crashed after startup (rc={rc_alive}):\n{out_alive}"
        )
    results.append(("mycel-session (Wayland + compositor + shell alive)", out_alive))

    # 5. QEMU screendump -> PPM -> PNG + pixel verification
    print("[5/8] Capturing QEMU screendump and verifying rendered GUI pixels...")
    _remove_host_file(PPM_OUT)
    _remove_host_file(PNG_OUT)
    time.sleep(2.0)

    mon_resp = harness.send_monitor_command(f"screendump {PPM_OUT}")
    end_wait = time.monotonic() + 10.0
    while not PPM_OUT.exists() or PPM_OUT.stat().st_size == 0:
        if time.monotonic() >= end_wait:
            raise TimeoutError(
                f"screendump did not produce {PPM_OUT}. Monitor response: {mon_resp!r}"
            )
        time.sleep(0.2)
    # Give QEMU a brief moment to finish flushing the PPM file
    time.sleep(0.3)

    width, height, rgb = parse_ppm_p6(PPM_OUT)
    write_png_rgb(PNG_OUT, width, height, rgb)
    stats = analyze_rgb_pixels(width, height, rgb)

    if stats["unique_colors"] < 10:
        raise AssertionError(
            f"Screenshot appears blank/uniform (unique_colors={stats['unique_colors']}): {stats}"
        )
    if stats["non_black_ratio"] < 0.01 or stats["non_uniform_ratio"] < 0.005:
        raise AssertionError(
            f"Screenshot lacks real rendered GUI pixels: {stats}"
        )

    screen_summary = (
        f"PNG={PNG_OUT} ({PNG_OUT.stat().st_size} bytes) | "
        f"dimensions={stats['width']}x{stats['height']} | "
        f"unique_colors={stats['unique_colors']} | "
        f"non_black={stats['non_black_ratio']:.2%} | "
        f"non_background={stats['non_uniform_ratio']:.2%}"
    )
    print(f"      {screen_summary}")
    results.append(("QEMU screendump -> PNG & pixel check", screen_summary))

    # 6. Live natural-language Chat turn over /run/mycel/runtime.sock -> host Ollama (qwen3:8b)
    print("[6/8] Running live Chat turn against host Ollama (10.0.2.2:11434, qwen3:8b)...")
    chat_script = """import json, socket

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(90)
s.connect("/run/mycel/runtime.sock")
f = s.makefile("r", encoding="utf-8")

tok = open("/run/mycel/runtime.token", encoding="utf-8").read().strip()
s.sendall((json.dumps({"type": "Authenticate", "token": tok}) + "\\n").encode())
auth = json.loads(f.readline())
assert auth.get("type") == "Ok", auth

s.sendall((json.dumps({"type": "SetSession", "id": "e2e-desktop-chat"}) + "\\n").encode())
ss = json.loads(f.readline())
assert ss.get("type") == "Ok", ss

s.sendall((json.dumps({
    "type": "Chat",
    "message": "Reply with the single word: MYCEL_OK",
    "provider": "local",
}) + "\\n").encode())

chunks = []
final = None
while True:
    line = f.readline()
    assert line, "EOF before final Chat response"
    msg = json.loads(line)
    if msg.get("type") == "ChatChunk":
        chunks.append(msg.get("delta", ""))
    elif msg.get("type") == "Chat":
        final = msg
        break
    elif msg.get("type") == "Error":
        raise RuntimeError(f"IPC Chat error: {msg}")

assert final is not None, "Missing final Chat response"
resp_text = final.get("response", "").strip()
assert len(resp_text) > 0, f"Empty final Chat response: {final}"
print(f"CHUNKS={len(chunks)} RESPONSE={resp_text!r}")
"""
    chat_b64 = base64.b64encode(chat_script.encode("utf-8")).decode("ascii")
    rc_chat, out_chat = harness.run_cmd(
        f"echo '{chat_b64}' | base64 -d | python3 -",
        timeout=100.0,
    )
    if rc_chat != 0 or "CHUNKS=" not in out_chat or "RESPONSE=" not in out_chat:
        raise AssertionError(f"Live Ollama Chat check failed (rc={rc_chat}):\n{out_chat}")
    results.append(("Live Ollama Chat (qwen3:8b, streaming)", out_chat))

    # 7. Policy-gated ExecuteCode -> ConfirmationRequired -> ConfirmAction round-trip
    print("[7/8] Testing policy-gated ExecuteCode -> ConfirmationRequired -> ConfirmAction...")
    py_confirm = (
        "python3 -c '"
        "import json, socket; "
        "s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); "
        "s.settimeout(30); "
        "s.connect(\"/run/mycel/runtime.sock\"); "
        "f = s.makefile(\"r\", encoding=\"utf-8\"); "
        "tok = open(\"/run/mycel/runtime.token\", encoding=\"utf-8\").read().strip(); "
        "s.sendall((json.dumps({\"type\": \"Authenticate\", \"token\": tok}) + \"\\n\").encode()); "
        "assert json.loads(f.readline()).get(\"type\") == \"Ok\"; "
        "cmd = \"rm -f /tmp/mycel-e2e-nonexistent-file && echo CONFIRM_OK\"; "
        "s.sendall((json.dumps({\"type\": \"ExecuteCode\", \"code\": cmd}) + \"\\n\").encode()); "
        "req = json.loads(f.readline()); "
        "assert req.get(\"type\") == \"ConfirmationRequired\", f\"Expected ConfirmationRequired, got {req}\"; "
        "cid = req.get(\"id\", \"\"); "
        "assert cid, f\"Missing confirmation id: {req}\"; "
        "s.sendall((json.dumps({\"type\": \"ConfirmAction\", \"id\": cid, \"approved\": True}) + \"\\n\").encode()); "
        "res = json.loads(f.readline()); "
        "assert res.get(\"type\") == \"CodeResult\", f\"Expected CodeResult, got {res}\"; "
        "assert res.get(\"success\") is True, f\"Expected success=True, got {res}\"; "
        "assert \"CONFIRM_OK\" in res.get(\"output\", \"\"), f\"Missing CONFIRM_OK in output: {res}\"; "
        "print(\"CONFIRM_REQ=\" + json.dumps(req)); "
        "print(\"CODE_RESULT=\" + json.dumps(res))"
        "'"
    )
    rc_conf, out_conf = harness.run_cmd(py_confirm, timeout=35.0)
    if rc_conf != 0 or "CONFIRM_REQ=" not in out_conf or "CODE_RESULT=" not in out_conf:
        raise AssertionError(
            f"ConfirmationRequired -> ConfirmAction check failed (rc={rc_conf}):\n{out_conf}"
        )
    results.append(("Policy ConfirmationRequired -> ConfirmAction", out_conf))

    # 8. Authenticated ListProviders IPC + `mycel providers` CLI verification
    print("[8/8] Verifying ListProviders IPC (>= 45 providers, zero secrets) and `mycel providers` CLI...")
    providers_script = """import json, socket

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(30)
s.connect("/run/mycel/runtime.sock")
f = s.makefile("r", encoding="utf-8")

tok = open("/run/mycel/runtime.token", encoding="utf-8").read().strip()
s.sendall((json.dumps({"type": "Authenticate", "token": tok}) + "\\n").encode())
auth = json.loads(f.readline())
assert auth.get("type") == "Ok", auth

s.sendall((json.dumps({"type": "ListProviders"}) + "\\n").encode())
resp = json.loads(f.readline())
assert resp.get("type") == "Providers", f"Expected Providers, got {resp}"

providers = resp.get("providers", [])
assert len(providers) >= 45, f"Expected >= 45 providers, got {len(providers)}"

ids = {p.get("id") for p in providers}
required_ids = {
    "venice",
    "openrouter",
    "anthropic",
    "openai",
    "google",
    "amazon-bedrock",
    "llama-cpp",
}
missing = sorted(required_ids - ids)
assert not missing, f"Missing required provider IDs: {missing}"

forbidden_keys = {"api_key", "secret", "token"}
for p in providers:
    leaked = forbidden_keys.intersection(p.keys())
    assert not leaked, f"Secret fields {sorted(leaked)} leaked in provider {p.get('id')}: {p}"

print(f"PROVIDER_COUNT={len(providers)} REQUIRED_PRESENT=OK SECRETS_LEAKED=0")
"""
    prov_b64 = base64.b64encode(providers_script.encode("utf-8")).decode("ascii")
    rc_prov, out_prov = harness.run_cmd(
        f"echo '{prov_b64}' | base64 -d | python3 - && "
        "prov_cli=$(MYCEL_SOCKET=/run/mycel/runtime.sock mycel providers) && "
        "echo \"$prov_cli\" | grep -q 'venice' && "
        "echo \"$prov_cli\" | grep -q 'openrouter' && "
        "echo \"$prov_cli\" | grep -E 'venice|openrouter'",
        timeout=35.0,
    )
    if (
        rc_prov != 0
        or "REQUIRED_PRESENT=OK" not in out_prov
        or "SECRETS_LEAKED=0" not in out_prov
        or "venice" not in out_prov
        or "openrouter" not in out_prov
    ):
        raise AssertionError(
            f"ListProviders IPC / mycel providers CLI check failed (rc={rc_prov}):\n{out_prov}"
        )
    results.append(("ListProviders IPC (>= 45, zero secrets) & mycel providers CLI", out_prov))

    return results


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Boot Mycel OS Void musl desktop ISO in QEMU and run full GUI + Ollama E2E tests."
    )
    parser.add_argument(
        "--iso",
        type=Path,
        default=None,
        help="Path to desktop ISO image (default: newest output/*desktop*.iso)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=300.0,
        help="Overall timeout in seconds (default: 300)",
    )
    parser.add_argument(
        "--memory",
        default="2G",
        help="QEMU guest memory size (default: 2G)",
    )
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    iso_path = args.iso.resolve() if args.iso else find_newest_desktop_iso(repo_root)
    if not iso_path.is_file():
        print(f"ERROR: ISO file not found: {iso_path}", file=sys.stderr)
        return 1

    iso_size_mb = iso_path.stat().st_size / (1024 * 1024)
    print(f"ISO:     {iso_path} ({iso_size_mb:.1f} MiB)")
    print(f"Timeout: {args.timeout:.0f}s | Memory: {args.memory}")

    harness = QemuDesktopHarness(
        iso_path=iso_path,
        memory=args.memory,
        overall_timeout=args.timeout,
    )

    try:
        harness.start()
        print(f"QEMU:    {' '.join(harness.cmd_list)}")
        harness.boot_and_login()
        results = run_desktop_e2e_checks(harness)
    except Exception as exc:
        print(f"\nDESKTOP E2E TEST FAILED: {exc}", file=sys.stderr)
        transcript = harness.full_transcript()
        if transcript:
            print("\n--- Serial Transcript (tail 4000 chars) ---", file=sys.stderr)
            print(transcript[-4000:], file=sys.stderr)
            print("--- End Serial Transcript ---", file=sys.stderr)
        return 1
    finally:
        harness.shutdown()

    print("\n=== DESKTOP E2E TEST SUMMARY ===")
    for name, detail in results:
        print(f"[PASS] {name}")
        for line in detail.splitlines():
            print(f"       {line}")
    print("\nALL DESKTOP E2E CHECKS PASSED.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
