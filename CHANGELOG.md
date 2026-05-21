# Changelog

All notable changes to llamastash will be documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project intends to follow [SemVer](https://semver.org/spec/v2.0.0.html) starting with the first stable release.

Entries are one-line summaries of noteworthy changes; follow the linked commit or PR for the full story.

## [Unreleased]

_No changes yet._

## [0.0.1] — [Unreleased]

First publicly-installable release. Single `llamastash` binary acts as TUI, CLI, and daemon; distribution lands across Cargo, a Homebrew tap, and a GitHub-hosted install script, with a marketing site at [llamastash.cli.rs](https://llamastash.cli.rs).

- Daemon-on-demand over a `0600` Unix socket with peercred auth; supervises `llama-server` children through `Launching → Loading → Ready / Error → Stopping → Stopped` with three-factor orphan re-adoption.
- GGUF header parser and async scanner for HuggingFace / Ollama / LM Studio caches; model identity is `(canonical path, BLAKE3 of header)`.
- TUI with grouped list + favorites + filter, launch picker, advanced flag panel, clipboard yank, streaming Chat / Embed / Rerank / Logs right pane, five themes (Catppuccin Macchiato default).
- CLI: `list` / `start` / `stop` / `status` / `logs` / `presets` / `favorites` / `daemon` — every read+mutation command supports `--json`, with documented exit codes and an auto-spawn-daemon flow (`--no-spawn` to opt out).
- `llamastash init` first-run wizard (R48): detect → install `llama-server` per OS×GPU → recommend + pull a GGUF → write `config.yaml` with `arch_defaults` → smoke launch → TUI handoff. Per-step `--install` / `--model` / `--config-step` overrides, `--recommended` / `--json` / `--offline` modes, and `--revision <SHA>` to pin HF commits.
- `llamastash doctor` read-only diagnostic with stable finding ids under `--json` (R74); `llamastash pull <owner/repo[:filename.gguf]>` on `hf-hub` (R65); `llamastash recommend` shortcut for hardware-aware GGUF picks ([`adfef21`](../../commit/adfef21)).
- Path-A recommender with VRAM-fit hard filter and composite ranking (benchmark × tok/s × params × recency), backed by a bundled benchmark snapshot refreshed by daily CI and vendored [`whichllm`](https://github.com/Andyyyy64/whichllm) catalog discovery ([`ae94ee3`](../../commit/ae94ee3)).
- Colored CLI output across every human-readable surface with `--no-colors` / `NO_COLOR` / non-TTY off-conditions; padded TTY tables for report commands; `--json` byte-stable regardless ([`96fed70`](../../commit/96fed70)).
- TUI `Ctrl+R` restarts the daemon preserving the parent dispatcher's resolved options; `Ctrl+Q` kills it (moved from `Shift+Q`); both stay discoverable via `?` only ([`adfef21`](../../commit/adfef21), [`0b6fc77`](../../commit/0b6fc77)).
- TUI HuggingFace pull dialog (`d` from the model list) — three-stage Search → File picker → Confirm modal backed by HF Hub's `/api/models` (debounced via `FetchClient`, fit-aware ✓/⚠/✗/— glyph column, sharded-set collapse, byte-accurate progress strip, FIFO queue with one active pull). `Ctrl+X` cancels the active pull mid-chunk; `Ctrl+D` deletes the focused GGUF on idle rows only (HF-cache layout deletes the whole repo dir, constrained to the `~/.cache/huggingface/hub` tree). CLI `--offline` / `LLAMASTASH_OFFLINE` flows through every spawned HF task ([`#4`](../../pull/4)).
- Shared modal `InputField` component drives the HF dialog, filter input, chat / embed / rerank composers, and advanced-panel free-text extras — uniform `e:edit / Esc:walk-back / Enter:Submit` contract with edit-state-aware chip strips. App-wide `Esc` walk-back order: Help → Confirm popup → HF dialog → input edit/clear/close → focus return → root no-op; `Backspace` no longer overloaded as a walk-back chord; `→` no longer jumps from the Models list to the right pane ([`#4`](../../pull/4)).
- Logs tab surfaces from `Error{cause}` launches — the focused-launch right pane auto-snaps to Logs on the transition so the failure tail is visible without an extra keystroke.
- HF Hub `trendingScore` query-token migration — legacy `sort=trending` now returns HTTP 400, so the `Trending` sort emits `trendingScore` and the URL shape stays uniform across every sort (legacy `search` / `filter` carve-outs reverted).
- `--llama-server <PATH>` is sticky — resolved path is written back into `config.yaml` on every invocation.
- `LLAMASTASH_STATE_DIR` / `LLAMASTASH_CONFIG_DIR` / `LLAMASTASH_CACHE_DIR` env overrides for side-by-side daemons (alongside the existing `LLAMASTASH_SOCKET`).
- Maintainer-only `llamastash uat` (`--features uat`, never shipped on release) drives real-hardware lifecycle tests with a structured JSON report; nightly Metal CI lane in [`.github/workflows/uat-metal-nightly.yml`](.github/workflows/uat-metal-nightly.yml) ([`d1c3a1d`](../../commit/d1c3a1d)).

## How to read this file

Tagged releases land under their version heading; in-flight work accumulates under **Unreleased** until the next tag promotes it. llamastash is pre-1.0 / WIP; the entire pre-release history is bundled under the first publishable tag, [0.0.1], rather than backfilled into a series of synthetic tags. The ledger starts there.
