import json
import os
from pathlib import Path
import re
import subprocess
import sys

# --- Pure-stdlib Validation & Sanitization Helpers ---

_FORBIDDEN_QUERY_CHARS = set(";|&$`\n\r\x00()<>\\'\"{}")


def _check_common_str(value: str, label: str, max_len: int = 128) -> str:
    if not isinstance(value, str):
        raise ValueError(f"{label} must be a string")
    if not value or len(value) > max_len:
        raise ValueError(f"{label} must be between 1 and {max_len} characters")
    if "\x00" in value or "\n" in value or "\r" in value:
        raise ValueError(f"{label} contains forbidden control characters")
    if ".." in value or "/" in value or "\\" in value:
        raise ValueError(f"{label} contains forbidden path characters")
    if value.startswith("-"):
        raise ValueError(f"{label} must not start with '-'")
    return value


def validate_package_name(package: str) -> str:
    """Validate a Void Linux package name (rejects shell metacharacters and flags)."""
    _check_common_str(package, "package")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,127}", package):
        raise ValueError(f"Invalid package name: {package!r}")
    return package


def validate_service_name(service: str) -> str:
    """Validate a runit service name (rejects path traversal and shell metacharacters)."""
    _check_common_str(service, "service")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}", service):
        raise ValueError(f"Invalid service name: {service!r}")
    return service


def validate_man_page(page: str) -> str:
    """Validate a man page name (rejects flags, paths, and shell metacharacters)."""
    _check_common_str(page, "man page")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:-]{0,127}", page):
        raise ValueError(f"Invalid man page name: {page!r}")
    return page


def validate_query_arg(query: str) -> str:
    """Validate a search query argument (rejects shell metacharacters and leading '-')."""
    _check_common_str(query, "query", max_len=256)
    if not query.strip():
        raise ValueError("query must not be empty or whitespace")
    if any(ch in _FORBIDDEN_QUERY_CHARS for ch in query):
        raise ValueError("query contains forbidden shell metacharacters")
    return query


def strip_man_overstrikes(text: str) -> str:
    """Strip backspace overstrike sequences produced by groff/man (replaces `col -b`)."""
    # Repeatedly strip `<char>\x08` pairs (handles bold/underline sequences cleanly)
    prev = None
    while prev != text:
        prev = text
        text = re.sub(r"[^\x08]\x08", "", text)
    return text.replace("\x08", "")


# --- Optional Third-Party Imports ---
# Guarded so pure-stdlib validators can be imported and tested without pip deps.

try:
    from mcp.server.fastmcp import FastMCP
except ImportError:  # pragma: no cover - fallback for stdlib-only test environments
    FastMCP = None  # type: ignore[assignment]

try:
    from bs4 import BeautifulSoup
except ImportError:  # pragma: no cover
    BeautifulSoup = None  # type: ignore[assignment]

try:
    import requests
except ImportError:  # pragma: no cover
    requests = None  # type: ignore[assignment]

try:
    from rank_bm25 import BM25Okapi
except ImportError:  # pragma: no cover
    BM25Okapi = None  # type: ignore[assignment]


class _NoopMcp:
    def tool(self):
        def decorator(fn):
            return fn

        return decorator

    def run(self):
        raise RuntimeError("mcp.server.fastmcp is not installed")


# Initialize FastMCP server
mcp = FastMCP("void-tools") if FastMCP is not None else _NoopMcp()

CACHE_DIR = Path.home() / ".cache" / "mycel" / "void_docs"
INDEX_FILE = CACHE_DIR / "search_index.json"
DOCS_URL = "https://docs.voidlinux.org"


def log(msg):
    sys.stderr.write(f"[void-tools] {msg}\n")
    sys.stderr.flush()


# --- RAG Engine ---


class VoidSearchEngine:
    def __init__(self):
        self.documents = []  # List of {"title": str, "content": str, "url": str}
        self.bm25 = None
        self.tokenized_corpus = []
        self.loaded = False

    def load_index(self):
        if self.loaded:
            return True

        if INDEX_FILE.exists():
            try:
                with open(INDEX_FILE, "r", encoding="utf-8") as f:
                    data = json.load(f)
                self.documents = data["documents"]
                self.tokenized_corpus = data["corpus"]
                if BM25Okapi is None:
                    log("rank_bm25 is not installed")
                    return False
                self.bm25 = BM25Okapi(self.tokenized_corpus)
                self.loaded = True
                return True
            except Exception as e:
                log(f"Failed to load index: {e}")
                return False
        return False

    def build_index(self):
        log("Building index...")
        self.documents = []

        # 1. Scrape Void Handbook (Simplified: Main sections)
        pages = [
            "/",
            "/installation/index.html",
            "/config/index.html",
            "/xbps/index.html",
            "/xbps/troubleshooting.html",
            "/config/services/index.html",  # Runit
            "/config/services/user-services.html",
        ]

        if requests is not None and BeautifulSoup is not None:
            for page in pages:
                url = f"{DOCS_URL}{page}"
                try:
                    log(f"Fetching {url}...")
                    resp = requests.get(url, timeout=10)
                    if resp.status_code == 200:
                        soup = BeautifulSoup(resp.content, "html.parser")
                        main_content = soup.find("main")
                        if main_content:
                            current_title = soup.title.string if soup.title else page
                            current_text = []

                            for element in main_content.descendants:
                                if element.name in ["h1", "h2", "h3"]:
                                    if current_text:
                                        self.documents.append(
                                            {
                                                "title": current_title,
                                                "content": " ".join(current_text),
                                                "url": url,
                                            }
                                        )
                                    current_title = element.get_text().strip()
                                    current_text = []
                                elif element.name == "p":
                                    current_text.append(element.get_text().strip())

                            if current_text:
                                self.documents.append(
                                    {
                                        "title": current_title,
                                        "content": " ".join(current_text),
                                        "url": url,
                                    }
                                )
                except Exception as e:
                    log(f"Error fetching {url}: {e}")

        # 2. Add local man page summaries (xbps, sv) without invoking a shell
        man_pages = [
            "xbps-install",
            "xbps-query",
            "xbps-remove",
            "xbps-reconfigure",
            "sv",
            "runit",
            "chroot",
        ]
        for page in man_pages:
            try:
                safe_page = validate_man_page(page)
                proc = subprocess.run(
                    ["man", "--", safe_page],
                    shell=False,
                    capture_output=True,
                    text=True,
                )
                if proc.returncode == 0:
                    cleaned = strip_man_overstrikes(proc.stdout)
                    self.documents.append(
                        {
                            "title": f"Man Page: {safe_page}",
                            "content": cleaned[:2000],  # First 2000 chars as summary
                            "url": f"man://{safe_page}",
                        }
                    )
            except Exception:
                pass

        # Build BM25
        self.tokenized_corpus = [doc["content"].lower().split() for doc in self.documents]
        if BM25Okapi is not None and self.tokenized_corpus:
            self.bm25 = BM25Okapi(self.tokenized_corpus)
        else:
            self.bm25 = None
        self.loaded = True

        # Save to cache as JSON only
        CACHE_DIR.mkdir(parents=True, exist_ok=True)
        with open(INDEX_FILE, "w", encoding="utf-8") as f:
            json.dump(
                {
                    "documents": self.documents,
                    "corpus": self.tokenized_corpus,
                },
                f,
            )

        return len(self.documents)

    def search(self, query: str, n=3):
        if not self.load_index():
            count = self.build_index()
            if count == 0:
                return ["Failed to build documentation index."]

        tokenized_query = query.lower().split()
        if not self.bm25:
            return ["Index not ready."]

        results = self.bm25.get_top_n(tokenized_query, self.documents, n=n)

        formatted = []
        for doc in results:
            formatted.append(
                f"### {doc['title']}\nSource: {doc['url']}\n\n{doc['content'][:500]}..."
            )

        return formatted


search_engine = VoidSearchEngine()

# --- Tools ---


def run_argv(argv: list[str]) -> str:
    """Run an explicit argument vector without a shell and return output."""
    try:
        result = subprocess.run(
            argv,
            shell=False,
            check=True,
            capture_output=True,
            text=True,
        )
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        return f"Error: {e.stderr}"


@mcp.tool()
def xbps_search(query: str) -> str:
    """Search for packages in the Void Linux repository"""
    try:
        safe_query = validate_query_arg(query)
    except ValueError as e:
        return f"Invalid argument: {e}"
    return run_argv(["xbps-query", "-Rs", "--", safe_query])


@mcp.tool()
def xbps_install(package: str) -> str:
    """Install a package (requires confirmation)"""
    try:
        safe_pkg = validate_package_name(package)
    except ValueError as e:
        return f"Invalid argument: {e}"
    return run_argv(["sudo", "xbps-install", "-S", "--", safe_pkg])


@mcp.tool()
def xbps_remove(package: str) -> str:
    """Remove a package (requires confirmation)"""
    try:
        safe_pkg = validate_package_name(package)
    except ValueError as e:
        return f"Invalid argument: {e}"
    return run_argv(["sudo", "xbps-remove", "-R", "--", safe_pkg])


@mcp.tool()
def service_status() -> str:
    """Check status of all runit services"""
    service_root = Path("/var/service")
    if not service_root.exists():
        return "Runit service directory /var/service not found."
    service_paths = sorted(str(p) for p in service_root.iterdir())
    if not service_paths:
        return "No runit services found in /var/service."
    return run_argv(["sv", "status", *service_paths])


@mcp.tool()
def service_control(service: str, action: str) -> str:
    """Control a service (start, stop, restart)"""
    if action not in ["start", "stop", "restart", "status"]:
        return "Invalid action. Use start, stop, restart, or status."
    try:
        safe_service = validate_service_name(service)
    except ValueError as e:
        return f"Invalid argument: {e}"
    return run_argv(["sudo", "sv", action, safe_service])


@mcp.tool()
def search_man_pages(query: str) -> str:
    """Search installed man pages for a keyword and return summaries"""
    try:
        safe_query = validate_query_arg(query)
    except ValueError as e:
        return f"Invalid argument: {e}"
    return run_argv(["apropos", "--", safe_query])


@mcp.tool()
def read_man_page(page: str) -> str:
    """Read a specific man page"""
    try:
        safe_page = validate_man_page(page)
    except ValueError as e:
        return f"Invalid argument: {e}"
    output = run_argv(["man", "--", safe_page])
    return strip_man_overstrikes(output)


@mcp.tool()
def search_void_handbook(query: str) -> str:
    """Search the Void Linux Handbook and documentation using RAG.
    Use this to understand how to configure the system, manage services, or solve errors.
    """
    results = search_engine.search(query)
    return "\n\n".join(results)


@mcp.tool()
def refresh_documentation_index() -> str:
    """Force rebuild of the documentation search index"""
    count = search_engine.build_index()
    return f"Index rebuilt with {count} chunks."


if __name__ == "__main__":
    mcp.run()
