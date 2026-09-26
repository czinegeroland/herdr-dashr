#!/usr/bin/env python3
"""Evaluates a JavaScript expression in the page showing a URL fragment.

Usage: cdp_eval.py <devtools-port> <url-fragment> <expression>

Test-only: the end-to-end suite uses it to read what the browser pane
really rendered. dashr itself never reads page content (the agent must not
see values). Needs `websocket-client`.
"""
import json
import sys
import urllib.request

import websocket


def main() -> int:
    port, fragment, expression = sys.argv[1:4]
    targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))
    page = next(t for t in targets if t["type"] == "page" and fragment in t["url"])
    socket = websocket.create_connection(page["webSocketDebuggerUrl"], suppress_origin=True, timeout=20)
    socket.send(json.dumps({"id": 1, "method": "Runtime.evaluate",
                            "params": {"expression": expression, "returnByValue": True}}))
    while True:
        message = json.loads(socket.recv())
        if message.get("id") == 1:
            print(message["result"]["result"].get("value", ""))
            return 0


if __name__ == "__main__":
    sys.exit(main())
