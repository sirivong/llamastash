//! ds4 (antirez's DwarfStar) backend — a **direct, process-per-model** peer
//! that runs exclusively the DeepSeek V4 Flash/PRO GGUFs through the
//! self-contained `ds4-server` binary.
//!
//! ds4-server is OpenAI/Anthropic-compatible but is *not* `llama-server`: it
//! speaks a smaller flag set, exposes no `/health` and no web UI, and reports
//! a fixed model **alias** (`deepseek-v4-flash` / `deepseek-v4-pro`) rather
//! than the file path. Every one of those divergences is encoded here so the
//! rest of the daemon (supervisor, probe, orphan sweep, proxy) stays
//! backend-agnostic.
//!
//! Routing keys on [`ds4_compatible`] — the header-level quant contract read
//! from ds4's own loader (`ds4.c`) — not on arch alone, because a current
//! upstream llama.cpp (b9840+) also runs `deepseek4` GGUFs. ds4 is the
//! *preferred* engine for the files it can run; llama.cpp is the fallback,
//! never a refusal (an older llama.cpp than b9840 rejects the file with
//! `unknown model architecture: 'deepseek4'`).
//!
//! Facts verified against a real ds4 build (`ds4-server --help`, master
//! 2026-06-17) and the published q2 Flash header:
//! - readiness: `ds4_engine_open` loads the model *before* `listen_on`, so a
//!   `GET /v1/models` 200 means the weights are resident (see [`readiness`]);
//! - flags: ds4-server has `--power`/`--tokens`/`--threads`/`--kv-disk-*`/
//!   `--ssd-streaming` but **not** `--quality` or `--mtp` (those are ds4-CLI
//!   only), so the native-knob table is 6 entries, not 8.

use std::path::{Path, PathBuf};

use super::identity::ModelIdentity;
use super::{
  Accelerator, AcceleratorSupport, Backend, KnobCapability, LaunchPlan, Lifecycle,
  NativeKnobResolution, ProcessLaunchSpec, Readiness, CREDENTIAL_ENV_STRIP,
};
use crate::daemon::context::MethodContext;
use crate::daemon::probe::ProbeOptions;
use crate::gguf::header::{GgufHeader, GgufValue};
use crate::launch::flag_aliases::KnobField;
use crate::launch::mode::LaunchMode;
use crate::launch::native_knobs::{translate, NativeKnobDescriptor, NativeKnobKind};
use crate::launch::params::LaunchParams;

/// The backend id — the stable string used in `BackendChoice`, `status`, the
/// `list`/`show` badge, and adoption dispatch.
pub const DS4_BACKEND_ID: &str = "ds4";

/// Executable name searched on `PATH` when `ds4.binary` is unset. The PATH
/// search (and this const) are compiled out under `test-fixtures` so tests
/// never auto-discover a host `ds4-server`.
#[cfg(not(feature = "test-fixtures"))]
const DS4_SERVER_BIN: &str = "ds4-server";

/// The fixed model aliases ds4-server advertises on `GET /v1/models`. Used
/// by both the readiness body check (D-ready) and orphan adoption (D-adopt).
/// Tolerant of future `deepseek-v4-*` ids via [`is_ds4_alias`].
pub const DS4_ALIAS_IDS: &[&str] = &["deepseek-v4-flash", "deepseek-v4-pro"];

/// The common prefix every ds4 `/v1/models` alias carries. Matching on this
/// (rather than the two exact ids) keeps readiness + adoption tolerant of a
/// future ds4-server build advertising e.g. `deepseek-v4-turbo`.
pub const DS4_ALIAS_PREFIX: &str = "deepseek-v4-";

/// Network-affecting flag heads ds4 adds on top of the base
/// loopback/credential denylist ([`crate::launch::params::FORBIDDEN_ADVANCED_PREFIXES`]).
/// `--cors` weakens the browser same-origin posture on the loopback child;
/// `--dist-` is forward-defense against ds4's distributed serving mode (the
/// current ds4-server build has no `--dist-*` flags, but the ds4 CLI does).
/// `--host`/`--listen` are already covered by the base set.
pub const DS4_FORBIDDEN_EXTRA_HEADS: &[&str] = &["--cors", "--dist-"];

// GGML tensor type ids (subset ds4's contract references). These are the
// on-disk `ggml_type` values in the GGUF tensor-info block.
const GGML_F32: u32 = 0;
const GGML_F16: u32 = 1;
const GGML_Q8_0: u32 = 8;
const GGML_Q2_K: u32 = 10;
const GGML_Q4_K: u32 = 12;
const GGML_IQ2_XXS: u32 = 16;
const GGML_I32: u32 = 26;

/// Whether `id` names a ds4 model alias (the fixed `/v1/models` id). Tolerant
/// of future `deepseek-v4-*` variants beyond the two shipped today.
pub fn is_ds4_alias(id: &str) -> bool {
  id.starts_with(DS4_ALIAS_PREFIX)
}

/// The **ds4-compatibility predicate** — the single routing signal (D-compat).
///
/// A GGUF is ds4-compatible when its arch is `deepseek4` **and** its
/// per-tensor-role quant recipe matches ds4's loader contract (read from
/// `ds4.c`): routed-expert tensors (`ffn_{gate,up,down}_exps`) are one of
/// `IQ2_XXS` / `Q2_K` / `Q4_K` (`tensor_is_routed_expert_type`), and every
/// other weight tensor is `F32` / `F16` / `Q8_0` (`matvec_any`) plus `I32`
/// index tables. Both published Flash/PRO variants pass; a generic
/// third-party `deepseek4` K-quant (Q4_K/Q6_K on attention tensors, Q6_K
/// experts) fails and stays an ordinary llama.cpp model.
///
/// Header-only: it reads tensor-info types, never weight bytes. Requires at
/// least one routed-expert tensor to be present, so a truncated or
/// metadata-only header can't false-positive.
///
/// Re-audit per ds4 release: the type sets are anchored to named `ds4.c`
/// functions and can drift if ds4 adds quant kernels.
pub fn ds4_compatible(header: &GgufHeader) -> bool {
  let arch = header.get("general.architecture").and_then(|v| v.as_str());
  if arch != Some("deepseek4") {
    return false;
  }
  let mut saw_expert = false;
  for t in &header.tensors {
    if is_routed_expert_tensor(&t.name) {
      saw_expert = true;
      if !matches!(t.ggml_type, GGML_IQ2_XXS | GGML_Q2_K | GGML_Q4_K) {
        return false;
      }
    } else if !matches!(t.ggml_type, GGML_F32 | GGML_F16 | GGML_Q8_0 | GGML_I32) {
      return false;
    }
  }
  // A real ds4 model has 43+ routed-expert tensors; requiring one guards
  // against a header whose tensor list was truncated or absent.
  saw_expert
}

/// Whether `filename` names one **half** of ds4's PRO distributed/split GGUF
/// pair (D-guard) — files that are unloadable *alone* by either engine:
/// `DeepSeek-V4-Pro-Q4K-Layers00-30.gguf` (a bare layer-range shard) and
/// `DeepSeek-V4-Pro-Q4K-Layers-31-output.gguf` (the output half).
///
/// Deliberately precise, **not** a bare `-Layers` substring: the single-file
/// Flash `…-Layers37-42Q4KExperts-…-imatrix-fixed.gguf` carries a layer range
/// *followed by quant descriptors* and is a fully launchable model — it must
/// not be refused. Pass the file name (or stem); matching is case-insensitive.
pub fn is_ds4_split_half(filename: &str) -> bool {
  // Strip a trailing `.gguf` so both the stem and the full name work.
  let s = filename
    .to_ascii_lowercase()
    .trim_end_matches(".gguf")
    .to_string();
  // Output half: `…-layers-31-output`.
  if s.contains("layers") && s.ends_with("-output") {
    return true;
  }
  // Layer-range half: the name ends with `layers<digits>-<digits>` and nothing
  // else. The single-file Flash has quant text after the range, so its `split_once`
  // right half is not all-digits and it is not matched.
  if let Some(idx) = s.rfind("layers") {
    let tail = &s[idx + "layers".len()..];
    let tail = tail.strip_prefix('-').unwrap_or(tail);
    if let Some((a, b)) = tail.split_once('-') {
      let all_digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
      if all_digits(a) && all_digits(b) {
        return true;
      }
    }
  }
  false
}

/// ds4's KV-cache byte model, computed from the header. Lives with the backend
/// (moved out of `gguf::memory`) so the ds4-specific cache geometry is
/// self-contained; `gguf::memory::kv_bytes` reaches it through the
/// [`Backend::kv_bytes`] hook, which gates it on the `deepseek4` arch — a
/// deepseek4 GGUF gets this figure even on the llama.cpp fallback, matching the
/// pre-seam behavior. This function itself is ungated (the caller decides), so
/// it stays byte-identical to the estimator code it replaced.
///
/// ds4 keeps, per layer, a small uncompressed recent window plus a compressed
/// cache of `ctx / compress_ratio[layer]` rows; every row is
/// `attention.key_length` F32 latents (mirrors ds4.c's KV policy). Reading the
/// per-layer `attention.compress_ratios` + `key_length` sizes both Flash and
/// PRO from their own headers (~0.5 GiB at 8k ctx, ~11 GiB at 1M for Flash)
/// instead of the naive GQA figure (`head_count_kv=1 × key_length × full ctx`),
/// which ignores the sequence compression and over-counts ~8x at long context.
pub fn ds4_kv_bytes(header: &GgufHeader, ctx: u64) -> u64 {
  let key_length = header.u64(&["deepseek4.attention.key_length"]).unwrap_or(0);
  // ds4 caches F32 latents (`sizeof(float)` in ds4.c).
  const BYTES_PER_ELEM: u64 = 4;
  // Per-layer uncompressed recent window; ds4 sizes it from the prefill chunk
  // (~4k rows). A fixed conservative floor, capped at the context length.
  const RAW_CAP_ROWS: u64 = 4096;
  let raw_rows = RAW_CAP_ROWS.min(ctx.max(1));
  let ratios: Vec<u64> = match header.get("deepseek4.attention.compress_ratios") {
    Some(GgufValue::Array(a)) => a.iter().filter_map(GgufValue::as_u64).collect(),
    _ => Vec::new(),
  };
  let mut rows: u64 = 0;
  if ratios.is_empty() {
    // No per-layer ratios (unexpected for a real ds4 GGUF): fall back to the
    // arch's densest ratio (4) across every layer.
    let n_layers = header.u64(&["deepseek4.block_count"]).unwrap_or(0);
    rows = n_layers.saturating_mul(raw_rows.saturating_add(ctx / 4));
  } else {
    for r in &ratios {
      rows = rows.saturating_add(raw_rows); // uncompressed recent window
      if *r != 0 {
        rows = rows.saturating_add(ctx / r); // compressed rows
      }
    }
  }
  rows
    .saturating_mul(key_length)
    .saturating_mul(BYTES_PER_ELEM)
}

/// Routed-expert tensor marker: `ffn_gate_exps` / `ffn_up_exps` /
/// `ffn_down_exps` all carry the `_exps` component. The shared expert
/// (`ffn_*_shexp`) and the router gate (`ffn_gate_inp`) do **not**, so they
/// correctly fall in the dense/other bucket.
fn is_routed_expert_tensor(name: &str) -> bool {
  name.contains("_exps")
}

/// ds4's native-knob descriptor table (D3) — 6 tunables that have no
/// llama.cpp IR slot. Ids are stable persistence keys; label/description
/// drive the launch-picker rows. Only flags the real `ds4-server` binary
/// accepts (verified via `--help`): `--quality` / `--mtp` are ds4-CLI-only
/// and deliberately excluded.
pub const DS4_NATIVE_KNOBS: &[NativeKnobDescriptor] = &[
  NativeKnobDescriptor {
    id: "power",
    label: "GPU power %",
    description: "GPU duty-cycle target, 1-100 (ds4 default 100)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "tokens",
    label: "Default tokens",
    description: "default max output tokens when a client omits a limit",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "threads",
    label: "CPU threads",
    description: "CPU helper thread count for host-side work",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "kv_disk_dir",
    label: "KV disk dir",
    description: "directory for ds4's persistent disk KV cache (user-owned, never cleaned)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "kv_disk_space_mb",
    label: "KV disk cap",
    description: "disk KV cache budget in MB (ds4 default 4096 when enabled)",
    kind: NativeKnobKind::FreeText,
  },
  NativeKnobDescriptor {
    id: "ssd_streaming",
    label: "SSD streaming",
    description: "stream weights from disk (below-RAM-floor mode; skips the admission gate)",
    kind: NativeKnobKind::Bool,
  },
];

/// `id → ds4-server flag head` mapping consumed by
/// [`crate::launch::native_knobs::translate`]. One row per descriptor.
const DS4_FLAG_MAP: &[(&str, &str)] = &[
  ("power", "--power"),
  ("tokens", "--tokens"),
  ("threads", "--threads"),
  ("kv_disk_dir", "--kv-disk-dir"),
  ("kv_disk_space_mb", "--kv-disk-space-mb"),
  ("ssd_streaming", "--ssd-streaming"),
];

/// Resolve the `ds4-server` binary: an explicit `ds4.binary` config path
/// (must exist), else the first `ds4-server` on `PATH`. Canonicalized.
/// Mirrors [`crate::backend::lemonade::resolve_lemond_binary`].
pub fn resolve_ds4_binary(configured: Option<&Path>) -> Option<PathBuf> {
  fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
  }
  if let Some(explicit) = configured {
    return explicit.is_file().then(|| canonical(explicit));
  }
  // Never auto-discover a host `ds4-server` on `PATH` under the test-fixtures
  // build — same reason as `resolve_lemond_binary`: a real daemon subprocess
  // (with ds4 default-on) must not pick up + leak the developer's system
  // binary. Tests point at an explicit fake `ds4.binary`.
  #[cfg(feature = "test-fixtures")]
  {
    None
  }
  #[cfg(not(feature = "test-fixtures"))]
  {
    let exe = if cfg!(windows) {
      format!("{DS4_SERVER_BIN}.exe")
    } else {
      DS4_SERVER_BIN.to_string()
    };
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
      let candidate = dir.join(&exe);
      if candidate.is_file() {
        return Some(canonical(&candidate));
      }
    }
    None
  }
}

/// The ds4 backend: direct, process-per-model, DeepSeek-V4-only.
#[derive(Debug, Clone)]
pub struct Ds4Backend {
  capabilities: KnobCapability,
}

impl Ds4Backend {
  pub fn new() -> Self {
    // ds4 honors exactly one IR knob — `Ctx` (→ `--ctx`). Everything else
    // llama.cpp-shaped is dropped per R6; ds4's own tunables ride the
    // native-knob channel.
    Self {
      capabilities: KnobCapability::of(&[KnobField::Ctx]),
    }
  }

  /// Build the process-per-model launch spec directly (tests and the
  /// orchestrator's spawn arm both consume this).
  pub fn process_spec(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> ProcessLaunchSpec {
    ProcessLaunchSpec {
      binary,
      argv: ds4_argv(params, port),
      // ds4 reads no env config, but strip the HF pull credentials it has no
      // reason to see — least-privilege applies at least as strongly to a
      // young third-party binary as to llama-server.
      env_remove: CREDENTIAL_ENV_STRIP.to_vec(),
      readiness: readiness(),
      probe,
    }
  }
}

impl Default for Ds4Backend {
  fn default() -> Self {
    Self::new()
  }
}

/// The ds4 readiness contract (D-ready): `GET /v1/models` → 200 **and** a
/// body whose model id is a ds4 alias. The alias check matters because ds4
/// leaves its reserved port *unbound* for the entire multi-minute load
/// (unlike llama-server's immediate bind), so a status-only 200 could come
/// from any process that grabbed the port meanwhile.
pub fn readiness() -> Readiness {
  Readiness::HttpPollModelId {
    path: "/v1/models".to_string(),
    ready_status: 200,
    // Match the `deepseek-v4-` prefix, not the two exact ids: the probe does a
    // substring check against the response body, so the prefix accepts every
    // shipped alias (`-flash` / `-pro`) plus a future `-turbo`, staying in
    // lockstep with `is_ds4_alias` / adoption. Scanning the whole body is
    // consistent with the existing foreign-process-on-port threat model.
    expect_model_ids: vec![DS4_ALIAS_PREFIX.to_string()],
  }
}

/// Build the ds4-server argv. Never `compose` (that emits llama-server
/// flags): `-m <path> --host 127.0.0.1 --port <port>`, `--ctx <n>` when the
/// resolved `Ctx` knob is set, the translated native knobs, then extras —
/// all under the ds4-extended loopback/credential strip. Never `--jinja` /
/// `--reasoning-format` (ds4-server has no such flags).
pub fn ds4_argv(params: &LaunchParams, port: u16) -> Vec<std::ffi::OsString> {
  use std::ffi::OsString;
  let mut argv: Vec<OsString> = vec![
    OsString::from("-m"),
    params.model_path.clone().into_os_string(),
    OsString::from("--host"),
    OsString::from("127.0.0.1"),
    OsString::from("--port"),
    OsString::from(port.to_string()),
  ];
  if let Some(ctx) = params.ctx {
    argv.push(OsString::from("--ctx"));
    argv.push(OsString::from(ctx.to_string()));
  }
  // Native knobs, strip-checked against base ∪ ds4 forbidden heads.
  argv.extend(translate(
    DS4_NATIVE_KNOBS,
    DS4_FLAG_MAP,
    &params.backend_knobs,
    DS4_FORBIDDEN_EXTRA_HEADS,
  ));
  // Free-form extras tail. The fail-fast `forbidden_in_extras_ext` in
  // `compose_and_spawn` already refused a banned head with a clear error;
  // this defensive strip is the belt-and-suspenders that guarantees no
  // banned flag reaches ds4-server even if a path skipped the fail-fast.
  for e in &params.extras {
    let lossy = e.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy);
    if crate::launch::params::is_forbidden_head_ext(head, DS4_FORBIDDEN_EXTRA_HEADS) {
      log::warn!("ds4_argv: stripping forbidden extra {head:?}");
      continue;
    }
    argv.push(e.clone());
  }
  argv
}

impl Backend for Ds4Backend {
  fn id(&self) -> &'static str {
    DS4_BACKEND_ID
  }

  fn lifecycle(&self) -> Lifecycle {
    Lifecycle::ProcessPerModel
  }

  fn capabilities(&self) -> &KnobCapability {
    &self.capabilities
  }

  fn native_knobs(&self) -> &'static [NativeKnobDescriptor] {
    DS4_NATIVE_KNOBS
  }

  fn forbidden_extra_heads(&self) -> &'static [&'static str] {
    DS4_FORBIDDEN_EXTRA_HEADS
  }

  // serves_web_ui: keeps the trait default (`false`) — ds4-server has no web UI,
  // so `/ui` never auto-pins a ds4 model.

  fn accelerators(&self) -> AcceleratorSupport {
    // CPU is the always-available floor; whether a given ds4-server build
    // drives Metal/CUDA/ROCm is a build variant invisible to us (mirrors
    // llama.cpp's conservative floor).
    AcceleratorSupport::from_list([Accelerator::Cpu])
  }

  fn identify(&self, path: &Path, header_bytes: &[u8]) -> ModelIdentity {
    // ds4 models stay plain GGUFs — same `(path, BLAKE3)` identity as
    // llama.cpp, so discovery / favorites / renames are unchanged.
    ModelIdentity::Gguf(crate::gguf::identity::compute(path, header_bytes))
  }

  fn prepare_launch(
    &self,
    params: &LaunchParams,
    port: u16,
    binary: PathBuf,
    probe: ProbeOptions,
  ) -> LaunchPlan {
    LaunchPlan::SpawnProcess(self.process_spec(params, port, binary, probe))
  }

  fn adoption_model_ids(&self) -> &'static [&'static str] {
    DS4_ALIAS_IDS
  }

  fn kv_bytes(&self, header: &GgufHeader, arch: Option<&str>, ctx_len: u64) -> Option<u64> {
    // The compressed-cache model, gated on the `deepseek4` arch exactly as the
    // pre-seam `if a == "deepseek4"` estimator branch was.
    (arch == Some("deepseek4")).then(|| ds4_kv_bytes(header, ctx_len))
  }

  fn auto_routes(&self, header: &GgufHeader) -> bool {
    // The header-level routing predicate: arch `deepseek4` + the per-tensor
    // quant contract. A compatible GGUF prefers ds4 (available + chat mode);
    // otherwise it falls back to llama.cpp — never a refusal.
    ds4_compatible(header)
  }

  fn serves_mode(&self, mode: LaunchMode) -> bool {
    // ds4-server serves chat/completions, not embeddings/rerank — a mode
    // mismatch routes to the fallback engine (a routing input, not an error).
    matches!(mode, LaunchMode::Chat)
  }

  fn refuses(&self, arch: Option<&str>, path: &Path) -> Option<String> {
    // Each half of the distributed/split PRO GGUF pair is unloadable *alone* by
    // any engine, and attempting it wastes a 100 GB+ load. Gated on the
    // `deepseek4` arch so an unrelated GGUF that merely matches the
    // `…-Layers00-30` filename pattern is never wrongly refused.
    if arch != Some("deepseek4") {
      return None;
    }
    let name = path.file_name().and_then(|n| n.to_str())?;
    is_ds4_split_half(name).then(|| {
      format!(
        "`{name}` is one half of ds4's distributed/split PRO GGUF — unloadable on its own. \
         ds4 distributed mode is unsupported; use a single-file DeepSeek-V4 GGUF, or pass \
         `--backend ds4` to attempt it anyway (ds4-server will surface its own error)."
      )
    })
  }

  fn available(&self, ctx: &MethodContext) -> bool {
    // Intent (default-on unless `ds4.enabled: false`, `--ds4`/env force) AND the
    // `ds4-server` binary resolves. The single availability predicate selection,
    // the split-file guard, and `status` all consult.
    ctx.ds4.intends_enabled(ctx.ds4_force)
      && resolve_ds4_binary(ctx.ds4.binary.as_deref()).is_some()
  }

  fn installed(&self, ctx: &MethodContext) -> bool {
    // Presence of the binary, independent of the enablement toggle.
    resolve_ds4_binary(ctx.ds4.binary.as_deref()).is_some()
  }

  fn status_enabled(&self, ctx: &MethodContext) -> Option<bool> {
    Some(self.available(ctx))
  }

  fn binary_path(&self, ctx: &MethodContext) -> Option<String> {
    resolve_ds4_binary(ctx.ds4.binary.as_deref()).map(|b| b.display().to_string())
  }

  fn process_marker(&self) -> Option<&'static str> {
    Some("ds4-server")
  }

  fn resolve_launch_binary(
    &self,
    ctx: &MethodContext,
    _default_binary: PathBuf,
    port: u16,
  ) -> Result<(PathBuf, u16), String> {
    // ds4 spawns `ds4-server` (not the device-owning `llama-server`) on the
    // reserved pool port.
    match resolve_ds4_binary(ctx.ds4.binary.as_deref()) {
      Some(bin) => Ok((bin, port)),
      None => Err(
        "ds4 backend selected but no `ds4-server` binary found; set `ds4.binary` \
         or put `ds4-server` on PATH (see docs/usage.md)"
          .to_string(),
      ),
    }
  }

  async fn resolve_native_knobs(
    &self,
    ctx: &MethodContext,
    params: &mut LaunchParams,
    weights_bytes: u64,
  ) -> NativeKnobResolution {
    // `ssd_streaming` Auto → on when residency won't fit. ds4 holds the full
    // model plus a cached-expert/KV working set the deepseek4 demand model can't
    // see (~1.25× weights), so a full-residency spawn OOM-kills mid-load
    // (ds4-server sets its own oom_score_adj=1000) — disk-stream instead. A user
    // on/off wins; only the unset/Auto knob resolves here.
    let mut out = NativeKnobResolution::default();
    if matches!(
      params.backend_knobs.get("ssd_streaming"),
      Some(crate::config::KnobValue::Set(_))
    ) {
      return out;
    }
    let Some(host_slot) = ctx.host_metrics.as_ref() else {
      return out;
    };
    let snapshot = host_slot.read().await.clone();
    if !crate::launch::admission::is_sampled(&snapshot) {
      return out;
    }
    let free = crate::launch::admission::effective_free_bytes(&snapshot);
    if ds4_should_auto_stream(weights_bytes, free) {
      params.backend_knobs.insert(
        "ssd_streaming".to_string(),
        crate::config::KnobValue::Set("true".to_string()),
      );
      out.auto_set.insert("ssd_streaming".to_string());
      let gib = crate::init::detection::fmt_gib;
      out.warnings.push(format!(
        "ds4 needs ~{} resident but only {} is free — enabled SSD streaming to launch \
         from disk (slower). Set `ssd_streaming: false` to force full residency.",
        gib(ds4_resident_estimate(weights_bytes)),
        gib(free)
      ));
    }
    out
  }

  fn bypasses_admission(&self, params: &LaunchParams) -> bool {
    // Streaming weights from disk skips the hard OOM refusal (on-disk bytes ≠
    // memory demand). Reads the resolved `ssd_streaming` knob.
    matches!(
      params.backend_knobs.get("ssd_streaming"),
      Some(crate::config::KnobValue::Set(v)) if v == "true"
    )
  }
}

/// ds4's resident working set estimate: ~1.25× raw weights (the cached-expert
/// pool + KV + runtime the `deepseek4` demand model can't see). Its own auto
/// budget targets ~99 GiB for an 80 GiB Flash quant, which this tracks.
/// Saturating so a pathological weight total can't overflow.
fn ds4_resident_estimate(weights_total: u64) -> u64 {
  weights_total.saturating_add(weights_total / 4)
}

/// Whether a ds4 launch should auto-enable SSD streaming: its resident estimate
/// exceeds the effective free memory, so a full-residency spawn would OOM-kill
/// mid-load. Pure so the memory decision is unit-testable without a live host
/// sampler.
fn ds4_should_auto_stream(weights_total: u64, free: u64) -> bool {
  ds4_resident_estimate(weights_total) > free
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::KnobValue;
  use crate::gguf::header::{GgufHeader, GgufValue, TensorInfo};
  use crate::launch::mode::LaunchMode;
  use std::collections::HashMap;
  use std::ffi::OsString;

  fn header(arch: &str, tensors: &[(&str, u32)]) -> GgufHeader {
    let mut metadata = HashMap::new();
    metadata.insert(
      "general.architecture".to_string(),
      GgufValue::String(arch.to_string()),
    );
    GgufHeader {
      version: 3,
      tensor_count: tensors.len() as u64,
      metadata,
      tensors: tensors
        .iter()
        .map(|(name, ty)| TensorInfo {
          name: name.to_string(),
          dims: vec![4096, 4096],
          ggml_type: *ty,
        })
        .collect(),
    }
  }

  fn params_with(ctx: Option<u32>, knobs: &[(&str, &str)]) -> LaunchParams {
    let mut p = LaunchParams::new(PathBuf::from("/m/ds4flash.gguf"), LaunchMode::Chat);
    p.ctx = ctx;
    for (k, v) in knobs {
      p.backend_knobs
        .insert((*k).to_string(), KnobValue::Set((*v).to_string()));
    }
    p
  }

  fn argv_strings(p: &LaunchParams, port: u16) -> Vec<String> {
    ds4_argv(p, port)
      .iter()
      .map(|s| s.to_string_lossy().into_owned())
      .collect()
  }

  // ---- compat predicate ----

  #[test]
  fn compat_accepts_both_published_recipes() {
    // q2 Flash: IQ2_XXS gate/up experts, Q2_K down experts, Q8_0/F16/F32/I32
    // elsewhere (matches the real published header).
    let q2 = header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS),
        ("blk.0.ffn_down_exps.weight", GGML_Q2_K),
        ("blk.0.ffn_gate_tid2eid.weight", GGML_I32),
        ("blk.0.attn_q.weight", GGML_Q8_0),
        ("token_embd.weight", GGML_F16),
        ("output_norm.weight", GGML_F32),
      ],
    );
    assert!(ds4_compatible(&q2), "q2 IQ2_XXS/Q2_K recipe");
    // q4 Flash: Q4_K experts.
    let q4 = header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_Q4_K),
        ("blk.0.ffn_up_exps.weight", GGML_Q4_K),
        ("blk.0.attn_q.weight", GGML_Q8_0),
        ("token_embd.weight", GGML_F16),
      ],
    );
    assert!(ds4_compatible(&q4), "q4 Q4_K expert recipe");
  }

  #[test]
  fn compat_rejects_for_the_right_reason() {
    // Wrong arch.
    assert!(!ds4_compatible(&header(
      "deepseek2",
      &[("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS)]
    )));
    // Q4_K on an attention (non-expert) projection — a generic K-quant.
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS),
        ("blk.0.attn_q.weight", GGML_Q4_K),
      ]
    )));
    // Q6_K expert (14) — outside the routed-expert set.
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[("blk.0.ffn_gate_exps.weight", 14)]
    )));
    // BF16 tensor (30) anywhere.
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[
        ("blk.0.ffn_gate_exps.weight", GGML_IQ2_XXS),
        ("token_embd.weight", 30),
      ]
    )));
    // deepseek4 arch but no expert tensors (metadata-only / truncated).
    assert!(!ds4_compatible(&header(
      "deepseek4",
      &[("token_embd.weight", GGML_F16)]
    )));
  }

  // ---- argv builder ----

  #[test]
  fn argv_defaults_only_is_loopback_pinned() {
    let p = params_with(None, &[]);
    assert_eq!(
      argv_strings(&p, 41100),
      vec![
        "-m",
        "/m/ds4flash.gguf",
        "--host",
        "127.0.0.1",
        "--port",
        "41100"
      ]
    );
  }

  #[test]
  fn argv_emits_ctx_and_native_knobs() {
    let p = params_with(
      Some(32768),
      &[
        ("power", "60"),
        ("kv_disk_dir", "/tmp/kv"),
        ("ssd_streaming", "true"),
      ],
    );
    let a = argv_strings(&p, 8000);
    assert!(a.windows(2).any(|w| w == ["--ctx", "32768"]));
    assert!(a.windows(2).any(|w| w == ["--power", "60"]));
    assert!(a.windows(2).any(|w| w == ["--kv-disk-dir", "/tmp/kv"]));
    // Bool knob emits a bare flag.
    assert!(a.contains(&"--ssd-streaming".to_string()));
    // Never a reasoning/jinja flag.
    assert!(!a.iter().any(|s| s == "--jinja" || s.contains("reasoning")));
  }

  #[test]
  fn argv_never_emits_jinja_even_when_params_ask() {
    let mut p = params_with(None, &[]);
    p.jinja = true;
    p.reasoning = true;
    let a = argv_strings(&p, 8000);
    assert!(!a.iter().any(|s| s == "--jinja"));
    assert!(!a.iter().any(|s| s.contains("reasoning")));
  }

  #[test]
  fn argv_strips_loopback_and_ds4_forbidden_extras() {
    let mut p = params_with(None, &[]);
    p.extras = vec![
      OsString::from("--host"),
      OsString::from("0.0.0.0"),
      OsString::from("--cors"),
      OsString::from("--dist-worker"),
      OsString::from("--power"),
      OsString::from("70"),
    ];
    let a = argv_strings(&p, 8000);
    // Forbidden heads gone; the benign `--power 70` survives.
    assert!(!a.contains(&"--cors".to_string()));
    assert!(!a.contains(&"--dist-worker".to_string()));
    // The security property: no rebind head survives — the only `--host` in
    // argv is our own loopback one. (An orphaned `0.0.0.0` value token is
    // inert without its `--host` flag; the real guard is the fail-fast
    // `forbidden_in_extras_ext` refusal in `compose_and_spawn`, which never
    // lets such extras reach here in production.)
    assert_eq!(a.iter().filter(|s| *s == "--host").count(), 1);
    assert!(a.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
    assert!(a.windows(2).any(|w| w == ["--power", "70"]));
  }

  #[test]
  fn argv_native_value_smuggling_forbidden_head_is_stripped() {
    // A free-text native value that tries to smuggle `--cors` is dropped.
    let p = params_with(None, &[("kv_disk_dir", "/x --cors")]);
    let a = argv_strings(&p, 8000);
    assert!(!a.iter().any(|s| s.contains("--cors")));
  }

  // ---- descriptor table + capabilities ----

  #[test]
  fn native_knob_ids_are_unique_and_documented() {
    let b = Ds4Backend::new();
    let descs = b.native_knobs();
    assert_eq!(
      descs.len(),
      6,
      "ds4 exposes 6 native knobs (no quality/mtp)"
    );
    let mut ids: Vec<&str> = descs.iter().map(|d| d.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 6, "ids are unique persistence keys");
    for d in descs {
      assert!(!d.label.is_empty(), "{} has a label", d.id);
      assert!(!d.description.is_empty(), "{} has a description", d.id);
      // Every descriptor id has a flag mapping.
      assert!(
        DS4_FLAG_MAP.iter().any(|(id, _)| *id == d.id),
        "{} has a flag mapping",
        d.id
      );
    }
  }

  #[test]
  fn capabilities_honor_only_ctx() {
    let b = Ds4Backend::new();
    assert!(b.capabilities().supports(KnobField::Ctx));
    assert!(!b.capabilities().supports(KnobField::NGpuLayers));
    assert!(!b.capabilities().supports(KnobField::FlashAttn));
  }

  #[test]
  fn readiness_is_models_endpoint_with_alias_check() {
    match readiness() {
      Readiness::HttpPollModelId {
        path,
        ready_status,
        expect_model_ids,
      } => {
        assert_eq!(path, "/v1/models");
        assert_eq!(ready_status, 200);
        // The prefix (a substring of every ds4 alias body) is what the probe
        // matches on, so `-flash` / `-pro` / a future `-turbo` all pass.
        assert_eq!(expect_model_ids, vec![DS4_ALIAS_PREFIX.to_string()]);
        assert!("deepseek-v4-flash".starts_with(&expect_model_ids[0]));
        assert!("deepseek-v4-turbo".starts_with(&expect_model_ids[0]));
      }
      _ => panic!("ds4 readiness must be HttpPollModelId"),
    }
  }

  #[test]
  fn split_half_guard_is_precise() {
    // The two genuinely-unlaunchable PRO split halves.
    assert!(is_ds4_split_half("DeepSeek-V4-Pro-Q4K-Layers00-30.gguf"));
    assert!(is_ds4_split_half(
      "DeepSeek-V4-Pro-Q4K-Layers-31-output.gguf"
    ));
    // The single-file Flash with a layer range in its name is launchable —
    // MUST NOT be refused.
    assert!(!is_ds4_split_half(
      "DeepSeek-V4-Flash-Layers37-42Q4KExperts-OtherExpertLayersIQ2XXSGateUp-Q2KDown-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix-fixed.gguf"
    ));
    // The standard single-file Flash/PRO quants are not halves.
    assert!(!is_ds4_split_half(
      "DeepSeek-V4-Flash-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-chat-v2-imatrix.gguf"
    ));
    assert!(!is_ds4_split_half(
      "DeepSeek-V4-Pro-IQ2XXS-w2Q2K-AProjQ8-SExpQ8-OutQ8-Instruct.gguf"
    ));
    // An unrelated model with no layer split.
    assert!(!is_ds4_split_half("qwen2.5-7b-instruct-q4_k_m.gguf"));
  }

  #[test]
  fn alias_matcher_tolerates_future_variants() {
    assert!(is_ds4_alias("deepseek-v4-flash"));
    assert!(is_ds4_alias("deepseek-v4-pro"));
    assert!(is_ds4_alias("deepseek-v4-turbo")); // future variant
    assert!(!is_ds4_alias("llama"));
    assert!(!is_ds4_alias("deepseek-v3"));
  }

  #[test]
  fn typed_knobs_outside_ctx_never_reach_argv() {
    let mut p = params_with(None, &[]);
    p.knobs.n_gpu_layers = Some(KnobValue::Set(99));
    p.knobs.flash_attn = Some(KnobValue::Set(true));
    let a = argv_strings(&p, 8000);
    assert!(!a.iter().any(|s| s == "-ngl" || s == "--n-gpu-layers"));
    // Precise flag check (the model path itself contains "flash").
    assert!(!a.iter().any(|s| s == "--flash-attn" || s == "-fa"));
  }

  /// Optional real-file check: when the published q2 Flash header is present
  /// in the local HF cache, the predicate must accept it. Ignored by default
  /// (needs the 86 GB download); run with `--ignored` on a UAT box.
  #[test]
  #[ignore]
  fn compat_accepts_real_published_flash_header() {
    let blob = PathBuf::from(
      "/mnt/work/huggingface/hub/models--antirez--deepseek-v4-gguf/blobs/\
       efc7ed607ff27076e3e501fc3fefefa33c0ed8cf1eff483a2b7fdc0c2e616668",
    );
    if !blob.is_file() {
      return;
    }
    let read =
      crate::gguf::header::read_path(&blob, crate::gguf::header::HeaderReadOptions::default())
        .expect("read real ds4 header");
    assert!(
      ds4_compatible(&read.header),
      "real published Flash must be ds4-compatible"
    );
  }

  #[test]
  fn auto_stream_triggers_only_when_residency_exceeds_free() {
    let gib = |g: u64| g * 1024 * 1024 * 1024;
    // 80 GiB Flash weights → ~100 GiB resident estimate.
    assert_eq!(ds4_resident_estimate(gib(80)), gib(100));
    // Won't fit: 100 GiB resident > 95 GiB free → stream (the Strix Halo case).
    assert!(ds4_should_auto_stream(gib(80), gib(95)));
    // Fits with headroom: 100 GiB resident < 200 GiB free → full residency.
    assert!(!ds4_should_auto_stream(gib(80), gib(200)));
    // Exact boundary is not a shortfall (estimate == free → no stream).
    assert!(!ds4_should_auto_stream(gib(80), gib(100)));
    // A pathological weight total saturates instead of overflowing.
    assert!(ds4_should_auto_stream(u64::MAX, gib(100)));
  }
}
