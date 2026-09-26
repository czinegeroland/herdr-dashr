#!/usr/bin/env node
// Turns the release archives into npm packages, verifying them on the way.
//
//   node scripts/build-npm-packages.mjs --version 0.1.0 \
//     --artifacts <dir of release archives and .sha256 files> \
//     --out <dir to write packages into>
//
// Ported from herdr-remote-channel. Produces one package per platform, each
// carrying that platform's `dashr` executable, plus the `herdr-dashr`
// package whose `bin` resolves among them at run time.
//
// Every archive is checked against its published `.sha256` before it is
// unpacked, and a mismatch or a missing platform aborts the whole build
// rather than producing a package that is merely missing one platform
// (requirement DASHR-TECH-005).

import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync, copyFileSync, rmSync, existsSync, chmodSync } from 'node:fs'
import { basename, join } from 'node:path'

const ROOT_PACKAGE = 'herdr-dashr'
const REPOSITORY = 'https://github.com/czinegeroland/herdr-dashr'

// Unscoped platform package names: a scope must be an npm user or org that
// already exists, and publishing under one you do not own fails with a bare
// E404 (herdr-remote-channel learned this on its first publish).
//
// Keep in step with `npm/dashr/bin.js`, `scripts/install.sh` and the release
// matrix; `crates/dashr-cli/tests/distribution.rs` holds them together.
const TARGETS = [
  { target: 'x86_64-unknown-linux-gnu', suffix: 'linux-x64', os: 'linux', cpu: 'x64' },
  { target: 'aarch64-unknown-linux-gnu', suffix: 'linux-arm64', os: 'linux', cpu: 'arm64' },
  { target: 'x86_64-apple-darwin', suffix: 'darwin-x64', os: 'darwin', cpu: 'x64' },
  { target: 'aarch64-apple-darwin', suffix: 'darwin-arm64', os: 'darwin', cpu: 'arm64' },
]

function arg(name) {
  const index = process.argv.indexOf(`--${name}`)
  if (index === -1 || !process.argv[index + 1]) {
    throw new Error(`missing --${name}`)
  }
  return process.argv[index + 1]
}

/** Refuses an archive whose digest does not match the published one. */
function verify(archive) {
  const checksumFile = `${archive}.sha256`
  if (!existsSync(checksumFile)) {
    throw new Error(`${basename(archive)} has no published checksum; refusing to package it`)
  }
  // `<digest>  <name>`, as sha256sum and shasum write it.
  const published = readFileSync(checksumFile, 'utf8').trim().split(/\s+/)[0].toLowerCase()
  const actual = createHash('sha256').update(readFileSync(archive)).digest('hex')
  if (actual !== published) {
    throw new Error(
      `checksum mismatch for ${basename(archive)}:\n  published ${published}\n  actual    ${actual}\nRefusing to package.`,
    )
  }
  return actual
}

function main() {
  const version = arg('version').replace(/^v/, '')
  const artifacts = arg('artifacts')
  const out = arg('out')
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`--version must be a semantic version, got "${version}"`)
  }

  rmSync(out, { recursive: true, force: true })
  mkdirSync(out, { recursive: true })

  const optionalDependencies = {}
  const verified = []

  for (const platform of TARGETS) {
    const archive = join(artifacts, `dashr-${version}-${platform.target}.tar.gz`)
    if (!existsSync(archive)) {
      throw new Error(`${basename(archive)} is missing; every supported platform must be published together`)
    }
    verified.push({ archive: basename(archive), digest: verify(archive) })

    const dir = join(out, platform.suffix)
    const staging = join(out, `.unpack-${platform.suffix}`)
    mkdirSync(dir, { recursive: true })
    mkdirSync(staging, { recursive: true })
    execFileSync('tar', ['-xzf', archive, '-C', staging])
    const unpacked = join(staging, 'dashr')
    if (!existsSync(unpacked)) {
      throw new Error(`${basename(archive)} does not contain a top-level dashr executable`)
    }
    copyFileSync(unpacked, join(dir, 'dashr'))
    chmodSync(join(dir, 'dashr'), 0o755)
    rmSync(staging, { recursive: true, force: true })

    const name = `${ROOT_PACKAGE}-${platform.suffix}`
    writeFileSync(
      join(dir, 'package.json'),
      `${JSON.stringify(
        {
          name,
          version,
          description: `The dashr executable (herdr-dashr) for ${platform.os} ${platform.cpu}.`,
          license: 'Apache-2.0',
          repository: { type: 'git', url: `git+${REPOSITORY}.git` },
          os: [platform.os],
          cpu: [platform.cpu],
          files: ['dashr'],
        },
        null,
        2,
      )}\n`,
    )
    optionalDependencies[name] = version
  }

  const rootDir = join(out, ROOT_PACKAGE)
  mkdirSync(rootDir, { recursive: true })
  copyFileSync('npm/dashr/bin.js', join(rootDir, 'bin.js'))
  copyFileSync('LICENSE', join(rootDir, 'LICENSE'))
  copyFileSync('README.md', join(rootDir, 'README.md'))
  writeFileSync(
    join(rootDir, 'package.json'),
    `${JSON.stringify(
      {
        name: ROOT_PACKAGE,
        version,
        description: 'Agent-built, live Grafana dashboards in a Herdr pane, with masking between your data and the agent.',
        license: 'Apache-2.0',
        repository: { type: 'git', url: `git+${REPOSITORY}.git` },
        homepage: REPOSITORY,
        keywords: ['herdr', 'herdr-plugin', 'grafana', 'dashboard', 'mcp', 'observability'],
        type: 'module',
        bin: { dashr: 'bin.js' },
        files: ['bin.js', 'LICENSE', 'README.md'],
        // No postinstall: npm resolves the one matching platform package and
        // the shim execs the binary out of it.
        optionalDependencies,
        engines: { node: '>=18' },
      },
      null,
      2,
    )}\n`,
  )

  console.log(`Built ${TARGETS.length + 1} packages for ${version} in ${out}`)
  for (const entry of verified) {
    console.log(`  verified ${entry.archive} ${entry.digest.slice(0, 16)}…`)
  }
}

main()
