#!/usr/bin/env python3
"""
Automated 2-VM Kernel WireGuard (`mycel0`) E2E test for Mycel OS.

Boots two Void Linux musl QEMU+KVM VMs connected by a shared QEMU socket virtual LAN:
  - Node A: lan0 (192.168.77.1/24, listen) + wan0 (user-mode NAT to host Ollama 10.0.2.2:11434)
  - Node B: lan0 (192.168.77.2/24, connect)

Verifies:
  1. Virtual LAN connectivity between Node A and Node B
  2. Real kernel WireGuard interface `mycel0` up on both VMs (`wg show mycel0`, `mycel mesh status`)
  3. Real `mycel mesh add-device` / `mycel mesh join` pairing over LAN endpoints
  4. Kernel WireGuard handshake (`wg show mycel0 latest-handshakes` > 0) and `ping` over `mycel0`
  5. Untrusted peer WireFrame rejection over `mycel0` (`sync_log.jsonl` count unchanged)
  6. Offline mutation on Node A (`DefineCommand` via `mycel` Chat + `AddConversationTurn` via `mycel "list commands"`)
  7. Automatic catch-up convergence on Node B after coming back online (`sync_log.jsonl` + live `CommandRegistry`)

Uses Python 3 standard library only.
"""

import argparse
import base64
import glob
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07")


def strip_ansi(text: str) -> str:
    return ANSI_ESCAPE_RE.sub("", text).replace("\r", "")


def find_default_iso(repo_root: Path) -> Path:
    smoke_pattern = str(repo_root / "output" / "mycel-os-smoke-*.iso")
    smoke_candidates = [Path(p) for p in glob.glob(smoke_pattern) if os.path.isfile(p)]
    if smoke_candidates:
        smoke_candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
        return smoke_candidates[0]

    all_pattern = str(repo_root / "output" / "*.iso")
    all_candidates = [Path(p) for p in glob.glob(all_pattern) if os.path.isfile(p)]
    if not all_candidates:
        raise FileNotFoundError(
            f"No ISO files found matching {smoke_pattern} or {all_pattern}"
        )
    all_candidates.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return all_candidates[0]


def pick_free_tcp_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class QemuVmHarness:
    """Manages a single QEMU+KVM VM with serial console attached to a PTY."""

    def __init__(
        self,
        name: str,
        iso_path: Path,
        memory: str,
        deadline: float,
        extra_qemu_args: list[str],
    ):
        self.name = name
        self.iso_path = iso_path
        self.memory = memory
        self.deadline = deadline
        self.extra_qemu_args = extra_qemu_args
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
            *self.extra_qemu_args,
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
            raise RuntimeError(f"[{self.name}] PTY is not open")
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
                    f"[{self.name}] QEMU exited prematurely with code {self.proc.returncode}"
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
            f"[{self.name}] Timed out waiting for pattern {pattern.pattern!r}. Last serial output:\n{tail}"
        )

    def wait_for_login_and_login(self) -> None:
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

    def must_run(self, command: str, timeout: float = 30.0) -> str:
        rc, out = self.run_cmd(command, timeout=timeout)
        if rc != 0:
            raise AssertionError(
                f"[{self.name}] Command failed (rc={rc}): {command}\nOutput:\n{out}"
            )
        return out

    def shutdown(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            try:
                self.send("\rpoweroff -f\r")
            except OSError:
                pass
            for _ in range(20):
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


def write_guest_file(vm: QemuVmHarness, path: str, content: str) -> None:
    b64 = base64.b64encode(content.encode("utf-8")).decode("ascii")
    vm.must_run(
        f"mkdir -p \"$(dirname '{path}')\" && "
        f"printf '%s' '{b64}' | base64 -d > '{path}'",
        timeout=15.0,
    )


def find_iface_by_mac(vm: QemuVmHarness, mac: str) -> str:
    out = vm.must_run(
        f"for f in /sys/class/net/*/address; do "
        f"if [ \"$(cat \"$f\")\" = \"{mac}\" ]; then "
        f"basename \"$(dirname \"$f\")\"; exit 0; fi; done; exit 1",
        timeout=15.0,
    )
    iface = out.strip().splitlines()[-1].strip()
    if not iface:
        raise AssertionError(f"[{vm.name}] Could not find interface for MAC {mac}")
    return iface


def build_mycel_config(wg_address: str) -> str:
    return (
        'ollama_url = "http://10.0.2.2:11434"\n'
        'local_model = "qwen3:8b"\n'
        'ipc_socket_path = "/run/mycel/runtime.sock"\n'
        'context_path = "/var/lib/mycel"\n'
        'code_path = "/var/cache/mycel/code"\n'
        "\n"
        "[mcp]\n"
        "enabled = false\n"
        "\n"
        "[sandbox]\n"
        'mode = "bwrap"\n'
        "\n"
        "[mesh]\n"
        "enabled = true\n"
        'transport = "wireguard"\n'
        'interface = "mycel0"\n'
        "listen_port = 51820\n"
        "sync_port = 51821\n"
        f'wg_address = "{wg_address}"\n'
        "mdns_enabled = false\n"
        "sync_ai_context = true\n"
    )


def restart_and_wait_runtime(vm: QemuVmHarness) -> None:
    vm.must_run("sv restart /var/service/mycel-runtime", timeout=20.0)
    vm.must_run(
        "for i in $(seq 1 30); do "
        "sv status /var/service/mycel-runtime 2>&1 | grep -q '^run:' && "
        "[ -S /run/mycel/runtime.sock ] && "
        "[ -f /run/mycel/runtime.token ] && "
        "ip link show mycel0 >/dev/null 2>&1 && exit 0; "
        "sleep 1; "
        "done; "
        "sv status /var/service/mycel-runtime 2>&1; "
        "ip link show mycel0 2>&1 || true; "
        "exit 1",
        timeout=40.0,
    )


def get_sync_log_entries(vm: QemuVmHarness) -> list[dict]:
    out = vm.must_run(
        "if [ -f /var/lib/mycel/sync_log.jsonl ]; then "
        "cat /var/lib/mycel/sync_log.jsonl; fi",
        timeout=15.0,
    )
    entries: list[dict] = []
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        entries.append(json.loads(line))
    return entries


def has_handshake(vm: QemuVmHarness) -> tuple[bool, str]:
    out = vm.must_run("wg show mycel0 latest-handshakes", timeout=15.0)
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].isdigit() and int(parts[1]) > 0:
            return True, out
    return False, out


def extract_pairing_code(output: str) -> str:
    m = re.search(r"Pairing Code:\s*(\S+)", output)
    if not m:
        raise AssertionError(f"Could not parse Pairing Code from output:\n{output}")
    return m.group(1)


def run_2vm_e2e(node_a: QemuVmHarness, node_b: QemuVmHarness) -> list[tuple[str, str]]:
    results: list[tuple[str, str]] = []

    # 1. Boot & Login both VMs concurrently (they are already starting)
    print("[1/9] Logging into Node A and Node B...")
    node_a.wait_for_login_and_login()
    node_b.wait_for_login_and_login()
    results.append(("Boot & login both VMs", "Node A and Node B logged in as root"))

    # 2. Configure virtual LAN interfaces by MAC and verify LAN ping
    print("[2/9] Configuring virtual LAN (192.168.77.1/24 <-> 192.168.77.2/24)...")
    lan_a = find_iface_by_mac(node_a, "52:54:00:12:34:01")
    wan_a = find_iface_by_mac(node_a, "52:54:00:12:34:11")
    lan_b = find_iface_by_mac(node_b, "52:54:00:12:34:02")

    node_a.must_run(
        f"ip addr flush dev {lan_a} || true; "
        f"ip addr add 192.168.77.1/24 dev {lan_a} && ip link set {lan_a} up",
        timeout=15.0,
    )
    node_b.must_run(
        f"ip addr flush dev {lan_b} || true; "
        f"ip addr add 192.168.77.2/24 dev {lan_b} && ip link set {lan_b} up",
        timeout=15.0,
    )
    # Ensure wan0 on Node A has user-net IPv4 and default route to 10.0.2.2 (Ollama)
    node_a.must_run(
        f"ip link set {wan_a} up && "
        f"(ip -4 addr show dev {wan_a} | grep -q '10\\.0\\.2\\.' || "
        f"(ip addr add 10.0.2.15/24 dev {wan_a} && ip route replace default via 10.0.2.2 dev {wan_a}))",
        timeout=15.0,
    )

    ping_lan = node_a.must_run("ping -c 1 -W 3 192.168.77.2", timeout=15.0)
    results.append(
        (
            "Virtual LAN connectivity",
            f"Node A ({lan_a}=192.168.77.1) -> Node B ({lan_b}=192.168.77.2)\n{ping_lan}",
        )
    )

    # 3. Write /etc/mycel/config.toml on both VMs and restart mycel-runtime
    print("[3/9] Writing /etc/mycel/config.toml and restarting mycel-runtime on both VMs...")
    write_guest_file(node_a, "/etc/mycel/config.toml", build_mycel_config("10.77.0.1/24"))
    write_guest_file(node_b, "/etc/mycel/config.toml", build_mycel_config("10.77.0.2/24"))
    restart_and_wait_runtime(node_a)
    restart_and_wait_runtime(node_b)

    # Verify mycel0 UP + mycel mesh status on BOTH VMs
    a_ip = node_a.must_run("ip link show mycel0 && ip addr show mycel0 && wg show mycel0", timeout=15.0)
    b_ip = node_b.must_run("ip link show mycel0 && ip addr show mycel0 && wg show mycel0", timeout=15.0)
    a_mesh = node_a.must_run("MYCEL_SOCKET=/run/mycel/runtime.sock mycel mesh status", timeout=15.0)
    b_mesh = node_b.must_run("MYCEL_SOCKET=/run/mycel/runtime.sock mycel mesh status", timeout=15.0)

    for label, mesh_out in [("Node A", a_mesh), ("Node B", b_mesh)]:
        if "wireguard" not in mesh_out.lower() or "mycel0" not in mesh_out:
            raise AssertionError(
                f"{label} `mycel mesh status` missing wireguard/mycel0:\n{mesh_out}"
            )
    results.append(
        (
            "Kernel WireGuard mycel0 & mycel mesh status",
            f"--- Node A ---\n{a_ip}\n{a_mesh}\n--- Node B ---\n{b_ip}\n{b_mesh}",
        )
    )

    # 4. Pair Node A and Node B via real `mycel mesh` CLI
    print("[4/9] Pairing Node A and Node B via `mycel mesh add-device` / `mycel mesh join`...")
    add_a = node_a.must_run(
        "MYCEL_SOCKET=/run/mycel/runtime.sock mycel mesh add-device --endpoint 192.168.77.1:51820",
        timeout=15.0,
    )
    code_a = extract_pairing_code(add_a)
    join_b = node_b.must_run(
        f"MYCEL_SOCKET=/run/mycel/runtime.sock mycel mesh join '{code_a}'",
        timeout=20.0,
    )

    add_b = node_b.must_run(
        "MYCEL_SOCKET=/run/mycel/runtime.sock mycel mesh add-device --endpoint 192.168.77.2:51820",
        timeout=15.0,
    )
    code_b = extract_pairing_code(add_b)
    join_a = node_a.must_run(
        f"MYCEL_SOCKET=/run/mycel/runtime.sock mycel mesh join '{code_b}'",
        timeout=20.0,
    )
    results.append(
        (
            "Mesh CLI pairing (add-device + join)",
            f"A->B:\n{add_a}\n{join_b}\nB->A:\n{add_b}\n{join_a}",
        )
    )

    # 5. Verify kernel WireGuard handshake and ping over mycel0
    print("[5/9] Verifying kernel WireGuard handshake and ping over mycel0...")
    ping_wg = node_a.must_run("ping -c 1 -W 5 10.77.0.2", timeout=15.0)
    hs_a_ok, hs_a_out = False, ""
    hs_b_ok, hs_b_out = False, ""
    for _ in range(10):
        hs_a_ok, hs_a_out = has_handshake(node_a)
        hs_b_ok, hs_b_out = has_handshake(node_b)
        if hs_a_ok and hs_b_ok:
            break
        time.sleep(0.5)
    if not (hs_a_ok and hs_b_ok):
        raise AssertionError(
            f"Kernel WireGuard handshake missing!\nNode A:\n{hs_a_out}\nNode B:\n{hs_b_out}"
        )
    results.append(
        (
            "Kernel WireGuard handshake & tunnel ping",
            f"{ping_wg}\nNode A handshakes:\n{hs_a_out}\nNode B handshakes:\n{hs_b_out}",
        )
    )

    # 6. Verify untrusted peer rejection over mycel0
    print("[6/9] Verifying untrusted WireFrame rejection on 10.77.0.1:51821...")
    before_count = len(get_sync_log_entries(node_a))
    untrusted_py = (
        "python3 -c '"
        "import base64, json, os, socket; "
        "frame = json.dumps({"
        "\"sender_id\": base64.b64encode(os.urandom(32)).decode(\"ascii\"), "
        "\"nonce\": [0]*12, "
        "\"encrypted_payload\": [1, 2, 3, 4, 5]"
        "}) + \"\\n\"; "
        "s = socket.create_connection((\"10.77.0.1\", 51821), timeout=5); "
        "s.sendall(frame.encode(\"utf-8\")); "
        "s.close()"
        "'"
    )
    node_b.must_run(untrusted_py, timeout=15.0)
    time.sleep(1.0)
    after_count = len(get_sync_log_entries(node_a))
    if after_count != before_count:
        raise AssertionError(
            f"Untrusted peer WireFrame modified Node A sync_log.jsonl! before={before_count}, after={after_count}"
        )
    results.append(
        (
            "Untrusted peer WireFrame rejection",
            f"Sent random-key WireFrame to 10.77.0.1:51821; Node A sync_log.jsonl count stayed {before_count}",
        )
    )

    # 7. Take Node B offline
    print("[7/9] Taking Node B offline (sv down mycel-runtime + ip link set mycel0 down)...")
    node_b.must_run(
        "sv down /var/service/mycel-runtime && ip link set mycel0 down",
        timeout=15.0,
    )
    results.append(("Node B offline", "mycel-runtime stopped and mycel0 down on Node B"))

    # 8. Mutate Node A while Node B is offline
    print("[8/9] Creating command + conversation turn on Node A while Node B is offline...")
    chat_out = node_a.must_run(
        "printf 'make a command called meshhello that runs echo MESH_CMD_OK\\nyes\\nquit\\n' | "
        "MYCEL_SOCKET=/run/mycel/runtime.sock mycel",
        timeout=90.0,
    )
    list_a = node_a.must_run(
        'MYCEL_SOCKET=/run/mycel/runtime.sock mycel "list commands"',
        timeout=60.0,
    )
    entries_a = get_sync_log_entries(node_a)
    ops_a = [e.get("operation", {}) for e in entries_a]
    has_define_a = any(
        "DefineCommand" in op and op["DefineCommand"].get("name") == "meshhello"
        for op in ops_a
    )
    has_turn_a = any("AddConversationTurn" in op for op in ops_a)
    cmds_json_a = node_a.must_run("cat /var/lib/mycel/commands.json", timeout=15.0)

    if not has_define_a:
        raise AssertionError(
            f"Node A sync_log.jsonl missing DefineCommand(meshhello)!\nChat:\n{chat_out}\nLog:\n{entries_a}"
        )
    if not has_turn_a:
        raise AssertionError(
            f"Node A sync_log.jsonl missing AddConversationTurn!\nList:\n{list_a}\nLog:\n{entries_a}"
        )
    if "meshhello" not in cmds_json_a:
        raise AssertionError(f"Node A commands.json missing meshhello:\n{cmds_json_a}")

    results.append(
        (
            "Node A offline mutations (DefineCommand + AddConversationTurn)",
            f"sync_log.jsonl entries={len(entries_a)}, DefineCommand(meshhello)=True, AddConversationTurn=True\n"
            f"Chat output:\n{chat_out}\nList commands:\n{list_a}",
        )
    )

    # 9. Bring Node B back online and wait for automatic catch-up convergence
    print("[9/9] Bringing Node B back online and waiting for automatic catch-up convergence...")
    node_b.must_run("sv up /var/service/mycel-runtime", timeout=20.0)
    node_b.must_run(
        "for i in $(seq 1 25); do "
        "sv status /var/service/mycel-runtime 2>&1 | grep -q '^run:' && "
        "[ -S /run/mycel/runtime.sock ] && "
        "ip link show mycel0 >/dev/null 2>&1 && exit 0; "
        "sleep 1; "
        "done; exit 1",
        timeout=30.0,
    )

    converged = False
    entries_b: list[dict] = []
    hs_b_reconnect = ""
    poll_deadline = min(node_b.deadline, time.monotonic() + 25.0)
    while time.monotonic() < poll_deadline:
        hs_ok, hs_b_reconnect = has_handshake(node_b)
        entries_b = get_sync_log_entries(node_b)
        ops_b = [e.get("operation", {}) for e in entries_b]
        has_def_b = any(
            "DefineCommand" in op and op["DefineCommand"].get("name") == "meshhello"
            for op in ops_b
        )
        has_turn_b = any("AddConversationTurn" in op for op in ops_b)
        if hs_ok and has_def_b and has_turn_b:
            converged = True
            break
        time.sleep(1.0)

    if not converged:
        raise AssertionError(
            f"Node B did not converge within 25s!\n"
            f"Handshakes:\n{hs_b_reconnect}\n"
            f"Node B sync_log.jsonl ({len(entries_b)} entries):\n{json.dumps(entries_b, indent=2)}"
        )

    list_b = node_b.must_run(
        'MYCEL_SOCKET=/run/mycel/runtime.sock mycel "list commands"',
        timeout=60.0,
    )
    if "meshhello" not in list_b:
        raise AssertionError(
            f"Node B `mycel \"list commands\"` did not show meshhello in live CommandRegistry:\n{list_b}"
        )

    results.append(
        (
            "Node B reconnect & automatic catch-up convergence",
            f"Handshakes:\n{hs_b_reconnect}\n"
            f"Node B sync_log.jsonl entries={len(entries_b)} (DefineCommand + AddConversationTurn present)\n"
            f"Node B `mycel \"list commands\"`:\n{list_b}",
        )
    )

    return results


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Boot 2 Mycel OS QEMU+KVM VMs and run live Kernel WireGuard (mycel0) E2E tests."
    )
    parser.add_argument(
        "--iso",
        type=Path,
        default=None,
        help="Path to ISO image (default: newest output/mycel-os-smoke-*.iso, else newest output/*.iso)",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=300.0,
        help="Overall timeout in seconds (default: 300)",
    )
    parser.add_argument(
        "--memory",
        default="1536M",
        help="QEMU guest memory size per VM (default: 1536M)",
    )
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    iso_path = args.iso.resolve() if args.iso else find_default_iso(repo_root)
    if not iso_path.is_file():
        print(f"ERROR: ISO file not found: {iso_path}", file=sys.stderr)
        return 1

    lan_port = pick_free_tcp_port()
    deadline = time.monotonic() + args.timeout

    iso_size_mb = iso_path.stat().st_size / (1024 * 1024)
    print(f"ISO:      {iso_path} ({iso_size_mb:.1f} MiB)")
    print(f"Timeout:  {args.timeout:.0f}s | Memory: {args.memory} per VM | LAN Port: 127.0.0.1:{lan_port}")

    node_a = QemuVmHarness(
        name="node-a",
        iso_path=iso_path,
        memory=args.memory,
        deadline=deadline,
        extra_qemu_args=[
            "-netdev",
            f"socket,id=lan0,listen=127.0.0.1:{lan_port}",
            "-device",
            "virtio-net-pci,netdev=lan0,mac=52:54:00:12:34:01",
            "-netdev",
            "user,id=wan0",
            "-device",
            "virtio-net-pci,netdev=wan0,mac=52:54:00:12:34:11",
        ],
    )
    node_b = QemuVmHarness(
        name="node-b",
        iso_path=iso_path,
        memory=args.memory,
        deadline=deadline,
        extra_qemu_args=[
            "-netdev",
            f"socket,id=lan0,connect=127.0.0.1:{lan_port}",
            "-device",
            "virtio-net-pci,netdev=lan0,mac=52:54:00:12:34:02",
        ],
    )

    try:
        node_a.start()
        print(f"Node A:   {' '.join(node_a.cmd_list)}")
        time.sleep(1.0)
        node_b.start()
        print(f"Node B:   {' '.join(node_b.cmd_list)}")

        results = run_2vm_e2e(node_a, node_b)
    except Exception as exc:
        print(f"\n2-VM WIREGUARD E2E FAILED: {exc}", file=sys.stderr)
        for vm in (node_a, node_b):
            transcript = vm.full_transcript()
            if transcript:
                print(f"\n--- {vm.name} serial transcript (tail 3000 chars) ---", file=sys.stderr)
                print(transcript[-3000:], file=sys.stderr)
        return 1
    finally:
        node_b.shutdown()
        node_a.shutdown()

    print("\n=== 2-VM KERNEL WIREGUARD E2E PASSED ===")
    for title, detail in results:
        print(f"\n[PASS] {title}")
        for line in detail.splitlines():
            print(f"  {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
