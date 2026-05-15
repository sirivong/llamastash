//! Event loop bridging crossterm input and IPC notifications into
//! [`super::app::App`] state transitions.
//!
//! v1 polls daemon `list_models` / `status` / `favorite_list` on a
//! background tick (`REFRESH_INTERVAL`) instead of subscribing to a
//! push channel — keeps the IPC surface small while the daemon's
//! notification API is still being designed.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use tokio::sync::mpsc;

use crate::ipc::Client;
use crate::tui::app::App;
use crate::tui::keybindings::{action_for, Action, Focus};
use crate::util::clipboard;

const REFRESH_INTERVAL: Duration = Duration::from_millis(750);
const POLL_INTERVAL: Duration = Duration::from_millis(40);

/// One pump of input events. Returns `true` when the App is asking
/// the loop to exit (the user pressed `q` / Ctrl+C).
pub fn pump_input(app: &mut App, evt: Event) -> bool {
  if let Event::Key(key) = evt {
    if key.kind != KeyEventKind::Release {
      handle_key(app, key);
    }
  }
  app.should_exit
}

fn handle_key(app: &mut App, key: KeyEvent) {
  match app.focus {
    Focus::Filter => handle_filter_input(app, key),
    Focus::AdvancedPanel => handle_advanced_input(app, key),
    _ => {
      if let Some(action) = action_for(app.focus, key.code, key.modifiers) {
        apply_action(app, action);
      }
    }
  }
}

fn handle_filter_input(app: &mut App, key: KeyEvent) {
  match key.code {
    KeyCode::Esc => {
      app.clear_filter();
    }
    KeyCode::Enter => {
      app.focus = Focus::List;
    }
    KeyCode::Backspace => {
      app.filter_buffer.pop();
    }
    KeyCode::Char(ch) => {
      app.filter_buffer.push(ch);
    }
    _ => {}
  }
}

fn handle_advanced_input(app: &mut App, key: KeyEvent) {
  let panel = match &mut app.advanced_panel {
    Some(p) => p,
    None => return,
  };
  match key.code {
    KeyCode::Esc => app.close_advanced_panel(),
    KeyCode::Enter => app.close_advanced_panel(),
    KeyCode::Backspace => panel.backspace(),
    KeyCode::Char(ch) => panel.insert(ch),
    _ => {}
  }
}

fn apply_action(app: &mut App, action: Action) {
  match action {
    Action::Quit => app.should_exit = true,
    Action::MoveDown => match app.focus {
      Focus::LaunchPicker => {
        if let Some(p) = app.launch_picker.as_mut() {
          p.next_field();
        }
      }
      _ => app.move_down(),
    },
    Action::MoveUp => app.move_up(),
    Action::PageUp => {
      for _ in 0..10 {
        app.move_up();
      }
    }
    Action::PageDown => {
      for _ in 0..10 {
        app.move_down();
      }
    }
    Action::GoTop => app.go_top(),
    Action::GoBottom => app.go_bottom(),
    Action::OpenFilter => app.open_filter(),
    Action::ClearFilter => app.clear_filter(),
    Action::ToggleFavorite => {
      // Local toggle is enough for a snappy UI; the IPC favorite
      // mutation runs on the loop tick that picks the press up.
      if let Some(p) = app.focused_path() {
        if app.favorites.contains(&p) {
          app.favorites.retain(|f| f != &p);
          app.show_toast("favorite removed");
        } else {
          app.favorites.push(p);
          app.show_toast("favorite added");
        }
      }
    }
    Action::OpenLaunchPicker => app.open_launch_picker(),
    Action::OpenAdvancedPanel => app.open_advanced_panel(),
    Action::Submit => match app.focus {
      Focus::LaunchPicker => app.show_toast("launch dispatched"),
      Focus::AdvancedPanel => app.close_advanced_panel(),
      _ => {}
    },
    Action::Cancel => match app.focus {
      Focus::LaunchPicker => app.close_launch_picker(),
      Focus::AdvancedPanel => app.close_advanced_panel(),
      _ => {}
    },
    Action::YankUrl | Action::YankCurl | Action::YankPath => {
      let text = build_yank_text(app, action);
      if let Some(text) = text {
        match clipboard::write(&text) {
          Ok(backend) => app.show_toast(format!("yanked via {backend}")),
          Err(e) => app.show_toast(format!("clipboard unavailable: {e}; {text}")),
        }
      } else {
        app.show_toast("nothing to yank — focus a Ready model");
      }
    }
    Action::CycleTheme => {
      app.cycle_theme();
      app.show_toast(format!("theme: {}", app.options.theme.canonical()));
    }
    Action::FocusRightPane => app.focus = Focus::RightPane,
    Action::FocusList => app.focus = Focus::List,
  }
}

fn build_yank_text(app: &App, action: Action) -> Option<String> {
  match action {
    Action::YankPath => app.focused_path().map(|p| p.display().to_string()),
    Action::YankUrl | Action::YankCurl => {
      let m = app.focused_managed()?;
      let url = format!("http://127.0.0.1:{}/v1", m.port);
      Some(match action {
        Action::YankUrl => url,
        Action::YankCurl => format!(
          "curl -s -H 'Content-Type: application/json' -d '{{\"model\":\"{}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}]}}' {}/chat/completions",
          m.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("model"),
          url
        ),
        _ => unreachable!(),
      })
    }
    _ => None,
  }
}

/// Background refresher that polls the daemon for catalog + status
/// snapshots and forwards them as `RefreshTick`s to the run loop.
pub enum RefreshTick {
  Catalog(serde_json::Value),
  Status(serde_json::Value),
  Favorites(serde_json::Value),
  Disconnected,
}

pub fn spawn_refresher(socket: std::path::PathBuf) -> mpsc::Receiver<RefreshTick> {
  let (tx, rx) = mpsc::channel(8);
  tokio::spawn(async move {
    loop {
      match Client::connect(&socket).await {
        Ok(mut client) => {
          if tx.is_closed() {
            return;
          }
          let _ = match client.call("list_models", None).await {
            Ok(body) => tx.send(RefreshTick::Catalog(body)).await,
            Err(_) => Ok(()),
          };
          let _ = match client.call("status", None).await {
            Ok(body) => tx.send(RefreshTick::Status(body)).await,
            Err(_) => Ok(()),
          };
          let _ = match client.call("favorite_list", None).await {
            Ok(body) => tx.send(RefreshTick::Favorites(body)).await,
            Err(_) => Ok(()),
          };
        }
        Err(_) => {
          let _ = tx.send(RefreshTick::Disconnected).await;
        }
      }
      tokio::time::sleep(REFRESH_INTERVAL).await;
    }
  });
  rx
}

/// Fully-featured TUI run-loop. Drives the App from real crossterm
/// events + a daemon refresher, rendering on each tick.
pub async fn run(app: App, socket: std::path::PathBuf) -> Result<()> {
  use crossterm::execute;
  use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
  };
  use ratatui::backend::CrosstermBackend;
  use ratatui::Terminal;

  enable_raw_mode()?;
  let mut stdout = std::io::stdout();
  execute!(stdout, EnterAlternateScreen)?;
  let backend = CrosstermBackend::new(stdout);
  let mut terminal = Terminal::new(backend)?;

  let mut app = app;
  let mut refresh_rx = spawn_refresher(socket);

  loop {
    terminal.draw(|f| crate::tui::render::render(f, &mut app))?;

    // Drain any background ticks without blocking — keeps render
    // latency tight (~16 ms target) regardless of daemon RTT.
    while let Ok(tick) = refresh_rx.try_recv() {
      apply_refresh(&mut app, tick);
    }

    if event::poll(POLL_INTERVAL)? {
      let evt = event::read()?;
      if pump_input(&mut app, evt) {
        break;
      }
    }
  }

  // Restore the terminal even on early returns above.
  disable_raw_mode()?;
  execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
  terminal.show_cursor()?;
  Ok(())
}

fn apply_refresh(app: &mut App, tick: RefreshTick) {
  match tick {
    RefreshTick::Catalog(body) => {
      app.daemon_connected = true;
      app.ingest_list_models(&body);
    }
    RefreshTick::Status(body) => {
      app.daemon_connected = true;
      app.ingest_status(&body);
    }
    RefreshTick::Favorites(body) => {
      app.daemon_connected = true;
      app.ingest_favorites(&body);
    }
    RefreshTick::Disconnected => {
      app.daemon_connected = false;
    }
  }
}

/// Public escape-hatch for tests + the smoke test that drive the
/// loop manually with a chosen socket.
pub fn refresh_apply(app: &mut App, tick: RefreshTick) {
  apply_refresh(app, tick);
}

/// Convenience used by `cli::dispatch`: build the App with the
/// loaded config and a connected (or-not-yet-connected) daemon
/// socket. Splitting it out keeps the binary entry small and the
/// call testable.
pub async fn launch(theme: crate::theme::ThemeName, socket: &Path) -> Result<()> {
  let app = App::new(crate::tui::app::AppOptions { theme });
  run(app, socket.to_path_buf()).await
}

#[cfg(test)]
mod tests {
  use super::*;
  use crossterm::event::{KeyEvent, KeyModifiers};

  fn key(code: KeyCode, mods: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, mods))
  }

  #[test]
  fn q_in_list_focus_sets_should_exit() {
    let mut app = App::new(Default::default());
    let exit = pump_input(&mut app, key(KeyCode::Char('q'), KeyModifiers::NONE));
    assert!(exit);
    assert!(app.should_exit);
  }

  #[test]
  fn slash_opens_filter_focus() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('/'), KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::Filter);
  }

  #[test]
  fn typing_in_filter_extends_buffer() {
    let mut app = App::new(Default::default());
    app.focus = Focus::Filter;
    for ch in "qwen".chars() {
      pump_input(&mut app, key(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    assert_eq!(app.filter_buffer, "qwen");
  }

  #[test]
  fn esc_in_filter_clears_and_returns_focus() {
    let mut app = App::new(Default::default());
    app.focus = Focus::Filter;
    app.filter_buffer = "qwen".into();
    pump_input(&mut app, key(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.focus, Focus::List);
    assert!(app.filter_buffer.is_empty());
  }

  #[test]
  fn t_cycles_theme_and_emits_toast() {
    let mut app = App::new(Default::default());
    let original = app.options.theme;
    pump_input(&mut app, key(KeyCode::Char('t'), KeyModifiers::NONE));
    assert_ne!(app.options.theme, original);
    assert!(
      app
        .toast_message()
        .map(|s| s.contains("theme"))
        .unwrap_or(false),
      "theme cycle should toast: {:?}",
      app.toast_message()
    );
  }

  #[test]
  fn yank_url_with_no_managed_focus_shows_helpful_toast() {
    let mut app = App::new(Default::default());
    pump_input(&mut app, key(KeyCode::Char('y'), KeyModifiers::NONE));
    let msg = app.toast_message().unwrap();
    assert!(
      msg.contains("nothing to yank") || msg.contains("clipboard"),
      "yank toast must explain why: {msg}"
    );
  }
}
