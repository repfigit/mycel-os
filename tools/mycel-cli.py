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

SOCKET_PATH = "/run/mycel/runtime.sock"
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

def send_request(request: dict) -> dict:
    """Send a request to the Mycel runtime via Unix socket."""
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(SOCKET_PATH)
        
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
        return {"error": "Mycel runtime not running. Start with: mycel-runtime"}
    except Exception as e:
        return {"error": str(e)}


def cmd_chat(args):
    """Interactive chat mode."""
    print(BANNER)
    print("Type 'quit' or 'exit' to leave.\n")
    
    session_id = os.urandom(16).hex()
    
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
        
        response = send_request({
            "type": "chat",
            "session_id": session_id,
            "input": user_input
        })
        
        if "error" in response:
            print(f"\nError: {response['error']}\n")
        elif "text" in response:
            print(f"\n{response['text']}\n")
        elif "code" in response:
            print(f"\n--- Code ---\n{response['code']}")
            print(f"\n--- Output ---\n{response['output']}\n")
        else:
            print(f"\n{response}\n")


def cmd_run(args):
    """Run a single command."""
    response = send_request({
        "type": "chat",
        "session_id": "cli-oneshot",
        "input": " ".join(args.command)
    })
    
    if "error" in response:
        print(f"Error: {response['error']}", file=sys.stderr)
        sys.exit(1)
    elif "text" in response:
        print(response['text'])
    elif "code" in response:
        print(f"--- Code ---\n{response['code']}")
        print(f"--- Output ---\n{response['output']}")
    else:
        print(json.dumps(response, indent=2))


def cmd_status(args):
    """Show runtime status."""
    response = send_request({"type": "status"})
    
    if "error" in response:
        print(f"Runtime: Not running")
        print(f"Error: {response['error']}")
        sys.exit(1)
    else:
        print(f"Runtime: Running")
        print(f"Version: {response.get('version', 'unknown')}")
        print(f"Local AI: {response.get('local_ai', 'unknown')}")
        print(f"Cloud AI: {response.get('cloud_ai', 'disabled')}")
        print(f"Mesh: {response.get('mesh', 'disabled')}")
        print(f"Collective: {response.get('collective', 'disabled')}")


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
    parser = argparse.ArgumentParser(
        description="Mycel CLI - The intelligent network beneath everything",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  mycel                     Start interactive chat
  mycel "find large files"  Run a single command
  mycel status              Show runtime status
  mycel mesh status         Show mesh network status
  mycel collective status   Show collective network status
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
    
    # If no subcommand, check if there are positional args (treat as run)
    # Otherwise, start chat mode
    if args.cmd is None:
        if len(sys.argv) > 1 and not sys.argv[1].startswith('-'):
            # Treat as a command
            args.command = sys.argv[1:]
            cmd_run(args)
        else:
            cmd_chat(args)
    else:
        args.func(args)


if __name__ == '__main__':
    main()
