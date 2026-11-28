import ipaddress
import socket
import urllib.parse

# --- Pure-stdlib SSRF Validation ---

_MAX_REDIRECTS = 5
_REDIRECT_STATUS_CODES = {301, 302, 303, 307, 308}


def _is_public_ip(ip_obj: ipaddress.IPv4Address | ipaddress.IPv6Address) -> bool:
    """Return True iff the IP address is a globally routable public address."""
    if isinstance(ip_obj, ipaddress.IPv6Address) and ip_obj.ipv4_mapped is not None:
        ip_obj = ip_obj.ipv4_mapped

    if (
        ip_obj.is_loopback
        or ip_obj.is_private
        or ip_obj.is_link_local
        or ip_obj.is_multicast
        or ip_obj.is_reserved
        or ip_obj.is_unspecified
        or not ip_obj.is_global
    ):
        return False

    return True


def validate_public_url(url: str) -> str:
    """Validate that a URL is http/https and resolves only to public IP addresses.

    Raises ValueError if the scheme is disallowed, hostname is missing, or any
    literal or DNS-resolved IP is loopback, private, link-local, ULA, multicast,
    reserved, or unspecified (including IPv4-mapped IPv6).
    """
    if not isinstance(url, str) or not url.strip():
        raise ValueError("URL must be a non-empty string")

    parsed = urllib.parse.urlparse(url.strip())
    scheme = parsed.scheme.lower()
    if scheme not in ("http", "https"):
        raise ValueError(f"Disallowed URL scheme: {parsed.scheme!r}")

    hostname = parsed.hostname
    if not hostname:
        raise ValueError("URL must include a valid hostname")

    if "%" in hostname:
        raise ValueError("URL hostname must not contain zone identifiers")

    lowered = hostname.lower()
    if lowered == "localhost" or lowered.endswith(".localhost"):
        raise ValueError(f"Disallowed local hostname: {hostname!r}")

    port = parsed.port or (443 if scheme == "https" else 80)

    # Check raw IP literal first
    try:
        ip_literal = ipaddress.ip_address(hostname)
    except ValueError:
        ip_literal = None

    if ip_literal is not None:
        if not _is_public_ip(ip_literal):
            raise ValueError(f"Disallowed non-public IP address: {hostname!r}")
        return url.strip()

    # Resolve DNS and validate every returned IP address
    try:
        addr_infos = socket.getaddrinfo(hostname, port, type=socket.SOCK_STREAM)
    except socket.gaierror as e:
        raise ValueError(f"Failed to resolve hostname {hostname!r}: {e}") from e

    if not addr_infos:
        raise ValueError(f"Hostname {hostname!r} resolved to no addresses")

    for family, _type, _proto, _canonname, sockaddr in addr_infos:
        raw_ip = sockaddr[0]
        # Strip IPv6 scope ID if present
        raw_ip = raw_ip.split("%", 1)[0]
        ip_obj = ipaddress.ip_address(raw_ip)
        if not _is_public_ip(ip_obj):
            raise ValueError(
                f"Hostname {hostname!r} resolved to non-public IP {raw_ip!r}"
            )

    return url.strip()


# --- Optional Third-Party Imports ---
# Guarded so validate_public_url can be imported and tested with stdlib only.

try:
    from mcp.server.fastmcp import FastMCP
except ImportError:  # pragma: no cover
    FastMCP = None  # type: ignore[assignment]

try:
    from duckduckgo_search import DDGS
except ImportError:  # pragma: no cover
    DDGS = None  # type: ignore[assignment]

try:
    import requests
except ImportError:  # pragma: no cover
    requests = None  # type: ignore[assignment]

try:
    from bs4 import BeautifulSoup
except ImportError:  # pragma: no cover
    BeautifulSoup = None  # type: ignore[assignment]


class _NoopMcp:
    def tool(self):
        def decorator(fn):
            return fn

        return decorator

    def run(self):
        raise RuntimeError("mcp.server.fastmcp is not installed")


# Initialize FastMCP server
mcp = FastMCP("web-tools") if FastMCP is not None else _NoopMcp()


@mcp.tool()
def web_search(query: str, max_results: int = 5) -> str:
    """Search the web for information using DuckDuckGo.
    Use this when you need current information, news, or answers not in your training data.
    """
    if DDGS is None:
        return "Search error: duckduckgo_search is not installed."
    try:
        with DDGS() as ddgs:
            results = list(ddgs.text(query, max_results=max_results))

        if not results:
            return "No results found."

        formatted = []
        for r in results:
            formatted.append(
                f"Title: {r['title']}\nLink: {r['href']}\nSnippet: {r['body']}\n"
            )

        return "\n".join(formatted)
    except Exception as e:
        return f"Search error: {str(e)}"


@mcp.tool()
def read_webpage(url: str) -> str:
    """Read the content of a specific webpage URL.
    Use this to get details from a search result.
    """
    if requests is None or BeautifulSoup is None:
        return "Failed to read page: requests or beautifulsoup4 is not installed."
    try:
        headers = {
            "User-Agent": (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
                "AppleWebKit/537.36 (KHTML, like Gecko) "
                "Chrome/91.0.4472.124 Safari/537.36"
            )
        }

        current_url = validate_public_url(url)
        resp = None
        for _ in range(_MAX_REDIRECTS + 1):
            resp = requests.get(
                current_url,
                headers=headers,
                timeout=10,
                allow_redirects=False,
            )
            if resp.status_code in _REDIRECT_STATUS_CODES:
                location = resp.headers.get("Location")
                if not location:
                    return "Failed to read page: redirect response missing Location header"
                next_url = urllib.parse.urljoin(current_url, location)
                current_url = validate_public_url(next_url)
                continue
            break
        else:
            return f"Failed to read page: exceeded maximum redirects ({_MAX_REDIRECTS})"

        if resp is None:
            return "Failed to read page: no response received"

        resp.raise_for_status()

        soup = BeautifulSoup(resp.content, "html.parser")

        # Remove scripts and styles
        for script in soup(["script", "style", "nav", "footer", "header"]):
            script.extract()

        text = soup.get_text()

        # Clean whitespace
        lines = (line.strip() for line in text.splitlines())
        chunks = (phrase.strip() for line in lines for phrase in line.split("  "))
        text = "\n".join(chunk for chunk in chunks if chunk)

        # Truncate if too long (approx 10k chars)
        if len(text) > 10000:
            text = text[:10000] + "\n...[truncated]..."

        return f"Source: {current_url}\n\n{text}"
    except Exception as e:
        return f"Failed to read page: {str(e)}"


if __name__ == "__main__":
    mcp.run()
