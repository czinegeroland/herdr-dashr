#!/usr/bin/env python3
"""Saves a PNG of the page showing a URL fragment.

Usage: cdp_screenshot.py <devtools-port> <url-fragment> <out.png>

Test-only: the end-to-end suite keeps pictures of the browser pane as a CI
artifact. dashr itself never reads page content. Needs `websocket-client`.
"""
import base64
import json
import sys
import urllib.request

import websocket


def main() -> int:
    port, fragment, out = sys.argv[1:4]
    targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))
    page = next(t for t in targets if t["type"] == "page" and fragment in t["url"])
    socket = websocket.create_connection(page["webSocketDebuggerUrl"], suppress_origin=True, timeout=20)
    socket.send(json.dumps({"id": 1, "method": "Page.captureScreenshot", "params": {"format": "png"}}))
    while True:
        message = json.loads(socket.recv())
        if message.get("id") == 1:
            with open(out, "wb") as handle:
                handle.write(base64.b64decode(message["result"]["data"]))
            return 0


if __name__ == "__main__":
    sys.exit(main())
