//! TUI rendering: sidebar, transcript, activity pane, message box, status bar and the permission dialog.
use crate::app::{Action, App, Entry, Role, Status};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::sync::atomic::Ordering;

const ACTIVITY_HEIGHT: u16 = 7;
const INPUT_HEIGHT: u16 = 3;
const SIDEBAR_PADDING: u16 = 6;
const AUTO_APPROVE_WIDTH: u16 = 22;
const DIALOG_MAX_WIDTH: u16 = 72;
const MAX_PICKER_ROWS: usize = 15;

/// Draws the whole screen and records click targets into `app.hitboxes`.
pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hitboxes.clear();
    let area = frame.area();
    let [main, status_bar] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let sidebar_width = app
        .chats
        .iter()
        .map(|c| c.name.len() as u16)
        .max()
        .unwrap_or(0)
        + SIDEBAR_PADDING;
    let [sidebar, content] =
        Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(10)]).areas(main);
    let [transcript, activity, input] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(ACTIVITY_HEIGHT),
        Constraint::Length(INPUT_HEIGHT),
    ])
    .areas(content);

    draw_sidebar(frame, app, sidebar);
    draw_transcript(frame, app, transcript);
    draw_activity(frame, app, activity);
    draw_input(frame, app, input);
    draw_status_bar(frame, app, status_bar);
    if app.picker.is_some() {
        draw_picker_dialog(frame, app, area);
    }
    // Permission prompts are urgent (an agent is blocked), so they sit on top of the picker.
    if !app.permissions.is_empty() {
        draw_permission_dialog(frame, app, area);
    }
}

/// Agent list; each row is clickable.
fn draw_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title(" agents ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    for (index, chat) in app.chats.iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1);
        if row.y >= inner.bottom() {
            break;
        }
        let marker = if index == app.selected { ">" } else { " " };
        let style = if index == app.selected {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        let line = Line::from(vec![
            Span::styled("●", Style::default().fg(status_color(chat.status))),
            Span::styled(format!(" {marker}{}", chat.name), style),
        ]);
        frame.render_widget(Paragraph::new(line), row);
        app.hitboxes.push((row, Action::Select(index)));
    }
}

/// Dot color per connection state.
fn status_color(status: Status) -> Color {
    match status {
        Status::Offline => Color::DarkGray,
        Status::Connecting | Status::Busy => Color::Yellow,
        Status::Idle => Color::Green,
    }
}

/// The selected agent's transcript, bottom-anchored and scrollable.
fn draw_transcript(frame: &mut Frame, app: &mut App, area: Rect) {
    let chat = &app.chats[app.selected];
    let title = format!(" {} - {} ", chat.name, status_label(chat.status));
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines: Vec<Line> = chat
        .entries
        .iter()
        .flat_map(|entry| entry_lines(entry, inner.width as usize))
        .collect();
    let height = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(height);
    let chat = &mut app.chats[app.selected];
    chat.scroll_up = chat.scroll_up.min(max_scroll);
    let start = max_scroll - chat.scroll_up;
    let visible: Vec<Line> = lines.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

/// Human-readable status for the transcript title.
fn status_label(status: Status) -> &'static str {
    match status {
        Status::Offline => "not running (send a message to start)",
        Status::Connecting => "connecting",
        Status::Idle => "ready",
        Status::Busy => "working",
    }
}

/// Wraps one transcript entry to `width` columns, with a role prefix on its first row.
fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let (prefix, style) = match entry.role {
        Role::User => (
            "you> ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Role::Agent => ("", Style::default()),
        Role::Tool => ("  tool: ", Style::default().fg(Color::Yellow)),
        Role::Image => ("  ", Style::default().fg(Color::Green)),
        Role::Error => ("error: ", Style::default().fg(Color::Red)),
    };
    let indent = " ".repeat(prefix.chars().count());
    let body_width = width.saturating_sub(prefix.chars().count()).max(1);
    let mut rows = Vec::new();
    for paragraph in entry.text.split('\n') {
        rows.extend(wrap(paragraph, body_width));
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let lead = if index == 0 { prefix } else { indent.as_str() };
            Line::from(Span::styled(format!("{lead}{row}"), style))
        })
        .collect()
}

/// Greedy word wrap to `width` chars; words longer than a row are split. Always returns at least one row.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut row = String::new();
    for word in text.split(' ') {
        let mut word = word.to_owned();
        let needed = if row.is_empty() { 0 } else { 1 } + word.chars().count();
        if row.chars().count() + needed > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        while word.chars().count() > width {
            let split = word
                .char_indices()
                .nth(width)
                .map_or(word.len(), |(i, _)| i);
            let rest = word.split_off(split);
            rows.push(std::mem::replace(&mut word, rest));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(&word);
    }
    rows.push(row);
    rows
}

/// The A2A delegation log: who asked whom, and tool use of delegated agents.
fn draw_activity(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" A2A activity ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = inner.height as usize;
    let lines: Vec<Line> = app
        .activity
        .iter()
        .skip(app.activity.len().saturating_sub(height))
        .map(|line| {
            Line::from(Span::styled(
                line.clone(),
                Style::default().fg(Color::Magenta),
            ))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The message box, scrolled horizontally so the cursor stays visible.
fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let title = format!(" message to {} (Enter sends) ", app.chat().name);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible_width = (inner.width as usize).saturating_sub(1).max(1);
    let start = app.cursor.saturating_sub(visible_width);
    let shown: String = app.input.iter().skip(start).take(visible_width).collect();
    frame.render_widget(Paragraph::new(shown), inner);
    if app.permissions.is_empty() && app.picker.is_none() {
        frame.set_cursor_position((inner.x + (app.cursor - start) as u16, inner.y));
    }
}

/// Bottom bar: clickable auto-approve switch and key hints.
fn draw_status_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let on = app.auto_allow.load(Ordering::Relaxed);
    let (label, color) = if on {
        ("auto-approve: ON ", Color::Red)
    } else {
        ("auto-approve: off", Color::Green)
    };
    let switch = Rect::new(area.x, area.y, AUTO_APPROVE_WIDTH.min(area.width), 1);
    frame.render_widget(
        Paragraph::new(format!(" {label}")).style(Style::default().bg(color).fg(Color::Black)),
        switch,
    );
    app.hitboxes.push((switch, Action::ToggleAutoApprove));
    let hints = Rect::new(
        switch.right(),
        area.y,
        area.width.saturating_sub(switch.width),
        1,
    );
    let text = " click agent/switch | Tab: next agent | wheel/PgUp/PgDn: scroll | Ctrl+C: quit";
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        hints,
    );
}

/// Modal listing the oldest permission request with clickable choices.
fn draw_permission_dialog(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(request) = app.permissions.front() else {
        return;
    };
    let mut rows: Vec<(String, Action)> = request
        .options
        .iter()
        .enumerate()
        .map(|(i, option)| (format!(" [{}] {} ", i + 1, option.label), Action::Choose(i)))
        .collect();
    rows.push((" [Esc] cancel ".to_owned(), Action::CancelPermission));
    let title = format!(" {} wants permission ", request.agent);
    let heading = request.title.clone();
    draw_dialog(frame, app, area, &title, &heading, rows);
}

/// Modal listing past sessions of an agent; digits 1-9 or a click reopens one.
fn draw_picker_dialog(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(picker) = app.picker.as_ref() else {
        return;
    };
    let mut rows: Vec<(String, Action)> = picker
        .sessions
        .iter()
        .take(MAX_PICKER_ROWS)
        .enumerate()
        .map(|(i, session)| {
            let title = session.title.as_deref().unwrap_or(&session.id);
            let when = session
                .updated_at
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(16)
                .collect::<String>();
            (
                format!(" [{}] {}  {}", i + 1, when, title),
                Action::PickSession(i),
            )
        })
        .collect();
    rows.push((" [Esc] close ".to_owned(), Action::ClosePicker));
    let title = format!(" resume a {} session ", app.chats[picker.agent].name);
    draw_dialog(
        frame,
        app,
        area,
        &title,
        "Pick a conversation to continue:",
        rows,
    );
}

/// Draws a centered modal with a heading and one clickable row per `(label, action)`.
/// The dialog replaces all other click targets, so nothing behind it can be hit.
fn draw_dialog(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    title: &str,
    heading: &str,
    rows: Vec<(String, Action)>,
) {
    let width = area.width.saturating_sub(4).min(DIALOG_MAX_WIDTH);
    let height = (rows.len() as u16 + 4).min(area.height);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(title.to_owned());
    let inner = block.inner(dialog);
    frame.render_widget(Clear, dialog);
    frame.render_widget(block, dialog);

    let heading_row = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(heading.to_owned()).style(Style::default().add_modifier(Modifier::BOLD)),
        heading_row,
    );
    app.hitboxes.clear();
    let button = Style::default().fg(Color::Black).bg(Color::Gray);
    for (offset, (label, action)) in rows.into_iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + 2 + offset as u16, inner.width, 1);
        if row.bottom() > inner.bottom() {
            break;
        }
        frame.render_widget(Paragraph::new(label).style(button), row);
        app.hitboxes.push((row, action));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Env;
    use crate::config::Config;
    use crate::event::{PermissionOption, PermissionRequest, UiEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, mpsc};

    /// An app with no agents configured, for rendering tests.
    fn app() -> App {
        let env = Env {
            config: Arc::new(Config::default()),
            exe: PathBuf::from("central"),
            cwd: PathBuf::from("."),
            a2a_url: String::new(),
            token: String::new(),
            ui: mpsc::channel().0,
        };
        App::new(env, Arc::new(AtomicBool::new(false)))
    }

    /// Renders one frame to an 80x24 test buffer and returns its text.
    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal.backend().to_string()
    }

    // The default screen lists every agent and records one click target per agent plus the switch.
    #[test]
    fn draws_agents_and_hitboxes() {
        let mut app = app();
        let text = screen(&mut app);
        assert!(
            text.contains("codex") && text.contains("antigravity") && text.contains("A2A activity")
        );
        assert_eq!(app.hitboxes.len(), app.chats.len() + 1);
    }

    // While a permission prompt is open only its buttons are clickable.
    #[test]
    fn dialog_replaces_hitboxes() {
        let mut app = app();
        let options = vec![PermissionOption {
            id: "o".into(),
            label: "Allow once".into(),
        }];
        app.apply(UiEvent::Permission(PermissionRequest {
            agent: "codex".into(),
            title: "run ls".into(),
            options,
            reply: mpsc::channel().0,
        }));
        let text = screen(&mut app);
        assert!(text.contains("Allow once") && text.contains("wants permission"));
        assert_eq!(app.hitboxes.len(), 2);
    }

    // The resume picker lists sessions, makes them clickable, and a click reopens the chosen one.
    #[test]
    fn picker_lists_sessions_and_click_resumes() {
        use crate::acp_client::{AgentEvent, SessionInfo};
        use crate::worker::Command;
        let mut app = app();
        let (worker, inbox) = mpsc::channel();
        app.chats[0].worker = Some(worker);
        let sessions = vec![
            SessionInfo {
                id: "s1".into(),
                title: Some("fix the bug".into()),
                updated_at: Some("2026-11-05T10:00:00Z".into()),
            },
            SessionInfo {
                id: "s2".into(),
                title: None,
                updated_at: None,
            },
        ];
        let agent = app.chats[0].name.clone();
        app.apply(UiEvent::Agent {
            agent,
            event: AgentEvent::Sessions(sessions),
        });
        let text = screen(&mut app);
        assert!(text.contains("fix the bug") && text.contains("resume a"));
        assert_eq!(app.hitboxes.len(), 3);
        let (rect, _) = app.hitboxes[0];
        app.mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: rect.x,
            row: rect.y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        });
        assert!(matches!(inbox.recv().unwrap(), Command::Resume(id) if id == "s1"));
        assert!(app.picker.is_none());
    }

    // Wrapping keeps rows within width and splits over-long words.
    #[test]
    fn wraps_text() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 5), [""]);
    }

    // Scrolling beyond the transcript is clamped, and the newest line shows at scroll 0.
    #[test]
    fn transcript_clamps_scroll_and_shows_latest() {
        let mut app = app();
        for i in 0..50 {
            app.chats[0].entries.push(Entry {
                role: Role::Agent,
                text: format!("line {i}"),
            });
        }
        app.chats[0].scroll_up = 10_000;
        let text = screen(&mut app);
        assert!(text.contains("line 0") && !text.contains("line 49"));
        assert!(app.chats[0].scroll_up < 10_000);
        app.chats[0].scroll_up = 0;
        assert!(screen(&mut app).contains("line 49"));
    }
}
