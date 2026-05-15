//! Headless TUI smoke tests against `ratatui::backend::TestBackend`.
//!
//! Each test renders one frame in a known state and asserts on
//! either the visible glyph layout or the App's post-render state.
//! No real terminal, no daemon — proves the render pipeline + key
//! handling stay coherent when the surrounding process changes.

use std::path::PathBuf;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use llamatui::discovery::{DiscoveredModel, ModelSource};
use llamatui::gguf::metadata::{ModeHint, ModelMetadata, Quant};
use llamatui::theme::ThemeName;
use llamatui::tui::app::{App, AppOptions};
use llamatui::tui::events::pump_input;
use llamatui::tui::render::render;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn fake_model(path: &str, parent: &str) -> DiscoveredModel {
  DiscoveredModel {
    path: PathBuf::from(path),
    parent: PathBuf::from(parent),
    source: ModelSource::UserPath,
    metadata: Some(ModelMetadata {
      arch: Some("llama".into()),
      total_parameters: Some(7_000_000_000),
      parameter_label: Some("7B".into()),
      quant: Quant::Q4_K,
      native_ctx: Some(8192),
      chat_template: None,
      tokenizer_kind: None,
      reasoning_hint: None,
      mode_hint: ModeHint::Chat,
    }),
    parse_error: None,
    split_siblings: Vec::new(),
  }
}

fn key(code: KeyCode, mods: KeyModifiers) -> Event {
  Event::Key(KeyEvent::new(code, mods))
}

fn render_to_string(app: &mut App, width: u16, height: u16) -> String {
  let backend = TestBackend::new(width, height);
  let mut terminal = Terminal::new(backend).expect("test terminal");
  terminal.draw(|f| render(f, app)).expect("render");
  let buf = terminal.backend().buffer();
  let mut out = String::with_capacity((width as usize + 1) * height as usize);
  for y in 0..buf.area.height {
    for x in 0..buf.area.width {
      out.push_str(buf[(x, y)].symbol());
    }
    out.push('\n');
  }
  out
}

#[test]
fn empty_app_renders_banner_help_and_empty_state() {
  let mut app = App::new(AppOptions::default());
  let frame = render_to_string(&mut app, 100, 20);
  assert!(frame.contains("llamatui"), "banner missing: {frame}");
  assert!(
    frame.contains("daemon: connecting"),
    "connection pill missing: {frame}"
  );
  assert!(
    frame.contains("No GGUFs surfaced"),
    "empty-state hint missing: {frame}"
  );
  // Help bar surfaces the canonical hotkeys.
  assert!(
    frame.contains("q") && frame.contains("/"),
    "help bar missing q + /: {frame}"
  );
}

#[test]
fn populated_app_renders_directory_groups_and_status_glyph() {
  let mut app = App::new(AppOptions::default());
  app.daemon_connected = true;
  app.models = vec![
    fake_model("/m/x/qwen.gguf", "/m/x"),
    fake_model("/m/y/phi.gguf", "/m/y"),
  ];
  let frame = render_to_string(&mut app, 120, 20);
  assert!(frame.contains("/m/x"), "directory group header missing");
  assert!(frame.contains("qwen"), "qwen row missing");
  assert!(frame.contains("phi"), "phi row missing");
  assert!(frame.contains("daemon: connected"));
}

#[test]
fn favorites_render_above_directory_groups() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![
    fake_model("/m/x/qwen.gguf", "/m/x"),
    fake_model("/m/y/phi.gguf", "/m/y"),
  ];
  app.favorites = vec![PathBuf::from("/m/x/qwen.gguf")];
  let frame = render_to_string(&mut app, 120, 20);
  let fav_pos = frame.find("Favorites").expect("favorites header rendered");
  let dir_pos = frame.find("/m/y").expect("directory header rendered");
  assert!(
    fav_pos < dir_pos,
    "favorites must render above directory groups (fav={fav_pos}, dir={dir_pos})"
  );
  // The ★ glyph is also surfaced on the favorited row.
  assert!(frame.contains("★"));
}

#[test]
fn slash_opens_filter_and_keystrokes_extend_buffer() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![
    fake_model("/m/qwen.gguf", "/m"),
    fake_model("/m/phi.gguf", "/m"),
  ];
  pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
  for ch in "qwen".chars() {
    pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
  }
  assert_eq!(app.filter_buffer, "qwen");
  let frame = render_to_string(&mut app, 100, 20);
  assert!(
    frame.contains("qwen"),
    "filtered row remains visible: {frame}"
  );
  assert!(
    !frame.contains("phi"),
    "phi row must be filtered out: {frame}"
  );
}

#[test]
fn enter_on_model_opens_launch_picker_overlay() {
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/qwen.gguf", "/m")];
  // Snap to the first model row (skipping the header).
  app.go_top();
  pump_input(&mut app, key(KeyCode::Enter, KeyModifiers::NONE));
  let frame = render_to_string(&mut app, 120, 24);
  assert!(
    frame.contains("Launch · qwen"),
    "launch picker title missing: {frame}"
  );
  assert!(frame.contains("Context"));
  assert!(frame.contains("Reasoning"));
  assert!(frame.contains("Advanced"));
}

#[test]
fn theme_cycle_swaps_palette_without_restart() {
  let mut app = App::new(AppOptions {
    theme: ThemeName::Macchiato,
  });
  pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
  assert_ne!(app.options.theme, ThemeName::Macchiato);
  // Render still produces a coherent frame with the new theme.
  let frame = render_to_string(&mut app, 80, 12);
  assert!(frame.contains("llamatui"));
}

#[test]
fn narrow_terminal_does_not_crash_render() {
  // Plan edge case: terminal width 60 cols → renderer must
  // tolerate the constraint without panicking. We don't pin the
  // exact truncation strategy here — just that the call succeeds.
  let mut app = App::new(AppOptions::default());
  app.models = vec![fake_model("/m/very-long-model-name-2.5b-Q4_K_M.gguf", "/m")];
  let frame = render_to_string(&mut app, 60, 12);
  assert!(frame.contains("llamatui"));
}
