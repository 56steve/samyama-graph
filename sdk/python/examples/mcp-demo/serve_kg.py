#!/usr/bin/env python3
"""Start the Samyama MCP server (stdio) for one knowledge graph.

Generalises the ten per-KG launcher scripts used to record the demo into a single
config-driven entry point. Reads the KG's row from ``kgs.yaml`` and starts an MCP
server bound to the matching tenant on a *running* Samyama engine.

Usage:
    python serve_kg.py <key>          # e.g. legal, cricket, bank, assetops, ...

Environment:
    SAMYAMA_URL   engine HTTP endpoint (default http://localhost:8080)
    SAMYAMA_WS    directory holding the KG repos (each is a public repo at
                  github.com/samyama-ai/<repo>). Default: the current directory.
                  Clone the KG repos you want into one folder and point SAMYAMA_WS at it.

Prerequisites: the engine is running, the KG's tenant is imported, and the KG repo
plus the `samyama` / `samyama_mcp` (and `fastmcp` for assetops) packages are importable.
"""
import os
import sys
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent
# The KG repos live outside this SDK (separate repos), so default to the current
# directory and let SAMYAMA_WS override. serve_kg.py errors clearly if a repo is absent.
WORKSPACE = Path(os.environ.get("SAMYAMA_WS", Path.cwd()))
ENGINE_URL = os.environ.get("SAMYAMA_URL", "http://localhost:8080")


def load_registry():
    return {e["key"]: e for e in yaml.safe_load((HERE / "kgs.yaml").read_text())}


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {Path(sys.argv[0]).name} <key>   (keys: "
                 f"{', '.join(load_registry())})")
    key = sys.argv[1]
    reg = load_registry()
    if key not in reg:
        sys.exit(f"unknown key '{key}'. known: {', '.join(reg)}")
    e = reg[key]

    repo_dir = WORKSPACE / e["repo"]
    if not repo_dir.is_dir():
        sys.exit(f"KG repo not found: {repo_dir}  (set SAMYAMA_WS to your workspace root)")
    sys.path.insert(0, str(repo_dir))

    from samyama import SamyamaClient
    client = SamyamaClient.connect(ENGINE_URL)

    if e["tools"] == "code":
        # assetops ships its own FastMCP server; point its global client at the
        # running engine and register its tool groups. Shim on_startup as a no-op
        # for fastmcp versions that lack the decorator.
        from fastmcp import FastMCP
        if not hasattr(FastMCP, "on_startup"):
            FastMCP.on_startup = lambda self, fn=None: (fn if fn else (lambda f: f))
        import mcp_server.server as srv
        srv.client = client
        srv.GRAPH = e["graph"]
        mcp = FastMCP(e["server"])
        srv.register_asset_tools(mcp)
        srv.register_failure_tools(mcp)
        srv.register_impact_tools(mcp)
        srv.register_analytics_tools(mcp)
        mcp.run()
        return

    from samyama_mcp.config import ToolConfig
    from samyama_mcp.server import SamyamaMCPServer
    if e["tools"] == "config":
        config = ToolConfig.from_yaml(str(repo_dir / "mcp_server" / "config.yaml"))
    else:  # "auto" — generate tools from the live schema
        config = ToolConfig()
    SamyamaMCPServer(client, graph=e["graph"], server_name=e["server"], config=config).run()


if __name__ == "__main__":
    main()
