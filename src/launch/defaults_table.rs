//! Built-in `(architecture, gpu_backend) → TypedKnobs` table.
//!
//! Authoritative opinion on launch flags for every (arch, backend)
//! the recommender can pick. Lives in code so a fresh install on
//! any supported backend gets sensible defaults without ever touching
//! YAML; the wizard no longer seeds `arch_defaults`. The YAML escape
//! hatch stays for hand-edited overrides.
//!
//! Maintenance note (lifted into `AGENTS.md`): when
//! `data/benchmark-snapshot.json` adds a new recommender pick, audit
//! the table coverage. Anything not explicitly listed falls through
//! to the `*` row.

use crate::config::TypedKnobs;
use crate::daemon::host_metrics::GpuFlavor;

/// Look up the built-in defaults row for `(arch, backend)`. The
/// architecture string is lower-cased; unknown architectures fall
/// back to the `*` row. The result already has `None` for fields the
/// row doesn't opinionate — the layered resolver fills the rest from
/// upstream layers (YAML, last_used, preset) or leaves them
/// unset (llama-server default).
///
/// `backend` carries the typed `GpuFlavor` view of
/// `HostMetricsSnapshot::gpu_backend`. `Unsampled` is treated
/// identically to `Unknown` — the brief window after daemon start
/// before the first sampler tick gets the conservative path, not the
/// GPU path.
pub fn lookup(arch: &str, backend: GpuFlavor) -> TypedKnobs {
  let arch = arch.to_ascii_lowercase();
  let explicit = lookup_explicit(arch.as_str(), backend);
  let fallback = lookup_wildcard(backend);
  merge(explicit, fallback)
}

fn lookup_wildcard(backend: GpuFlavor) -> TypedKnobs {
  match backend {
    // Conservative `*` row: GPU backends seed n_gpu_layers=99 only;
    // flash_attn opt-in is per-arch (some architectures don't
    // support flash-attn at all). CPU / unknown / unsampled get
    // nothing.
    GpuFlavor::Nvidia | GpuFlavor::Amd | GpuFlavor::AppleMetal | GpuFlavor::Multi => TypedKnobs {
      n_gpu_layers: Some(99),
      ..TypedKnobs::default()
    },
    GpuFlavor::CpuOnly | GpuFlavor::Unknown | GpuFlavor::Unsampled => TypedKnobs::default(),
  }
}

/// Architecture-specific row. `None` means "no explicit row — caller
/// falls through to the wildcard".
fn lookup_explicit(arch: &str, backend: GpuFlavor) -> Option<TypedKnobs> {
  // Architectures we explicitly cover. Anything else falls through to
  // the `*` row.
  if !COVERED_ARCHS.contains(&arch) {
    return None;
  }
  let mut k = TypedKnobs::default();
  // GPU layers: every covered arch on every GPU backend gets 99.
  if matches!(
    backend,
    GpuFlavor::Nvidia | GpuFlavor::Amd | GpuFlavor::AppleMetal
  ) {
    k.n_gpu_layers = Some(99);
  }
  // flash-attn: only the flash-attn-eligible architectures on
  // nvidia / apple_metal. AMD/HIP coverage is uneven — leave to user
  // override. Vulkan/unknown can't enumerate VRAM safely; CPU
  // obviously doesn't apply.
  if FLASH_ATTN_ELIGIBLE.contains(&arch)
    && matches!(backend, GpuFlavor::Nvidia | GpuFlavor::AppleMetal)
  {
    k.flash_attn = Some(true);
  }
  Some(k)
}

/// Layer `over` onto `under`, taking each `Some` from `over` first.
fn merge(over: Option<TypedKnobs>, under: TypedKnobs) -> TypedKnobs {
  let Some(over) = over else { return under };
  TypedKnobs {
    ctx: over.ctx.or(under.ctx),
    reasoning: over.reasoning.or(under.reasoning),
    n_gpu_layers: over.n_gpu_layers.or(under.n_gpu_layers),
    threads: over.threads.or(under.threads),
    cache_type_k: over.cache_type_k.or(under.cache_type_k),
    cache_type_v: over.cache_type_v.or(under.cache_type_v),
    flash_attn: over.flash_attn.or(under.flash_attn),
    mlock: over.mlock.or(under.mlock),
    no_mmap: over.no_mmap.or(under.no_mmap),
    parallel: over.parallel.or(under.parallel),
    batch_size: over.batch_size.or(under.batch_size),
    ubatch_size: over.ubatch_size.or(under.ubatch_size),
    rope_freq_scale: over.rope_freq_scale.or(under.rope_freq_scale),
    keep: over.keep.or(under.keep),
    device: over.device.or(under.device),
  }
}

/// Architectures the table explicitly covers. Cross-referenced
/// against `data/benchmark-snapshot.json` so every recommender pick
/// hits an explicit row or the `*` fallback.
const COVERED_ARCHS: &[&str] = &[
  "llama",
  "llama2",
  "llama3",
  "llama4",
  "qwen2",
  "qwen2_moe",
  "qwen3",
  "qwen3_moe",
  "qwen3moe",
  "qwen3next",
  "mistral",
  "mixtral",
  "gemma",
  "gemma2",
  "gemma3",
  "phi",
  "phi3",
  "deepseek",
  "deepseek2",
  "deepseek3",
  "granite",
  "falcon",
  "stablelm",
  "command-r",
];

/// Architectures the table opts into `flash_attn: Some(true)` on
/// flash-attn-eligible backends (nvidia / apple_metal).
const FLASH_ATTN_ELIGIBLE: &[&str] = &[
  "qwen2",
  "qwen2_moe",
  "qwen3",
  "qwen3_moe",
  "qwen3moe",
  "qwen3next",
  "llama2",
  "llama3",
  "llama4",
];

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn qwen2_on_nvidia_sets_ngl_and_flash_attn() {
    let k = lookup("qwen2", GpuFlavor::Nvidia);
    assert_eq!(k.n_gpu_layers, Some(99));
    assert_eq!(k.flash_attn, Some(true));
  }

  #[test]
  fn qwen2_on_cpu_only_sets_nothing() {
    let k = lookup("qwen2", GpuFlavor::CpuOnly);
    assert_eq!(k.n_gpu_layers, None);
    assert_eq!(k.flash_attn, None);
  }

  #[test]
  fn unknown_arch_on_nvidia_falls_back_to_wildcard() {
    let k = lookup("entirely-unknown-arch", GpuFlavor::Nvidia);
    assert_eq!(k.n_gpu_layers, Some(99), "wildcard row covers ngl");
    assert_eq!(k.flash_attn, None, "wildcard does not opt into flash_attn");
  }

  #[test]
  fn qwen2_on_unknown_backend_returns_all_none() {
    let k = lookup("qwen2", GpuFlavor::Unknown);
    assert_eq!(k.n_gpu_layers, None);
    assert_eq!(k.flash_attn, None);
  }

  #[test]
  fn qwen2_on_unsampled_treated_as_unknown() {
    // Brief window after daemon start before first sampler tick.
    // Conservative path: no GPU defaults.
    let k = lookup("qwen2", GpuFlavor::Unsampled);
    assert_eq!(k.n_gpu_layers, None);
    assert_eq!(k.flash_attn, None);
  }

  #[test]
  fn qwen2_on_amd_has_ngl_but_no_flash_attn() {
    let k = lookup("qwen2", GpuFlavor::Amd);
    assert_eq!(k.n_gpu_layers, Some(99));
    assert_eq!(
      k.flash_attn, None,
      "HIP flash-attn coverage is uneven — leave it to user override"
    );
  }

  #[test]
  fn gemma_on_nvidia_has_ngl_but_no_flash_attn() {
    let k = lookup("gemma", GpuFlavor::Nvidia);
    assert_eq!(k.n_gpu_layers, Some(99));
    assert_eq!(
      k.flash_attn, None,
      "gemma not on the flash-attn opt-in list at v1"
    );
  }

  #[test]
  fn arch_lookup_is_case_insensitive() {
    let k = lookup("QWEN2", GpuFlavor::Nvidia);
    assert_eq!(k.n_gpu_layers, Some(99));
    assert_eq!(k.flash_attn, Some(true));
  }

  #[test]
  fn apple_metal_qwen3_gets_flash_attn() {
    let k = lookup("qwen3", GpuFlavor::AppleMetal);
    assert_eq!(k.flash_attn, Some(true));
    assert_eq!(k.n_gpu_layers, Some(99));
  }
}
