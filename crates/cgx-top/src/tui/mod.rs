pub mod app;
pub mod ui;

use app::AppState;
use cgx_core::FrdModel;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use std::path::Path;
use std::time::Duration;

pub fn run_tui(
    model: FrdModel,
    file_path: &Path,
    initial_set: Option<&str>,
) -> anyhow::Result<()> {
    let filename = file_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("model.frd")
        .to_string();

    let file_size_kb = std::fs::metadata(file_path)
        .map(|m| m.len() as f64 / 1024.0)
        .unwrap_or(0.0);

    let mut app = AppState::new(model, filename, file_size_kb, initial_set);

    // Initialize terminal using Ratatui 0.29
    let mut terminal = ratatui::init();

    let res = run_loop(&mut terminal, &mut app);

    // Clean up and restore terminal
    ratatui::restore();

    res
}

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut AppState,
) -> anyhow::Result<()> {
    while !app.should_quit {
        terminal.draw(|f| ui::draw(f, app))?;

        // Poll for keyboard events with 50ms timeout for responsive UI
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if app.show_help_modal {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter | KeyCode::Char(' ') => {
                            app.show_help_modal = false;
                        }
                        _ => {}
                    }
                } else if app.show_set_modal {
                    match key.code {
                        KeyCode::Esc => {
                            app.show_set_modal = false;
                        }
                        KeyCode::Up => {
                            let list = app.filtered_set_list();
                            if !list.is_empty() {
                                if app.set_modal_cursor == 0 {
                                    app.set_modal_cursor = list.len() - 1;
                                } else {
                                    app.set_modal_cursor -= 1;
                                }
                            }
                        }
                        KeyCode::Down => {
                            let list = app.filtered_set_list();
                            if !list.is_empty() {
                                app.set_modal_cursor = (app.set_modal_cursor + 1) % list.len();
                            }
                        }
                        KeyCode::Enter => {
                            let list = app.filtered_set_list();
                            if let Some(&(orig_idx, _)) = list.get(app.set_modal_cursor) {
                                app.update_active_set(orig_idx);
                            }
                            app.show_set_modal = false;
                        }
                        KeyCode::Backspace => {
                            app.set_search_query.pop();
                            app.set_modal_cursor = 0;
                        }
                        KeyCode::Char(c) => {
                            app.set_search_query.push(c);
                            app.set_modal_cursor = 0;
                        }
                        _ => {}
                    }
                } else {
                    // Normal dashboard navigation
                    match key.code {
                        // Quit
                        KeyCode::Char('q') | KeyCode::Esc => {
                            app.should_quit = true;
                        }

                        // Help
                        KeyCode::Char('?') | KeyCode::Char('h') => {
                            app.show_help_modal = true;
                        }

                        // Step / Mode / Load navigation (W / S or Up / Down)
                        KeyCode::Char('w') | KeyCode::Char('W') | KeyCode::Up => {
                            app.prev_step();
                        }
                        KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Down => {
                            app.next_step();
                        }

                        // Set rotation (A / D or Left / Right)
                        KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Left => {
                            app.prev_set();
                        }
                        KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Right => {
                            app.next_set();
                        }

                        // Instant reset to ALL nodes with key '0'
                        KeyCode::Char('0') => {
                            app.reset_all_nodes();
                        }

                        // Direct jump to steps/modes 1-9
                        KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                            let digit = c.to_digit(10).unwrap() as usize;
                            app.jump_to_step(digit);
                        }

                        // Open Set Selection Modal
                        KeyCode::Char(' ') | KeyCode::Enter => {
                            app.show_set_modal = true;
                            app.set_search_query.clear();
                            app.set_modal_cursor = 0;
                        }

                        // Switch active field view
                        KeyCode::Tab => {
                            app.toggle_field_view();
                        }

                        // Home / End
                        KeyCode::Home | KeyCode::Char('g') => {
                            app.selected_step_idx = 0;
                        }
                        KeyCode::End | KeyCode::Char('G') => {
                            if !app.model.steps.is_empty() {
                                app.selected_step_idx = app.model.steps.len() - 1;
                            }
                        }

                        // Page Up / Down (jump 5 steps)
                        KeyCode::PageUp => {
                            app.selected_step_idx = app.selected_step_idx.saturating_sub(5);
                        }
                        KeyCode::PageDown => {
                            if !app.model.steps.is_empty() {
                                app.selected_step_idx = (app.selected_step_idx + 5).min(app.model.steps.len() - 1);
                            }
                        }

                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}
