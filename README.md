# herdr-dashr

A [Herdr](https://github.com/herdrdev/herdr) plugin that opens an
**agent-built, live Grafana dashboard inside a Herdr pane**, with a coding
agent underneath to reshape it.

- **Real Grafana, in the terminal.** The top pane shows Grafana in kiosk mode
  through [terminal-browser](https://github.com/zenbu-labs/terminal-browser);
  without it, a text status view.
- **No model in the data path.** Grafana refreshes the data. The agent only
  changes the dashboard, through an MCP server.
- **Privacy by construction.** The agent designs from schemas and masked
  samples (`<email#1>`, `<ipv4#2>`); only you see real values.
- **Disposable.** A pane-owned Grafana container with a read-only root and
  tmpfs storage on a loopback port, removed when the pane closes.
- **CodePipeline bootstrap.** Ctrl-click a CodePipeline URL: the plugin finds
  the stacks it deployed and builds a first dashboard of their log groups,
  queues and DLQs, state machines and Lambdas before the agent says a word.

`docs/PRD.md` is the authoritative specification and delivery ledger.

## Status

Under active development; see the PRD's delivery ledger.

## License

Apache-2.0
