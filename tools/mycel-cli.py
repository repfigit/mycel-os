#!/usr/bin/env python3
"""
Mycel CLI - Command line interface for Mycel OS

This is a simple Python CLI for development/testing.
The production CLI will be built into mycel-runtime (Rust).
"""

import argparse
import json
import os
import sys
import socket
import uuid
from pathlib import Path

# Dev mode uses /tmp, production uses /run/mycel
SOCKET_PATH = os.environ.get("MYCEL_SOCKET", "/tmp/mycel-dev.sock")
VERSION = "0.1.0"


def token_path_for_socket(socket_path: str) -> Path:
    """Derive the token file path from a socket path, or use MYCEL_TOKEN_FILE."""
    override = os.environ.get("MYCEL_TOKEN_FILE", "").strip()
    if override:
        return Path(override)
    return Path(socket_path).with_suffix(".token")


def load_auth_token(socket_path: str = SOCKET_PATH) -> str:
    """Load auth token from MYCEL_AUTH_TOKEN or dynamically from the token file."""
    env_token = os.environ.get("MYCEL_AUTH_TOKEN", "").strip()
    if env_token:
        return env_token
    token_path = token_path_for_socket(socket_path)
    try:
        return token_path.read_text(encoding="utf-8").strip()
    except OSError:
        return ""

BANNER = """
    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗
    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║
    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║
    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║
    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗
    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝

    The intelligent network beneath everything.
"""

def _send_once(request: dict, need_auth: bool, token: str, session_id: str = "") -> dict:
    """Open a single Unix socket connection, authenticate if needed, and send request."""
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(120)  # 2 min timeout for LLM responses
    try:
        sock.connect(SOCKET_PATH)
        sock_file = sock.makefile("r", encoding="utf-8")
        try:
            if need_auth and token:
                auth_req = {"type": "Authenticate", "token": token}
                sock.sendall(json.dumps(auth_req).encode("utf-8") + b"\n")
                auth_line = sock_file.readline()
                if not auth_line:
                    return {"type": "Error", "message": "Connection closed during authentication"}
                auth_resp = json.loads(auth_line)
                if auth_resp.get("type") != "Ok":
                    return auth_resp

            if session_id:
                sess_req = {"type": "SetSession", "id": session_id}
                sock.sendall(json.dumps(sess_req).encode("utf-8") + b"\n")
                sess_line = sock_file.readline()
                if not sess_line:
                    return {"type": "Error", "message": "Connection closed during SetSession"}
                sess_resp = json.loads(sess_line)
                if sess_resp.get("type") != "Ok":
                    return sess_resp

            sock.sendall(json.dumps(request).encode("utf-8") + b"\n")

            chunks = []
            while True:
                line = sock_file.readline()
                if not line:
                    if chunks:
                        return {"type": "Chat", "response": "".join(chunks)}
                    return {"type": "Error", "message": "Connection closed without response"}
                line = line.strip()
                if not line:
                    continue
                msg = json.loads(line)
                if msg.get("type") == "ChatChunk":
                    chunks.append(msg.get("delta", msg.get("content", "")))
                    continue
                return msg
        finally:
            sock_file.close()
    finally:
        sock.close()


def send_request(request: dict, need_auth: bool = True, session_id: str = "") -> dict:
    """Send a request to the Mycel runtime via Unix socket."""
    try:
        env_token = os.environ.get("MYCEL_AUTH_TOKEN", "").strip()
        token = load_auth_token(SOCKET_PATH) if need_auth else ""
        resp = _send_once(request, need_auth, token, session_id=session_id)

        # If auth failed and token came from file (not MYCEL_AUTH_TOKEN), re-read and retry once
        if (
            need_auth
            and not env_token
            and resp.get("type") == "Error"
            and "auth" in resp.get("message", "").lower()
        ):
            refreshed = load_auth_token(SOCKET_PATH)
            if refreshed:
                resp = _send_once(request, need_auth, refreshed, session_id=session_id)

        return resp
    except FileNotFoundError:
        return {"type": "Error", "message": f"Mycel runtime not running. Socket not found: {SOCKET_PATH}"}
    except socket.timeout:
        return {"type": "Error", "message": "Request timed out"}
    except Exception as e:
        return {"type": "Error", "message": str(e)}


def cmd_chat(args):
    """Interactive chat mode."""
    print(BANNER)
    print("Type 'quit' or 'exit' to leave.")
    print("Prefix with @local or @cloud to force a provider.\n")
    session_id = str(uuid.uuid4())

    while True:
        try:
            user_input = input("mycel> ").strip()
        except (EOFError, KeyboardInterrupt):
            print("\nThe network rests. Goodbye!")
            break

        if not user_input:
            continue
        if user_input.lower() in ('quit', 'exit'):
            print("The network rests. Goodbye!")
            break

        # Check for provider prefix
        provider = "auto"
        if user_input.startswith("@local "):
            provider = "local"
            user_input = user_input[7:]
        elif user_input.startswith("@cloud "):
            provider = "cloud"
            user_input = user_input[7:]

        response = send_request({
            "type": "Chat",
            "message": user_input,
            "provider": provider
        }, session_id=session_id)

        if response.get("type") == "Error":
            print(f"\nError: {response.get('message', 'Unknown error')}\n")
        elif response.get("type") == "Chat":
            print(f"\n{response.get('response', '')}\n")
        elif response.get("type") == "CodeResult":
            print(f"\n{response.get('output', '')}\n")
        elif response.get("type") == "Ok":
            print(f"\n{response.get('message', 'Ok')}\n")
        elif response.get("type") == "ConfirmationRequired":
            req_id = response.get("id", "")
            print(f"\nConfirmation required (id: {req_id})")
            if response.get("action"):
                print(f"Action: {response['action']}")
            if response.get("reason"):
                print(f"Reason: {response['reason']}")
            if response.get("code"):
                print(f"Code:\n{response['code']}")
            try:
                ans = input("Confirm? [y/N]: ").strip().lower()
            except (EOFError, KeyboardInterrupt):
                ans = "n"
            approved = ans in ("y", "yes")
            confirm_resp = send_request({
                "type": "ConfirmAction",
                "id": req_id,
                "approved": approved,
            }, session_id=session_id)
            if confirm_resp.get("type") == "Error":
                print(f"\nError: {confirm_resp.get('message', 'Unknown error')}\n")
            elif confirm_resp.get("type") == "Chat":
                print(f"\n{confirm_resp.get('response', '')}\n")
            elif confirm_resp.get("type") == "CodeResult":
                print(f"\n{confirm_resp.get('output', '')}\n")
            elif confirm_resp.get("type") == "Ok":
                print(f"\n{confirm_resp.get('message', 'Ok')}\n")
            else:
                print(f"\n{confirm_resp}\n")
        else:
            print(f"\n{response}\n")


def cmd_run(args):
    """Run a single command."""
    user_input = " ".join(args.command)

    # Check for provider prefix
    provider = "auto"
    if user_input.startswith("@local "):
        provider = "local"
        user_input = user_input[7:]
    elif user_input.startswith("@cloud "):
        provider = "cloud"
        user_input = user_input[7:]

    response = send_request({
        "type": "Chat",
        "message": user_input,
        "provider": provider
    })

    if response.get("type") == "Error":
        print(f"Error: {response.get('message', 'Unknown error')}", file=sys.stderr)
        sys.exit(1)
    elif response.get("type") == "Chat":
        print(response.get('response', ''))
    elif response.get("type") == "ConfirmationRequired":
        print(f"Confirmation required (id: {response.get('id', '')})")
        if response.get("action"):
            print(f"Action: {response['action']}")
        if response.get("reason"):
            print(f"Reason: {response['reason']}")
        if response.get("code"):
            print(f"Code:\n{response['code']}")
    else:
        print(json.dumps(response, indent=2))


def cmd_status(args):
    """Show runtime status."""
    response = send_request({"type": "Status"})

    if response.get("type") == "Error":
        print(f"Runtime: Not running")
        print(f"Error: {response.get('message', 'Unknown')}")
        sys.exit(1)
    elif response.get("type") == "Status":
        print(f"Runtime: Running")
        print(f"Version: {response.get('version', 'unknown')}")
        print(f"Sessions: {response.get('sessions', 0)}")
        print(f"LLM Model: {response.get('llm_model', 'unknown')}")
    else:
        print(f"Unexpected response: {response}")


def cmd_providers(args):
    """List configured/available LLM providers."""
    response = send_request({"type": "ListProviders"})

    if response.get("type") == "Error":
        print(f"Error: {response.get('message', 'Unknown error')}", file=sys.stderr)
        sys.exit(1)
    elif response.get("type") == "Providers":
        active_provider = response.get("active_provider")
        active_model = response.get("active_model")
        print(f"Active Provider: {active_provider or 'none'}")
        print(f"Active Model: {active_model or 'none'}")
        providers = response.get("providers", [])
        if not providers:
            print("Providers: none")
            return
        print("Providers:")
        for p in providers:
            pid = p.get("id", "unknown")
            tags = []
            if active_provider and pid == active_provider:
                tags.append("[active]")
            if p.get("configured"):
                tags.append("[configured]")
            tag_str = f" {' '.join(tags)}" if tags else ""
            api = p.get("api", "unknown")
            env_var = p.get("env_var", "")
            default_model = p.get("default_model", "")
            print(f"  - {pid}{tag_str} ({api}) env={env_var} default_model={default_model}")
    else:
        print(f"Unexpected response: {response}", file=sys.stderr)
        sys.exit(1)


def cmd_provider(args):
    """Switch active LLM provider and optional model."""
    response = send_request({
        "type": "SetProvider",
        "provider": args.provider,
        "model": args.model,
    })

    if response.get("type") == "Error":
        print(f"Error: {response.get('message', 'Unknown error')}", file=sys.stderr)
        sys.exit(1)
    elif response.get("type") == "Ok":
        print(response.get("message", "Ok"))
    else:
        print(f"Unexpected response: {response}", file=sys.stderr)
        sys.exit(1)


def _handle_mesh_error(response: dict) -> None:
    """Exit with code 1 if the runtime returned an Error response."""
    if response.get("type") == "Error":
        print(f"Error: {response.get('message', 'Unknown error')}", file=sys.stderr)
        sys.exit(1)
    if "error" in response:
        print(f"Error: {response['error']}", file=sys.stderr)
        sys.exit(1)


def _print_peer_added(response: dict) -> None:
    """Print details from a PeerAdded response."""
    peer = response.get("peer", {})
    addrs = ", ".join(peer.get("addresses", [])) or "none"
    print(f"Peer added: {peer.get('name', 'unnamed')} ({peer.get('id', 'unknown')})")
    print(f"Status: {peer.get('status', 'unknown')}")
    print(f"Addresses: {addrs}")
    print(f"Synced events: {response.get('synced_events', 0)}")


def cmd_mesh(args):
    """Mesh network commands."""
    if args.mesh_cmd == "status":
        response = send_request({"type": "SyncStatus"})
        _handle_mesh_error(response)
        enabled = "enabled" if response.get("enabled") else "disabled"
        print(f"Mesh Sync: {enabled}")
        print(f"Device ID: {response.get('device_id', 'unknown')}")
        print(f"Transport: {response.get('transport', 'unknown')}")
        if response.get("interface"):
            print(f"Interface: {response['interface']}")
        if response.get("listen_addr"):
            print(f"Listen Addr: {response['listen_addr']}")
        if response.get("public_endpoint"):
            print(f"Public Endpoint: {response['public_endpoint']}")
        relay_status = "connected" if bool(response.get("relay_connected", False)) else "disconnected"
        print(f"Relay: {relay_status}")
        print(f"Trusted Peers: {response.get('trusted_peers', 0)}")
        print(f"Connected Peers: {response.get('connected_peers', 0)}")
        print(f"Event Log Length: {response.get('event_log_len', 0)}")
        peers = response.get("peers", [])
        if peers:
            print("Peers:")
            for peer in peers:
                addrs = ", ".join(peer.get("addresses", [])) or "none"
                print(f"  - {peer.get('name', 'unnamed')} ({peer.get('id', 'unknown')}) [{peer.get('status', 'unknown')}] {addrs}")

    elif args.mesh_cmd == "add-device":
        response = send_request({"type": "GeneratePairingCode", "endpoint": args.endpoint})
        _handle_mesh_error(response)
        print(f"Device ID: {response.get('device_id', 'unknown')}")
        if response.get("endpoint"):
            print(f"Endpoint: {response['endpoint']}")
        print(f"Pairing Code: {response.get('code', 'N/A')}")

    elif args.mesh_cmd == "join":
        code = args.code.strip() if args.code else input("Enter pairing code: ").strip()
        response = send_request({"type": "PairPeer", "code": code})
        _handle_mesh_error(response)
        _print_peer_added(response)

    elif args.mesh_cmd == "pair":
        response = send_request({
            "type": "AddTrustedPeer",
            "peer_id": args.peer_id,
            "name": args.name,
            "endpoint": args.endpoint,
        })
        _handle_mesh_error(response)
        _print_peer_added(response)

    elif args.mesh_cmd == "sync":
        response = send_request({"type": "TriggerSync", "peer_id": args.peer})
        _handle_mesh_error(response)
        print(response.get("message", "Sync triggered"))


def cmd_collective(args):
    """Collective network commands."""
    if args.collective_cmd == "status":
        response = send_request({"type": "CollectiveStatus"})
        _handle_mesh_error(response)
        enabled = "enabled" if response.get("enabled") else "disabled"
        print(f"Collective: {enabled}")
        print(f"NEAR Network: {response.get('near_network', 'unknown')}")
        print(f"NEAR RPC: {response.get('near_rpc_url', 'unknown')}")
        print(f"NEAR Account: {response.get('near_account') or 'not configured'}")
        print(f"NEAR Connected: {'yes' if response.get('near_connected') else 'no'}")
        print(f"IPFS Enabled: {'yes' if response.get('ipfs_enabled') else 'no'}")
        print(f"IPFS API: {response.get('ipfs_api', 'unknown')}")
        print(f"Bittensor Enabled: {'yes' if response.get('bittensor_enabled') else 'no'}")
        print(f"Share Patterns: {'yes' if response.get('share_patterns_enabled') else 'no'}")
        print(f"Local Patterns: {response.get('local_patterns', 0)}")
        print(f"Patterns Shared: {response.get('patterns_shared', 0)}")

    elif args.collective_cmd == "patterns":
        response = send_request({"type": "ListPatterns"})
        _handle_mesh_error(response)
        patterns = response.get("patterns", [])
        if not patterns:
            print("No collective patterns stored.")
            return
        print(f"Collective Patterns ({len(patterns)}):")
        for p in patterns:
            pid = p.get("id", "unknown")
            name = p.get("name", "unnamed")
            cat = p.get("category", "general")
            desc = p.get("description", "")
            cid = p.get("ipfs_cid")
            cid_str = f" [cid:{cid}]" if cid else ""
            print(f"  - {name} ({cat}) [{pid}]{cid_str}: {desc}")

    elif args.collective_cmd == "share":
        response = send_request({
            "type": "SharePattern",
            "name": args.name,
            "description": args.description,
            "category": args.category,
        })
        _handle_mesh_error(response)
        pattern = response.get("pattern", {})
        print(f"Shared pattern: {pattern.get('name', args.name)} ({pattern.get('id', 'unknown')})")
        if pattern.get("ipfs_cid"):
            print(f"IPFS CID: {pattern['ipfs_cid']}")
        print(f"Description: {pattern.get('description', '')}")


def _print_wine_result(response: dict) -> None:
    """Print details from a WineResult response."""
    print(f"App: {response.get('app_name', 'unknown')}")
    print(f"Executable: {response.get('exe_path', 'unknown')}")
    print(f"Prefix: {response.get('prefix_path', 'unknown')}")
    print(f"Sandboxed: {'yes' if response.get('sandboxed') else 'no'}")
    print(f"Success: {'yes' if response.get('success') else 'no'}")
    if response.get("timed_out"):
        print("Timed Out: yes")
    if response.get("exit_code") is not None:
        print(f"Exit Code: {response['exit_code']}")
    if response.get("stdout"):
        print(f"Stdout:\n{response['stdout']}")
    if response.get("stderr"):
        print(f"Stderr:\n{response['stderr']}")


def cmd_wine(args):
    """Windows app compatibility (Wine) commands."""
    if args.wine_cmd == "status":
        response = send_request({"type": "WineStatus"})
        _handle_mesh_error(response)
        enabled = "enabled" if response.get("enabled") else "disabled"
        print(f"Wine: {enabled}")
        print(f"Binary: {response.get('wine_binary', 'unknown')}")
        print(f"Available: {'yes' if response.get('available') else 'no'}")
        if response.get("resolved_binary"):
            print(f"Resolved Binary: {response['resolved_binary']}")
        if response.get("wine_version"):
            print(f"Version: {response['wine_version']}")
        print(f"Sandbox (bwrap): {'yes' if response.get('sandbox_with_bwrap') else 'no'}")
        print(f"Allow Network: {'yes' if response.get('allow_network') else 'no'}")
        if response.get("prefix_root"):
            print(f"Prefix Root: {response['prefix_root']}")
        prefixes = response.get("prefixes", [])
        if prefixes:
            print(f"Prefixes: {', '.join(prefixes)}")
        if response.get("install_guidance"):
            print(f"Guidance: {response['install_guidance']}")

    elif args.wine_cmd == "run":
        response = send_request({
            "type": "RunWindowsApp",
            "app_name": args.app,
            "exe_path": args.exe,
            "args": args.args,
        })
        if response.get("type") == "Error":
            print(f"Error: {response.get('message', 'Unknown error')}", file=sys.stderr)
            sys.exit(1)
        elif response.get("type") == "ConfirmationRequired":
            req_id = response.get("id", "")
            print(f"Confirmation required (id: {req_id})")
            if response.get("action"):
                print(f"Action: {response['action']}")
            if response.get("reason"):
                print(f"Reason: {response['reason']}")
            try:
                ans = input("Confirm? [y/N]: ").strip().lower()
            except (EOFError, KeyboardInterrupt):
                ans = "n"
            approved = ans in ("y", "yes")
            confirm_resp = send_request({
                "type": "ConfirmAction",
                "id": req_id,
                "approved": approved,
            })
            if confirm_resp.get("type") == "Error":
                print(f"Error: {confirm_resp.get('message', 'Unknown error')}", file=sys.stderr)
                sys.exit(1)
            elif confirm_resp.get("type") == "WineResult":
                _print_wine_result(confirm_resp)
            elif confirm_resp.get("type") == "Ok":
                print(confirm_resp.get("message", "Ok"))
            else:
                print(json.dumps(confirm_resp, indent=2))
        elif response.get("type") == "WineResult":
            _print_wine_result(response)
        else:
            print(json.dumps(response, indent=2))


def main():
    # Check for direct query mode first (most common use case)
    if len(sys.argv) > 1 and sys.argv[1] not in ('chat', 'run', 'status', 'providers', 'provider', 'mesh', 'collective', 'wine', '-h', '--help', '--version'):
        # Direct query: mycel "tell me a joke"
        class Args:
            command = sys.argv[1:]
        cmd_run(Args())
        return

    parser = argparse.ArgumentParser(
        description="Mycel CLI - The intelligent network beneath everything",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  mycel                         Start interactive chat
  mycel "tell me a joke"        Run a single query
  mycel "@cloud explain quantum" Force cloud LLM
  mycel "@local what time is it" Force local LLM
  mycel status                  Show runtime status
  mycel providers               List available LLM providers
  mycel provider venice llama-3.3-70b  Switch active provider and model
  mycel mesh status             Show mesh sync status
  mycel mesh add-device         Generate a mesh pairing code
  mycel mesh join               Join a mesh using a pairing code

Environment:
  MYCEL_SOCKET      Socket path (default: /tmp/mycel-dev.sock)
  MYCEL_TOKEN_FILE  Token file path (default: <socket_stem>.token, e.g. /tmp/mycel-dev.token)
  MYCEL_AUTH_TOKEN  Auth token override (takes precedence over token file)
"""
    )

    parser.add_argument('--version', action='version', version=f'Mycel CLI {VERSION}')

    subparsers = parser.add_subparsers(dest='cmd')

    # Chat (default)
    chat_parser = subparsers.add_parser('chat', help='Interactive chat mode')
    chat_parser.set_defaults(func=cmd_chat)

    # Run
    run_parser = subparsers.add_parser('run', help='Run a single command')
    run_parser.add_argument('command', nargs='+', help='Command to run')
    run_parser.set_defaults(func=cmd_run)

    # Status
    status_parser = subparsers.add_parser('status', help='Show runtime status')
    status_parser.set_defaults(func=cmd_status)

    # Providers
    providers_parser = subparsers.add_parser('providers', help='List available LLM providers')
    providers_parser.set_defaults(func=cmd_providers)

    # Provider
    provider_parser = subparsers.add_parser('provider', help='Switch active LLM provider')
    provider_parser.add_argument('provider', help='Provider ID (or provider/model)')
    provider_parser.add_argument('model', nargs='?', default=None, help='Optional model name')
    provider_parser.set_defaults(func=cmd_provider)

    # Mesh
    mesh_parser = subparsers.add_parser('mesh', help='Mesh network commands')
    mesh_subparsers = mesh_parser.add_subparsers(dest='mesh_cmd', required=True)

    mesh_status_parser = mesh_subparsers.add_parser('status', help='Show mesh sync status')
    mesh_status_parser.set_defaults(func=cmd_mesh)

    mesh_add_parser = mesh_subparsers.add_parser('add-device', help='Generate a pairing code for another device')
    mesh_add_parser.add_argument('--endpoint', default=None, help='Optional HOST:PORT endpoint to advertise')
    mesh_add_parser.set_defaults(func=cmd_mesh)

    mesh_join_parser = mesh_subparsers.add_parser('join', help='Pair with a peer using a pairing code')
    mesh_join_parser.add_argument('code', nargs='?', default=None, help='Pairing code or raw base64 peer ID')
    mesh_join_parser.set_defaults(func=cmd_mesh)

    mesh_pair_parser = mesh_subparsers.add_parser('pair', help='Add a trusted peer directly')
    mesh_pair_parser.add_argument('peer_id', help='Base64 peer public key')
    mesh_pair_parser.add_argument('--name', default=None, help='Optional peer name')
    mesh_pair_parser.add_argument('--endpoint', default=None, help='Optional HOST:PORT endpoint')
    mesh_pair_parser.set_defaults(func=cmd_mesh)

    mesh_sync_parser = mesh_subparsers.add_parser('sync', help='Trigger mesh sync')
    mesh_sync_parser.add_argument('--peer', default=None, help='Optional peer ID to sync with')
    mesh_sync_parser.set_defaults(func=cmd_mesh)

    # Collective
    collective_parser = subparsers.add_parser('collective', help='Collective network commands')
    collective_subparsers = collective_parser.add_subparsers(dest='collective_cmd', required=True)

    collective_status_parser = collective_subparsers.add_parser('status', help='Show collective status')
    collective_status_parser.set_defaults(func=cmd_collective)

    collective_patterns_parser = collective_subparsers.add_parser('patterns', help='List stored collective patterns')
    collective_patterns_parser.set_defaults(func=cmd_collective)

    collective_share_parser = collective_subparsers.add_parser('share', help='Share a pattern with the collective')
    collective_share_parser.add_argument('name', help='Pattern name')
    collective_share_parser.add_argument('description', help='Pattern description')
    collective_share_parser.add_argument('--category', default='general', help='Pattern category (default: general)')
    collective_share_parser.set_defaults(func=cmd_collective)

    # Wine
    wine_parser = subparsers.add_parser('wine', help='Windows application compatibility (Wine)')
    wine_subparsers = wine_parser.add_subparsers(dest='wine_cmd', required=True)

    wine_status_parser = wine_subparsers.add_parser('status', help='Show Wine subsystem status')
    wine_status_parser.set_defaults(func=cmd_wine)

    wine_run_parser = wine_subparsers.add_parser('run', help='Run a Windows executable in an isolated Wine prefix')
    wine_run_parser.add_argument('app', help='Application name (used for Wine prefix isolation)')
    wine_run_parser.add_argument('exe', help='Path to Windows executable (.exe)')
    wine_run_parser.add_argument('args', nargs=argparse.REMAINDER, help='Arguments passed to the executable')
    wine_run_parser.set_defaults(func=cmd_wine)

    args = parser.parse_args()

    # Default to chat mode if no subcommand
    if args.cmd is None:
        cmd_chat(args)
    else:
        args.func(args)


if __name__ == '__main__':
    main()
