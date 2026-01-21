from mcp.server.fastmcp import FastMCP
from duckduckgo_search import DDGS
import requests
from bs4 import BeautifulSoup
import re

# Initialize FastMCP server
mcp = FastMCP("web-tools")

@mcp.tool()
def web_search(query: str, max_results: int = 5) -> str:
    """Search the web for information using DuckDuckGo.
    Use this when you need current information, news, or answers not in your training data.
    """
    try:
        with DDGS() as ddgs:
            results = list(ddgs.text(query, max_results=max_results))
            
        if not results:
            return "No results found."
            
        formatted = []
        for r in results:
            formatted.append(f"Title: {r['title']}\nLink: {r['href']}\nSnippet: {r['body']}\n")
            
        return "\n".join(formatted)
    except Exception as e:
        return f"Search error: {str(e)}"

@mcp.tool()
def read_webpage(url: str) -> str:
    """Read the content of a specific webpage URL.
    Use this to get details from a search result.
    """
    try:
        headers = {
            "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"
        }
        resp = requests.get(url, headers=headers, timeout=10)
        resp.raise_for_status()
        
        soup = BeautifulSoup(resp.content, 'html.parser')
        
        # Remove scripts and styles
        for script in soup(["script", "style", "nav", "footer", "header"]):
            script.extract()
            
        text = soup.get_text()
        
        # Clean whitespace
        lines = (line.strip() for line in text.splitlines())
        chunks = (phrase.strip() for line in lines for phrase in line.split("  "))
        text = '\n'.join(chunk for chunk in chunks if chunk)
        
        # Truncate if too long (approx 10k chars)
        if len(text) > 10000:
            text = text[:10000] + "\n...[truncated]..."
            
        return f"Source: {url}\n\n{text}"
    except Exception as e:
        return f"Failed to read page: {str(e)}"

if __name__ == "__main__":
    mcp.run()
