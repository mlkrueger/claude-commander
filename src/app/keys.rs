use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, AppMode, NewSessionState, PanelFocus, key_event_to_bytes};
use crate::setup;

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // macOS sends Option+letter as a Unicode character rather than ESC+letter.
        // Normalize the common ones back to ALT+letter so all handlers work correctly.
        let key = normalize_option_key(key);

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
            AppMode::McpConfirm => self.handle_mcp_confirm_key(key),
            AppMode::AttachDriverPicker {
                target_session_id,
                drivers,
                restore_picker_selected,
            } => {
                let target = *target_session_id;
                let driver_count = drivers.len();
                let selected_driver = drivers.get(self.picker_selected).cloned();
                let restore = *restore_picker_selected;
                self.handle_attach_driver_picker_key(
                    key,
                    target,
                    driver_count,
                    selected_driver,
                    restore,
                );
            }
        }
    }

    /// Phase 5: handle `y`/`n`/`Esc` while the MCP confirmation
    /// modal is open. Resolves the pending `ConfirmRequest`'s
    /// oneshot with the user's answer, clears the pending slot,
    /// and returns to the dashboard. Non-matching keys are ignored
    /// so the user can't accidentally bypass the modal.
    fn handle_mcp_confirm_key(&mut self, key: KeyEvent) {
        use crate::mcp::ConfirmResponse;
        let resp = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ConfirmResponse::Allow),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ConfirmResponse::Deny),
            _ => None,
        };
        let Some(resp) = resp else { return };
        if let Some(req) = self.pending_confirm.take() {
            // `send` fails if the MCP handler's oneshot receiver has
            // already been dropped (e.g. the handler's 25s timeout
            // fired first). Nothing to do about it here — we still
            // clear the modal so the UI doesn't wedge.
            let _ = req.resp_tx.send(resp);
        }
        self.mode = AppMode::Dashboard;
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
            if self.focus == PanelFocus::FileTree {
                let dir = self
                    .sessions_lock()
                    .selected()
                    .map(|s| s.working_dir.clone());
                if let Some(dir) = dir
                    && dir != self.file_tree.root.path
                {
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
                let moved = {
                    let mut mgr = self.sessions_lock();
                    if !mgr.is_empty() {
                        mgr.select_next();
                        true
                    } else {
                        false
                    }
                };
                if moved {
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Up => {
                let moved = {
                    let mut mgr = self.sessions_lock();
                    if !mgr.is_empty() {
                        mgr.select_prev();
                        true
                    } else {
                        false
                    }
                };
                if moved {
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Enter => {
                let inner_rows = self.terminal_rows.saturating_sub(3);
                let inner_cols = self.terminal_cols.saturating_sub(2);
                let id = {
                    let mut mgr = self.sessions_lock();
                    mgr.selected_mut().map(|session| {
                        let id = session.id;
                        session.try_resize(inner_cols, inner_rows);
                        id
                    })
                };
                if let Some(id) = id {
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
                let label = self.sessions_lock().selected().map(|s| s.label.clone());
                if let Some(label) = label {
                    self.input_buffer = label;
                    self.mode = AppMode::RenamePrompt;
                }
            }
            KeyCode::Char('f') => self.fork_selected(),
            KeyCode::Char('R') => self.resume_selected(),
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
                let has_any = !self.sessions_lock().is_empty();
                if has_any {
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
                if !self.sessions_lock().is_empty() {
                    self.send_file_to_session(0);
                }
                self.mode = AppMode::Editor;
            }
            _ => {}
        }
    }

    fn handle_session_view_key(&mut self, key: KeyEvent, session_id: usize) {
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('d') {
            // Reset scroll state so the dashboard's detail panel shows live content.
            if self.session_view_scroll > 0 {
                let mgr = self.sessions_lock();
                if let Some(session) = mgr.get(session_id) {
                    let mut parser = crate::session::lock_parser(&session.parser);
                    parser.screen_mut().set_scrollback(0);
                }
            }
            self.session_view_scroll = 0;
            self.user_scrolled = false;
            self.mode = AppMode::Dashboard;
            return;
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('g') {
            // Diagnostic: probe the session's vt100 screen state so we can
            // see whether Claude Code is in alternate-screen mode, what
            // mouse protocol it requested, and how many rows of scrollback
            // are actually populated. Trick for row count: set_scrollback
            // clamps to the underlying VecDeque len, so after asking for
            // usize::MAX, screen.scrollback() returns the true length.
            let info = {
                let mgr = self.sessions_lock();
                mgr.get(session_id).map(|session| {
                    let mut parser = crate::session::lock_parser(&session.parser);
                    let prev = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(usize::MAX);
                    let sb_len = parser.screen().scrollback();
                    parser.screen_mut().set_scrollback(prev);
                    let screen = parser.screen();
                    format!(
                        "alt_screen={} mouse_mode={:?} mouse_enc={:?} sb_rows={} sb_offset={}",
                        screen.alternate_screen(),
                        screen.mouse_protocol_mode(),
                        screen.mouse_protocol_encoding(),
                        sb_len,
                        prev,
                    )
                })
            };
            if let Some(info) = info {
                log::info!("session {session_id} screen probe: {info}");
                self.status_message = Some(format!("probe: {info}"));
                self.status_message_tick = self.tick_count;
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('s') {
            let pos = self
                .sessions_lock()
                .iter()
                .position(|s| s.id == session_id)
                .unwrap_or(0);
            self.picker_selected = pos;
            self.mode = AppMode::SessionPicker(session_id);
            return;
        }

        let bytes = key_event_to_bytes(&key);
        if !bytes.is_empty() {
            let mut mgr = self.sessions_lock();
            if let Some(session) = mgr.get_mut(session_id) {
                session.try_write(&bytes);
            }
        }
    }

    fn handle_session_picker_key(&mut self, key: KeyEvent, from_session_id: usize) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::SessionView(from_session_id);
            }
            KeyCode::Char('a') => {
                // Phase 6 Task 5: attach the highlighted session to a
                // driver. Resolve the current selection into a session
                // id before transitioning so the driver picker knows
                // what to attach on Enter.
                let picker_idx = self.picker_selected;
                let target_id = self.sessions_lock().iter().nth(picker_idx).map(|s| s.id);
                if let Some(target_id) = target_id {
                    self.open_attach_driver_picker(target_id);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.sessions_lock().len();
                if len > 0 {
                    self.picker_selected = (self.picker_selected + 1) % len;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let len = self.sessions_lock().len();
                if len > 0 {
                    self.picker_selected = self.picker_selected.checked_sub(1).unwrap_or(len - 1);
                }
            }
            KeyCode::Enter => {
                let picker_idx = self.picker_selected;
                let inner_rows = self.terminal_rows.saturating_sub(3);
                let inner_cols = self.terminal_cols.saturating_sub(2);
                let id = {
                    let mut mgr = self.sessions_lock();
                    let id = mgr.iter().nth(picker_idx).map(|s| s.id);
                    if let Some(id) = id
                        && let Some(session) = mgr.get_mut(id)
                    {
                        session.try_resize(inner_cols, inner_rows);
                    }
                    if id.is_some() {
                        mgr.set_selected(picker_idx);
                    }
                    id
                };
                if let Some(id) = id {
                    self.mode = AppMode::SessionView(id);
                }
            }
            KeyCode::Char('n') => {
                let dir = self
                    .sessions_lock()
                    .get(from_session_id)
                    .map(|s| s.working_dir.clone())
                    .unwrap_or_else(|| self.working_dir.clone());
                self.spawn_session_kind(crate::app::SessionKind::Claude, dir, vec![], None);
                // spawn_with_role auto-selects the new session; mirror that
                // into picker_selected so Enter immediately enters it.
                let new_idx = self.sessions_lock().len().saturating_sub(1);
                self.picker_selected = new_idx;
            }
            _ => {}
        }
    }

    /// Phase 6 Task 5: driver sub-picker keys. Up/Down cycle through
    /// live driver indices; Enter commits the attachment; Esc aborts
    /// back to the main session picker the user came from.
    fn handle_attach_driver_picker_key(
        &mut self,
        key: KeyEvent,
        target_session_id: usize,
        driver_count: usize,
        selected_driver: Option<(usize, String)>,
        restore_picker_selected: usize,
    ) {
        match key.code {
            KeyCode::Esc => {
                // Return to the session picker with its original
                // highlight row restored — see
                // pr-review-phase-6-tasks-3-to-7.md finding 2 on
                // PR #22.
                self.picker_selected = restore_picker_selected;
                self.mode = AppMode::SessionPicker(target_session_id);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if driver_count > 0 {
                    self.picker_selected = (self.picker_selected + 1) % driver_count;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if driver_count > 0 {
                    self.picker_selected = self
                        .picker_selected
                        .checked_sub(1)
                        .unwrap_or(driver_count - 1);
                }
            }
            KeyCode::Enter => {
                if let Some((driver_id, driver_label)) = selected_driver {
                    // `attach_session_to_driver` re-checks that the
                    // driver is still live at commit time; if it
                    // exited while the picker was open, the call
                    // logs a warning and no-ops.
                    self.attach_session_to_driver(driver_id, target_session_id);
                    self.status_message = Some(format!(
                        "Attached session {target_session_id} to driver {driver_label}"
                    ));
                }
                // Same restore as Esc: the user landed in the driver
                // picker from a specific session-picker row and
                // should return to that row.
                self.picker_selected = restore_picker_selected;
                self.mode = AppMode::SessionPicker(target_session_id);
            }
            _ => {}
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if !self.input_buffer.is_empty() {
                    let mut mgr = self.sessions_lock();
                    if let Some(session) = mgr.selected_mut() {
                        session.label = self.input_buffer.clone();
                    }
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
        // Picker overlay absorbs all keys while it's open.
        if self
            .new_session
            .as_ref()
            .is_some_and(|s| s.picker.is_some())
        {
            self.handle_new_session_picker_key(key);
            return;
        }

        let focused = match &self.new_session {
            Some(s) => s.focused,
            None => return,
        };

        match key.code {
            KeyCode::Esc => {
                let return_to = self.new_session.as_ref().and_then(|s| s.return_to_session);
                self.new_session = None;
                self.mode = match return_to {
                    Some(id) => AppMode::SessionView(id),
                    None => AppMode::Dashboard,
                };
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
            KeyCode::Char('f') if focused == 1 && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.open_new_session_picker();
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

    fn open_new_session_picker(&mut self) {
        let Some(state) = &mut self.new_session else {
            return;
        };
        let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/"));
        state.picker = Some(crate::fs::tree::FileTree::new(home));
        state.status_message = None;
    }

    fn handle_new_session_picker_key(&mut self, key: KeyEvent) {
        let Some(state) = &mut self.new_session else {
            return;
        };
        let Some(picker) = &mut state.picker else {
            return;
        };

        match key.code {
            KeyCode::Esc => {
                state.picker = None;
            }
            KeyCode::Up => picker.move_up(),
            KeyCode::Down => picker.move_down(),
            KeyCode::Char(' ') => picker.toggle_selected(),
            KeyCode::Enter => {
                if let Some(path) = picker.selected_path()
                    && path.is_dir()
                {
                    let p = path.to_path_buf();
                    let home = dirs::home_dir().unwrap_or_default();
                    let display = match p.strip_prefix(&home).ok() {
                        Some(rel) if rel.as_os_str().is_empty() => "~".to_string(),
                        Some(rel) => format!("~/{}", rel.display()),
                        None => format!("{}", p.display()),
                    };
                    state.dir_input = display;
                    state.picker = None;
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

        log::debug!(
            "mouse event: kind={:?} col={} row={} mode={:?}",
            mouse.kind,
            mouse.column,
            mouse.row,
            std::mem::discriminant(&self.mode)
        );

        let scroll_lines: usize = 3;
        match mouse.kind {
            MouseEventKind::ScrollUp => match &self.mode {
                AppMode::Dashboard => match self.focus {
                    PanelFocus::SessionList => {
                        let moved = {
                            let mut mgr = self.sessions_lock();
                            if !mgr.is_empty() {
                                mgr.select_up_by(scroll_lines);
                                true
                            } else {
                                false
                            }
                        };
                        if moved {
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
                    // Two cases:
                    //   1. Child asked for mouse tracking (e.g. less, vim,
                    //      tmux, htop). Forward the SGR wheel escape so the
                    //      program handles it natively.
                    //   2. Child didn't ask for mouse (Claude Code, plain
                    //      shells). Scroll the vt100 primary-screen
                    //      scrollback locally via session_view_scroll,
                    //      which the render path feeds to set_scrollback().
                    let id = *id;
                    let probe = {
                        let mut mgr = self.sessions_lock();
                        mgr.get_mut(id).map(|session| {
                            let mouse_mode = {
                                let parser = crate::session::lock_parser(&session.parser);
                                parser.screen().mouse_protocol_mode()
                            };
                            if mouse_mode != vt100::MouseProtocolMode::None {
                                let col = mouse.column.saturating_add(1);
                                let row = mouse.row.saturating_add(1);
                                let bytes = format!("\x1b[<64;{col};{row}M").into_bytes();
                                for _ in 0..scroll_lines {
                                    session.try_write(&bytes);
                                }
                                None
                            } else {
                                let mut parser = crate::session::lock_parser(&session.parser);
                                let prev = parser.screen().scrollback();
                                parser.screen_mut().set_scrollback(usize::MAX);
                                let len = parser.screen().scrollback();
                                parser.screen_mut().set_scrollback(prev);
                                Some(len)
                            }
                        })
                    };
                    if let Some(Some(sb_rows)) = probe {
                        self.session_view_scroll = self
                            .session_view_scroll
                            .saturating_add(scroll_lines)
                            .min(sb_rows);
                        self.user_scrolled = true;
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
                        let moved = {
                            let mut mgr = self.sessions_lock();
                            if !mgr.is_empty() {
                                mgr.select_down_by(scroll_lines);
                                true
                            } else {
                                false
                            }
                        };
                        if moved {
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
                AppMode::SessionView(id) => {
                    let id = *id;
                    let forwarded = {
                        let mut mgr = self.sessions_lock();
                        mgr.get_mut(id).is_some_and(|session| {
                            let mouse_mode = {
                                let parser = crate::session::lock_parser(&session.parser);
                                parser.screen().mouse_protocol_mode()
                            };
                            if mouse_mode != vt100::MouseProtocolMode::None {
                                let col = mouse.column.saturating_add(1);
                                let row = mouse.row.saturating_add(1);
                                let bytes = format!("\x1b[<65;{col};{row}M").into_bytes();
                                for _ in 0..scroll_lines {
                                    session.try_write(&bytes);
                                }
                                true
                            } else {
                                false
                            }
                        })
                    };
                    if !forwarded {
                        self.session_view_scroll =
                            self.session_view_scroll.saturating_sub(scroll_lines);
                        if self.session_view_scroll == 0 {
                            self.user_scrolled = false;
                        }
                    }
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

/// macOS converts Option+letter into a Unicode character before it reaches the
/// terminal, stripping the modifier. Map the characters used by ccom's Alt+
/// shortcuts back to their ALT+letter equivalents so all key handlers work.
fn normalize_option_key(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.is_empty() {
        return key;
    }
    let letter = match key.code {
        KeyCode::Char('ß') => 's', // Option+s
        KeyCode::Char('∂') => 'd', // Option+d
        KeyCode::Char('©') => 'g', // Option+g
        KeyCode::Char('µ') => 'm', // Option+m
        KeyCode::Char('π') => 'p', // Option+p
        _ => return key,
    };
    KeyEvent {
        code: KeyCode::Char(letter),
        modifiers: KeyModifiers::ALT,
        kind: key.kind,
        state: key.state,
    }
}
