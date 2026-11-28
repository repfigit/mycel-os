#!/usr/bin/env python3
"""
Automated QEMU serial-console smoke test for Mycel OS (Void Linux musl) ISO.

Boots the ISO in QEMU with KVM and serial console attached to a PTY, logs in
as root on the Void live image, and verifies:
  1. Void musl environment (uname -a, musl libc / ld-musl-x86_64.so.1)
  2. Runit stage 2 & mycel-runtime service running (`sv status /var/service/mycel-runtime`)
  3. `/run/mycel/runtime.sock` and `/run/mycel/runtime.token` exist with 0600 permissions
  4. IPC Ping (unauthenticated -> Pong), `mycel status` CLI, and authenticated
     raw JSON Status (`version` present, `llm_model == "qwen3:8b"`)
  5. Bubblewrap sandbox inside guest (`bwrap --ro-bind / / -- true`)

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


def find_newest_iso(repo_root: Path) -> Path:
    pattern = str(repo_root / "output" / "*.iso")
    candidates = [Path(p) for p in glob.glob(pattern) if os.path.isfile(p)]
    if not candidates:
        raise FileNotFoundError(f"No ISO files found matching {pattern}")
    candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return candidates[0]


class QemuSerialHarness:
    """Manages QEMU process with serial console attached to a PTY."""

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
        print("[1/6] Waiting for Void Linux live serial login prompt...")
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

        # Disable shell echo/paging surprises and ensure clean prompt readiness
        print("      Logged in as root.")

    def run_cmd(self, command: str, timeout: float = 30.0) -> tuple[int, str]:
        token = uuid.uuid4().hex[:10]
        # Use %s formatting so the echoed command line does not match rendered markers.
        # Run command in a subshell (...) so `exit` never terminates the login shell.
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


def run_smoke_checks(harness: QemuSerialHarness) -> list[tuple[str, str]]:
    results: list[tuple[str, str]] = []

    # 2. Void musl environment
    print("[2/6] Checking Void musl environment...")
    rc, out = harness.run_cmd(
        "uname -a && (ldd --version 2>&1 || true) && ls -la /lib/ld-musl-x86_64.so.1",
        timeout=20.0,
    )
    if rc != 0 or "musl" not in out.lower():
        raise AssertionError(f"Void musl check failed (rc={rc}):\n{out}")
    results.append(("Void musl environment", out))

    # 3. Runit stage 2 & mycel-runtime service
    print("[3/6] Checking runit stage 2 & mycel-runtime service...")
    rc, out = harness.run_cmd(
        "for i in $(seq 1 30); do "
        "sv status /var/service/mycel-runtime 2>&1 | grep -q '^run:' && "
        "sv status /var/service/mycel-runtime && exit 0; "
        "sleep 1; "
        "done; "
        "sv status /var/service/mycel-runtime 2>&1; exit 1",
        timeout=40.0,
    )
    if rc != 0 or "run:" not in out:
        raise AssertionError(
            f"mycel-runtime runit service not running (rc={rc}):\n{out}"
        )
    results.append(("Runit mycel-runtime service", out))

    # 4. Socket & token permissions (0600)
    print("[4/6] Checking /run/mycel/runtime.sock & runtime.token (0600)...")
    rc, out = harness.run_cmd(
        "for i in $(seq 1 15); do "
        "[ -S /run/mycel/runtime.sock ] && [ -f /run/mycel/runtime.token ] && break; "
        "sleep 1; "
        "done; "
        "ls -la /run/mycel/runtime.sock /run/mycel/runtime.token && "
        "sock_perm=$(stat -c '%a' /run/mycel/runtime.sock) && "
        "tok_perm=$(stat -c '%a' /run/mycel/runtime.token) && "
        "echo \"SOCK_PERM=$sock_perm TOK_PERM=$tok_perm\" && "
        "[ \"$sock_perm\" = \"600\" ] && [ \"$tok_perm\" = \"600\" ]",
        timeout=25.0,
    )
    if rc != 0 or "SOCK_PERM=600" not in out or "TOK_PERM=600" not in out:
        raise AssertionError(f"Socket/token permission check failed (rc={rc}):\n{out}")
    results.append(("Socket & token 0600 permissions", out))

    # 5. IPC Ping, mycel status CLI, and raw JSON Status
    print("[5/6] Checking IPC Ping, mycel status CLI, and authenticated Status JSON...")
    rc_cli, out_cli = harness.run_cmd(
        "MYCEL_SOCKET=/run/mycel/runtime.sock mycel status",
        timeout=20.0,
    )
    if rc_cli != 0 or "Runtime: Running" not in out_cli or "qwen3:8b" not in out_cli:
        raise AssertionError(
            f"`mycel status` CLI check failed (rc={rc_cli}):\n{out_cli}"
        )

    py_ipc = (
        "python3 -c '"
        "import json, socket; "
        "s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); "
        "s.settimeout(10); "
        "s.connect(\"/run/mycel/runtime.sock\"); "
        "f = s.makefile(\"r\", encoding=\"utf-8\"); "
        "s.sendall(b\"{\\\"type\\\":\\\"Ping\\\"}\\n\"); "
        "pong = json.loads(f.readline()); "
        "assert pong.get(\"type\") == \"Pong\", pong; "
        "tok = open(\"/run/mycel/runtime.token\", encoding=\"utf-8\").read().strip(); "
        "assert len(tok) > 0, \"empty token\"; "
        "s.sendall((json.dumps({\"type\": \"Authenticate\", \"token\": tok}) + \"\\n\").encode()); "
        "auth = json.loads(f.readline()); "
        "assert auth.get(\"type\") == \"Ok\", auth; "
        "s.sendall(b\"{\\\"type\\\":\\\"Status\\\"}\\n\"); "
        "st = json.loads(f.readline()); "
        "assert st.get(\"type\") == \"Status\", st; "
        "assert st.get(\"version\"), st; "
        "assert st.get(\"llm_model\") == \"qwen3:8b\", st; "
        "print(\"PING=\" + json.dumps(pong)); "
        "print(\"STATUS=\" + json.dumps(st))"
        "'"
    )
    rc_ipc, out_ipc = harness.run_cmd(py_ipc, timeout=20.0)
    if rc_ipc != 0 or "PING=" not in out_ipc or "STATUS=" not in out_ipc:
        raise AssertionError(f"Raw IPC Ping/Status check failed (rc={rc_ipc}):\n{out_ipc}")
    results.append(("IPC Ping + mycel status + JSON Status", f"{out_cli}\n{out_ipc}"))

    # 6. Bubblewrap sandbox check
    print("[6/6] Checking bubblewrap sandbox inside guest...")
    rc_bwrap, out_bwrap = harness.run_cmd("bwrap --ro-bind / / -- true", timeout=20.0)
    if rc_bwrap != 0:
        raise AssertionError(f"Bubblewrap check failed (rc={rc_bwrap}):\n{out_bwrap}")
    results.append(("Bubblewrap sandbox (bwrap --ro-bind / / -- true)", "exit 0"))

    return results


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Boot Mycel OS Void musl ISO in QEMU and run serial-console smoke tests."
    )
    parser.add_argument(
        "--iso",
        type=Path,
        default=None,
        help="Path to ISO image (default: newest output/*.iso)",
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
    iso_path = args.iso.resolve() if args.iso else find_newest_iso(repo_root)
    if not iso_path.is_file():
        print(f"ERROR: ISO file not found: {iso_path}", file=sys.stderr)
        return 1

    iso_size_mb = iso_path.stat().st_size / (1024 * 1024)
    print(f"ISO:     {iso_path} ({iso_size_mb:.1f} MiB)")
    print(f"Timeout: {args.timeout:.0f}s | Memory: {args.memory}")

    harness = QemuSerialHarness(
        iso_path=iso_path,
        memory=args.memory,
        overall_timeout=args.timeout,
    )

    try:
        harness.start()
        print(f"QEMU:    {' '.join(harness.cmd_list)}")
        harness.boot_and_login()
        results = run_smoke_checks(harness)
    except Exception as exc:
        print(f"\nSMOKE TEST FAILED: {exc}", file=sys.stderr)
        transcript = harness.full_transcript()
        if transcript:
            print("\n--- Serial Transcript (tail 4000 chars) ---", file=sys.stderr)
            print(transcript[-4000:], file=sys.stderr)
            print("--- End Serial Transcript ---", file=sys.stderr)
        return 1
    finally:
        harness.shutdown()

    print("\n=== SMOKE TEST SUMMARY ===")
    for name, detail in results:
        print(f"[PASS] {name}")
        for line in detail.splitlines():
            print(f"       {line}")
    print("\nALL SMOKE CHECKS PASSED.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
