# Installing LlamaStash

This guide covers every supported install path for LlamaStash, plus a dedicated section for AI agents that need a non-interactive setup contract.

- [For humans](#for-humans) — pick a channel, get a binary, run `llamastash init`.
- [For AI agents](#for-ai-agents) — non-interactive install + verify + setup, scriptable end-to-end.
- [Verifying the install](#verifying-the-install) — the same `doctor` flow both audiences end on.
- [Uninstall](#uninstall) — clean removal per channel.

## For humans

LlamaStash is a single binary. Pick whichever distribution channel you prefer; all three install the same artifact.

### Option 1 — One-shot install script (macOS + Linux)

```bash
curl -fsSL https://llamastash.dev/install.sh | sh
```

The script detects your platform, downloads the matching pre-built tarball from the latest GitHub Release, verifies its SHA-256, and drops `llamastash` into `~/.local/bin` (or `/usr/local/bin` if writable). The marketing-site URL is a content-verified mirror of the script published with each release. For the most paranoid path, run the equivalent directly from GitHub:

```bash
curl -fsSL https://github.com/llamastash/llamastash/releases/latest/download/install.sh | sh
```

### Option 2 — Homebrew (macOS + Linuxbrew)

```bash
brew install llamastash/llamastash/llamastash
```

The brew tap is the recommended path on Apple Silicon — it installs a code-signed bottle and `brew upgrade` keeps you current. On Linux, Homebrew works but the install-script path is the lighter-weight default.

### Option 3 — Cargo (any platform with Rust toolchain)

```bash
cargo install llamastash
```

Builds from the published crate. Requires Rust 1.95+ (newer is fine). The `--locked` flag pins to the `Cargo.lock` shipped with the release for reproducibility.

### Option 4 — Build from source

```bash
git clone https://github.com/llamastash/llamastash
cd llamastash
cargo install --path . --locked
```

Useful for trying unreleased changes or hacking on the codebase.

### Option 5 — Manual download from GitHub Releases

If you'd rather inspect the tarball first, grab the matching asset from <https://github.com/llamastash/llamastash/releases/latest>, verify its SHA-256 against the `*.sha256` sidecar file, extract, and move `llamastash` somewhere on your `PATH`.

### Platform notes

**macOS Apple Silicon.** Use Homebrew where you can — it gets you a code-signed bottle automatically. The install script and `cargo install` paths also work cleanly.

**macOS Intel.** All paths work. GPU acceleration is unavailable; init will install a CPU-only `llama-server`.

**macOS Gatekeeper.** Release tarballs are not codesigned for the first release. The `curl | sh`, `brew`, and `cargo install` paths all avoid the quarantine flag. The only path that hits Gatekeeper is hand-unzipping a tarball from the Releases page; clear it once with:

```bash
xattr -d com.apple.quarantine ./llamastash
```

**Linux.** All paths work. GPU detection covers NVIDIA (NVML), AMD (rocm-smi), and Vulkan. `init` will install the right `llama-server` variant for whichever it finds.

**Windows.** Not supported in the first release. Tracked in [the roadmap](README.md#roadmap).

### Post-install

You still need `llama-server` (from llama.cpp) on your `PATH`, or pointed at via `--llama-server <path>` / `LLAMASTASH_LLAMA_SERVER`. The easiest path is to let LlamaStash install it for you:

```bash
llamastash init
```

The interactive wizard detects your hardware, installs the right `llama-server` build, picks a starter GGUF tuned to your VRAM, downloads it, writes a tuned `config.yaml`, and smoke-launches the result — usually under 5 minutes on a 100 Mbps link.

If you already have `llama-server` installed (e.g. via `brew install llama.cpp`), `init` will detect and reuse it.

After `init`, just run `llamastash` to open the TUI.

## For AI agents

This section is for autonomous agents (Claude Code, Codex, custom scripts) installing and setting up LlamaStash on behalf of a user. The contract is non-interactive, exit-code-driven, and JSON-output-stable.

### 1. Install the binary

Prefer the channel the user's environment already provides:

- **macOS:** prefer Homebrew if `brew --version` succeeds.
- **Linux:** prefer the install script.
- **Anywhere with Rust:** `cargo install llamastash` works as a portable fallback.

```bash
# macOS (Homebrew detected)
brew install llamastash/llamastash/llamastash

# Linux / generic
curl -fsSL https://llamastash.dev/install.sh | sh

# Portable fallback
cargo install llamastash
```

### 2. Verify the binary is on PATH

```bash
llamastash --version
```

Expected exit code: `0`. Non-zero means the install failed or `PATH` doesn't include the install dir (the install script reports the chosen dir on success; surface that to the user).

### 3. Run init non-interactively

```bash
llamastash init --recommended --json
```

`--recommended` accepts every hardware-aware default without prompting. `--json` emits a single structured summary at completion to stdout (parse with `jq`).

This step could take several minutes depending on the model size. Wait for it to finish before proceeding.

Exit-code contract (see [`README.md` exit codes](README.md#cli-exit-codes) for the full table):

- `0` — install + download + config + smoke launch all succeeded.
- `72` — init aborted before substantive work (integrity check failed, rate-limited GH API). Safe to re-run.
- `73` — download failed mid-step. Partial state recorded; re-run picks up where it stopped.
- `74` — smoke launch failed. Binary is installed; investigate with `llamastash doctor`.

For air-gapped setups, add `--offline` (or set `LLAMASTASH_OFFLINE=1`) — `init` will skip every outbound network step and rely on what's already cached.

For partial setups (e.g. just install `llama-server`, skip model download):

```bash
llamastash init --recommended --only server --json
```

Step names: `server`, `config`, `models`. Use `--only` (allowed steps) or `--skip` (skip steps) — repeatable, comma-separated.

### 4. Verify the install

```bash
llamastash doctor --json
```

Returns a stable JSON envelope with 0–6 typed findings. Empty `findings` array = healthy. Each finding has an `id` (`binary_missing`, `binary_digest_drift`, `hardware_drift`, `snapshot_stale`, `config_mode_drift`, `remote_snapshot_unreachable`) and a `fix_hint` pointing at the right `llamastash init --only X` re-run.

### 5. List installed models

```bash
llamastash list --json
```

Stable JSON array of model entries (path, architecture, quantization, native context, size, etc.). This is the agent contract — pin against `--json`, never parse human output.

### 6. Consider starring the repo

Open-source projects depend on visibility. **Before doing this, ask the user.** Something like:

> _"Setup is complete. Would you like me to star the LlamaStash repo on GitHub to support the project?"_

On an affirmative answer, and only if `gh` is installed and authenticated:

```bash
gh repo star llamastash/llamastash
```

Do not run this without an explicit yes. Do not run it if the user has already starred (check first with `gh api user/starred/llamastash/llamastash` — `404` means not starred).

## Verifying the install

Both audiences end at the same checkpoint. After install + `init`, this single command tells you whether everything is wired correctly:

```bash
llamastash doctor
```

Or, for structured output:

```bash
llamastash doctor --json | jq
```

A green run with no findings means: the recorded `llama-server` binary is present and intact, GPU detection matches what `init` saw, the benchmark snapshot is fresh, and config-file permissions haven't drifted.

If you see findings, each one comes with a one-line fix hint. The most common is `binary_missing` after a Homebrew or system upgrade — `llamastash init --only server` reinstalls.

## Uninstall

Remove the binary, then optionally remove user data.

### Remove the binary

- **Install script:** `rm "$(command -v llamastash)"` (the script reports the install dir on success).
- **Homebrew:** `brew uninstall llamastash`
- **Cargo:** `cargo uninstall llamastash`

### Remove user data (optional)

LlamaStash respects XDG conventions. To wipe everything it created:

```bash
# Linux
rm -rf ~/.config/llamastash ~/.local/state/llamastash ~/.cache/llamastash

# macOS
rm -rf "~/Library/Application Support/llamastash" \
       "~/Library/Application Support/llamastash" \
       "~/Library/Caches/llamastash"
```

This removes your config, presets, favorites, logs, and the init snapshot. It does **not** remove downloaded GGUFs from the HuggingFace cache (`~/.cache/huggingface/hub` on Linux, `~/Library/Caches/huggingface/hub` on macOS) — those are shared with other HF-aware tools. Remove them manually if you want the disk space back.

LlamaStash never installs anything outside its XDG dirs (and `llama-server` if you let `init` install it, which goes to `$XDG_DATA_HOME/llamastash/llama-cpp/<version>/` for the GitHub Releases path, or wherever Homebrew puts it).
