# herdr-dashr

[![CI](https://github.com/czinegeroland/herdr-dashr/actions/workflows/build-and-test.yml/badge.svg?branch=main)](https://github.com/czinegeroland/herdr-dashr/actions/workflows/build-and-test.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Platforms: Linux • macOS • Windows](https://img.shields.io/badge/platforms-Linux%20%E2%80%A2%20macOS%20%E2%80%A2%20Windows-blue.svg)](#quickstart)

Live Grafana dashboards your AI agent builds, right beside your chat in Herdr.

Ask for a dashboard and keep talking. Your agent opens a small dashr pane next
to the conversation and builds the dashboard; Ctrl-click the link to watch it
live in your browser. It changes as you ask.

<img width="1648" height="945" alt="image" src="https://github.com/user-attachments/assets/56602517-9021-4c66-810c-c86e03a50df1" />

<img width="1916" height="909" alt="image" src="https://github.com/user-attachments/assets/5807e138-2b71-41ee-9c75-ab11f267d22b" />

**Nothing left behind.** Each pane runs its own Grafana in Docker. Close the
pane and it's gone.

## Quickstart

You need [Herdr](https://herdr.dev/docs/install/), Docker and Node.js 18+.

```sh
herdr plugin install czinegeroland/herdr-dashr
```

That's it: the plugin, the `dashr` command and the agent skill for your coding
agents.

## License

[Apache-2.0](LICENSE)
