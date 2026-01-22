#!/usr/bin/env python3
"""
Void Linux Tools MCP Server

Provides MCP tools for Void Linux system management:
- Package management (xbps)
- Service control (runit)
- System information

Falls back gracefully when not running on Void Linux (for development).
"""

import asyncio
import os
import shutil
import subprocess
import sys
from typing import Any

# MCP imports - using the official SDK
try:
    from mcp.server import Server
    from mcp.server.stdio import stdio_server
    from mcp.types import (
        Tool,
        TextContent,
        CallToolResult,
    )
except ImportError:
    print("MCP SDK not installed. Install with: pip install mcp", file=sys.stderr)
    sys.exit(1)


def is_void_linux() -> bool:
    """Check if running on Void Linux."""
    try:
        with open("/etc/os-release") as f:
            content = f.read()
            return "Void" in content
    except FileNotFoundError:
        return False


def run_command(cmd: list[str], timeout: int = 30) -> tuple[str, str, int]:
    """Run a command and return (stdout, stderr, returncode)."""
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return result.stdout, result.stderr, result.returncode
    except subprocess.TimeoutExpired:
        return "", "Command timed out", 124
    except FileNotFoundError:
        return "", f"Command not found: {cmd[0]}", 127


# Detect environment
IS_VOID = is_void_linux()
HAS_XBPS = shutil.which("xbps-query") is not None
HAS_APT = shutil.which("apt-cache") is not None
HAS_RUNIT = os.path.exists("/run/runit") or shutil.which("sv") is not None
HAS_SYSTEMD = shutil.which("systemctl") is not None


# Create MCP server
server = Server("void-tools")


@server.list_tools()
async def list_tools() -> list[Tool]:
    """Return list of available tools."""
    tools = []

    # Shell command tool - the most useful for an OS assistant
    tools.append(Tool(
        name="shell_command",
        description="Execute a shell command and return the output. Use for: listing files, checking status, running scripts, etc.",
        inputSchema={
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute (e.g., 'ls -la', 'cat /etc/os-release', 'df -h')"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the command (optional)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30)"
                }
            },
            "required": ["command"]
        }
    ))

    # File read tool
    tools.append(Tool(
        name="file_read",
        description="Read the contents of a file. Returns the text content.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read"
                },
                "lines": {
                    "type": "integer",
                    "description": "Maximum lines to read (default: all, use to limit large files)"
                }
            },
            "required": ["path"]
        }
    ))

    # File write tool
    tools.append(Tool(
        name="file_write",
        description="Write content to a file. Creates the file if it doesn't exist.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                },
                "append": {
                    "type": "boolean",
                    "description": "Append instead of overwrite (default: false)"
                }
            },
            "required": ["path", "content"]
        }
    ))

    # File list tool
    tools.append(Tool(
        name="file_list",
        description="List files in a directory with details.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path to list (default: current directory)"
                },
                "pattern": {
                    "type": "string",
                    "description": "Filter by glob pattern (e.g., '*.py', '*.txt')"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "List recursively (default: false)"
                }
            },
            "required": []
        }
    ))

    # File search tool
    tools.append(Tool(
        name="file_search",
        description="Search for files by name or content.",
        inputSchema={
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to search in"
                },
                "name": {
                    "type": "string",
                    "description": "File name pattern to search for"
                },
                "content": {
                    "type": "string",
                    "description": "Search for files containing this text"
                }
            },
            "required": ["path"]
        }
    ))

    # Package search tool
    tools.append(Tool(
        name="xbps_search",
        description="Search for packages in the repository. On Void Linux uses xbps-query, falls back to apt-cache on Debian/Ubuntu.",
        inputSchema={
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (package name or description)"
                }
            },
            "required": ["query"]
        }
    ))

    # Package info tool
    tools.append(Tool(
        name="xbps_info",
        description="Get detailed information about a package. Shows version, description, dependencies, etc.",
        inputSchema={
            "type": "object",
            "properties": {
                "package": {
                    "type": "string",
                    "description": "Package name to get info about"
                }
            },
            "required": ["package"]
        }
    ))

    # Package install tool (requires confirmation)
    tools.append(Tool(
        name="xbps_install",
        description="Install a package. REQUIRES USER CONFIRMATION. On Void uses xbps-install, falls back to apt on Debian/Ubuntu.",
        inputSchema={
            "type": "object",
            "properties": {
                "package": {
                    "type": "string",
                    "description": "Package name to install"
                }
            },
            "required": ["package"]
        }
    ))

    # Service status tool
    tools.append(Tool(
        name="service_status",
        description="Get the status of a system service. On Void uses sv, falls back to systemctl on systemd systems.",
        inputSchema={
            "type": "object",
            "properties": {
                "service": {
                    "type": "string",
                    "description": "Service name"
                }
            },
            "required": ["service"]
        }
    ))

    # Service control tool (requires confirmation)
    tools.append(Tool(
        name="service_control",
        description="Control a system service (start/stop/restart). REQUIRES USER CONFIRMATION.",
        inputSchema={
            "type": "object",
            "properties": {
                "service": {
                    "type": "string",
                    "description": "Service name"
                },
                "action": {
                    "type": "string",
                    "enum": ["up", "down", "restart"],
                    "description": "Action to perform: up (start), down (stop), or restart"
                }
            },
            "required": ["service", "action"]
        }
    ))

    # System info tool
    tools.append(Tool(
        name="system_info",
        description="Get system information: OS details, kernel version, CPU, memory, etc.",
        inputSchema={
            "type": "object",
            "properties": {},
            "required": []
        }
    ))

    # Package remove tool (requires confirmation)
    tools.append(Tool(
        name="xbps_remove",
        description="Remove an installed package. REQUIRES USER CONFIRMATION. On Void uses xbps-remove, falls back to apt on Debian/Ubuntu.",
        inputSchema={
            "type": "object",
            "properties": {
                "package": {
                    "type": "string",
                    "description": "Package name to remove"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Remove packages that depend on this one (default: false)"
                }
            },
            "required": ["package"]
        }
    ))

    # List installed packages tool
    tools.append(Tool(
        name="xbps_list_installed",
        description="List installed packages, optionally filtered by a pattern.",
        inputSchema={
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Filter pattern (optional, e.g. 'python*')"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of packages to return (default: 50)"
                }
            },
            "required": []
        }
    ))

    # List services tool
    tools.append(Tool(
        name="service_list",
        description="List all available services and their current status.",
        inputSchema={
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "Filter services by name pattern (optional)"
                },
                "running_only": {
                    "type": "boolean",
                    "description": "Show only running services (default: false)"
                }
            },
            "required": []
        }
    ))

    return tools


@server.call_tool()
async def call_tool(name: str, arguments: dict[str, Any]) -> CallToolResult:
    """Handle tool calls."""

    # Core shell/file tools (most useful)
    if name == "shell_command":
        return await shell_command(
            arguments.get("command", ""),
            arguments.get("cwd"),
            arguments.get("timeout", 30)
        )
    elif name == "file_read":
        return await file_read(
            arguments.get("path", ""),
            arguments.get("lines")
        )
    elif name == "file_write":
        return await file_write(
            arguments.get("path", ""),
            arguments.get("content", ""),
            arguments.get("append", False)
        )
    elif name == "file_list":
        return await file_list(
            arguments.get("path", "."),
            arguments.get("pattern"),
            arguments.get("recursive", False)
        )
    elif name == "file_search":
        return await file_search(
            arguments.get("path", "."),
            arguments.get("name"),
            arguments.get("content")
        )
    # Package management tools
    elif name == "xbps_search":
        return await xbps_search(arguments.get("query", ""))
    elif name == "xbps_info":
        return await xbps_info(arguments.get("package", ""))
    elif name == "xbps_install":
        return await xbps_install(arguments.get("package", ""))
    elif name == "service_status":
        return await service_status(arguments.get("service", ""))
    elif name == "service_control":
        return await service_control(
            arguments.get("service", ""),
            arguments.get("action", "status")
        )
    elif name == "system_info":
        return await system_info()
    elif name == "xbps_remove":
        return await xbps_remove(
            arguments.get("package", ""),
            arguments.get("recursive", False)
        )
    elif name == "xbps_list_installed":
        return await xbps_list_installed(
            arguments.get("pattern"),
            arguments.get("limit", 50)
        )
    elif name == "service_list":
        return await service_list(
            arguments.get("filter"),
            arguments.get("running_only", False)
        )
    else:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Unknown tool: {name}")],
            isError=True
        )


# ============================================================================
# CORE TOOLS - Shell & File Operations
# ============================================================================

async def shell_command(command: str, cwd: str | None = None, timeout: int = 30) -> CallToolResult:
    """Execute a shell command."""
    if not command:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: command is required")],
            isError=True
        )

    # Security: block obviously dangerous commands
    dangerous = ["rm -rf /", "mkfs", ":(){:|:&};:", "dd if=/dev/zero of=/dev/"]
    for d in dangerous:
        if d in command:
            return CallToolResult(
                content=[TextContent(type="text", text=f"Error: Blocked dangerous command pattern: {d}")],
                isError=True
            )

    try:
        result = subprocess.run(
            command,
            shell=True,
            capture_output=True,
            text=True,
            timeout=timeout,
            cwd=cwd
        )

        output = ""
        if result.stdout:
            output += result.stdout
        if result.stderr:
            if output:
                output += "\n--- stderr ---\n"
            output += result.stderr

        if not output.strip():
            output = "(no output)"

        # Limit output size
        if len(output) > 10000:
            output = output[:10000] + "\n... (output truncated)"

        return CallToolResult(
            content=[TextContent(type="text", text=output.strip())]
        )
    except subprocess.TimeoutExpired:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error: Command timed out after {timeout}s")],
            isError=True
        )
    except Exception as e:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error: {str(e)}")],
            isError=True
        )


async def file_read(path: str, lines: int | None = None) -> CallToolResult:
    """Read file contents."""
    if not path:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: path is required")],
            isError=True
        )

    try:
        # Expand ~ and resolve path
        path = os.path.expanduser(path)

        if not os.path.exists(path):
            return CallToolResult(
                content=[TextContent(type="text", text=f"Error: File not found: {path}")],
                isError=True
            )

        if os.path.isdir(path):
            return CallToolResult(
                content=[TextContent(type="text", text=f"Error: '{path}' is a directory, not a file")],
                isError=True
            )

        with open(path, "r", errors="replace") as f:
            if lines:
                content = "".join(f.readline() for _ in range(lines))
            else:
                content = f.read()

        # Limit output size
        if len(content) > 50000:
            content = content[:50000] + "\n... (file truncated, use 'lines' parameter to read specific sections)"

        return CallToolResult(
            content=[TextContent(type="text", text=content if content else "(empty file)")]
        )
    except PermissionError:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error: Permission denied: {path}")],
            isError=True
        )
    except Exception as e:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error reading file: {str(e)}")],
            isError=True
        )


async def file_write(path: str, content: str, append: bool = False) -> CallToolResult:
    """Write content to a file."""
    if not path:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: path is required")],
            isError=True
        )

    try:
        path = os.path.expanduser(path)

        # Create parent directories if needed
        parent = os.path.dirname(path)
        if parent and not os.path.exists(parent):
            os.makedirs(parent)

        mode = "a" if append else "w"
        with open(path, mode) as f:
            f.write(content)

        action = "appended to" if append else "written to"
        return CallToolResult(
            content=[TextContent(type="text", text=f"Successfully {action} {path} ({len(content)} bytes)")]
        )
    except PermissionError:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error: Permission denied: {path}")],
            isError=True
        )
    except Exception as e:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error writing file: {str(e)}")],
            isError=True
        )


async def file_list(path: str = ".", pattern: str | None = None, recursive: bool = False) -> CallToolResult:
    """List files in a directory."""
    try:
        path = os.path.expanduser(path)

        if not os.path.exists(path):
            return CallToolResult(
                content=[TextContent(type="text", text=f"Error: Path not found: {path}")],
                isError=True
            )

        if not os.path.isdir(path):
            return CallToolResult(
                content=[TextContent(type="text", text=f"Error: '{path}' is not a directory")],
                isError=True
            )

        import glob

        if recursive:
            search_pattern = os.path.join(path, "**", pattern or "*")
            files = glob.glob(search_pattern, recursive=True)
        else:
            search_pattern = os.path.join(path, pattern or "*")
            files = glob.glob(search_pattern)

        if not files:
            return CallToolResult(
                content=[TextContent(type="text", text=f"No files found in {path}" + (f" matching '{pattern}'" if pattern else ""))]
            )

        # Sort and format output
        files.sort()

        results = []
        for f in files[:100]:  # Limit to 100 entries
            try:
                stat = os.stat(f)
                size = stat.st_size
                is_dir = os.path.isdir(f)
                name = os.path.relpath(f, path)
                if is_dir:
                    name += "/"
                if size > 1024 * 1024:
                    size_str = f"{size / (1024*1024):.1f}M"
                elif size > 1024:
                    size_str = f"{size / 1024:.1f}K"
                else:
                    size_str = f"{size}B"
                results.append(f"{size_str:>8}  {name}")
            except:
                results.append(f"{'?':>8}  {os.path.relpath(f, path)}")

        output = "\n".join(results)
        if len(files) > 100:
            output += f"\n... and {len(files) - 100} more files"

        return CallToolResult(
            content=[TextContent(type="text", text=output)]
        )
    except Exception as e:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error listing files: {str(e)}")],
            isError=True
        )


async def file_search(path: str, name: str | None = None, content: str | None = None) -> CallToolResult:
    """Search for files by name or content."""
    try:
        path = os.path.expanduser(path)

        if not os.path.exists(path):
            return CallToolResult(
                content=[TextContent(type="text", text=f"Error: Path not found: {path}")],
                isError=True
            )

        if not name and not content:
            return CallToolResult(
                content=[TextContent(type="text", text="Error: Provide either 'name' pattern or 'content' to search for")],
                isError=True
            )

        results = []

        # Use find for name search, grep for content search
        if name and not content:
            cmd = ["find", path, "-name", name, "-type", "f"]
            stdout, stderr, code = run_command(cmd, timeout=30)
            if stdout.strip():
                results = stdout.strip().split("\n")[:50]
        elif content:
            cmd = ["grep", "-rl", content, path]
            if name:
                cmd = ["grep", "-rl", "--include", name, content, path]
            stdout, stderr, code = run_command(cmd, timeout=30)
            if stdout.strip():
                results = stdout.strip().split("\n")[:50]

        if not results:
            msg = f"No files found"
            if name:
                msg += f" matching '{name}'"
            if content:
                msg += f" containing '{content}'"
            return CallToolResult(
                content=[TextContent(type="text", text=msg)]
            )

        return CallToolResult(
            content=[TextContent(type="text", text="\n".join(results))]
        )
    except Exception as e:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Error searching: {str(e)}")],
            isError=True
        )


# ============================================================================
# PACKAGE MANAGEMENT TOOLS
# ============================================================================

async def xbps_search(query: str) -> CallToolResult:
    """Search for packages."""
    if not query:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: query is required")],
            isError=True
        )

    if HAS_XBPS:
        # Void Linux: use xbps-query
        stdout, stderr, code = run_command(["xbps-query", "-Rs", query])
    elif HAS_APT:
        # Debian/Ubuntu fallback: use apt-cache
        stdout, stderr, code = run_command(["apt-cache", "search", query])
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No package manager available (xbps or apt)")],
            isError=True
        )

    if code != 0:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Search failed: {stderr or stdout}")],
            isError=True
        )

    if not stdout.strip():
        return CallToolResult(
            content=[TextContent(type="text", text=f"No packages found matching '{query}'")]
        )

    # Limit output to first 20 results
    lines = stdout.strip().split("\n")[:20]
    result = "\n".join(lines)
    if len(stdout.strip().split("\n")) > 20:
        result += f"\n... and {len(stdout.strip().split(chr(10))) - 20} more results"

    return CallToolResult(
        content=[TextContent(type="text", text=result)]
    )


async def xbps_info(package: str) -> CallToolResult:
    """Get package information."""
    if not package:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: package name is required")],
            isError=True
        )

    if HAS_XBPS:
        # Try installed package first
        stdout, stderr, code = run_command(["xbps-query", "-S", package])
        if code != 0:
            # Try repository
            stdout, stderr, code = run_command(["xbps-query", "-RS", package])
    elif HAS_APT:
        stdout, stderr, code = run_command(["apt-cache", "show", package])
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No package manager available")],
            isError=True
        )

    if code != 0:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Package '{package}' not found")],
            isError=True
        )

    return CallToolResult(
        content=[TextContent(type="text", text=stdout.strip())]
    )


async def xbps_install(package: str) -> CallToolResult:
    """Install a package (dry-run only for safety)."""
    if not package:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: package name is required")],
            isError=True
        )

    # For safety, we only return the install command - actual installation
    # should be confirmed by the user through the Mycel confirmation flow
    if HAS_XBPS:
        cmd = f"sudo xbps-install -S {package}"
    elif HAS_APT:
        cmd = f"sudo apt install {package}"
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No package manager available")],
            isError=True
        )

    # Check if package exists first
    info_result = await xbps_info(package)
    if info_result.isError:
        return info_result

    return CallToolResult(
        content=[TextContent(
            type="text",
            text=f"To install '{package}', run:\n\n{cmd}\n\nPackage info:\n{info_result.content[0].text[:500]}"
        )]
    )


async def service_status(service: str) -> CallToolResult:
    """Get service status."""
    if not service:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: service name is required")],
            isError=True
        )

    if HAS_RUNIT:
        stdout, stderr, code = run_command(["sv", "status", service])
    elif HAS_SYSTEMD:
        stdout, stderr, code = run_command(["systemctl", "status", service, "--no-pager"])
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No service manager available (runit or systemd)")],
            isError=True
        )

    output = stdout or stderr
    return CallToolResult(
        content=[TextContent(type="text", text=output.strip() or "No output")]
    )


async def service_control(service: str, action: str) -> CallToolResult:
    """Control a service (returns command for safety)."""
    if not service:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: service name is required")],
            isError=True
        )

    if action not in ("up", "down", "restart"):
        return CallToolResult(
            content=[TextContent(type="text", text=f"Invalid action: {action}. Use: up, down, restart")],
            isError=True
        )

    # Map actions for different init systems
    if HAS_RUNIT:
        cmd = f"sudo sv {action} {service}"
    elif HAS_SYSTEMD:
        systemd_action = {"up": "start", "down": "stop", "restart": "restart"}[action]
        cmd = f"sudo systemctl {systemd_action} {service}"
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No service manager available")],
            isError=True
        )

    # Get current status
    status_result = await service_status(service)

    return CallToolResult(
        content=[TextContent(
            type="text",
            text=f"To {action} service '{service}', run:\n\n{cmd}\n\nCurrent status:\n{status_result.content[0].text}"
        )]
    )


async def system_info() -> CallToolResult:
    """Get system information."""
    info_parts = []

    # OS information
    if os.path.exists("/etc/os-release"):
        try:
            with open("/etc/os-release") as f:
                for line in f:
                    if line.startswith(("NAME=", "VERSION=", "ID=")):
                        info_parts.append(line.strip())
        except Exception:
            pass

    # Kernel version
    stdout, _, _ = run_command(["uname", "-r"])
    if stdout:
        info_parts.append(f"Kernel: {stdout.strip()}")

    # Architecture
    stdout, _, _ = run_command(["uname", "-m"])
    if stdout:
        info_parts.append(f"Architecture: {stdout.strip()}")

    # Check for musl vs glibc
    stdout, _, _ = run_command(["ldd", "--version"])
    if "musl" in stdout.lower():
        info_parts.append("C Library: musl")
    elif "glibc" in stdout.lower() or "GNU" in stdout:
        info_parts.append("C Library: glibc")

    # Memory info
    if os.path.exists("/proc/meminfo"):
        try:
            with open("/proc/meminfo") as f:
                for line in f:
                    if line.startswith(("MemTotal:", "MemAvailable:")):
                        info_parts.append(line.strip())
        except Exception:
            pass

    # CPU info
    if os.path.exists("/proc/cpuinfo"):
        try:
            with open("/proc/cpuinfo") as f:
                for line in f:
                    if line.startswith("model name"):
                        info_parts.append(f"CPU: {line.split(':')[1].strip()}")
                        break
        except Exception:
            pass

    # Environment detection
    info_parts.append("")
    info_parts.append("--- Detected Environment ---")
    info_parts.append(f"Void Linux: {'Yes' if IS_VOID else 'No'}")
    info_parts.append(f"XBPS available: {'Yes' if HAS_XBPS else 'No'}")
    info_parts.append(f"APT available: {'Yes' if HAS_APT else 'No'}")
    info_parts.append(f"Runit available: {'Yes' if HAS_RUNIT else 'No'}")
    info_parts.append(f"Systemd available: {'Yes' if HAS_SYSTEMD else 'No'}")

    return CallToolResult(
        content=[TextContent(type="text", text="\n".join(info_parts))]
    )


async def xbps_remove(package: str, recursive: bool = False) -> CallToolResult:
    """Remove a package (returns command for safety)."""
    if not package:
        return CallToolResult(
            content=[TextContent(type="text", text="Error: package name is required")],
            isError=True
        )

    # For safety, we only return the remove command - actual removal
    # should be confirmed by the user through the Mycel confirmation flow
    if HAS_XBPS:
        flags = "-R" if recursive else ""
        cmd = f"sudo xbps-remove {flags} {package}".strip()
    elif HAS_APT:
        cmd = f"sudo apt remove {package}"
        if recursive:
            cmd = f"sudo apt autoremove {package}"
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No package manager available")],
            isError=True
        )

    # Check if package is installed
    if HAS_XBPS:
        stdout, stderr, code = run_command(["xbps-query", package])
        if code != 0:
            return CallToolResult(
                content=[TextContent(type="text", text=f"Package '{package}' is not installed")],
                isError=True
            )
        pkg_info = stdout.strip()
    elif HAS_APT:
        stdout, stderr, code = run_command(["dpkg", "-s", package])
        if code != 0 or "not installed" in stderr.lower():
            return CallToolResult(
                content=[TextContent(type="text", text=f"Package '{package}' is not installed")],
                isError=True
            )
        pkg_info = stdout.strip()[:500]
    else:
        pkg_info = "Unable to verify installation status"

    return CallToolResult(
        content=[TextContent(
            type="text",
            text=f"To remove '{package}', run:\n\n{cmd}\n\nPackage info:\n{pkg_info}"
        )]
    )


async def xbps_list_installed(pattern: str | None = None, limit: int = 50) -> CallToolResult:
    """List installed packages."""
    if HAS_XBPS:
        if pattern:
            # List with pattern filter
            stdout, stderr, code = run_command(["xbps-query", "-s", pattern])
        else:
            # List all installed
            stdout, stderr, code = run_command(["xbps-query", "-l"])
    elif HAS_APT:
        if pattern:
            stdout, stderr, code = run_command(["dpkg", "-l", f"*{pattern}*"])
        else:
            stdout, stderr, code = run_command(["dpkg", "-l"])
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No package manager available")],
            isError=True
        )

    if code != 0:
        return CallToolResult(
            content=[TextContent(type="text", text=f"Failed to list packages: {stderr or stdout}")],
            isError=True
        )

    if not stdout.strip():
        msg = f"No installed packages matching '{pattern}'" if pattern else "No packages installed"
        return CallToolResult(
            content=[TextContent(type="text", text=msg)]
        )

    # Limit output
    lines = stdout.strip().split("\n")
    total = len(lines)
    limited_lines = lines[:limit]
    result = "\n".join(limited_lines)

    if total > limit:
        result += f"\n\n... showing {limit} of {total} packages"

    return CallToolResult(
        content=[TextContent(type="text", text=result)]
    )


async def service_list(filter_pattern: str | None = None, running_only: bool = False) -> CallToolResult:
    """List all services and their status."""
    services = []

    if HAS_RUNIT:
        # Runit: services are directories in /var/service (enabled) or /etc/sv (available)
        service_dirs = []

        # Check enabled services
        enabled_dir = "/var/service"
        if os.path.exists(enabled_dir):
            try:
                for name in os.listdir(enabled_dir):
                    svc_path = os.path.join(enabled_dir, name)
                    if os.path.isdir(svc_path) or os.path.islink(svc_path):
                        service_dirs.append((name, True))
            except PermissionError:
                pass

        # Check available but not enabled
        available_dir = "/etc/sv"
        if os.path.exists(available_dir):
            enabled_names = {s[0] for s in service_dirs}
            try:
                for name in os.listdir(available_dir):
                    if name not in enabled_names:
                        svc_path = os.path.join(available_dir, name)
                        if os.path.isdir(svc_path):
                            service_dirs.append((name, False))
            except PermissionError:
                pass

        for name, enabled in service_dirs:
            if filter_pattern and filter_pattern.lower() not in name.lower():
                continue

            if enabled:
                stdout, _, code = run_command(["sv", "status", name], timeout=5)
                status = stdout.strip() if code == 0 else "unknown"
                is_running = "run:" in status.lower()
            else:
                status = "disabled (not enabled)"
                is_running = False

            if running_only and not is_running:
                continue

            services.append(f"{'[*]' if is_running else '[ ]'} {name}: {status}")

    elif HAS_SYSTEMD:
        # Systemd: use systemctl list-units
        cmd = ["systemctl", "list-units", "--type=service", "--no-pager", "--no-legend"]
        if not running_only:
            cmd.append("--all")

        stdout, stderr, code = run_command(cmd, timeout=10)
        if code == 0:
            for line in stdout.strip().split("\n"):
                if not line.strip():
                    continue
                parts = line.split()
                if len(parts) >= 4:
                    name = parts[0].replace(".service", "")
                    if filter_pattern and filter_pattern.lower() not in name.lower():
                        continue
                    load_state = parts[1]
                    active_state = parts[2]
                    sub_state = parts[3]
                    is_running = active_state == "active"
                    marker = "[*]" if is_running else "[ ]"
                    services.append(f"{marker} {name}: {active_state} ({sub_state})")
    else:
        return CallToolResult(
            content=[TextContent(type="text", text="No service manager available (runit or systemd)")],
            isError=True
        )

    if not services:
        msg = "No services found"
        if filter_pattern:
            msg += f" matching '{filter_pattern}'"
        if running_only:
            msg += " (running only)"
        return CallToolResult(
            content=[TextContent(type="text", text=msg)]
        )

    # Sort alphabetically
    services.sort(key=lambda s: s.lower())

    header = "Services"
    if filter_pattern:
        header += f" matching '{filter_pattern}'"
    if running_only:
        header += " (running only)"
    header += f" ({len(services)} total):\n"

    return CallToolResult(
        content=[TextContent(type="text", text=header + "\n".join(services))]
    )


async def main():
    """Run the MCP server."""
    async with stdio_server() as (read_stream, write_stream):
        await server.run(
            read_stream,
            write_stream,
            server.create_initialization_options()
        )


if __name__ == "__main__":
    asyncio.run(main())
