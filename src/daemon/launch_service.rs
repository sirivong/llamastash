//! The daemon-side launch pipeline.
//!
//! `compose_and_spawn` is the one code path that turns a parsed
//! `StartParams` into a running supervised model: input validation →
//! identity / arch resolution → race-safe port reservation → layered
//! knob merge → memory admission → supervisor spawn → registry insert →
//! last-params recorder. The IPC `start_model` handler and the proxy's
//! auto-start path both call it, so the two surfaces can never drift in
//! how a launch is composed. Managed-multiplexer (Lemonade) launches
//! branch off into `start_delegated_lemonade`, anchored on the shared
//! umbrella rather than a per-model child.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::backend::identity::ModelIdentity;
use crate::backend::{Backend, LaunchPlan};
use crate::config::MAX_CTX_TOKENS;
use crate::config::{KnobValue, KnobValueOpt};
use crate::daemon::context::{MethodContext, PersistedState};
use crate::daemon::registry::LaunchId;
use crate::daemon::shutdown::ShutdownToken;
use crate::daemon::state_store::RunningSnapshot;
use crate::daemon::supervisor::{
  spawn as supervisor_spawn, ManagedModel, ManagedSpawn, ManagedState,
};
use crate::gguf::header::{read_path as read_gguf_header, HeaderReadOptions};
use crate::gguf::identity::ModelId;
use crate::ipc::methods::resolve_model_id_and_arch;
use crate::ipc::protocol::{ErrorCode, ErrorObject};
use crate::launch::mode::LaunchMode;
use crate::launch::params::LaunchParams;

/// Wire-shape for the `start_model` IPC method. The fields land
/// verbatim from JSON-RPC; `compose_and_spawn` consumes the parsed
/// struct so the proxy's auto-start path can build one
/// directly without going through JSON.
#[derive(Deserialize, Default, Clone)]
pub(crate) struct StartParams {
  /// Absolute path to the GGUF the user wants to launch. We compute
  /// the canonical `ModelId` by reading its header on the daemon
  /// side rather than trusting the caller — keeps the surface
  /// minimal for CLI/TUI clients.
  pub(crate) model_path: PathBuf,
  #[serde(default)]
  pub(crate) mode: Option<LaunchModeWire>,
  #[serde(default)]
  pub(crate) ctx: Option<u32>,
  #[serde(default)]
  pub(crate) port: Option<u16>,
  /// Soft port preference — if the supplied port is free at
  /// reservation time, use it; otherwise allocate from the
  /// configured range. Distinct from `port` which is strict and
  /// errors on conflict. The TUI sets this so a returning user
  /// lands on their previously-bound port without scripted clients
  /// silently losing strict semantics.
  #[serde(default)]
  pub(crate) prefer_port: Option<u16>,
  #[serde(default)]
  pub(crate) reasoning: Option<bool>,
  /// Caller-supplied typed knob overrides. Each `Some` field lands
  /// on the `LayerLabel::User` layer of the resolver, outranking
  /// last_used / arch_default / built-in.
  #[serde(default)]
  pub(crate) knobs: crate::config::TypedKnobs,
  /// Free-form argv tail for `llama-server` flags the typed editor
  /// doesn't model. Appended after the resolved knobs.
  #[serde(default)]
  pub(crate) extras: Vec<String>,
  /// Per-backend native-knob values, keyed by descriptor id (see
  /// [`crate::launch::native_knobs`]). Passed through to the backend's
  /// `prepare_launch`; not layered by the typed-knob resolver. Empty for
  /// llama.cpp / Lemonade (`#[serde(default)]` keeps existing clients'
  /// payloads byte-stable). ds4 is the first consumer.
  #[serde(default)]
  pub(crate) backend_knobs: std::collections::BTreeMap<String, crate::config::KnobValue<String>>,
  /// Optional path to a multimodal projector (mmproj) file. When
  /// `None`, the daemon auto-detects by scanning the parent
  /// directory of the model for a `mmproj-<stem>.gguf` companion.
  /// Set to `Some(path)` to override the auto-detection, or leave
  /// as `None` to let the daemon find it automatically.
  #[serde(default)]
  pub(crate) mmproj_path: Option<PathBuf>,
  /// Per-model backend override. `None` / `auto` runs the identity
  /// rule (GGUF → llama.cpp, registry → its backend); an explicit value
  /// forces a backend. Set by `start --backend` and the TUI Launch picker.
  #[serde(default)]
  pub(crate) backend: Option<crate::launch::params::BackendChoice>,
  /// How the caller selected launch params — drives whether the daemon
  /// applies the model's configured `default:` preset and `last_params`
  /// inheritance. Absent on the wire ⇒ `Default` (no selection), which is
  /// what the proxy's `StartParams::default()` auto-start path sends.
  #[serde(default)]
  pub(crate) selection: LaunchSelection,
}

/// How a launch chose its parameters. See the resolver rule in
/// `compose_and_spawn`: `Default` applies the effective default
/// (`PresetDefault` → `LastUsed`); `Explicit` means the caller already
/// flattened a named preset / inline flags into `knobs`/`extras` (skip the
/// default layer, let `last_params` fill knob gaps, extras verbatim);
/// `Auto` is pure fit (skip the default layer and `last_params`, no extras).
#[derive(Deserialize, Serialize, Clone, Copy, Default, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaunchSelection {
  #[default]
  Default,
  Explicit,
  Auto,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LaunchModeWire {
  Chat,
  Embedding,
  Rerank,
}

impl From<LaunchModeWire> for LaunchMode {
  fn from(m: LaunchModeWire) -> Self {
    match m {
      LaunchModeWire::Chat => LaunchMode::Chat,
      LaunchModeWire::Embedding => LaunchMode::Embedding,
      LaunchModeWire::Rerank => LaunchMode::Rerank,
    }
  }
}

/// Output of [`compose_and_spawn`] — everything the caller needs to
/// observe the launch from the outside. The IPC handler projects
/// this onto the JSON-RPC response; the proxy's auto-start path
///  keeps the `ManagedModel` handle so it can poll the state
/// machine without going through the registry snapshot.
pub(crate) struct StartedLaunch {
  pub(crate) launch_id: LaunchId,
  pub(crate) model_id: ModelId,
  pub(crate) port: u16,
  pub(crate) model: ManagedModel,
  pub(crate) log_path: PathBuf,
  /// Non-fatal advisories surfaced to the caller (CLI human output / TUI
  /// toast): capability-dropped knobs, the deepseek4 KV-blind admission note,
  /// and the `ssd_streaming` admission-bypass note. Empty on a clean launch.
  pub(crate) warnings: Vec<String>,
}

/// The one launch-composition pipeline, for callers that already have a
/// parsed [`StartParams`]: the IPC `start_model` handler and the proxy's
/// auto-start path. Performs validation → arch resolve → port
/// reservation → layered knob merge → supervisor spawn → registry insert
/// → last_params recorder, so the two call sites share one code path.
/// Returns the live [`StartedLaunch`] handle on success; the
/// [`ErrorObject`] form on any failure stays JSON-RPC compatible so the
/// IPC handler can forward it verbatim.
pub(crate) async fn compose_and_spawn(
  ctx: &MethodContext,
  parsed: StartParams,
  origin: crate::daemon::supervisor::LaunchOrigin,
) -> Result<StartedLaunch, ErrorObject> {
  // Pure input-validation lives before the daemon's launch-env
  // lookup so a malformed request gives an actionable
  // `InvalidParams` error even on misconfigured daemons.
  if parsed.port.is_some() && parsed.prefer_port.is_some() {
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      "set exactly one of `port` (strict) or `prefer_port` (soft preference)",
    ));
  }
  let env = ctx.launch.as_ref().ok_or_else(|| {
    ErrorObject::new(
      ErrorCode::InternalError,
      "daemon launch environment not configured (binary / port range / log dir missing)",
    )
  })?;

  // Resolve identity + (for GGUF) the architecture. A Lemonade synthetic path
  // (`lemonade://<name>`) has no local file, so derive a backend identity from
  // the registry name instead of reading a header — that is what makes the
  // managed-multiplexer dispatch below select Lemonade rather than crashing on
  // the missing GGUF. Every other path is a local GGUF: one header read yields
  // both the canonical id and the arch.
  let (id, arch, native_ctx, ds4_compatible, identity): (
    ModelId,
    Option<String>,
    Option<u32>,
    bool,
    ModelIdentity,
  ) = match crate::backend::lemonade::registry_name_from_path(&parsed.model_path) {
    Some(name) => {
      let backend_id = crate::backend::identity::BackendModelId {
        backend: crate::backend::lemonade::LEMONADE_BACKEND_ID.to_string(),
        name: name.to_string(),
      };
      // A synthetic ModelId keeps the file-keyed plumbing (log path, running
      // snapshot retention) working; the sentinel header hash marks it as
      // not-a-GGUF. Arch + native_ctx are `None` — lemond owns the recipe,
      // not us, so the strict-fit ctx gate never applies to a Lemonade row.
      let synthetic = ModelId {
        path: parsed.model_path.clone(),
        header_blake3: [0u8; 32],
      };
      (
        synthetic,
        None,
        None,
        false,
        ModelIdentity::Backend(backend_id),
      )
    }
    None => {
      let (id, arch, native_ctx, ds4_compatible) = resolve_model_id_and_arch(&parsed.model_path)?;
      let identity: ModelIdentity = id.clone().into();
      (id, arch, native_ctx, ds4_compatible, identity)
    }
  };

  // Split-PRO guard (D-guard): each half of ds4's distributed Q4 GGUF pair is
  // unloadable *alone* by either engine, and attempting it wastes a 100 GB+
  // load. Refuse pre-spawn on an auto-routed launch (an explicit `--backend`
  // passes through so the engine can surface its own error). Gated on the
  // `deepseek4` arch so an *unrelated* GGUF that merely matches the
  // `…-Layers00-30` filename pattern is never wrongly refused.
  if parsed.backend.unwrap_or_default() == crate::launch::params::BackendChoice::Auto
    && arch.as_deref() == Some("deepseek4")
  {
    if let Some(name) = parsed.model_path.file_name().and_then(|n| n.to_str()) {
      if crate::backend::ds4::is_ds4_split_half(name) {
        return Err(ErrorObject::new(
          ErrorCode::InvalidParams,
          format!(
            "`{name}` is one half of ds4's distributed/split PRO GGUF — unloadable on its own. \
             ds4 distributed mode is unsupported; use a single-file DeepSeek-V4 GGUF, or pass \
             `--backend ds4` to attempt it anyway (ds4-server will surface its own error)."
          ),
        ));
      }
    }
  }

  // Mode resolution: explicit override > catalog hint > default to chat.
  // The CLI surface refuses to default silently when discovery says
  // "Unknown" (cli_args.rs::StartArgs::mode comment), but the daemon
  // is one layer down; a missing override here means the caller has
  // already accepted the default.
  let mode = parsed
    .mode
    .map(LaunchMode::from)
    .unwrap_or(LaunchMode::Chat);

  // Reject pinned port values that would corrupt our internal state
  // or require root: any value below 1024 (which also covers 0, the
  // "OS picks for me" sentinel llama-server would pick a port we
  // never track) needs root and is almost certainly a typo / hostile.
  for p in parsed.port.iter().chain(parsed.prefer_port.iter()) {
    if *p < 1024 {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("port {p} is not in the allowed range (>= 1024)"),
      ));
    }
  }
  // Validate ctx token-window bound.
  if let Some(c) = parsed.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }

  // Port allocation — race-safe. `reserve_port` is a CAS across
  // `collect_in_use_ports → allocate → reserve` so two concurrent
  // `start_model` calls cannot both walk away with the same port.
  // We must collect the live in-use list before taking the
  // reservation mutex, since `collect_in_use_ports` itself awaits
  // supervisor read locks.
  let live_in_use = collect_in_use_ports(ctx).await;
  let port = if let Some(preferred) = parsed.prefer_port {
    // Soft preference: try the requested port first; on conflict
    // fall back to the auto-allocator so a returning TUI user
    // doesn't fail launches just because their old port is taken.
    match ctx
      .supervisors
      .reserve_port(Some(preferred), &live_in_use, &env.port_range)
      .await
    {
      Ok(p) => p,
      Err(_) => ctx
        .supervisors
        .reserve_port(None, &live_in_use, &env.port_range)
        .await
        .map_err(|e| {
          ErrorObject::new(
            ErrorCode::InternalError,
            format!("port allocation failed: {e}"),
          )
        })?,
    }
  } else {
    ctx
      .supervisors
      .reserve_port(parsed.port, &live_in_use, &env.port_range)
      .await
      .map_err(|e| {
        ErrorObject::new(
          ErrorCode::InternalError,
          format!("port allocation failed: {e}"),
        )
      })?
  };

  // Compose LaunchParams with the layered resolver. Precedence
  // (highest first): caller-supplied `knobs` → daemon's persisted
  // `last_params` for this model → YAML `arch_defaults[architecture]`
  // → built-in `(arch, backend)` table → llama-server's own default.
  let mut launch_params = LaunchParams::new(parsed.model_path.clone(), mode);
  launch_params.port = Some(port);
  // Per-model backend override: `None` keeps the default `Auto`
  // (identity rule); an explicit choice from `start --backend` / the TUI
  // picker overrides it.
  launch_params.backend = parsed.backend.unwrap_or_default();

  // Resolve the backend up front (D-route) so both the launch plan *and* the
  // cross-backend contamination gate below see the same decision. Selection
  // honors the per-model override, then the ds4-compatibility signal (a
  // compatible GGUF prefers ds4 when it's available and the mode fits), then
  // the identity rule (`Auto` → GGUF binds llama.cpp; a `deepseek4` GGUF still
  // runs there as fallback).
  let sel_ctx = crate::backend::SelectionContext {
    ds4_compatible,
    ds4_available: ctx.ds4_available(),
    // ds4-server serves chat/completions, not embeddings/rerank — a mode
    // mismatch routes to llama.cpp (a routing input, not an error).
    ds4_mode_ok: mode == LaunchMode::Chat,
  };
  let inference_backend =
    crate::backend::resolve_backend_for_launch(&identity, launch_params.backend, &sel_ctx);
  let resolved_backend_id = crate::backend::Backend::id(&inference_backend).to_string();

  // The model's last successful launch params + the backend it resolved to.
  // Cloned once here and reused for the last-used knob layer below.
  let last_params_entry = {
    let snap = ctx.state.snapshot().await;
    snap
      .last_params
      .iter()
      .find(|e| e.id == identity)
      .map(|e| (e.params.clone(), e.resolved_backend.clone()))
  };
  // D-contamination: the implicit LastUsed layer + extras inheritance apply
  // only when the stored launch resolved to the *same* backend, so llama.cpp
  // extras (`--rope-freq-base …`) saved before ds4 existed can't poison a ds4
  // spawn (and vice versa). Explicit config (presets, inline extras) is
  // untouched. A legacy row with no tag reads as `llamacpp`.
  let last_params_backend_ok = last_params_entry
    .as_ref()
    .map(|(_, tag)| tag == &resolved_backend_id)
    .unwrap_or(false);
  let last_params = last_params_entry
    .as_ref()
    .filter(|_| last_params_backend_ok)
    .map(|(p, _)| p.clone());

  // The model's configured `default:` preset (config-only), resolved
  // server-side so it applies uniformly on CLI plain `start`, the TUI, and
  // proxy auto-start. Only a no-selection launch consults it, so explicit /
  // auto launches skip the preset-store snapshot + catalog projection. Read
  // via the same `effective_presets` the IPC handlers use.
  let is_default_sel = matches!(parsed.selection, LaunchSelection::Default);
  let effective_default = if is_default_sel {
    let store = ctx.presets.snapshot().await;
    let rows = crate::ipc::methods::catalog_rows(ctx).await;
    let key = crate::util::paths::path_basename(&parsed.model_path);
    let path_str = parsed.model_path.display().to_string();
    Some(crate::launch::presets::effective_presets(
      &key,
      &path_str,
      arch.as_deref(),
      &store,
      &rows,
    ))
  } else {
    None
  };

  // Collapse the launch into one resolution shape. `Auto` (explicit
  // `--preset auto`) and a no-selection launch whose config default is
  // `auto` both mean "pure fit": skip the default-preset and last_params
  // layers entirely. A no-selection launch otherwise applies the effective
  // default (the `PresetDefault` layer when `default:` names a preset, then
  // last_params). An explicit launch carries its own flattened knobs/extras.
  let default_is_auto = effective_default
    .as_ref()
    .is_some_and(|e| e.default_is_auto());
  let pure_fit = matches!(parsed.selection, LaunchSelection::Auto) || default_is_auto;
  let no_selection = is_default_sel && !pure_fit;

  // Free-form extras (whole-list, no per-flag merge). Explicit inline extras
  // are always honored verbatim. Otherwise a no-selection launch inherits the
  // effective default's extras (the default preset's when `default:` names one
  // with extras, else last_params'); everything else (pure fit, or an
  // explicit preset that carried no extras) gets none. This supersedes the
  // PR #49 origin gate — inheritance is driven by "did the caller make a
  // selection", not Manual-vs-AutoStart, and `auto` is the clean "no inherit"
  // gesture.
  launch_params.extras = if !parsed.extras.is_empty() {
    parsed.extras.iter().cloned().map(OsString::from).collect()
  } else if no_selection {
    effective_default
      .as_ref()
      .and_then(|e| e.default_preset())
      .map(|np| np.params.extras.clone())
      .filter(|e| !e.is_empty())
      .or_else(|| last_params.as_ref().map(|p| p.extras.clone()))
      .unwrap_or_default()
  } else {
    Vec::new()
  };
  // Native knobs (not layered by the typed-knob resolver): explicit inline
  // values win verbatim; else a no-selection relaunch inherits the last-used
  // native knobs — but only through the backend-matched `last_params` gate
  // above (D-contamination), so a ds4 relaunch re-applies its `--power` /
  // `--kv-disk-*` while a cross-backend run inherits nothing. Empty for
  // llama.cpp / Lemonade.
  launch_params.backend_knobs = if !parsed.backend_knobs.is_empty() {
    parsed.backend_knobs.clone()
  } else if no_selection {
    last_params
      .as_ref()
      .map(|p| p.backend_knobs.clone())
      .unwrap_or_default()
  } else {
    std::collections::BTreeMap::new()
  };
  // Resolve the multimodal projector: an explicit `mmproj_path` wins;
  // otherwise auto-detect a companion next to the model — unless the
  // caller is already managing the projector through `extras`
  // (`--mmproj` to pin a path, `--no-mmproj` to force text-only), in
  // which case auto-detection would only emit a redundant second flag.
  launch_params.mmproj_path = parsed.mmproj_path.clone().or_else(|| {
    if extras_manage_mmproj(&launch_params.extras) {
      None
    } else {
      crate::discovery::scanner::find_mmproj(&parsed.model_path)
    }
  });

  // Merge the caller's top-level `ctx` and `reasoning` into the
  // User-layer typed knobs so they participate in the resolver chain
  // alongside the other typed fields. The wire payload keeps the
  // top-level fields for backward compat with scripted clients —
  // they're projected onto the typed knob slots here, with explicit
  // `knobs.{ctx,reasoning}` overrides winning if the caller set both.
  let mut user_knobs = parsed.knobs.clone();
  if user_knobs.ctx.is_none() {
    user_knobs.ctx = parsed.ctx.map(KnobValue::Set);
  }
  if user_knobs.reasoning.is_none() {
    user_knobs.reasoning = parsed.reasoning.map(KnobValue::Set);
  }

  // Last-used knobs from the snapshot taken above, so a returning user
  // inherits the knobs they last shipped.
  let last_params_knobs = last_params
    .as_ref()
    .map(|p| p.knobs.clone())
    .unwrap_or_default();
  // The default preset's knobs (no-selection + named default only). Built
  // via `preset_body_from_launch_params` so the preset's `ctx`/`reasoning`
  // (held as `LaunchParams` siblings) fold back into the knob set.
  let default_preset_knobs = if no_selection {
    effective_default
      .as_ref()
      .and_then(|e| e.default_preset())
      .map(|np| crate::launch::presets::preset_body_from_launch_params(&np.params).knobs)
  } else {
    None
  };
  let empty_yaml = crate::config::TypedKnobs::default();
  let yaml_knobs = arch
    .as_deref()
    .and_then(|a| env.arch_defaults.get(a))
    .unwrap_or(&empty_yaml);
  let backend = current_backend_flavor(ctx).await;
  let builtin_knobs = match arch.as_deref() {
    Some(a) => crate::launch::defaults_table::lookup(a, backend),
    None => crate::launch::defaults_table::lookup("", backend),
  };
  // Build the precedence chain per resolution shape. `User` always leads.
  // `PresetDefault` (named config default) ranks below User, above LastUsed.
  // `LastUsed` is skipped under pure fit. yaml + built-in share the
  // `ArchDefault` chip — yaml wins per field via precedence order.
  use crate::launch::params::LayerLabel;
  let mut layers: Vec<(LayerLabel, &crate::config::TypedKnobs)> =
    vec![(LayerLabel::User, &user_knobs)];
  if let Some(k) = default_preset_knobs.as_ref() {
    layers.push((LayerLabel::PresetDefault, k));
  }
  if !pure_fit {
    layers.push((LayerLabel::LastUsed, &last_params_knobs));
  }
  layers.push((LayerLabel::ArchDefault, yaml_knobs));
  layers.push((LayerLabel::ArchDefault, &builtin_knobs));
  let mut resolved = crate::launch::params::resolve_layered(&layers);
  // Seed knobs no layer filled per the default launch mode: under
  // `Auto` a layer-less knob delegates to `--fit` (an Auto knob emits
  // nothing, exactly like the unset slot it replaces). The mode is
  // `Config.default_launch_mode` (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`),
  // threaded through `LaunchEnv`.
  crate::launch::params::seed_layerless(&mut resolved, env.default_launch_mode);
  // Project resolved ctx/reasoning back onto the top-level
  // `LaunchParams` fields — `compose` emits them inline (ctx as
  // `-c <N>`, reasoning as the `--jinja --reasoning-format deepseek`
  // bundle).
  // An `Auto` ctx/reasoning collapses to "no inline flag" here
  // (`set_value()` → `None`): `compose` emits nothing and `--fit`
  // governs ctx, the chat template governs reasoning.
  launch_params.ctx = resolved.knobs.ctx.set_value().copied();
  launch_params.reasoning = resolved
    .knobs
    .reasoning
    .set_value()
    .copied()
    .unwrap_or(false);
  // `--jinja` default comes from config; `compose` ORs in reasoning so
  // the reasoning toggle still forces it even when this is `false`.
  // `--jinja` is a llamastash default, so the bench parity escape hatch
  // (`LLAMASTASH_BENCH_DISABLE_DEFAULTS`) suppresses it too — keeps
  // `start` byte-identical to raw `llama-server` for Suite-A overhead.
  launch_params.jinja =
    env.jinja_default && !crate::launch::params::bench_disable_defaults_from_env();
  launch_params.knobs = resolved.knobs;
  // Close the `knobs.ctx` bypass of `MAX_CTX_TOKENS` (the early check
  // only saw the top-level `parsed.ctx`): validate the *resolved* ctx,
  // which folds in both `parsed.ctx` and a typed `knobs.ctx` set via the
  // editor or last-params.
  if let Some(c) = launch_params.ctx {
    if c > MAX_CTX_TOKENS {
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!("ctx {c} exceeds maximum {MAX_CTX_TOKENS}"),
      ));
    }
  }
  // Leave `device` exactly as the resolver chain set it. When no layer
  // selected one it stays `None`, so `compose()` emits no `--device`
  // and `llama-server` keeps its default (auto-select / split across
  // every visible GPU) — the documented backwards-compatible behavior.

  // Context sizing is delegated to llama-server's `--fit`: when `ctx`
  // is unset (Auto / Inherited), `compose` emits `--fit-ctx <floor>` so
  // fit sizes the window for the available memory but never collapses
  // below the floor. llamastash keeps budget *authority* via the
  // admission gate (the sysfs-backed pool reading), not by computing
  // ctx here. A pinned `ctx` suppresses the floor (fit honors the pin).
  launch_params.fit_ctx_floor = Some(env.fit_ctx_floor);

  // Reject loopback-breaking / auth-bypass extras flags before
  // spawn. `compose` strips defensively too, but failing fast here
  // gives callers a clear error instead of a silently-different argv.
  // Release the reservation first so a retry can re-use the port —
  // otherwise a client that repeatedly submits a banned flag would
  // permanently exhaust the port pool.
  let banned = crate::launch::params::forbidden_in_extras(&launch_params.extras);
  if !banned.is_empty() {
    ctx.supervisors.release_reserved_port(port).await;
    return Err(ErrorObject::new(
      ErrorCode::InvalidParams,
      format!(
        "extras flags refused (loopback / auth contract): {}",
        banned.join(", ")
      ),
    ));
  }

  // Per-launch log file under cache_dir/logs/<short-id>-<ts>.log.
  let log_path = build_log_path(&env.log_dir, &id);

  // Scale the probe budget by total weight bytes so a slow load
  // (large multipart GGUF, HIP/ROCm upload, cold disk) doesn't trip
  // the default 120 s timeout. The catalog row carries the
  // multipart-aware total via `discovery::shard_sizes`. Fall back to
  // the path's on-disk total when the model isn't in the catalog
  // (direct-path launches that bypass scan).
  let total_weight_bytes = launch_total_bytes(ctx, &launch_params.model_path).await;
  let scaled_probe = env.probe.scale_for_model(total_weight_bytes);

  // Pick the binary that owns the chosen `--device` selector. The
  // selector (`Vulkan0`, `CUDA0`, …) came from a specific binary's
  // `--list-devices`, so we must spawn *that* binary or the selector
  // is invalid. Unset / empty device falls back to the default binary
  // with no `--device`.
  let selector = launch_params
    .knobs
    .device
    .set_value()
    .map(String::as_str)
    .filter(|s| !s.is_empty())
    .map(str::to_string);
  let launch_binary = match selector {
    Some(sel) => {
      let owning_binary = {
        let catalog = env.device_catalog.read().await;
        catalog
          .iter()
          .find(|d| d.selector == sel)
          .map(|d| d.binary.clone())
      };
      owning_binary.unwrap_or_else(|| {
        // Stale persisted selector or the catalog probe failed. Drop
        // the selector so `compose()` doesn't emit an invalid
        // `--device` the default binary would reject, and spawn the
        // default binary with auto-select.
        log::warn!(
          "device selector {sel:?} not in launch catalog; dropping it and spawning default binary {}",
          env.binary.display()
        );
        launch_params.knobs.device = None;
        env.binary.clone()
      })
    }
    None => env.binary.clone(),
  };

  // `inference_backend` was resolved up front (before the last_params gate).
  // The orchestrator owns the branch on plan shape below.

  // Backend-specific extras denylist, checked once the backend is known: ds4
  // adds `--cors` / `--dist-` on top of the base loopback/auth heads already
  // refused above. Release the port before returning so a retry can reuse it.
  let extra_heads = crate::backend::Backend::forbidden_extra_heads(&inference_backend);
  if !extra_heads.is_empty() {
    let backend_banned =
      crate::launch::params::forbidden_in_extras_ext(&launch_params.extras, extra_heads);
    if !backend_banned.is_empty() {
      ctx.supervisors.release_reserved_port(port).await;
      return Err(ErrorObject::new(
        ErrorCode::InvalidParams,
        format!(
          "extras flags refused for the {} backend (network / loopback contract): {}",
          crate::backend::Backend::id(&inference_backend),
          backend_banned.join(", ")
        ),
      ));
    }
  }

  // Non-fatal advisories accumulated across composition, surfaced on the
  // `StartedLaunch` (CLI human output / TUI toast).
  let mut warnings: Vec<String> = Vec::new();
  // Dropped-knob surfacing (R6): typed knobs the user set that the resolved
  // backend can't honor are silently dropped from argv — tell the user which.
  // ds4 honors only `Ctx`, so a `--flash-attn` on a ds4-routed model warns.
  {
    let caps = crate::backend::Backend::capabilities(&inference_backend);
    let dropped: Vec<&str> = crate::launch::flag_aliases::knob_specs()
      .iter()
      .filter(|spec| !caps.supports(spec.field) && field_is_set(&launch_params.knobs, spec.field))
      .map(|spec| spec.canonical)
      .collect();
    if !dropped.is_empty() {
      let msg = format!(
        "{} does not support these knobs — dropped from the launch: {}",
        crate::backend::Backend::id(&inference_backend),
        dropped.join(", ")
      );
      log::warn!("{msg}");
      warnings.push(msg);
    }
  }

  // ds4 auto-streaming (D-admission): ds4 holds the full model plus a
  // cached-expert/KV working set the deepseek4 demand model can't see (~1.25×
  // weights in practice). When that won't fit, a non-streaming launch OOM-kills
  // mid-load — ds4-server marks itself the first OOM victim (oom_score_adj=1000).
  // So on a host where residency won't fit, auto-enable `ssd_streaming` before
  // `prepare_launch` composes argv, letting the launch succeed from a bounded
  // disk cache. Skipped when the user set the knob either way (explicit wins).
  let mut ssd_streaming_auto = false;
  if inference_backend.id() == crate::backend::ds4::DS4_BACKEND_ID
    && !matches!(
      launch_params.backend_knobs.get("ssd_streaming"),
      Some(crate::config::KnobValue::Set(_))
    )
  {
    if let Some(host_slot) = ctx.host_metrics.as_ref() {
      let snapshot = host_slot.read().await.clone();
      if crate::launch::admission::is_sampled(&snapshot) {
        let free = crate::launch::admission::effective_free_bytes(&snapshot);
        if ds4_should_auto_stream(total_weight_bytes, free) {
          launch_params.backend_knobs.insert(
            "ssd_streaming".to_string(),
            crate::config::KnobValue::Set("true".to_string()),
          );
          ssd_streaming_auto = true;
          let gib = crate::init::detection::fmt_gib;
          let msg = format!(
            "ds4 needs ~{} resident but only {} is free — enabled SSD streaming to launch \
             from disk (slower). Set `ssd_streaming: false` to force full residency.",
            gib(ds4_resident_estimate(total_weight_bytes)),
            gib(free)
          );
          log::warn!("{msg}");
          warnings.push(msg);
        }
      }
    }
  }

  // Per-backend binary pick for the spawn. A managed-multiplexer backend
  // (Lemonade) supervises its own umbrella executable on its own loopback
  // port; ds4 spawns `ds4-server` (not `llama-server`) on the reserved pool
  // port; llama.cpp uses the device-owning binary chosen above. Byte-identical
  // to the prior llama.cpp / Lemonade paths — the ds4 arm is purely additive.
  let (plan_binary, plan_port) =
    if inference_backend.lifecycle() == crate::backend::Lifecycle::ManagedMultiplexer {
      match crate::backend::lemonade::resolve_lemond_binary(&ctx.lemonade) {
        Some(bin) => (bin, ctx.lemonade.port),
        None => {
          ctx.supervisors.release_reserved_port(port).await;
          return Err(ErrorObject::new(
            ErrorCode::InvalidParams,
            "lemonade backend selected but no `lemond` binary found; set `lemonade.binary` \
             or put `lemond` on PATH (see docs/lemonade-setup.md)"
              .to_string(),
          ));
        }
      }
    } else if inference_backend.id() == crate::backend::ds4::DS4_BACKEND_ID {
      match crate::backend::ds4::resolve_ds4_binary(ctx.ds4.binary.as_deref()) {
        Some(bin) => (bin, port),
        None => {
          ctx.supervisors.release_reserved_port(port).await;
          return Err(ErrorObject::new(
            ErrorCode::InvalidParams,
            "ds4 backend selected but no `ds4-server` binary found; set `ds4.binary` \
             or put `ds4-server` on PATH (see docs/usage.md)"
              .to_string(),
          ));
        }
      }
    } else {
      (launch_binary, port)
    };

  let launch_spec =
    match inference_backend.prepare_launch(&launch_params, plan_port, plan_binary, scaled_probe) {
      LaunchPlan::SpawnProcess(spec) => spec,
      LaunchPlan::DelegateToManager(spec) => {
        // Managed-multiplexer (Lemonade): ensure the shared umbrella is up on
        // `plan_port` and delegate the model to it, rather than spawning a
        // per-model child. The umbrella binds its own configured port, not the
        // launch-pool reservation, so release `port` first (holding it would
        // leak a pool slot). Routing of a Lemonade model's requests through the
        // proxy is handled separately (catalog-source-based; see
        // `proxy::route`).
        ctx.supervisors.release_reserved_port(port).await;
        return start_delegated_lemonade(
          ctx,
          spec,
          plan_port,
          id,
          identity,
          log_path,
          launch_params,
        )
        .await;
      }
    };

  // Pre-spawn admission: project this launch's demand floor and
  // refuse *before* spawn if it won't fit the sampled budget minus the
  // bytes other in-flight launches already reserved. This is the safety
  // net `--fit` can't provide on UMA (its free reading conflates GTT
  // with system RAM). Keyed by the reserved `port` (unique per in-flight
  // launch); released when the child settles or on any failure below.
  // Best-effort: skipped entirely when there is no host-metrics sample
  // yet (the `port` reservation still gates the pool). Only the
  // process-spawn path reaches here — Lemonade's umbrella returned
  // above.
  // ds4's `ssd_streaming` native knob (D-admission): on-disk bytes ≠ memory
  // demand when weights stream from disk, so the hard OOM refusal is skipped
  // for that launch (logged + surfaced). Keys on the native knob only — an
  // extras-spelled `--ssd-streaming` still hits the gate (documented).
  let ssd_streaming = matches!(
    launch_params.backend_knobs.get("ssd_streaming"),
    Some(crate::config::KnobValue::Set(v)) if v == "true"
  );
  let mut admitted = false;
  if identity.as_gguf().is_some() {
    if let Some(host_slot) = ctx.host_metrics.as_ref() {
      let snapshot = host_slot.read().await.clone();
      if crate::launch::admission::is_sampled(&snapshot) {
        let effective_ctx = launch_params.ctx.unwrap_or(env.fit_ctx_floor);
        let free = crate::launch::admission::effective_free_bytes(&snapshot);
        let gpu_backend = snapshot.gpu_backend.clone();
        let model_path = launch_params.model_path.clone();
        let knobs = launch_params.knobs.clone();
        let arch_owned = arch.clone();
        // Shard-aware weight total (sums every split sibling); the
        // per-shard `weights_bytes(header)` would only see the primary
        // shard. Computed above for probe scaling — reused here so a
        // split GGUF is not under-projected and wrongly admitted.
        let weights_total = total_weight_bytes;
        let demand = tokio::task::spawn_blocking(move || {
          let header = read_gguf_header(&model_path, HeaderReadOptions::default())
            .ok()?
            .header;
          Some(crate::launch::admission::project_demand(
            &header,
            arch_owned.as_deref(),
            &knobs,
            effective_ctx,
            &gpu_backend,
            weights_total,
          ))
        })
        .await
        .ok()
        .flatten();
        if let Some(demand) = demand {
          if let Err(refusal) = ctx.admission.try_admit(u64::from(port), demand, free) {
            if ssd_streaming {
              // Bypass the OOM refusal: streaming weights don't need full
              // residency. Not admitted (no ledger reservation held); spawn
              // proceeds. Skip the note when streaming was auto-enabled just
              // above — that path already explained why, in memory terms.
              if !ssd_streaming_auto {
                let msg = format!(
                  "ssd_streaming is set — skipped the memory admission gate ({})",
                  format_admission_refusal(&refusal)
                );
                log::warn!("{msg}");
                warnings.push(msg);
              }
            } else {
              ctx.supervisors.release_reserved_port(port).await;
              return Err(ErrorObject::with_data(
                ErrorCode::ResourceExhausted,
                format_admission_refusal(&refusal),
                serde_json::json!({ "cause": "launch_refused" }),
              ));
            }
          } else {
            admitted = true;
          }
        }
      }
    }
  }

  // Strict-fit ctx-clamp gate: only meaningful when ctx is
  // delegated to `--fit` (`ctx == None`) and we know the trained window
  // to compare its resolution against. A pinned ctx or unknown window
  // leaves the gate off. `strict_fit` then decides whether a floor-pinned
  // resolution withholds Ready (refuse) or just flags a soft notice.
  let fit_gate = (launch_params.ctx.is_none() && native_ctx.is_some()).then(|| {
    crate::daemon::supervisor::FitGate {
      floor: env.fit_ctx_floor,
      native: native_ctx.unwrap_or(0),
      strict: env.strict_fit,
    }
  });
  let spawn_result = supervisor_spawn(ManagedSpawn {
    id: id.clone(),
    params: launch_params.clone(),
    port,
    mode,
    log_path: log_path.clone(),
    plan: launch_spec,
    origin,
    fit_gate,
  })
  .await;
  let model = match spawn_result {
    Ok(m) => m,
    Err(e) => {
      // Free the reserved port + admission hold so a retry can re-use them.
      ctx.supervisors.release_reserved_port(port).await;
      if admitted {
        ctx.admission.release(u64::from(port));
      }
      return Err(ErrorObject::new(
        ErrorCode::InternalError,
        format!("supervisor spawn: {e}"),
      ));
    }
  };

  let launch_id = ctx.supervisors.next_id();
  ctx
    .supervisors
    .insert(launch_id.clone(), model.clone())
    .await;
  // Live supervisor now owns the port; drop the in-flight reservation.
  ctx.supervisors.release_reserved_port(port).await;

  // Persist running snapshot. Retain by `(id, port)` so the same
  // GGUF launched twice against different ports persists both
  // snapshots — the orphan sweep can then re-adopt either one on
  // daemon restart instead of silently dropping the older.
  let pid = model.pid().await.unwrap_or(0) as i32;
  let started_at = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  ctx
    .state
    .mutate(|s| {
      s.running.retain(|r| !(r.id == identity && r.port == port));
      s.running.push(RunningSnapshot {
        id: identity.clone(),
        pid,
        port,
        started_at,
        params: launch_params.clone(),
        actuals: Default::default(),
        resolved_backend: resolved_backend_id.clone(),
      });
    })
    .await;

  // Background task: when the supervisor reaches Ready, stamp
  // last_params — only updated on a *successful* Loading → Ready
  // transition. We poll because ManagedModel doesn't expose a
  // transition channel yet.
  //
  // Persist the *user-supplied* knob deltas, not the full resolved set
  // — so source chips in the picker stay meaningful (a knob the user
  // never touched keeps re-resolving from yaml / built-in / model
  // default instead of being frozen as `(last used)`). "Remembered
  // values win" depends on this: only what the user actually set
  // (including an explicit `Auto` sentinel) is remembered, so the
  // resolver re-derives the rest next launch. The resolved top-level
  // `ctx`/`reasoning` and the force-copied `device` are dropped too —
  // they were resolver output, not user intent, and re-pinning them
  // would freeze a value the user never chose.
  let mut persist_params = launch_params.clone();
  persist_params.knobs = user_knobs;
  persist_params.ctx = None;
  persist_params.reasoning = false;
  persist_params.backend_knobs =
    backend_knobs_for_persist(&launch_params.backend_knobs, ssd_streaming_auto);
  // `jinja` is config-derived (set from `Config.jinja` after resolution)
  // and re-read from config on every launch — `resolve_layered` never
  // consults the persisted value. So the clone's resolved value is kept
  // as-is: it makes the `last-params` view report what this launch
  // actually used (honest for `jinja: false`) without ever freezing a
  // value, since the next launch overwrites it from the live config.
  spawn_last_params_recorder(
    ctx.state.clone(),
    model.clone(),
    identity.clone(),
    persist_params,
    // Tag the recorded row with the backend this launch resolved to
    // (D-contamination) so a future cross-backend launch of the same model
    // skips the LastUsed layer + extras inheritance.
    resolved_backend_id,
    // Slow HIP/Metal loads routinely exceed the old fixed 180 s wall
    // clock; key the recorder's deadline off the same size-scaled probe
    // budget the supervisor uses (base +2 h cap) so a slow load still
    // reaches Ready *and* gets its params recorded — otherwise the next
    // launch finds no remembered value and wrongly seeds Auto.
    scaled_probe.timeout,
    ctx.shutdown.clone(),
  );

  // Settle the admission reservation when the child leaves Loading
  // (Ready: real allocation is now visible to the sampler; Error /
  // Stopped: the slot is freed). Keyed by `port`; idempotent.
  if admitted {
    spawn_admission_settle(
      ctx.admission.clone(),
      model.clone(),
      port,
      scaled_probe.timeout,
      ctx.shutdown.clone(),
    );
  }

  Ok(StartedLaunch {
    launch_id,
    model_id: id,
    port,
    model,
    log_path,
    warnings,
  })
}

/// Whether `field` holds a concrete (`Set`) value in `knobs` — the view the
/// dropped-knob warning needs (an `Auto` / unset knob emits nothing, so it is
/// not "dropped" in any user-visible sense).
fn field_is_set(
  knobs: &crate::config::TypedKnobs,
  field: crate::launch::flag_aliases::KnobField,
) -> bool {
  knobs.slot(field).as_u32().is_some()
    || knobs.slot(field).as_f32().is_some()
    || knobs.slot(field).as_bool().is_some()
    || knobs.slot(field).as_str().is_some()
}

/// Start a model on a managed-multiplexer backend (Lemonade): ensure the
/// shared `lemond` umbrella is supervised + ready, then preload the model
/// through its API. Returns a [`StartedLaunch`] anchored on the umbrella
/// (the supervised process), so status + stop see one umbrella for all
/// Lemonade models. Routing of inference is handled by the proxy
/// (catalog-source-based; see [`crate::proxy::route`]).
async fn start_delegated_lemonade(
  ctx: &MethodContext,
  spec: crate::backend::ManagerLaunchSpec,
  umbrella_port: u16,
  id: ModelId,
  identity: ModelIdentity,
  log_path: PathBuf,
  params: LaunchParams,
) -> Result<StartedLaunch, ErrorObject> {
  use crate::backend::lemonade::{ensure_umbrella, umbrella_launch_id, LemonadeClient};

  // The preload must not POST `/api/v1/load` until the umbrella's HTTP
  // server is actually accepting connections; bound the readiness wait by
  // the umbrella's own probe budget (its probe resolves to Ready/Error
  // first, so this is only a backstop for an umbrella that never settles).
  let ready_timeout = spec.umbrella.probe.timeout + Duration::from_secs(2);

  let umbrella = match ensure_umbrella(
    &ctx.supervisors,
    umbrella_port,
    spec.umbrella,
    log_path.clone(),
  )
  .await
  {
    Ok(m) => m,
    Err(e) => {
      return Err(ErrorObject::new(
        ErrorCode::InternalError,
        format!("lemonade umbrella failed to start: {e}"),
      ));
    }
  };
  // An already-running umbrella keeps its own port; trust the handle over the
  // requested `umbrella_port` so a reused umbrella routes to the right place.
  let serving_port = umbrella.port();

  // Preload the model so an explicit launch makes it resident (chat would
  // autoload too), forwarding the launch params lemond honors: `ctx_size`
  // plus the free-form extras as the recipe-scoped `*_args` string.
  //
  // The load runs as a background task: a cold load can take lemond's
  // full 120 s budget, which is far past the CLI's IPC reply timeout —
  // awaiting it here meant the client hung up and hyper cancelled this
  // handler mid-preload, silently dropping the launch. The task records
  // its outcome in the registry's delegated-state map (`Loading` →
  // `Ready` / `Error{cause}`), which is what `status` reports for the
  // row — so a model lemond can't load shows `error` with lemond's
  // message instead of a phantom `ready`.
  ctx
    .supervisors
    .set_delegated_state(&spec.model.name, ManagedState::Loading)
    .await;
  {
    let registry = ctx.supervisors.clone();
    let name = spec.model.name.clone();
    let params = params.clone();
    let umbrella = umbrella.clone();
    tokio::spawn(async move {
      // `ensure_umbrella` returns at `Loading`; the load POST would race
      // the umbrella's bind and hit connection-refused on a cold start.
      // Wait for `/live` to pass (Ready) before talking to it.
      if let Err(cause) = umbrella.wait_until_ready(ready_timeout).await {
        let cause = format!("lemonade umbrella not ready: {cause}");
        log::warn!("lemonade: preload of `{name}` aborted: {cause}");
        registry
          .set_delegated_state(&name, ManagedState::Error { cause })
          .await;
        return;
      }
      let outcome = match LemonadeClient::new(serving_port) {
        Ok(client) => {
          let opts = lemonade_load_options(&client, &name, &params).await;
          client.load_with(&name, &opts).await
        }
        Err(e) => Err(e),
      };
      match outcome {
        Ok(()) => {
          registry
            .set_delegated_state(&name, ManagedState::Ready)
            .await;
        }
        Err(e) => {
          log::warn!("lemonade: preload of `{name}` failed: {e}");
          registry
            .set_delegated_state(
              &name,
              ManagedState::Error {
                cause: e.to_string(),
              },
            )
            .await;
        }
      }
    });
  }

  // Persist a running snapshot at the umbrella port for status visibility.
  let pid = umbrella.pid().await.unwrap_or(0) as i32;
  let started_at = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  ctx
    .state
    .mutate(|s| {
      s.running
        .retain(|r| !(r.id == identity && r.port == serving_port));
      s.running.push(RunningSnapshot {
        id: identity.clone(),
        pid,
        port: serving_port,
        started_at,
        params: params.clone(),
        actuals: Default::default(),
        resolved_backend: crate::backend::lemonade::LEMONADE_BACKEND_ID.to_string(),
      });
    })
    .await;

  Ok(StartedLaunch {
    launch_id: umbrella_launch_id(),
    model_id: id,
    port: serving_port,
    model: umbrella,
    log_path,
    // Lemonade's delegated path carries no ds4/deepseek4 advisories.
    warnings: Vec::new(),
  })
}

/// Project the launch params onto lemond's load-option surface: `ctx`
/// (when the user set one) becomes `ctx_size`; non-empty extras become
/// the recipe-scoped args string (`llamacpp_args` / `whispercpp_args` /
/// `flm_args` — lemond names the field after the model's recipe, read
/// from the umbrella's own model list). Extras are dropped with a
/// warning when the recipe can't be resolved — guessing the field would
/// silently feed flags to the wrong engine.
async fn lemonade_load_options(
  client: &crate::backend::lemonade::LemonadeClient,
  name: &str,
  params: &LaunchParams,
) -> crate::backend::lemonade::LoadOptions {
  let recipe_args = if params.extras.is_empty() {
    None
  } else {
    let joined = params
      .extras
      .iter()
      .map(|s| s.to_string_lossy())
      .collect::<Vec<_>>()
      .join(" ");
    let recipe = client
      .list_model_entries()
      .await
      .ok()
      .and_then(|entries| entries.into_iter().find(|e| e.id == name))
      .and_then(|e| e.recipe);
    match recipe {
      Some(recipe) => Some((format!("{recipe}_args"), joined)),
      None => {
        log::warn!(
          "lemonade: dropping extras for `{name}` — could not resolve its recipe \
           from the umbrella's model list"
        );
        None
      }
    }
  };
  crate::backend::lemonade::LoadOptions {
    ctx_size: params.ctx,
    recipe_args,
  }
}

/// ds4's resident working set estimate: ~1.25× raw weights, covering the
/// cached-expert pool + KV + runtime the deepseek4 demand model can't see.
/// Its own auto budget targets ~99 GiB for an 80 GiB Flash quant, which this
/// tracks. Saturating so a pathological weight total can't overflow.
fn ds4_resident_estimate(weights_total: u64) -> u64 {
  weights_total.saturating_add(weights_total / 4)
}

/// Whether a ds4 launch should auto-enable SSD streaming: its resident
/// estimate exceeds the effective free memory, so a full-residency spawn
/// would OOM-kill mid-load (ds4-server marks itself the first OOM victim).
/// Pure so the memory decision is unit-testable without a live host sampler.
fn ds4_should_auto_stream(weights_total: u64, free: u64) -> bool {
  ds4_resident_estimate(weights_total) > free
}

/// The `backend_knobs` to persist into `last_params`: the resolved set, minus
/// the *auto-enabled* `ssd_streaming`. That knob is a one-time response to the
/// launch's current memory pressure, not a user opt-in — freezing it into
/// `last_params` would make the next no-selection relaunch inherit it as
/// explicit (skipping the pressure re-evaluation *and* the OOM admission gate)
/// even after RAM frees up. A user-set / inherited `ssd_streaming` (i.e. auto
/// did not fire) is preserved. Pure so the invariant is unit-testable.
fn backend_knobs_for_persist(
  resolved: &std::collections::BTreeMap<String, KnobValue<String>>,
  ssd_streaming_auto: bool,
) -> std::collections::BTreeMap<String, KnobValue<String>> {
  let mut out = resolved.clone();
  if ssd_streaming_auto {
    out.remove("ssd_streaming");
  }
  out
}

/// Human-readable admission refusal: the effective free (post-headroom),
/// what other launches hold, this launch's projected demand, and the
/// remediation menu — so the number is self-explaining and actionable.
fn format_admission_refusal(refusal: &crate::launch::admission::Refusal) -> String {
  // One canonical GiB formatter (bytes ÷ 1024³, 1 decimal) shared with
  // every other memory surface — see `crate::init::detection::fmt_gib`.
  let gib = crate::init::detection::fmt_gib;
  format!(
    "launch refused: needs {} but only {} is free (effective {} after headroom, minus {} reserved by in-flight launches). \
     Stop a resident model, pin a smaller --ctx, lower fit_ctx_floor, or retry once a model frees memory.",
    gib(refusal.demand_bytes),
    gib(refusal.available_bytes()),
    gib(refusal.effective_free_bytes),
    gib(refusal.reserved_bytes),
  )
}

/// Poll the child until it leaves Loading (Ready / Error / Stopped) and
/// drop its admission reservation. Mirrors the recorder's poll shape;
/// bounded by the scaled probe budget and the shutdown token so a child
/// that never settles can't leak the reservation forever.
///
/// Best-effort hand-off: the reservation drops on Ready, but the 1 Hz
/// host-metrics sampler may not yet reflect the child's freshly-committed
/// allocation, so a concurrent launch in that sub-second window can see
/// stale free *and* no reservation. The window is bounded by one sample
/// tick and the in-process load check is the final OOM backstop, so this
/// is accepted rather than papered over with a longer hold.
fn spawn_admission_settle(
  ledger: Arc<crate::launch::admission::Ledger>,
  model: ManagedModel,
  port: u16,
  probe_budget: Duration,
  shutdown: ShutdownToken,
) {
  tokio::spawn(async move {
    let deadline = Instant::now() + probe_budget;
    loop {
      match model.state().await {
        ManagedState::Ready | ManagedState::Error { .. } | ManagedState::Stopped => {
          ledger.release(u64::from(port));
          return;
        }
        _ => {}
      }
      if Instant::now() > deadline {
        ledger.release(u64::from(port));
        return;
      }
      tokio::select! {
        _ = shutdown.wait_until_triggered() => {
          ledger.release(u64::from(port));
          return;
        }
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
      }
    }
  });
}

fn spawn_last_params_recorder(
  state: PersistedState,
  model: ManagedModel,
  id: ModelIdentity,
  params: LaunchParams,
  resolved_backend: String,
  probe_budget: Duration,
  shutdown: ShutdownToken,
) {
  tokio::spawn(async move {
    // Wait out the same size-scaled probe budget the supervisor uses
    // (base 120 s + up to 2 h for very large weights) so a slow load
    // still gets its params recorded on the Loading → Ready transition.
    // The poll also observes the daemon's shutdown token so SIGTERM
    // during a pending Loading state doesn't block clean process exit.
    let deadline = Instant::now() + probe_budget;
    loop {
      match model.state().await {
        ManagedState::Ready => {
          state
            .mutate(|s| s.upsert_last_params(id.clone(), params.clone(), resolved_backend.clone()))
            .await;
          // Post-launch actuals: stamp what `--fit` actually chose
          // on the running snapshot so `status` / the TUI Running view /
          // `show` can render the resolved context. The supervisor's
          // readiness gate already fetched `/props` for fit-governed
          // launches (to run the strict-fit ctx-clamp check) and stashed
          // the result on the model, so reuse it instead of fetching
          // twice; only fall back to a fetch when the gate didn't run
          // (pinned ctx / no trained-window metadata). Best-effort — an
          // empty result (no `/props`, transport error) leaves the row
          // "unavailable".
          if let Some(port) = params.port {
            let mut actuals = model.actuals().await;
            if actuals.is_empty() {
              actuals = crate::daemon::actuals::fetch(port, Duration::from_secs(5)).await;
            }
            if !actuals.is_empty() {
              let id = id.clone();
              state
                .mutate(move |s| {
                  if let Some(snap) = s.running.iter_mut().find(|r| r.id == id && r.port == port) {
                    snap.actuals = actuals;
                  }
                })
                .await;
            }
          }
          return;
        }
        ManagedState::Error { .. } | ManagedState::Stopped => return,
        _ => {}
      }
      if Instant::now() > deadline {
        return;
      }
      tokio::select! {
        _ = shutdown.wait_until_triggered() => return,
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
      }
    }
  });
}

async fn collect_in_use_ports(ctx: &MethodContext) -> Vec<u16> {
  let mut ports: Vec<u16> = ctx
    .supervisors
    .snapshot()
    .await
    .into_iter()
    .map(|(_, m)| m.port())
    .collect();
  // Also avoid colliding with `llama-server` processes that this
  // daemon didn't spawn directly but were launched by *some*
  // llamastash instance — typically a previous run of this daemon
  // or a sibling UAT/test daemon whose state.json the orphan sweep
  // didn't see. The `LLAMASTASH_LAUNCHED=1` env marker (stamped by
  // `supervisor::spawn`) is what makes these recognisable; the port
  // gets parsed out of the orphan's argv in `orphans::sweep`.
  //
  // The bind probe in `ports::try_bind_probe` already rejects an
  // externally-held port at reservation time, so this list is a
  // hint to the allocator rather than a correctness gate — it just
  // lets us skip straight past known-busy slots instead of probing
  // them one by one on every launch.
  let externals = ctx.external.read().await;
  for ext in externals.iter() {
    if ext.launched_by_llamastash {
      if let Some(p) = ext.port {
        if !ports.contains(&p) {
          ports.push(p);
        }
      }
    }
  }
  ports
}

/// Does the caller's `extras` tail already manage the multimodal
/// projector? `--mmproj <path>` pins a projector and `--no-mmproj`
/// force-disables it; in either case the daemon must not auto-detect
/// one too. Matches the flag head in both space form (`--mmproj`) and
/// equals form (`--mmproj=/path`), case-insensitively. `--no-mmproj-offload`
/// (offload tuning, projector still on) is left to auto-detect, so the
/// match is exact rather than a prefix test.
fn extras_manage_mmproj(extras: &[OsString]) -> bool {
  extras.iter().any(|e| {
    let lossy = e.to_string_lossy();
    let head = lossy.split('=').next().unwrap_or(&lossy);
    head.eq_ignore_ascii_case("--mmproj") || head.eq_ignore_ascii_case("--no-mmproj")
  })
}

/// Total on-disk weight bytes for the model the launch handler is
/// about to spawn. Prefers the catalog row (which already includes
/// split-shard aggregation via `discovery::shard_sizes`); falls back
/// to `shard_sizes::on_disk_total` on the bare path for direct
/// launches that bypass scan. `0` when neither path is reachable —
/// the probe scaler treats that as "no signal, keep the default".
async fn launch_total_bytes(ctx: &MethodContext, model_path: &std::path::Path) -> u64 {
  let snap = ctx.catalog.snapshot().await;
  if let Some(row) = snap.iter().find(|m| m.path == model_path) {
    if let Some(b) = row.metadata.as_ref().and_then(|md| md.weights_bytes) {
      return b;
    }
    return crate::discovery::shard_sizes::on_disk_total(&row.path, &row.split_siblings);
  }
  crate::discovery::shard_sizes::on_disk_total(model_path, &[])
}

/// Live GPU-backend flavor — keys the built-in defaults table.
/// Reads the host-metrics sampler when available; falls back to
/// `Unsampled` (treated identically to `Unknown` by the table) when
/// the daemon has no sampler attached (catalog-only tests).
async fn current_backend_flavor(ctx: &MethodContext) -> crate::daemon::host_metrics::GpuFlavor {
  if let Some(slot) = &ctx.host_metrics {
    let snap = slot.read().await;
    return snap.flavor();
  }
  crate::daemon::host_metrics::GpuFlavor::Unsampled
}

fn build_log_path(log_dir: &std::path::Path, id: &ModelId) -> PathBuf {
  let stem = id
    .path
    .file_stem()
    .and_then(|s| s.to_str())
    .unwrap_or("model");
  let ts = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  let short = id.short_fingerprint();
  log_dir.join(format!("{stem}-{short}-{ts}.log"))
}

#[cfg(test)]
mod tests {
  use tokio::sync::RwLock;

  use super::*;
  use crate::config::loader::LemonadeConfig;
  use crate::daemon::context::LaunchEnv;
  use crate::daemon::probe::ProbeOptions;
  use crate::daemon::registry::SupervisorRegistry;

  #[test]
  fn ds4_auto_stream_triggers_only_when_residency_exceeds_free() {
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

  #[test]
  fn auto_ssd_streaming_is_not_persisted_but_explicit_and_others_are() {
    use crate::config::KnobValue;
    let mut resolved = std::collections::BTreeMap::new();
    resolved.insert("power".to_string(), KnobValue::Set("80".to_string()));
    resolved.insert(
      "ssd_streaming".to_string(),
      KnobValue::Set("true".to_string()),
    );

    // Auto-enabled → the `ssd_streaming` key is stripped from what we persist,
    // so a later relaunch re-evaluates memory pressure and the OOM gate isn't
    // silently disabled; unrelated native knobs survive.
    let auto = backend_knobs_for_persist(&resolved, true);
    assert!(
      !auto.contains_key("ssd_streaming"),
      "auto-enabled ssd_streaming must not be frozen into last_params"
    );
    assert_eq!(auto.get("power"), Some(&KnobValue::Set("80".to_string())));

    // User-set / inherited (auto did not fire) → preserved verbatim.
    let explicit = backend_knobs_for_persist(&resolved, false);
    assert_eq!(
      explicit.get("ssd_streaming"),
      Some(&KnobValue::Set("true".to_string())),
      "a user-set ssd_streaming must persist"
    );
  }

  #[test]
  fn extras_manage_mmproj_detects_explicit_projector_flags() {
    let pin = vec![OsString::from("--mmproj"), OsString::from("/m/p.gguf")];
    assert!(extras_manage_mmproj(&pin), "space-form --mmproj");
    let pin_eq = vec![OsString::from("--MMPROJ=/m/p.gguf")];
    assert!(
      extras_manage_mmproj(&pin_eq),
      "equals-form, case-insensitive"
    );
    let disable = vec![OsString::from("--no-mmproj")];
    assert!(extras_manage_mmproj(&disable), "--no-mmproj force-disable");
    // Offload tuning leaves the projector on → auto-detect still runs.
    let offload = vec![OsString::from("--no-mmproj-offload")];
    assert!(
      !extras_manage_mmproj(&offload),
      "--no-mmproj-offload is not projector management"
    );
    let unrelated = vec![OsString::from("--threads"), OsString::from("8")];
    assert!(!extras_manage_mmproj(&unrelated));
  }

  #[tokio::test]
  async fn lemonade_start_without_binary_releases_reserved_port() {
    use crate::config::loader::PortRange;
    use crate::gguf::test_fixtures::build_minimal_gguf;
    use crate::launch::params::BackendChoice;

    // A real (minimal) GGUF on disk so `compose_and_spawn` clears header
    // resolution and reaches the backend-selection seam.
    let dir = tempfile::tempdir().expect("tempdir");
    let model_path = dir.path().join("tiny.gguf");
    std::fs::write(&model_path, build_minimal_gguf("llama")).expect("write gguf");

    // A single-port range on a probe-clear port. Find one the allocator
    // accepts (tolerates TIME_WAIT), then release it so the run under test
    // starts from an empty reservation set.
    let registry = SupervisorRegistry::new();
    let mut found = None;
    for _ in 0..16 {
      let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port");
      let p = l.local_addr().unwrap().port();
      drop(l);
      let range = PortRange { start: p, end: p };
      if registry.reserve_port(None, &[], &range).await.is_ok() {
        registry.release_reserved_port(p).await;
        found = Some(p);
        break;
      }
    }
    let port = found.expect("at least one of 16 attempts lands on a probe-clear port");
    let range = PortRange {
      start: port,
      end: port,
    };

    let env = LaunchEnv {
      // Never spawned on this path — the managed-multiplexer arm errors out
      // before any process launch.
      binary: PathBuf::from("/nonexistent/llama-server"),
      port_range: range,
      log_dir: dir.path().to_path_buf(),
      probe: ProbeOptions::default(),
      arch_defaults: Default::default(),
      device_catalog: Arc::new(RwLock::new(Vec::new())),
      default_launch_mode: Default::default(),
      fit_ctx_floor: 16384,
      strict_fit: false,
      jinja_default: true,
    };

    // Lemonade enabled but pointed at a binary that does not exist. The
    // explicit-`binary` branch never falls back to PATH, so resolution is
    // deterministically `None` even on a host that has a real `lemond`
    // installed — the test can't be fooled by the dev machine's PATH.
    let ctx = MethodContext::new(ShutdownToken::new())
      .with_supervisors(registry)
      .with_launch_env(env)
      .with_lemonade(LemonadeConfig {
        enabled: true,
        binary: Some(PathBuf::from("/nonexistent/lemond-xyz")),
        port: 13305,
      });

    let parsed = StartParams {
      model_path,
      // Force the managed-multiplexer seam: an explicit Lemonade override
      // outranks the GGUF identity rule.
      backend: Some(BackendChoice::Lemonade),
      ..Default::default()
    };

    // `StartedLaunch` (the Ok variant) isn't `Debug`, so match rather than
    // `expect_err`.
    let err = match compose_and_spawn(
      &ctx,
      parsed,
      crate::daemon::supervisor::LaunchOrigin::Manual,
    )
    .await
    {
      Ok(_) => panic!("unresolvable lemond binary must error"),
      Err(e) => e,
    };
    assert_eq!(err.code, ErrorCode::InvalidParams.as_i32());
    assert!(
      err.message.contains("lemond"),
      "error should name the missing lemond binary, got: {}",
      err.message
    );

    // The reservation must have been released: the single-port range is
    // allocatable again only if `compose_and_spawn` dropped its hold on the
    // error path (otherwise the range is exhausted and this errors).
    let reclaimed = ctx
      .supervisors
      .reserve_port(None, &[], &range)
      .await
      .expect("reserved port must be released on the lemonade-unavailable error path");
    assert_eq!(reclaimed, port);
  }

  /// A `MethodContext` wired with a real (single-port) launch env and a
  /// minimal GGUF on disk, so `compose_and_spawn` clears identity
  /// resolution and reaches the post-header validation seams (port / ctx
  /// bounds) without ever spawning a process. Returns `(ctx, model_path,
  /// _tempdir-guard)`; keep the guard alive for the test's duration.
  async fn ctx_with_env_and_gguf() -> (MethodContext, PathBuf, tempfile::TempDir) {
    use crate::config::loader::PortRange;
    use crate::gguf::test_fixtures::build_minimal_gguf;

    let dir = tempfile::tempdir().expect("tempdir");
    let model_path = dir.path().join("tiny.gguf");
    std::fs::write(&model_path, build_minimal_gguf("llama")).expect("write gguf");

    let env = LaunchEnv {
      binary: PathBuf::from("/nonexistent/llama-server"),
      port_range: PortRange {
        start: 41000,
        end: 41000,
      },
      log_dir: dir.path().to_path_buf(),
      probe: ProbeOptions::default(),
      arch_defaults: Default::default(),
      device_catalog: Arc::new(RwLock::new(Vec::new())),
      default_launch_mode: Default::default(),
      fit_ctx_floor: 16384,
      strict_fit: false,
      jinja_default: true,
    };
    let ctx = MethodContext::new(ShutdownToken::new())
      .with_supervisors(SupervisorRegistry::new())
      .with_launch_env(env);
    (ctx, model_path, dir)
  }

  async fn expect_invalid_params(ctx: &MethodContext, parsed: StartParams) -> String {
    match compose_and_spawn(ctx, parsed, crate::daemon::supervisor::LaunchOrigin::Manual).await {
      Ok(_) => panic!("expected an InvalidParams error, launch succeeded"),
      Err(e) => {
        assert_eq!(e.code, ErrorCode::InvalidParams.as_i32());
        e.message
      }
    }
  }

  #[tokio::test]
  async fn compose_rejects_both_port_and_prefer_port() {
    // The mutual-exclusion guard runs before the env lookup, so a bare
    // context (no launch env) is enough to exercise it.
    let ctx = MethodContext::new(ShutdownToken::new());
    let parsed = StartParams {
      model_path: PathBuf::from("/m/x.gguf"),
      port: Some(11500),
      prefer_port: Some(11501),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains("exactly one of"), "got: {msg}");
  }

  #[tokio::test]
  async fn compose_rejects_privileged_port() {
    let (ctx, model_path, _guard) = ctx_with_env_and_gguf().await;
    let parsed = StartParams {
      model_path,
      port: Some(80),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains(">= 1024"), "got: {msg}");
  }

  #[tokio::test]
  async fn compose_rejects_ctx_over_maximum() {
    let (ctx, model_path, _guard) = ctx_with_env_and_gguf().await;
    let parsed = StartParams {
      model_path,
      ctx: Some(crate::config::MAX_CTX_TOKENS + 1),
      ..Default::default()
    };
    let msg = expect_invalid_params(&ctx, parsed).await;
    assert!(msg.contains("exceeds maximum"), "got: {msg}");
  }

  #[test]
  fn format_admission_refusal_reports_every_number() {
    // The refusal string must surface demand, available (effective −
    // reserved), effective free, and reserved bytes so the operator can
    // see exactly why the launch was turned away.
    let refusal = crate::launch::admission::Refusal {
      demand_bytes: 8 * 1024 * 1024 * 1024,
      effective_free_bytes: 10 * 1024 * 1024 * 1024,
      reserved_bytes: 4 * 1024 * 1024 * 1024,
    };
    let msg = format_admission_refusal(&refusal);
    assert!(msg.contains("launch refused"));
    // demand 8 GiB, available 6 GiB (10 − 4), effective 10 GiB, reserved 4 GiB.
    assert!(msg.contains("8.0 GiB"), "demand: {msg}");
    assert!(msg.contains("6.0 GiB"), "available: {msg}");
    assert!(msg.contains("10.0 GiB"), "effective free: {msg}");
    assert!(msg.contains("4.0 GiB"), "reserved: {msg}");
    // Remediation menu is part of the contract — it tells the user what
    // to do next.
    assert!(msg.contains("Stop a resident model"));
  }

  #[test]
  fn build_log_path_uses_stem_fingerprint_and_timestamp() {
    let id = crate::gguf::identity::ModelId {
      path: PathBuf::from("/models/Qwen3-7B-Q4_K_M.gguf"),
      header_blake3: [0xabu8; 32],
    };
    let path = build_log_path(std::path::Path::new("/var/log/ls"), &id);
    let name = path.file_name().unwrap().to_string_lossy();
    // `<stem>-<short-fingerprint>-<unix-ts>.log`
    assert!(name.starts_with("Qwen3-7B-Q4_K_M-"), "stem prefix: {name}");
    assert!(name.ends_with(".log"), "log suffix: {name}");
    assert!(
      name.contains(&id.short_fingerprint()),
      "embeds the short fingerprint: {name}"
    );
    assert_eq!(path.parent().unwrap(), std::path::Path::new("/var/log/ls"));
  }

  #[test]
  fn build_log_path_falls_back_to_model_stem_for_pathless_id() {
    // An id whose path has no file stem (e.g. a bare directory) still
    // produces a usable log filename rather than panicking.
    let id = crate::gguf::identity::ModelId {
      path: PathBuf::from("/"),
      header_blake3: [0u8; 32],
    };
    let path = build_log_path(std::path::Path::new("/tmp"), &id);
    let name = path.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("model-"), "fallback stem: {name}");
  }

  #[test]
  fn launch_selection_defaults_to_default_and_round_trips() {
    // An absent `selection` on the wire is the no-selection default — what
    // the proxy's `StartParams::default()` auto-start path relies on.
    let parsed: StartParams =
      serde_json::from_value(serde_json::json!({"model_path": "/m/x.gguf"})).unwrap();
    assert_eq!(parsed.selection, LaunchSelection::Default);
    assert_eq!(StartParams::default().selection, LaunchSelection::Default);
    for (s, want) in [
      ("default", LaunchSelection::Default),
      ("explicit", LaunchSelection::Explicit),
      ("auto", LaunchSelection::Auto),
    ] {
      let p: StartParams =
        serde_json::from_value(serde_json::json!({"model_path": "/m/x.gguf", "selection": s}))
          .unwrap();
      assert_eq!(p.selection, want, "selection {s} round-trips");
    }
  }
}
