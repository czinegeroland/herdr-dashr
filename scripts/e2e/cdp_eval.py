#!/usr/bin/env python3
"""Evaluates a JavaScript expression in the page showing a URL fragment.

Usage: cdp_eval.py <devtools-port> <url-fragment> <expression> [frame-fragment]

With a frame fragment, the expression runs inside the page's frame whose URL
contains it (the dashboard inside dashr's page, from another origin).

Test-only: the end-to-end suite uses it to read what the human's browser
really shows. dashr itself never reads page content (the agent must not
see values). Needs `websocket-client`.
"""
import json
import sys
import urllib.request

import websocket


class Page:
    def __init__(self, url):
        self.socket = websocket.create_connection(url, suppress_origin=True, timeout=20)
        self.next_id = 0
        self.events = []

    def call(self, method, params=None):
        self.next_id += 1
        self.socket.send(json.dumps({"id": self.next_id, "method": method, "params": params or {}}))
        while True:
            message = json.loads(self.socket.recv())
            if message.get("id") == self.next_id:
                return message.get("result", {})
            self.events.append(message)


def frames(tree):
    yield tree["frame"]
    for child in tree.get("childFrames", []):
        yield from frames(child)


def main() -> int:
    port, fragment, expression = sys.argv[1:4]
    frame_fragment = sys.argv[4] if len(sys.argv) > 4 else None
    targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))
    target = next(t for t in targets if t["type"] == "page" and fragment in t["url"])
    page = Page(target["webSocketDebuggerUrl"])
    params = {"expression": expression, "returnByValue": True}
    if frame_fragment:
        tree = page.call("Page.getFrameTree")["frameTree"]
        frame = next(f for f in frames(tree) if frame_fragment in f.get("url", ""))
        world = page.call("Page.createIsolatedWorld", {"frameId": frame["id"], "worldName": "e2e"})
        params["contextId"] = world["executionContextId"]
    result = page.call("Runtime.evaluate", params)
    print(result.get("result", {}).get("value", ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
