#!/usr/bin/env python3
"""A minimal MCP stdio client for the end-to-end suite.

Usage: mcp_client.py <dashr> <state-dir> <config-dir> <session> <script.json>

The script is a JSON list of {"tool": name, "arguments": {...}} or
{"read_resource": uri}. The resource list is printed after tools/list. Each
result is printed as one JSON line: {"tool", "isError", "text"}. Image
results are saved beside the script as <script>.<tool>.<n>.png.
"""
import base64
import json
import subprocess
import sys


def main() -> int:
    dashr, state_dir, config_dir, session, script_path = sys.argv[1:6]
    with open(script_path, encoding="utf-8") as handle:
        script = json.load(handle)
    process = subprocess.Popen(
        [dashr, "--state-dir", state_dir, "--config-dir", config_dir, "mcp", "--session", session],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    next_id = 0

    def request(method, params=None):
        nonlocal next_id
        next_id += 1
        message = {"jsonrpc": "2.0", "id": next_id, "method": method}
        if params is not None:
            message["params"] = params
        process.stdin.write(json.dumps(message) + "\n")
        process.stdin.flush()
        line = process.stdout.readline()
        if not line:
            raise SystemExit(f"server closed during {method}")
        answer = json.loads(line)
        if "error" in answer:
            raise SystemExit(f"{method}: {answer['error']}")
        return answer["result"]

    init = request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                   "clientInfo": {"name": "e2e", "version": "1"}})
    process.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
    process.stdin.flush()
    print(json.dumps({"tool": "initialize", "isError": False, "text": json.dumps(init)}))
    tools = request("tools/list")
    print(json.dumps({"tool": "tools/list", "isError": False,
                      "text": json.dumps([tool["name"] for tool in tools["tools"]])}))
    resources = request("resources/list")
    print(json.dumps({"tool": "resources/list", "isError": False,
                      "text": json.dumps([r["uri"] for r in resources.get("resources", [])])}))
    for step in script:
        if "read_resource" in step:
            read = request("resources/read", {"uri": step["read_resource"]})
            print(json.dumps({"tool": "resources/read", "isError": False,
                              "text": read["contents"][0]["text"]}))
            continue
        result = request("tools/call", {"name": step["tool"], "arguments": step.get("arguments", {})})
        text = "".join(item.get("text", "") for item in result["content"] if item["type"] == "text")
        images = [item for item in result["content"] if item["type"] == "image"]
        for number, image in enumerate(images):
            path = f"{script_path}.{step['tool']}.{number}.png"
            with open(path, "wb") as handle:
                handle.write(base64.b64decode(image["data"]))
            text += f"[image {image['mimeType']} saved to {path}]"
        print(json.dumps({"tool": step["tool"], "isError": result.get("isError", False), "text": text}))
    process.stdin.close()
    process.wait(timeout=10)
    return 0


if __name__ == "__main__":
    sys.exit(main())
