#!/usr/bin/env node
// Resolves the prebuilt `dashr` for this platform and runs it.
//
// Ported from herdr-remote-channel's shim. The binary is not downloaded at
// install time: each platform's executable is published inside its own npm
// package, this package depends on all of them through
// `optionalDependencies` with `os` and `cpu` constraints, and npm installs
// exactly the one that can run here. npm serves the bytes under its own
// integrity hash, and `scripts/build-npm-packages.mjs` checked each
// executable against its published SHA-256 before packing it.

import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
import { dirname, join, sep } from 'node:path'

const require = createRequire(import.meta.url)
const binary = 'dashr'

// Keep in step with `scripts/build-npm-packages.mjs`, `scripts/install.sh`
// and the release matrix; `crates/dashr-cli/tests/distribution.rs` holds them
// together.
const PACKAGES = {
  'linux x64': 'linux-x64',
  'linux arm64': 'linux-arm64',
  'darwin x64': 'darwin-x64',
  'darwin arm64': 'darwin-arm64',
}

// Run from `npm/dashr` inside a source checkout (a symlinked
// `node_modules/herdr-dashr`), this file uses the checkout's own build in
// `bin/`. An npm install never runs from `npm/dashr`: there it is a copy at
// the package root.
const here = dirname(fileURLToPath(import.meta.url))

if (here.endsWith(`${sep}npm${sep}dashr`)) {
  const local = join(here, '..', '..', 'bin', binary)
  if (!existsSync(local)) {
    console.error(
      `dashr: this is a source checkout, but ${local} has not been built.\n` +
        `Build it with: cargo build --release --locked --bin dashr && install -m 0755 target/release/dashr bin/dashr`,
    )
    process.exit(1)
  }
  run(local)
} else {
  run(published())
}

// The prebuilt executable npm installed for this platform.
function published() {
  const key = `${process.platform} ${process.arch}`
  const suffix = PACKAGES[key]
  if (!suffix) {
    console.error(
      `dashr: no prebuilt binary for ${key}.\n` +
        `Supported: ${Object.keys(PACKAGES).join(', ')}.\n` +
        `Build from source instead: cargo install --git https://github.com/czinegeroland/herdr-dashr dashr-cli`,
    )
    process.exit(1)
  }
  const pkg = `herdr-dashr-${suffix}`
  try {
    return require.resolve(`${pkg}/${binary}`)
  } catch {
    console.error(
      `dashr: ${pkg} is not installed, so there is no binary for ${key}.\n` +
        `If you installed with --no-optional, reinstall without it.`,
    )
    process.exit(1)
  }
}

// Runs the executable in the foreground and exits the way it did.
function run(resolved) {
  // Asynchronous, so this process stays responsive to signals and forwards
  // them: the dashboard pane stops its Grafana on SIGHUP, and a shim that
  // swallowed it would leave the container running.
  const child = spawn(resolved, process.argv.slice(2), { stdio: 'inherit' })

  child.on('error', (error) => {
    console.error(`dashr: could not run ${resolved}: ${error.message}`)
    process.exit(1)
  })

  for (const signal of ['SIGTERM', 'SIGINT', 'SIGHUP']) {
    process.on(signal, () => {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill(signal)
      }
    })
  }

  child.on('exit', (status, signal) => {
    const codes = { SIGHUP: 1, SIGINT: 2, SIGTERM: 15 }
    process.exit(status === null ? 128 + (codes[signal] ?? 15) : status)
  })
}
