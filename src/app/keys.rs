use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, AppMode, NewSessionState, PanelFocus, key_event_to_bytes};
use crate::setup;

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            match &self.mode {
                AppMode::SessionView(_) | AppMode::SessionPicker(_) => {}
                _ => {
                    self.should_quit = true;
                    return;
                }
            }
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
            self.toggle_mouse_capture = true;
            return;
        }

        if key.code == KeyCode::Char('t') && matches!(self.mode, AppMode::Dashboard) {
            let next = self.theme.name.next();
            self.theme = crate::ui::theme::Theme::new(next);
            self.status_message = Some(format!("Theme: {}", next.label()));
            return;
        }

        match &self.mode {
            AppMode::Dashboard => self.handle_dashboard_key(key),
            AppMode::SessionView(id) => {
                let id = *id;
                self.handle_session_view_key(key, id);
            }
            AppMode::SessionPicker(from_id) => {
                let from_id = *from_id;
                self.handle_session_picker_key(key, from_id);
            }
            AppMode::Editor => self.handle_editor_key(key),
            AppMode::RenamePrompt => self.handle_rename_key(key),
            AppMode::NewSessionModal => self.handle_new_session_modal_key(key),
            AppMode::SendFilePrompt => self.handle_send_file_key(key),
            AppMode::Setup => self.handle_setup_key(key),
            AppMode::QuitConfirm => self.handle_quit_confirm_key(key),
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        if self.show_help {
            if key.code == KeyCode::Esc || key.code == KeyCode::Char('?') {
                self.show_help = false;
            }
            return;
        }

        if key.code == KeyCode::Tab {
            self.focus = match self.focus {
                PanelFocus::FileTree => PanelFocus::SessionList,
                PanelFocus::SessionList => PanelFocus::FileTree,
            };
            if self.focus == PanelFocus::FileTree
                && let Some(session) = self.sessions.selected()
            {
                let dir = session.working_dir.clone();
                if dir != self.file_tree.root.path {
                    self.file_tree.set_root(dir);
                }
            }
            return;
        }

        match self.focus {
            PanelFocus::SessionList => self.handle_session_list_key(key),
            PanelFocus::FileTree => self.handle_file_tree_key(key),
        }
    }

    fn handle_session_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.mode = AppMode::QuitConfirm,
            KeyCode::Down => {
                if !self.sessions.is_empty() {
                    self.sessions.select_next();
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Up => {
                if !self.sessions.is_empty() {
                    self.sessions.select_prev();
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Enter => {
                let inner_rows = self.terminal_rows.saturating_sub(3);
                let inner_cols = self.terminal_cols.saturating_sub(2);
                if let Some(session) = self.sessions.selected_mut() {
                    let id = session.id;
                    session.try_resize(inner_cols, inner_rows);
                    self.session_view_scroll = 0;
                    self.user_scrolled = false;
                    self.mode = AppMode::SessionView(id);
                }
            }
            KeyCode::Char('n') => {
                self.new_session = Some(NewSessionState::new());
                self.mode = AppMode::NewSessionModal;
            }
            KeyCode::Char('a') => self.approve_selected(),
            KeyCode::Char('d') => self.deny_selected(),
            KeyCode::Char('r') => {
                if let Some(session) = self.sessions.selected() {
                    self.input_buffer = session.label.clone();
                    self.mode = AppMode::RenamePrompt;
                }
            }
            KeyCode::Char('K') => self.kill_selected(),
            KeyCode::Char('c') => self.send_commit_prompt(),
            KeyCode::Char('x') => self.clear_dead_sessions(),
            KeyCode::Char('S') => {
                self.setup_items = setup::missing_items();
                self.setup_selected = 0;
                self.mode = AppMode::Setup;
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            _ => {}
        }
    }

    fn handle_file_tree_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.mode = AppMode::QuitConfirm,
            KeyCode::Down => {
                self.file_tree.move_down();
                self.adjust_file_tree_scroll();
            }
            KeyCode::Up => {
                self.file_tree.move_up();
                self.adjust_file_tree_scroll();
            }
            KeyCode::Enter | KeyCode::Right => {
                if let Some(path) = self.file_tree.selected_path()
                    && path.is_dir()
                {
                    self.file_tree.toggle_selected();
                }
            }
            KeyCode::Left => {
                if let Some(path) = self.file_tree.selected_path()
                    && path.is_dir()
                {
                    self.file_tree.toggle_selected();
                }
            }
            KeyCode::Char('n') => {
                if let Some(path) = self.file_tree.selected_path() {
                    let dir = if path.is_dir() {
                        path.to_path_buf()
                    } else {
                        path.parent().unwrap_or(path).to_path_buf()
                    };
                    self.new_session = Some(NewSessionState::with_dir(dir.display().to_string()));
                    self.mode = AppMode::NewSessionModal;
                    self.focus = PanelFocus::SessionList;
                }
            }
            KeyCode::Char('R') => {
                self.file_tree.refresh();
            }
            KeyCode::Char('e') => {
                if let Some(path) = self.file_tree.selected_path()
                    && path.is_file()
                {
                    self.open_editor(path.to_path_buf());
                }
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            _ => {}
        }
    }

    fn handle_editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Char('s') if ctrl => {
                if let Some(editor) = &mut self.editor
                    && let Err(e) = editor.save()
                {
                    editor.message = Some(format!("Save failed: {e}"));
                }
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.editor = None;
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Char('p') if ctrl => {
                if !self.sessions.is_empty() {
                    self.mode = AppMode::SendFilePrompt;
                } else if let Some(editor) = &mut self.editor {
                    editor.message = Some("No sessions to send to.".to_string());
                }
            }
            KeyCode::Up => {
                if let Some(editor) = &mut self.editor {
                    editor.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(editor) = &mut self.editor {
                    editor.move_down();
                }
            }
            KeyCode::Left => {
                if let Some(editor) = &mut self.editor {
                    editor.move_left();
                }
            }
            KeyCode::Right => {
                if let Some(editor) = &mut self.editor {
                    editor.move_right();
                }
            }
            KeyCode::Home => {
                if let Some(editor) = &mut self.editor {
                    editor.move_home();
                }
            }
            KeyCode::End => {
                if let Some(editor) = &mut self.editor {
                    editor.move_end();
                }
            }
            KeyCode::PageUp => {
                if let Some(editor) = &mut self.editor {
                    let page = self.terminal_rows.saturating_sub(4) as usize;
                    editor.page_up(page);
                }
            }
            KeyCode::PageDown => {
                if let Some(editor) = &mut self.editor {
                    let page = self.terminal_rows.saturating_sub(4) as usize;
                    editor.page_down(page);
                }
            }
            KeyCode::Enter => {
                if let Some(editor) = &mut self.editor {
                    editor.insert_newline();
                }
            }
            KeyCode::Backspace => {
                if let Some(editor) = &mut self.editor {
                    editor.backspace();
                }
            }
            KeyCode::Delete => {
                if let Some(editor) = &mut self.editor {
                    editor.delete();
                }
            }
            KeyCode::Tab => {
                if let Some(editor) = &mut self.editor {
                    for _ in 0..4 {
                        editor.insert_char(' ');
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(editor) = &mut self.editor {
                    editor.insert_char(c);
                }
            }
            _ => {}
        }

        if let Some(editor) = &mut self.editor {
            let visible = self.terminal_rows.saturating_sub(4) as usize;
            editor.ensure_cursor_visible(visible);
        }
    }

    fn handle_send_file_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Editor;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap() as usize;
                self.send_file_to_session(idx);
                self.mode = AppMode::Editor;
            }
            KeyCode::Enter => {
                if !self.sessions.is_empty() {
                    self.send_file_to_session(0);
                }
                self.mode = AppMode::Editor;
            }
            _ => {}
        }
    }

    fn handle_session_view_key(&mut self, key: KeyEvent, session_id: usize) {
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('d') {
            self.mode = AppMode::Dashboard;
            return;
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('s') {
            if self.sessions.len() > 1 {
                self.picker_selected = self
                    .sessions
                    .iter()
                    .position(|s| s.id == session_id)
                    .unwrap_or(0);
                self.mode = AppMode::SessionPicker(session_id);
            }
            return;
        }

        let bytes = key_event_to_bytes(&key);
        if !bytes.is_empty()
            && let Some(session) = self.sessions.get_mut(session_id)
        {
            session.try_write(&bytes);
        }
    }

    fn handle_session_picker_key(&mut self, key: KeyEvent, from_session_id: usize) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::SessionView(from_session_id);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.sessions.is_empty() {
                    self.picker_selected = (self.picker_selected + 1) % self.sessions.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.sessions.is_empty() {
                    self.picker_selected = self
                        .picker_selected
                        .checked_sub(1)
                        .unwrap_or(self.sessions.len() - 1);
                }
            }
            KeyCode::Enter => {
                let picker_idx = self.picker_selected;
                let id = self.sessions.iter().nth(picker_idx).map(|s| s.id);
                if let Some(id) = id {
                    if let Some(session) = self.sessions.get_mut(id) {
                        let inner_rows = self.terminal_rows.saturating_sub(3);
                        let inner_cols = self.terminal_cols.saturating_sub(2);
                        session.try_resize(inner_cols, inner_rows);
                    }
                    self.sessions.set_selected(picker_idx);
                    self.mode = AppMode::SessionView(id);
                }
            }
            _ => {}
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if !self.input_buffer.is_empty()
                    && let Some(session) = self.sessions.selected_mut()
                {
                    session.label = self.input_buffer.clone();
                }
                self.input_buffer.clear();
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Esc => {
                self.input_buffer.clear();
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
    }

    fn handle_new_session_modal_key(&mut self, key: KeyEvent) {
        let focused = match &self.new_session {
            Some(s) => s.focused,
            None => return,
        };

        match key.code {
            KeyCode::Esc => {
                self.new_session = None;
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Enter => {
                self.spawn_from_modal();
            }
            KeyCode::Up => {
                if let Some(state) = &mut self.new_session
                    && state.focused > 0
                {
                    state.focused -= 1;
                    state.status_message = None;
                }
            }
            KeyCode::Down => {
                if let Some(state) = &mut self.new_session
                    && state.focused < state.field_count() - 1
                {
                    state.focused += 1;
                    state.status_message = None;
                }
            }
            KeyCode::Tab if focused == 1 => {
                self.tab_complete_path();
            }
            KeyCode::Left | KeyCode::Right if focused == 0 => {
                if let Some(state) = &mut self.new_session {
                    state.kind = state.kind.toggle();
                    state.status_message = None;
                }
            }
            KeyCode::Char(' ') if focused == 0 => {
                if let Some(state) = &mut self.new_session {
                    state.kind = state.kind.toggle();
                    state.status_message = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(state) = &mut self.new_session {
                    match focused {
                        1 => {
                            state.dir_input.pop();
                        }
                        2 => {
                            state.flags_input.pop();
                        }
                        _ => {}
                    }
                    state.status_message = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(state) = &mut self.new_session {
                    match focused {
                        1 => state.dir_input.push(c),
                        2 => state.flags_input.push(c),
                        _ => {}
                    }
                    state.status_message = None;
                }
            }
            _ => {}
        }
    }

    fn handle_quit_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.should_quit = true;
            }
            _ => {
                self.mode = AppMode::Dashboard;
            }
        }
    }

    fn handle_setup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.setup_banner_dismissed = true;
                setup::mark_initialized();
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Up => {
                self.setup_selected = self.setup_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if !self.setup_items.is_empty() {
                    self.setup_selected =
                        (self.setup_selected + 1).min(self.setup_items.len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                self.spawn_setup_session();
            }
            _ => {}
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::MouseEventKind;

        let scroll_lines: usize = 3;
        match mouse.kind {
            MouseEventKind::ScrollUp => match &self.mode {
                AppMode::Dashboard => match self.focus {
                    PanelFocus::SessionList => {
                        if !self.sessions.is_empty() {
                            self.sessions.select_up_by(scroll_lines);
                            self.update_file_tree_for_selected();
                        }
                    }
                    PanelFocus::FileTree => {
                        for _ in 0..scroll_lines {
                            self.file_tree.move_up();
                        }
                        self.adjust_file_tree_scroll();
                    }
                },
                AppMode::SessionView(id) => {
                    let id = *id;
                    if let Some(session) = self.sessions.get(id) {
                        let mut parser = crate::session::lock_parser(&session.parser);
                        parser.screen_mut().set_scrollback(usize::MAX);
                        let max_scroll = parser.screen().scrollback();
                        let desired = self.session_view_scroll + scroll_lines;
                        self.session_view_scroll = desired.min(max_scroll);
                        self.user_scrolled = self.session_view_scroll > 0;
                        parser.screen_mut().set_scrollback(0);
                    }
                }
                AppMode::Editor => {
                    if let Some(editor) = &mut self.editor {
                        for _ in 0..scroll_lines {
                            editor.move_up();
                        }
                        let visible = self.terminal_rows.saturating_sub(4) as usize;
                        editor.ensure_cursor_visible(visible);
                    }
                }
                _ => {}
            },
            MouseEventKind::ScrollDown => match &self.mode {
                AppMode::Dashboard => match self.focus {
                    PanelFocus::SessionList => {
                        if !self.sessions.is_empty() {
                            self.sessions.select_down_by(scroll_lines);
                            self.update_file_tree_for_selected();
                        }
                    }
                    PanelFocus::FileTree => {
                        for _ in 0..scroll_lines {
                            self.file_tree.move_down();
                        }
                        self.adjust_file_tree_scroll();
                    }
                },
                AppMode::SessionView(_) => {
                    self.session_view_scroll =
                        self.session_view_scroll.saturating_sub(scroll_lines);
                    self.user_scrolled = self.session_view_scroll > 0;
                }
                AppMode::Editor => {
                    if let Some(editor) = &mut self.editor {
                        for _ in 0..scroll_lines {
                            editor.move_down();
                        }
                        let visible = self.terminal_rows.saturating_sub(4) as usize;
                        editor.ensure_cursor_visible(visible);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}
