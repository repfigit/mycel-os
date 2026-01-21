from mcp.server.fastmcp import FastMCP
import subprocess
import shutil
import os
import json
import urllib.request
import re
from pathlib import Path
from bs4 import BeautifulSoup
import requests
from rank_bm25 import BM25Okapi
import pickle
import time
import sys

# Initialize FastMCP server
mcp = FastMCP("void-tools")

CACHE_DIR = Path.home() / ".cache" / "mycel" / "void_docs"
CACHE_DIR.mkdir(parents=True, exist_ok=True)
INDEX_FILE = CACHE_DIR / "search_index.pkl"
DOCS_URL = "https://docs.voidlinux.org"

def log(msg):
    sys.stderr.write(f"[void-tools] {msg}\n")
    sys.stderr.flush()

# --- RAG Engine ---

class VoidSearchEngine:
    def __init__(self):
        self.documents = [] # List of {"title": str, "content": str, "url": str}
        self.bm25 = None
        self.tokenized_corpus = []
        self.loaded = False

    def load_index(self):
        if self.loaded:
            return True
        
        if INDEX_FILE.exists():
            try:
                with open(INDEX_FILE, "rb") as f:
                    data = pickle.load(f)
                    self.documents = data["documents"]
                    self.tokenized_corpus = data["corpus"]
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
        # We'll crawl the sidebar links if possible, or just key pages
        pages = [
            "/",
            "/installation/index.html",
            "/config/index.html",
            "/xbps/index.html",
            "/xbps/troubleshooting.html",
            "/config/services/index.html", # Runit
            "/config/services/user-services.html"
        ]
        
        for page in pages:
            url = f"{DOCS_URL}{page}"
            try:
                log(f"Fetching {url}...")
                resp = requests.get(url, timeout=10)
                if resp.status_code == 200:
                    soup = BeautifulSoup(resp.content, 'html.parser')
                    main_content = soup.find('main')
                    if main_content:
                        # Split by headers to create chunks
                        current_title = soup.title.string if soup.title else page
                        current_text = []
                        
                        for element in main_content.descendants:
                            if element.name in ['h1', 'h2', 'h3']:
                                if current_text:
                                    self.documents.append({
                                        "title": current_title,
                                        "content": " ".join(current_text),
                                        "url": url
                                    })
                                current_title = element.get_text().strip()
                                current_text = []
                            elif element.name == 'p':
                                current_text.append(element.get_text().strip())
                                
                        if current_text:
                            self.documents.append({
                                "title": current_title,
                                "content": " ".join(current_text),
                                "url": url
                            })
            except Exception as e:
                log(f"Error fetching {url}: {e}")

        # 2. Add local man page summaries (xbps, sv)
        man_pages = ["xbps-install", "xbps-query", "xbps-remove", "xbps-reconfigure", "sv", "runit", "chroot"]
        for page in man_pages:
            try:
                proc = subprocess.run(f"man {page} | col -b", shell=True, capture_output=True, text=True)
                if proc.returncode == 0:
                    self.documents.append({
                        "title": f"Man Page: {page}",
                        "content": proc.stdout[:2000], # First 2000 chars as summary
                        "url": f"man://{page}"
                    })
            except:
                pass

        # Build BM25
        self.tokenized_corpus = [doc["content"].lower().split() for doc in self.documents]
        self.bm25 = BM25Okapi(self.tokenized_corpus)
        self.loaded = True
        
        # Save to cache
        with open(INDEX_FILE, "wb") as f:
            pickle.dump({
                "documents": self.documents,
                "corpus": self.tokenized_corpus
            }, f)
            
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
            formatted.append(f"### {doc['title']}\nSource: {doc['url']}\n\n{doc['content'][:500]}...")
            
        return formatted

search_engine = VoidSearchEngine()

# --- Tools ---

def run_command(cmd):
    """Run a shell command and return output"""
    try:
        result = subprocess.run(
            cmd, 
            shell=True, 
            check=True, 
            capture_output=True, 
            text=True
        )
        return result.stdout.strip()
    except subprocess.CalledProcessError as e:
        return f"Error: {e.stderr}"

@mcp.tool()
def xbps_search(query: str) -> str:
    """Search for packages in the Void Linux repository"""
    return run_command(f"xbps-query -Rs '{query}'")

@mcp.tool()
def xbps_install(package: str) -> str:
    """Install a package (requires confirmation)"""
    return run_command(f"sudo xbps-install -S {package}")

@mcp.tool()
def xbps_remove(package: str) -> str:
    """Remove a package (requires confirmation)"""
    return run_command(f"sudo xbps-remove -R {package}")

@mcp.tool()
def service_status() -> str:
    """Check status of all runit services"""
    if os.path.exists("/var/service"):
        return run_command("sv status /var/service/*")
    return "Runit service directory /var/service not found."

@mcp.tool()
def service_control(service: str, action: str) -> str:
    """Control a service (start, stop, restart)"""
    if action not in ["start", "stop", "restart", "status"]:
        return "Invalid action. Use start, stop, restart, or status."
    return run_command(f"sudo sv {action} {service}")

@mcp.tool()
def search_man_pages(query: str) -> str:
    """Search installed man pages for a keyword and return summaries"""
    return run_command(f"apropos '{query}'")

@mcp.tool()
def read_man_page(page: str) -> str:
    """Read a specific man page"""
    return run_command(f"man {page} | col -b")

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
