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
from pathlib import Path

# Dev mode uses /tmp, production uses /run/mycel
SOCKET_PATH = os.environ.get("MYCEL_SOCKET", "/tmp/mycel-dev.sock")
AUTH_TOKEN = os.environ.get("MYCEL_AUTH_TOKEN", "")
VERSION = "0.1.0"

BANNER = """
    ███╗   ███╗██╗   ██╗ ██████╗███████╗██╗
    ████╗ ████║╚██╗ ██╔╝██╔════╝██╔════╝██║
    ██╔████╔██║ ╚████╔╝ ██║     █████╗  ██║
    ██║╚██╔╝██║  ╚██╔╝  ██║     ██╔══╝  ██║
    ██║ ╚═╝ ██║   ██║   ╚██████╗███████╗███████╗
    ╚═╝     ╚═╝   ╚═╝    ╚═════╝╚══════╝╚══════╝

    The intelligent network beneath everything.
"""

def send_request(request: dict, need_auth: bool = True) -> dict:
    """Send a request to the Mycel runtime via Unix socket."""
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(120)  # 2 min timeout for LLM responses
        sock.connect(SOCKET_PATH)

        # Authenticate first if needed and we have a token
        if need_auth and AUTH_TOKEN:
            auth_req = {"type": "Authenticate", "token": AUTH_TOKEN}
            sock.sendall(json.dumps(auth_req).encode() + b'\n')
            auth_resp = b''
            while b'\n' not in auth_resp:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                auth_resp += chunk

        # Send request
        sock.sendall(json.dumps(request).encode() + b'\n')

        # Receive response
        response = b''
        while True:
            chunk = sock.recv(4096)
            if not chunk:
                break
            response += chunk
            if b'\n' in chunk:
                break

        sock.close()
        return json.loads(response.decode())
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
        })

        if response.get("type") == "Error":
            print(f"\nError: {response.get('message', 'Unknown error')}\n")
        elif response.get("type") == "Chat":
            print(f"\n{response.get('response', '')}\n")
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


def cmd_mesh(args):
    """Mesh network commands."""
    if args.mesh_cmd == "status":
        response = send_request({"type": "mesh_status"})
        if "error" in response:
            print(f"Error: {response['error']}")
        else:
            print(f"Mesh Status: {response.get('status', 'unknown')}")
            print(f"Devices: {response.get('device_count', 0)}")
            for device in response.get('devices', []):
                status = "●" if device.get('online') else "○"
                print(f"  {status} {device['name']} - {device.get('last_sync', 'never')}")

    elif args.mesh_cmd == "add-device":
        response = send_request({"type": "mesh_add_device"})
        if "error" in response:
            print(f"Error: {response['error']}")
        else:
            print(f"Pairing code: {response.get('pairing_code', 'N/A')}")
            print(f"Expires in: {response.get('expires_in', 'N/A')}")
            if response.get('qr_code'):
                print(f"\nQR Code:\n{response['qr_code']}")

    elif args.mesh_cmd == "join":
        code = input("Enter pairing code: ").strip()
        response = send_request({"type": "mesh_join", "code": code})
        if "error" in response:
            print(f"Error: {response['error']}")
        else:
            print(f"Successfully joined mesh!")


def cmd_collective(args):
    """Collective network commands."""
    if args.collective_cmd == "status":
        response = send_request({"type": "collective_status"})
        if "error" in response:
            print(f"Error: {response['error']}")
        else:
            print(f"NEAR Account: {response.get('near_account', 'not configured')}")
            print(f"Bittensor Hotkey: {response.get('bittensor_hotkey', 'not configured')}")
            print(f"Patterns Shared: {response.get('patterns_shared', 0)}")
            print(f"Patterns Adopted: {response.get('patterns_adopted', 0)}")
            print(f"Rewards Earned: {response.get('rewards', '0 TAO')}")

    elif args.collective_cmd == "share":
        response = send_request({"type": "collective_share"})
        if "error" in response:
            print(f"Error: {response['error']}")
        else:
            print(f"Shared {response.get('count', 0)} patterns to the collective.")


def main():
    # Check for direct query mode first (most common use case)
    if len(sys.argv) > 1 and sys.argv[1] not in ('chat', 'run', 'status', 'mesh', 'collective', '-h', '--help', '--version'):
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

Environment:
  MYCEL_SOCKET      Socket path (default: /tmp/mycel-dev.sock)
  MYCEL_AUTH_TOKEN  Auth token from runtime logs
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

    # Mesh
    mesh_parser = subparsers.add_parser('mesh', help='Mesh network commands')
    mesh_parser.add_argument('mesh_cmd', choices=['status', 'add-device', 'join'],
                            help='Mesh subcommand')
    mesh_parser.set_defaults(func=cmd_mesh)

    # Collective
    collective_parser = subparsers.add_parser('collective', help='Collective network commands')
    collective_parser.add_argument('collective_cmd', choices=['status', 'share'],
                                   help='Collective subcommand')
    collective_parser.set_defaults(func=cmd_collective)

    args = parser.parse_args()

    # Default to chat mode if no subcommand
    if args.cmd is None:
        cmd_chat(args)
    else:
        args.func(args)


if __name__ == '__main__':
    main()
