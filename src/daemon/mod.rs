//! Daemon process: lockfile, control-plane HTTP listener, signal
//! handling, supervisor lifecycle.
//!
//! `run_foreground(opts)` does the whole lifecycle in the calling
//! process. `start_detached` re-execs the binary as a child with `setsid`
//! applied between `fork` and `exec`, then waits for the runtime info
//! file to appear before returning. The child is the daemon; no
//! in-runtime `fork()` is involved, which keeps the tokio runtime safe.

pub mod auth;
pub mod control_plane;
pub mod discovery_task;
pub mod host_metrics;
pub mod lockfile;
pub mod orphans;
pub mod ports;
pub mod probe;
pub mod registry;
pub mod resources;
pub mod runtime_file;
pub mod shutdown;
pub mod state_store;
pub mod supervisor;

use std::{
  net::IpAddr,
  path::{Path, PathBuf},
  sync::Arc,
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};

use self::{
  auth::IpcToken,
  control_plane::BindResult,
  discovery_task::DiscoveryOptions,
  lockfile::{acquire, AcquireOutcome},
  registry::SupervisorRegistry,
  runtime_file::RuntimeInfo,
  shutdown::{install_signal_handlers, ShutdownToken},
  state_store::{load as load_state, RunningSnapshot},
};
use crate::config::loader::{LemonadeConfig, PortRange, ProxyConfig};
use crate::daemon::probe::ProbeOptions;
use crate::discovery::ModelCatalog;
use crate::ipc::methods::{LaunchEnv, MethodContext, PersistedState};
use crate::proxy::{self, server::ProxyStatus};

/// Options for starting the daemon. `state_dir` holds the PID
/// lockfile and the per-instance `runtime.json` (which carries the
/// bearer token + control-plane URL clients attach with). Defaults
/// to the OS-conventional paths via `util::paths`; tests and
/// alternate deployments override.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
  pub state_dir: PathBuf,
  /// Per-launch log directory. Each `start_model` opens a file
  /// under here so the supervisor's stdout/stderr tee + the
  /// `logs_tail` IPC method have a durable backing store.
  pub log_dir: PathBuf,
  /// `llama-server` binary path. `None` defers resolution to
  /// `start_model` time (current behaviour for tests that never
  /// launch); production startup pre-resolves so the daemon fails
  /// fast if the binary is missing.
  pub binary: Option<PathBuf>,
  /// Additional `llama-server` binaries (from `config.llama_server_paths`)
  /// probed for devices alongside `binary`. Each is queried with
  /// `--list-devices` at startup; the union (deduped by selector)
  /// becomes the launch device catalog. Lets one install offer
  /// CUDA / ROCm / Vulkan launches by pointing at the matching
  /// single-backend builds. Backend is inferred from each binary's
  /// device names — never declared in config.
  pub extra_binaries: Vec<PathBuf>,
  /// Listening-port range Unit 5's allocator probes. Defaults to
  /// the plan's `41100..=41300`.
  pub port_range: PortRange,
  /// Discovery roots and scan tunables. An empty `scan_roots` list
  /// leaves the catalog empty until the user adds paths via the TUI
  /// or CLI; tests construct one of these with a temp dir seeded
  /// with `.gguf` fixtures.
  pub discovery: DiscoveryOptions,
  /// Per-launch health-probe deadline. `None` keeps the
  /// [`ProbeOptions::default`] timeout (120 s); production wiring
  /// passes through `Config::probe_timeout_secs` so users can raise
  /// it for very large models on slow disks.
  pub probe_timeout_secs: Option<u64>,
  /// Per-architecture launch defaults from `Config.arch_defaults` —
  /// user escape hatch over the built-in `(arch, gpu_backend)` table.
  /// The daemon's `start_model` handler merges these into the
  /// layered resolver with `LayerLabel::ArchDefault`, between
  /// `LastUsed` and `BuiltIn`. Default: empty map.
  pub arch_defaults: std::collections::BTreeMap<String, crate::config::TypedKnobs>,
  /// Extra CLI args to propagate to the re-exec'd child when
  /// `start_detached` spawns the daemon. Tests leave this empty;
  /// production builds it from the parent's `--model-path` /
  /// `--no-scan` / `--llama-server` / `--config` flags so the
  /// detached child resolves the same discovery surface the parent
  /// would have. Without propagation the child rebuilds its options
  /// from an empty `Cli` and silently ignores the user's flags.
  pub propagated_cli_args: Vec<std::ffi::OsString>,
  /// OpenAI-compat proxy router config (enabled flag + loopback
  /// port). Sourced from the user's `[proxy]` section; enabled by
  /// default. Normal mode prefers `127.0.0.1:11435`; Ollama-compat
  /// mode prefers `127.0.0.1:11434`. Tests that don't care about the
  /// proxy can keep the default — the listener binds on an
  /// unprivileged port and is best-effort (bind failure is
  /// non-fatal).
  pub proxy: ProxyConfig,
  /// Lemonade backend config (opt-in enable + the user's `lemond` path +
  /// port). Sourced from the `[lemonade]` section, OR-ed with the
  /// `--lemonade` flag / `LLAMASTASH_LEMONADE` env. When `lemonade.enabled`
  /// is false (the default) the daemon never runs Lemonade discovery,
  /// supervises the umbrella, or routes to it.
  pub lemonade: LemonadeConfig,
  /// Control-plane HTTP listener port. Phase A of the Windows+HTTP-IPC
  /// plan: the bearer-token-authed JSON-RPC server binds here. `0`
  /// means "let the kernel pick" (used by tests for collision
  /// isolation); production wiring uses
  /// [`control_plane::DEFAULT_CONTROL_PORT`] with a small scan window
  /// for fallback. The bound address + bearer token are written to
  /// `runtime.json` at startup so attaching CLI / TUI clients can
  /// discover them.
  pub control_plane_port: u16,
  /// `daemon start --force`: came up despite an *indicated* backend failing
  /// its precheck (missing `llama-server`, missing/blocked Lemonade umbrella).
  /// The CLI gate (`precheck_indicated_backends`) is what enforces fail-fast;
  /// this only needs to ride through the detached re-exec so the foreground
  /// child skips the same gate the parent already waived.
  pub force: bool,
  /// Knob seeding mode (R1) from `Config.default_launch_mode`
  /// (+ `LLAMASTASH_DEFAULT_LAUNCH_MODE`). Threaded into `LaunchEnv`.
  pub default_launch_mode: crate::config::DefaultLaunchMode,
  /// `--fit-ctx` floor (R7) from `Config.fit_ctx_floor`
  /// (+ `LLAMASTASH_FIT_CTX_FLOOR`), validated `1..=MAX_CTX_TOKENS`.
  pub fit_ctx_floor: u32,
  /// Strict-fit mode (R19) from `Config.strict_fit`
  /// (+ `LLAMASTASH_STRICT_FIT`).
  pub strict_fit: bool,
}

impl DaemonOptions {
  /// Test/utility helper: pin every path under one root directory.
  /// Production callers should prefer `from_defaults` plus the CLI's
  /// `build_options` flow, which threads config-driven overrides
  /// through.
  pub fn rooted_at(root: PathBuf) -> Self {
    let log_dir = root.join("logs");
    Self {
      state_dir: root,
      log_dir,
      binary: None,
      extra_binaries: Vec::new(),
      port_range: PortRange::default(),
      discovery: DiscoveryOptions::new(Vec::new()),
      probe_timeout_secs: None,
      arch_defaults: std::collections::BTreeMap::new(),
      default_launch_mode: crate::config::DefaultLaunchMode::default(),
      fit_ctx_floor: crate::config::DEFAULT_FIT_CTX_FLOOR,
      strict_fit: false,
      propagated_cli_args: Vec::new(),
      // Tests using `rooted_at` rarely care about the proxy; bind
      // attempts are best-effort so even a port-collision is silent
      // from the test's standpoint. Tests that *do* want the proxy
      // off can flip `enabled` after construction.
      proxy: ProxyConfig::default(),
      lemonade: LemonadeConfig::default(),
      // Port `0` makes every test pick an ephemeral free slot — no
      // cross-test contention on the
      // [`control_plane::DEFAULT_CONTROL_PORT`] the production CLI
      // uses via `from_defaults`.
      control_plane_port: 0,
      force: false,
    }
  }

  /// Build options using the conventional XDG / macOS paths. Returns an
  /// error if the platform can't supply a state directory.
  pub fn from_defaults() -> Result<Self> {
    let state_dir = crate::util::paths::state_dir()
      .context("could not resolve a state directory for this platform")?;
    let log_dir = crate::util::paths::log_dir()
      .context("could not resolve a cache/log directory for this platform")?;
    Ok(Self {
      state_dir,
      log_dir,
      binary: None,
      extra_binaries: Vec::new(),
      port_range: PortRange::default(),
      // Production-default discovery: no scan roots until a later
      // commit threads config + CLI flags through `handle_start`.
      // Empty roots still produce a working daemon — `list_models`
      // returns `{"models": []}`.
      discovery: DiscoveryOptions::new(Vec::new()),
      probe_timeout_secs: None,
      arch_defaults: std::collections::BTreeMap::new(),
      default_launch_mode: crate::config::DefaultLaunchMode::default(),
      fit_ctx_floor: crate::config::DEFAULT_FIT_CTX_FLOOR,
      strict_fit: false,
      propagated_cli_args: Vec::new(),
      proxy: ProxyConfig::default(),
      lemonade: LemonadeConfig::default(),
      control_plane_port: control_plane::DEFAULT_CONTROL_PORT,
      force: false,
    })
  }
}

/// Outcome of starting the daemon — surfaces the "another daemon is
/// already running" case so the CLI can exit 0 with a friendly message
/// rather than a generic error.
pub enum StartOutcome {
  /// Daemon ran to clean shutdown.
  RanToCompletion,
  /// Another instance is already running.
  AlreadyRunning(i32),
}

/// The fail-closed proxy backstop: `true` when the daemon must refuse
/// to bind the proxy because it would face the network with no auth.
/// That is exactly a non-loopback `host`, no configured key, and no
/// `--insecure-no-auth` opt-out. Loopback (any address in `127/8`,
/// `::1`), a configured key, or the explicit opt-out each clear it.
///
/// Extracted so the security boundary has a direct truth-table test —
/// `run_foreground` itself is hard to unit-test, and a stray `&&`→`||`
/// or dropped `!` here would silently expose an unauthenticated proxy.
fn must_refuse_insecure_proxy(host: IpAddr, has_api_key: bool, insecure_no_auth: bool) -> bool {
  !host.is_loopback() && !has_api_key && !insecure_no_auth
}

/// Run the daemon in the current process. Returns when the shutdown
/// token is triggered (via the `shutdown` method, SIGINT, or SIGTERM).
pub async fn run_foreground(opts: DaemonOptions) -> Result<StartOutcome> {
  // 1. PID lockfile.
  let lockfile = match acquire(&opts.state_dir).context("acquiring PID lockfile")? {
    AcquireOutcome::Acquired(lock) => lock,
    AcquireOutcome::AlreadyRunning { pid, .. } => return Ok(StartOutcome::AlreadyRunning(pid)),
  };

  // 2. State directory: created lazily by the lockfile / state-store /
  // runtime-file writers. No socket file to clear — the control plane
  // binds a TCP listener (§8c) and writes its URL+token into
  // `runtime.json` instead.

  // 3. Shutdown plumbing.
  let token = ShutdownToken::new();
  let _signal_task = install_signal_handlers(token.clone());

  // 4. Discovery. The catalog is shared between the discovery task
  // (writer) and the IPC dispatcher (reader). An empty scan_roots
  // produces a working daemon with an empty catalog — `list_models`
  // returns `{"models": []}`.
  let catalog = ModelCatalog::new();
  // Lemonade discovery is opt-in and off by default, so a standard install
  // never contacts `lemond`. Only an enabled backend threads its port in.
  let mut discovery_opts = opts.discovery.clone();
  if opts.lemonade.enabled {
    discovery_opts.lemonade_port = Some(opts.lemonade.port);
  }
  let _discovery = discovery_task::spawn(catalog.clone(), discovery_opts);

  // 5. Persisted state — favorites, presets, last_params, running.
  // A parse failure does NOT block daemon start: the file is moved
  // to `state.json.broken-<ts>` and the daemon comes up with
  // defaults. Same posture as the plan's
  // `state.json corruption could brick the daemon` mitigation.
  let persisted_state = match load_state(&opts.state_dir) {
    Ok(s) => s,
    Err(e) => {
      log::warn!("state-store load failed; starting with defaults: {e}");
      quarantine_broken_state(&opts.state_dir);
      Default::default()
    }
  };

  // 6. Orphan / external sweep. Live recorded PIDs that still answer
  // `/v1/models` correctly are surfaced as *external* processes
  // (read-only; `stop_external` can target them) rather than re-
  // entering `state.running`. Rebuilding a full `ManagedModel` for
  // an adopted entry would require re-attaching to the live child's
  // stdout/stderr, which isn't feasible after process boundaries.
  // Without this routing change, adopted entries would persist in
  // `state.running` forever — `stop_model` returns InvalidParams
  // for them (no live supervisor), and every subsequent restart
  // would re-adopt the same row.
  let recorded_running: Vec<RunningSnapshot> = persisted_state.running.clone();
  let sweep = orphans::sweep(orphans::SweepInputs::new(&recorded_running)).await;
  let mut state_after_sweep = persisted_state;
  // Clear `running` — only the IPC `start_model` path repopulates
  // this slot via live supervisors going forward.
  state_after_sweep.running.clear();
  if let Err(e) = state_store::save(&opts.state_dir, &state_after_sweep) {
    log::warn!("state-store: failed to persist after orphan sweep: {e}");
  }
  // Merge adopted snapshots into the external slot. They share the
  // ExternalProcess shape (pid + cmdline + model_path); a synthetic
  // cmdline + best-effort start_time keeps the structure consistent.
  let mut external_combined = sweep.external.clone();
  for adopted in &sweep.adopted {
    if external_combined
      .iter()
      .any(|e| e.pid == adopted.pid as u32)
    {
      continue;
    }
    let start_time_secs = lookup_start_time(adopted.pid as u32).unwrap_or(0);
    external_combined.push(orphans::ExternalProcess {
      pid: adopted.pid as u32,
      cmdline: format!(
        "llama-server --port {} -m {}",
        adopted.port,
        adopted
          .id
          .as_gguf()
          .map(|g| g.path.display().to_string())
          .unwrap_or_default()
      ),
      model_path: adopted.id.as_gguf().map(|g| g.path.clone()),
      start_time_secs,
      port: Some(adopted.port),
      // Adopted entries went through our state.json before the
      // restart — by construction they were launched by *this*
      // daemon's previous instance and therefore carry the same
      // env marker. Marking them keeps `collect_in_use_ports`
      // consistent across the adopted-vs-external split.
      launched_by_llamastash: true,
    });
  }
  log::info!(
    "orphan sweep: {} adopted (now external), {} stale, {} external",
    sweep.adopted.len(),
    sweep.stale.len(),
    sweep.external.len()
  );

  // 7. GPU probe (best-effort). Always returns *some* `GpuInfo`,
  // even if it's `CpuOnly`. Seeded for the fallback `ctx.gpu` slot
  // before the sampler's first tick lands.
  let initial_gpu = crate::gpu::probe();

  // 7b. Host-metrics sampler (1 Hz). Re-probes the active GPU
  // backend each tick for live util/temp/VRAM; sysinfo handles
  // host CPU% + RAM. The sampler also owns the live `GpuInfo` cell
  // so `status.gpu` follows hotplug instead of staying pinned to the
  // boot snapshot.
  let sampler =
    crate::daemon::host_metrics::spawn(token.clone(), std::time::Duration::from_secs(1));

  // 8. Wire the dispatcher context.
  let supervisors = SupervisorRegistry::new();
  let persisted = PersistedState::new(state_after_sweep, Some(opts.state_dir.clone()));
  // Construct the proxy status cell *before* the context so the IPC
  // `status` handler and the proxy listener task share the same
  // handle. Unit 5 surfaces the cell via `status.proxy`; Unit 1
  // already locked the seeded variant (`Disabled`) so a daemon with
  // `proxy.enabled: false` reads as disabled even before §8b runs.
  let proxy_status_cell = proxy::server::new_status_cell();
  let mut ctx = MethodContext::with_catalog(token.clone(), catalog)
    .with_supervisors(supervisors)
    .with_gpu(initial_gpu)
    .with_sampler(sampler)
    .with_state(persisted)
    .with_external(external_combined)
    .with_proxy_status(std::sync::Arc::clone(&proxy_status_cell))
    .with_lemonade(opts.lemonade.clone());
  if let Some(binary) = opts.binary.clone() {
    if let Err(e) = std::fs::create_dir_all(&opts.log_dir) {
      log::warn!(
        "could not create log dir {}: {e}; logs may fail to open",
        opts.log_dir.display()
      );
    }
    let probe = match opts.probe_timeout_secs {
      Some(secs) => ProbeOptions {
        timeout: std::time::Duration::from_secs(secs),
        ..ProbeOptions::default()
      },
      None => ProbeOptions::default(),
    };
    // Build the launch device catalog from the default binary plus any
    // configured extras. The default binary leads so it wins selector
    // collisions (e.g. two builds both exposing `Vulkan0`).
    let mut catalog_binaries = vec![binary.clone()];
    for extra in &opts.extra_binaries {
      if !catalog_binaries.contains(extra) {
        catalog_binaries.push(extra.clone());
      }
    }
    // Populate the catalog in the background: probing each binary with
    // `--list-devices` is best-effort I/O we must keep off the startup
    // critical path so the detached-start parent's `runtime.json` wait
    // never trips. The cell starts empty and flips to the full set once
    // the probe finishes.
    let device_catalog = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    {
      let cell = Arc::clone(&device_catalog);
      let binary_count = catalog_binaries.len();
      tokio::spawn(async move {
        let built = tokio::task::spawn_blocking(move || {
          crate::launch::list_devices::build_catalog(&catalog_binaries)
        })
        .await
        .unwrap_or_default();
        log::info!(
          "launch device catalog: {} device(s) across {} binary(ies)",
          built.len(),
          binary_count
        );
        *cell.write().await = built;
      });
    }
    ctx = ctx.with_launch_env(LaunchEnv {
      binary,
      port_range: opts.port_range,
      log_dir: opts.log_dir.clone(),
      probe,
      arch_defaults: opts.arch_defaults.clone(),
      device_catalog,
      default_launch_mode: opts.default_launch_mode,
      fit_ctx_floor: opts.fit_ctx_floor,
      strict_fit: opts.strict_fit,
    });
  } else {
    log::info!(
      "daemon started without `llama-server` binary resolved; `start_model` will return an error until one is configured"
    );
  }

  // 8a. Lemonade umbrella supervision (opt-in). When the backend is enabled,
  // bring up the one shared `lemond` umbrella on its configured loopback port
  // so discovery (which probes that port) and proxy routing (which forwards to
  // the registered umbrella) both work without the user first issuing an
  // explicit `start`. Done in a background task: the supervisor blocks until
  // `/live` is ready, which must not delay the daemon's listeners. A missing
  // `lemond` binary is a clean warning, never a daemon-start failure —
  // llamastash never installs it. The `start_model` path's `ensure_umbrella`
  // is idempotent, so it reuses this umbrella rather than spawning a second.
  if opts.lemonade.enabled {
    match crate::backend::lemonade::resolve_lemond_binary(&opts.lemonade) {
      Some(binary) => {
        let port = opts.lemonade.port;
        if let Err(e) = std::fs::create_dir_all(&opts.log_dir) {
          log::warn!(
            "could not create log dir {}: {e}; lemonade umbrella logs may fail to open",
            opts.log_dir.display()
          );
        }
        let probe = match opts.probe_timeout_secs {
          Some(secs) => ProbeOptions {
            timeout: std::time::Duration::from_secs(secs),
            ..ProbeOptions::default()
          },
          None => ProbeOptions::default(),
        };
        let umbrella = crate::backend::lemonade::umbrella_process_spec(port, binary, probe);
        let registry = ctx.supervisors.clone();
        let log_path = opts.log_dir.join("lemonade-umbrella.log");
        // `ensure_umbrella` refuses to adopt a foreign process already holding
        // the port (returns `PortInUse`) rather than logging a false "supervised"
        // and 503-ing opaquely at routing time. Surface that case plainly. Not
        // fatal: the daemon (and llama.cpp routing) stay up; only Lemonade
        // routing is unavailable until the conflict is resolved.
        //
        // A port held only by teardown remnants (FIN-WAIT-2 / TIME-WAIT
        // leftovers of a just-stopped daemon's `lemond`) is waited out
        // first: the kernel clears them within its fin-timeout (~60 s),
        // so a quick `daemon stop && daemon start --lemonade` brings the
        // umbrella up as soon as the port frees instead of failing.
        tokio::spawn(async move {
          use crate::backend::lemonade::{umbrella_port_state, UmbrellaPortState};
          let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
          let mut waiting_logged = false;
          loop {
            if umbrella_port_state(port) == UmbrellaPortState::Free {
              match crate::backend::lemonade::ensure_umbrella(
                &registry,
                port,
                umbrella.clone(),
                log_path.clone(),
              )
              .await
              {
                Ok(_) => {
                  log::info!("lemonade umbrella supervised on 127.0.0.1:{port}");
                  return;
                }
                // Lost a probe-to-spawn race (e.g. the previous daemon's
                // dying `lemond` flickering through teardown) — retry
                // within the window like any other transient holder.
                Err(crate::daemon::supervisor::SpawnError::PortInUse(_)) => {}
                Err(e) => {
                  log::warn!("lemonade umbrella failed to start at boot: {e}");
                  return;
                }
              }
            }
            // Held — by a still-exiting previous umbrella (Listening, drops
            // within its SIGTERM→SIGKILL grace) or by kernel teardown
            // remnants (FIN-WAIT-2 / TIME-WAIT, clear within the
            // fin-timeout). Both resolve on their own; a genuinely foreign
            // holder is normally caught by `daemon start`'s precheck before
            // this task ever runs, so only after the window do we call it
            // foreign and give up.
            if std::time::Instant::now() >= deadline {
              log::error!(
                "lemonade: 127.0.0.1:{port} is already in use — llamastash could not start its \
                 own managed `lemond`. Stop whatever holds that port (e.g. a manually started \
                 `lemond`) or set `lemonade.port`; Lemonade model routing will return 503 until \
                 this is resolved."
              );
              return;
            }
            if !waiting_logged {
              log::info!(
                "lemonade: 127.0.0.1:{port} is still held (previous umbrella exiting, or its \
                 sockets draining); retrying for up to 90 s…"
              );
              waiting_logged = true;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
          }
        });
      }
      None => log::warn!(
        "lemonade enabled but no `lemond` binary found (set `lemonade.binary` or put `lemond` on PATH); skipping umbrella supervision"
      ),
    }
  }

  // 8b. OpenAI-compat proxy listener. Spawned between the
  // host-metrics sampler (which the proxy doesn't depend on but
  // which is the canonical "background sampler" anchor) and the
  // unix-socket accept loop so the IPC ctx is fully populated
  // before the proxy reads from it. Bind failure is intentionally
  // non-fatal — the proxy is a convenience surface; the daemon's
  // primary contract (IPC + supervisor) survives a port collision.
  // The status cell holds the outcome (Disabled / Listening /
  // PortInUse / Unbound); Unit 5's IPC `status` handler reads it
  // via the clone attached to `ctx` above (§8).
  if opts.proxy.enabled {
    let host = opts.proxy.effective_host();
    let addr = proxy::server::listen_addr(host, opts.proxy.effective_port());
    // Fail-closed backstop: never expose a non-loopback proxy with no
    // bearer key unless the operator explicitly opted out. The CLI
    // `daemon start --proxy-host` path auto-provisions a key so users
    // don't normally reach this; the backstop catches config-only and
    // auto-spawn paths that bypass the CLI. The daemon and the control
    // plane keep running — only the proxy listener is skipped.
    if must_refuse_insecure_proxy(
      host,
      opts.proxy.api_key.is_some(),
      opts.proxy.insecure_no_auth,
    ) {
      log::error!(
        "proxy: refusing to bind {addr} without authentication. Set proxy.api_key \
         (e.g. run `llamastash daemon start --proxy-host {host}` to auto-generate one) \
         or pass --insecure-no-auth. Daemon continues without the proxy."
      );
      if let Ok(mut guard) = proxy_status_cell.write() {
        *guard = ProxyStatus::RefusedInsecure { addr };
      }
    } else {
      // Loud heads-up whenever the proxy faces the network.
      if !host.is_loopback() {
        let auth_note = if opts.proxy.api_key.is_some() {
          "bearer auth required"
        } else {
          "NO authentication (--insecure-no-auth)"
        };
        let reachable = if host.is_unspecified() {
          format!(
            "port {} on all interfaces (use this machine's LAN IP)",
            opts.proxy.effective_port()
          )
        } else {
          format!("http://{addr}")
        };
        log::warn!(
          "proxy: exposed on the LAN at {reachable} ({auth_note}); the control plane \
           and llama-server children stay loopback"
        );
      }
      let state = proxy::ProxyState::from_context_with_auth(
        &ctx,
        opts.proxy.ollama_compat,
        opts.proxy.fallback_enabled,
        opts.proxy.api_key.clone(),
      );
      let token_for_proxy = token.clone();
      let status_for_proxy = std::sync::Arc::clone(&proxy_status_cell);
      let serve_opts = proxy::server::ServeOptions {
        header_read_timeout: std::time::Duration::from_secs(opts.proxy.header_read_timeout_secs),
        ..proxy::server::ServeOptions::default()
      };
      // Idle-TTL eviction sweeper. Skipped entirely when
      // `idle_ttl_secs = 0` (operator disabled it). Runs in parallel
      // with the listener; uses the same shutdown token so daemon stop
      // tears both down at once.
      if opts.proxy.idle_ttl_secs > 0 {
        let state_for_evict = std::sync::Arc::clone(&state);
        let token_for_evict = token.clone();
        let ttl = std::time::Duration::from_secs(opts.proxy.idle_ttl_secs);
        supervisor::spawn_supervised("proxy_eviction_sweeper", async move {
          proxy::eviction::run(state_for_evict, ttl, token_for_evict).await;
        });
      }
      supervisor::spawn_supervised("proxy_listener", async move {
        if let Err(e) = proxy::server::serve_with_options(
          state,
          addr,
          token_for_proxy,
          status_for_proxy,
          serve_opts,
        )
        .await
        {
          log::warn!("proxy listener task ended with error: {e}");
        }
      });
    }
  } else {
    log::info!("proxy listener disabled in config; daemon stays IPC-only");
    if let Ok(mut guard) = proxy_status_cell.write() {
      *guard = ProxyStatus::Disabled;
    }
  }

  // 8c. Control-plane HTTP listener — the daemon's only RPC surface.
  // Binds a loopback TCP port; auth is bearer-token; the token + URL
  // are written to `runtime.json` under `state_dir` so attaching CLI
  // / TUI clients can pick them up. Bind failure is fatal here (the
  // proxy is best-effort but the control plane *is* the daemon's
  // primary contract); the error propagates so the caller sees a
  // clean exit-code-1 instead of a daemon with no RPC surface.
  let control_token = Arc::new(IpcToken::generate());
  let control_addr = control_plane::loopback_addr(opts.control_plane_port);
  let (control_listener, control_bound) = match control_plane::bind(control_addr).await {
    BindResult::Bound {
      listener: control_listener,
      addr: control_bound,
    } => (control_listener, control_bound),
    BindResult::AllPortsInUse { last_addr } => {
      return Err(anyhow!(
        "control plane: ports {}..={} all in use; cannot start daemon",
        opts.control_plane_port,
        last_addr.port()
      ));
    }
    BindResult::Failed { addr, error } => {
      return Err(anyhow!(
        "control plane: failed to bind {addr}: {error}; cannot start daemon"
      ));
    }
  };
  let ipc_url = format!("http://{control_bound}");
  log::info!("control plane listening on {ipc_url}");
  let info = RuntimeInfo {
    schema_version: 1,
    ipc_url: ipc_url.clone(),
    ipc_token: control_token.as_str().to_owned(),
    started_at_unix: SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_secs())
      .unwrap_or_default(),
    daemon_pid: std::process::id() as i32,
  };
  if let Err(e) = runtime_file::save(&opts.state_dir, &info) {
    log::warn!("control plane: could not persist runtime.json: {e}");
  }
  // Attach the bound URL to the dispatcher's context so `status` can
  // surface it under `daemon.ipc_url`. Done after the listener resolves
  // (the configured port may differ from the bound port when a scan
  // landed on an offset).
  ctx = ctx.with_ipc_url(ipc_url);
  let control_token_for_serve = Arc::clone(&control_token);
  let control_ctx = ctx.clone();
  let control_token_signal = token.clone();
  supervisor::spawn_supervised("control_plane_listener", async move {
    if let Err(e) = control_plane::serve(
      control_listener,
      control_token_for_serve,
      control_ctx,
      control_token_signal,
    )
    .await
    {
      log::warn!("control plane listener task ended with error: {e}");
    }
  });

  // 9. Wait for shutdown. The control plane runs as a supervised
  // background task; the foreground future parks on the shutdown
  // token so SIGINT/SIGTERM/IPC `shutdown` all unblock the same way.
  token.wait_until_triggered().await;

  // 9b. SIGTERM-then-SIGKILL every supervised `llama-server` before
  // exiting. The supervisor's `pre_exec(setsid)` makes each child a
  // session leader so it survives a daemon crash (R42's orphan
  // adoption rescues those on the next start). For *deliberate*
  // exits — `daemon stop`, SIGINT, SIGTERM, IPC `shutdown` — we
  // don't want children to leak. The 5 s grace mirrors
  // `default_grace_secs` in the IPC `stop_model` handler.
  let stopped = crate::ipc::methods::stop_all_managed(&ctx, Duration::from_secs(5)).await;
  if !stopped.is_empty() {
    log::info!("shutdown: stopped {} managed launch(es)", stopped.len());
  }

  // 10. Cleanup. Lockfile cleans itself in Drop; `runtime.json` is
  // best-effort removed so a fresh daemon never reads a stale
  // URL/token pair (the lockfile is the authoritative liveness check,
  // but stale runtime.json would just cost the next client an extra
  // retry).
  runtime_file::remove(&opts.state_dir);
  drop(lockfile);

  Ok(StartOutcome::RanToCompletion)
}

/// Best-effort `start_time` lookup for a PID. Used to seed
/// [`ExternalProcess::start_time_secs`] for adopted entries the
/// daemon can't itself supervise. Falls back to 0 if `sysinfo` has
/// no record (rare; means the PID has already exited).
fn lookup_start_time(pid: u32) -> Option<u64> {
  use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
  // `everything()` over a blank kind: explicit about wanting all
  // process metadata so `start_time()` is reliably populated across
  // sysinfo versions and platforms. The cost is one extra /proc read
  // per call, negligible at boot-sweep scale.
  let refresh = ProcessRefreshKind::everything();
  let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
  sys.refresh_processes_specifics(
    sysinfo::ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
    true,
    refresh,
  );
  // Filter `start_time == 0` so the consumer's `live != 0` guard
  // sees `None` instead of a meaningless zero.
  sys
    .process(Pid::from_u32(pid))
    .map(|p| p.start_time())
    .filter(|s| *s != 0)
}

/// Move a malformed `state.json` aside so the daemon can restart
/// with defaults. The plan's `state-json corruption` mitigation
/// — keeps the user's prior data on disk for inspection.
fn quarantine_broken_state(state_dir: &Path) {
  let src = state_dir.join("state.json");
  if !src.exists() {
    return;
  }
  let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or_default();
  let dst = state_dir.join(format!("state.json.broken-{ts}"));
  if let Err(e) = std::fs::rename(&src, &dst) {
    log::warn!(
      "could not quarantine corrupt state.json to {}: {e}",
      dst.display()
    );
  }
}

/// Re-exec the current binary as a detached daemon child and wait for it
/// to bind its socket. The parent returns to the user's shell once the
/// socket is connectable; the child is the long-lived daemon.
///
/// We deliberately do **not** call `fork()` ourselves: this function may
/// be invoked from inside a multithreaded tokio runtime, and `fork()` in
/// that situation only carries the calling thread into the child, leaving
/// any mutex held by another thread permanently locked. `Command::spawn`
/// with `pre_exec(setsid)` gets us a properly detached child without
/// touching the runtime.
///
/// Mechanism:
/// 1. Spawn `llamastash daemon start` (foreground mode) with `stdin`/
///    `stdout`/`stderr` redirected to `/dev/null` and `setsid` applied
///    between `fork` and `exec`.
/// 2. Poll for `runtime.json` (the HTTP control plane's handshake
///    file) for up to ~3s. Success → daemon is ready; return.
/// 3. If the child has already exited (e.g. AlreadyRunning), reap it
///    and surface its exit status.
#[cfg(unix)]
pub fn start_detached(opts: DaemonOptions) -> Result<StartOutcome> {
  let exe = std::env::current_exe().context("locating current executable for --detach")?;
  start_detached_with_exe(opts, exe)
}

/// Detached-start with an explicit executable path. Production callers
/// should use [`start_detached`], which resolves `current_exe()` itself.
/// Integration tests use this overload to point at the test binary so
/// they can exercise the full re-exec path against temp `DaemonOptions`.
#[cfg(unix)]
#[doc(hidden)]
pub fn start_detached_with_exe(opts: DaemonOptions, exe: PathBuf) -> Result<StartOutcome> {
  use std::{
    os::unix::process::CommandExt,
    process::{Command, Stdio},
  };

  // Fast path: a live daemon already owns the lockfile. Don't spawn a
  // child only to have it bail out.
  if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
    if matches!(runtime_file::load(&opts.state_dir), Ok(Some(_))) {
      return Ok(StartOutcome::AlreadyRunning(pid));
    }
  }

  let mut cmd = Command::new(&exe);
  // Global flags (`--model-path`, `--no-scan`, `--llama-server`,
  // `--config`) must appear before the subcommand. clap accepts them
  // either side because they are `global = true`, but front-loading
  // them keeps the child's argv readable in `ps` output and avoids
  // any clap parse-order surprises.
  for arg in &opts.propagated_cli_args {
    cmd.arg(arg);
  }
  cmd
    .arg("daemon")
    .arg("start")
    // The re-exec'd child must run in the foreground — otherwise it
    // hits the same "detach by default" branch we just executed and
    // spawns *its own* grandchild, recursing into a fork bomb. The
    // child IS the daemon; `setsid` (applied below) is what actually
    // backgrounds it from the original shell's perspective.
    .arg("--foreground")
    // Propagate the caller-supplied state directory to the re-exec'd
    // child via the hidden flag. Without this, the child rebuilt
    // `DaemonOptions` from XDG defaults and silently ignored the
    // parent's choices.
    .arg("--state-dir")
    .arg(&opts.state_dir)
    // Propagate the effective proxy port so a `daemon start --detach
    // --proxy-port N` doesn't drop the override on re-exec. We pass
    // the *resolved* port (`effective_port`) so the child binds the
    // same address even when the parent inferred it from
    // `ollama_compat` rather than a literal `port:` value. Idempotent
    // when the child re-reads the same config file (same value).
    .arg("--proxy-port")
    .arg(opts.proxy.effective_port().to_string());
  // Carry the Ollama-compat mode bool through so the child also
  // serves the `"Ollama is running"` identity on `GET /` — the env
  // var alone isn't reliable across a detached re-exec.
  if opts.proxy.ollama_compat {
    cmd.arg("--ollama-compat");
  }
  // Carry the LAN bind host + insecure opt-out through so a detached
  // re-exec keeps them (they are per-invocation overrides, not config).
  // The child re-resolves the API key from config — it is never passed
  // via argv, which would leak the secret in the process list.
  if let Some(host) = opts.proxy.host {
    cmd.arg("--proxy-host").arg(host.to_string());
  }
  if opts.proxy.insecure_no_auth {
    cmd.arg("--insecure-no-auth");
  }
  // Carry the opt-in Lemonade enable through the re-exec so a
  // `daemon start --lemonade` (detached) keeps the backend on in the child.
  // The env var alone isn't reliable across a detached re-exec.
  if opts.lemonade.enabled {
    cmd.arg("--lemonade");
  }
  // Carry `--force` through so the foreground child skips the same backend
  // precheck the parent already waived; without it the child re-runs the gate
  // and exits, defeating the whole point of `--force`.
  if opts.force {
    cmd.arg("--force");
  }
  cmd
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

  // SAFETY: `pre_exec` runs in the child between fork and exec. We call
  // only async-signal-safe operations: `setsid` is on the POSIX
  // async-signal-safe list. No locks, allocations, or other tokio state
  // are touched here.
  unsafe {
    cmd.pre_exec(|| {
      if libc::setsid() < 0 {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }

  let mut child = cmd.spawn().context("spawning detached daemon")?;

  // Poll for the runtime info file to appear. `runtime_file::save`
  // happens after the control plane has bound its TCP port, so a
  // present file means the daemon is ready to accept HTTP requests.
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if let Some(status) = child.try_wait()? {
      if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
        return Ok(StartOutcome::AlreadyRunning(pid));
      }
      return Err(anyhow!(
        "detached daemon exited before binding control plane (exit code: {:?})",
        status.code()
      ));
    }
    if matches!(runtime_file::load(&opts.state_dir), Ok(Some(_))) {
      return Ok(StartOutcome::RanToCompletion);
    }
    if std::time::Instant::now() > deadline {
      // Don't leave the child orphaned if it's hung — kill and reap.
      let _ = child.kill();
      let _ = child.wait();
      return Err(anyhow!(
        "detached daemon did not bind control plane within 3s (state_dir: {})",
        opts.state_dir.display()
      ));
    }
    std::thread::sleep(Duration::from_millis(50));
  }
}

/// Windows detached-start. Unlike Unix there's no `fork()` or
/// `setsid()` — `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` at spawn
/// time gives us a hidden-console child outside the parent's group,
/// and the surrounding `runtime.json` poll loop is identical to the
/// Unix path.
///
/// `CREATE_NO_WINDOW` (not `DETACHED_PROCESS`) keeps the symmetry with
/// `WindowsProcessControl::spawn_supervised` — DETACHED_PROCESS gives
/// the child no console at all, which interacts poorly with tokio's
/// piped stdio on Windows (surfaced in CI as the supervisor never
/// reaching Ready). The daemon itself uses `Stdio::null()` so the
/// immediate tokio bug wouldn't bite, but matching the supervisor's
/// spawn flags keeps the daemon-as-grandparent → supervised-child
/// console inheritance well-defined instead of relying on an unspecified
/// `DETACHED_PROCESS → CREATE_NO_WINDOW` interaction.
#[cfg(windows)]
pub fn start_detached(opts: DaemonOptions) -> Result<StartOutcome> {
  let exe = std::env::current_exe().context("locating current executable for --detach")?;
  start_detached_with_exe(opts, exe)
}

#[cfg(windows)]
#[doc(hidden)]
pub fn start_detached_with_exe(opts: DaemonOptions, exe: PathBuf) -> Result<StartOutcome> {
  use std::{
    os::windows::process::CommandExt,
    process::{Command, Stdio},
  };
  use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

  // Fast path: a live daemon already owns the state dir. Don't spawn a
  // child only to have it bail.
  if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
    if matches!(runtime_file::load(&opts.state_dir), Ok(Some(_))) {
      return Ok(StartOutcome::AlreadyRunning(pid));
    }
  }

  let mut cmd = Command::new(&exe);
  for arg in &opts.propagated_cli_args {
    cmd.arg(arg);
  }
  cmd
    .arg("daemon")
    .arg("start")
    .arg("--foreground")
    .arg("--state-dir")
    .arg(&opts.state_dir)
    .arg("--proxy-port")
    .arg(opts.proxy.effective_port().to_string());
  if opts.proxy.ollama_compat {
    cmd.arg("--ollama-compat");
  }
  // Carry the LAN bind host + insecure opt-out through so a detached
  // re-exec keeps them (they are per-invocation overrides, not config).
  // The child re-resolves the API key from config — it is never passed
  // via argv, which would leak the secret in the process list.
  if let Some(host) = opts.proxy.host {
    cmd.arg("--proxy-host").arg(host.to_string());
  }
  if opts.proxy.insecure_no_auth {
    cmd.arg("--insecure-no-auth");
  }
  // Carry the opt-in Lemonade enable through the re-exec so a
  // `daemon start --lemonade` (detached) keeps the backend on in the child.
  // The env var alone isn't reliable across a detached re-exec.
  if opts.lemonade.enabled {
    cmd.arg("--lemonade");
  }
  // Carry `--force` through so the foreground child skips the same backend
  // precheck the parent already waived.
  if opts.force {
    cmd.arg("--force");
  }
  cmd
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);

  // Stop the detached daemon from inheriting the launcher's own stdio
  // handles. Even with the child's std handles set to NUL above, Windows
  // spawns with `bInheritHandles=TRUE` (std needs it to hand the NUL
  // handles over), which also leaks every *other* inheritable handle the
  // launcher holds — including its stdout/stderr pipe when invoked as
  // `llamastash <cmd> | consumer`. The long-lived daemon would then keep
  // that pipe's write end open for its whole life, so the consumer never
  // sees EOF and hangs (e.g. `llamastash start --json | jq`, or the UAT
  // harness's `wait_with_output`). Clearing the inherit flag on our own
  // std handles right before the spawn closes the leak; we print + exit
  // immediately after, so no later child needs them inheritable.
  clear_std_handle_inheritance();

  let mut child = cmd.spawn().context("spawning detached daemon")?;

  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  loop {
    if let Some(status) = child.try_wait()? {
      if let Some(pid) = existing_daemon_pid(&opts.state_dir) {
        return Ok(StartOutcome::AlreadyRunning(pid));
      }
      return Err(anyhow!(
        "detached daemon exited before binding control plane (exit code: {:?})",
        status.code()
      ));
    }
    if matches!(runtime_file::load(&opts.state_dir), Ok(Some(_))) {
      return Ok(StartOutcome::RanToCompletion);
    }
    if std::time::Instant::now() > deadline {
      let _ = child.kill();
      let _ = child.wait();
      return Err(anyhow!(
        "detached daemon did not bind control plane within 3s (state_dir: {})",
        opts.state_dir.display()
      ));
    }
    std::thread::sleep(Duration::from_millis(50));
  }
}

/// Clear `HANDLE_FLAG_INHERIT` on this process's std handles so a
/// detached daemon spawned immediately after does not inherit them.
/// See the call site in [`start_detached_with_exe`] for the full
/// rationale (Windows `bInheritHandles` pipe-leak → consumer hangs).
#[cfg(windows)]
fn clear_std_handle_inheritance() {
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};
  for h in [
    std::io::stdin().as_raw_handle(),
    std::io::stdout().as_raw_handle(),
    std::io::stderr().as_raw_handle(),
  ] {
    if h.is_null() {
      continue;
    }
    // SAFETY: `h` is a live std handle owned by this process. Clearing
    // the inherit flag is a documented metadata-only op — it affects
    // only whether *future* child processes inherit the handle, never
    // this process's own use of it.
    unsafe {
      SetHandleInformation(h as HANDLE, HANDLE_FLAG_INHERIT, 0);
    }
  }
}

/// Returns the PID owning the daemon lockfile if (and only if) a live
/// process currently holds its `flock`. Used by `start_detached` to
/// short-circuit when an existing daemon already owns the socket, and
/// by the TUI's restart path to wait until an old daemon has fully
/// released its lock before spawning a replacement.
///
/// Probing via `flock` rather than `kill(pid, 0)` matches `acquire`'s
/// ownership model: a recycled-PID collision can't masquerade as a live
/// daemon because the kernel released the lock when the original daemon
/// died, regardless of what the on-disk PID still says.
#[cfg(unix)]
pub(crate) fn existing_daemon_pid(state_dir: &Path) -> Option<i32> {
  use std::os::unix::io::AsRawFd;
  let pidfile = state_dir.join("daemon.pid");
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&pidfile)
    .ok()?;
  // SAFETY: `flock(2)` is a kernel syscall over a borrowed fd; no memory
  // is touched. `file` outlives the call.
  let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
  if ret == 0 {
    // We just acquired the lock — no daemon is running. Dropping `file`
    // closes the fd and releases the lock.
    return None;
  }
  // Lock contended → a daemon owns the pidfile. Read the recorded PID
  // for the friendly "already running" message; ownership is decided by
  // the lock, the PID value is informational.
  let raw = std::fs::read_to_string(&pidfile).ok()?;
  raw.trim().parse::<i32>().ok().filter(|p| *p > 0)
}

/// Windows backend for the daemon-liveness probe. Mirrors the Unix
/// path exactly: open `daemon.pid` and attempt a non-blocking
/// `LockFileEx`. If the lock is contended a daemon owns it; read the
/// recorded PID for the friendly "already running" message. If the
/// lock acquires (or the file doesn't exist), no daemon is running.
#[cfg(windows)]
pub(crate) fn existing_daemon_pid(state_dir: &Path) -> Option<i32> {
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, UnlockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
  };
  use windows_sys::Win32::System::IO::OVERLAPPED;
  const MAXDWORD: u32 = u32::MAX;

  let pidfile = state_dir.join("daemon.pid");
  let file = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(&pidfile)
    .ok()?;
  // SAFETY: OVERLAPPED is POD; zero-init satisfies LockFileEx's
  // synchronous-mode contract.
  let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
  // SAFETY: handle borrowed from `file` for the call's duration.
  let ok = unsafe {
    LockFileEx(
      file.as_raw_handle() as _,
      LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
      0,
      MAXDWORD,
      MAXDWORD,
      &mut overlapped as *mut _,
    )
  };
  if ok != 0 {
    // We acquired the lock — no daemon is running. Release before
    // dropping so the file handle close is the only side effect.
    let mut o2: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: matched UnlockFileEx for the LockFileEx call above.
    unsafe {
      UnlockFileEx(
        file.as_raw_handle() as _,
        0,
        MAXDWORD,
        MAXDWORD,
        &mut o2 as *mut _,
      );
    }
    return None;
  }
  // Lock contended → a daemon owns the pidfile. Read the recorded PID
  // for the friendly message; ownership is decided by the lock, the
  // PID value is informational.
  let raw = std::fs::read_to_string(&pidfile).ok()?;
  raw.trim().parse::<i32>().ok().filter(|p| *p > 0)
}

// Re-export the symbols downstream callers reach for.
#[allow(unused_imports)]
pub use lockfile::AcquireOutcome as LockfileOutcome;
#[allow(unused_imports)]
pub use lockfile::Lockfile as DaemonLockfile;

/// Default drain timeout exposed for callers (tests, CLI status command).
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = control_plane::DRAIN_TIMEOUT;

#[cfg(test)]
mod tests {
  use super::must_refuse_insecure_proxy;
  use std::net::IpAddr;

  fn ip(s: &str) -> IpAddr {
    s.parse().expect("valid ip")
  }

  #[test]
  fn refuse_insecure_proxy_truth_table() {
    // Loopback never refuses, regardless of key / opt-out — the
    // historical same-UID posture is always allowed.
    for host in ["127.0.0.1", "127.0.0.2", "::1"] {
      for has_key in [false, true] {
        for insecure in [false, true] {
          assert!(
            !must_refuse_insecure_proxy(ip(host), has_key, insecure),
            "loopback {host} must never be refused (key={has_key}, insecure={insecure})"
          );
        }
      }
    }

    // Non-loopback: refuse ONLY when there's no key AND no opt-out.
    for host in ["0.0.0.0", "192.168.1.5", "::", "2001:db8::1"] {
      assert!(
        must_refuse_insecure_proxy(ip(host), false, false),
        "{host} with no key and no opt-out must be refused"
      );
      assert!(
        !must_refuse_insecure_proxy(ip(host), true, false),
        "{host} with a key must bind (auth enforced)"
      );
      assert!(
        !must_refuse_insecure_proxy(ip(host), false, true),
        "{host} with --insecure-no-auth must bind (operator opted out)"
      );
      // A key present alongside the opt-out still binds (and auth wins
      // downstream — the key is honored regardless of the flag).
      assert!(!must_refuse_insecure_proxy(ip(host), true, true));
    }
  }
}
