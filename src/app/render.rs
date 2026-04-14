use ratatui::Frame;
use ratatui::layout::Rect;

use super::{App, AppMode, PanelFocus, SessionKind};
use crate::session::SessionStatus;
use crate::ui::layout::AppLayout;
use crate::ui::panels::command_bar::{self, CommandBar, CommandBarMode};
use crate::ui::panels::editor::EditorPanel;
use crate::ui::panels::file_tree::FileTreePanel;
use crate::ui::panels::session_list::SessionListPanel;
use crate::ui::panels::session_picker::SessionPickerPanel;
use crate::ui::panels::session_view::SessionViewPanel;
use crate::ui::panels::usage_graph::UsageGraphPanel;

impl App {
    pub fn draw(&self, frame: &mut Frame) {
        let th = &self.theme;
        let tick = self.tick_count;

        match &self.mode {
            AppMode::Editor | AppMode::SendFilePrompt => {
                self.draw_editor_mode(frame, th, tick);
            }
            AppMode::Dashboard
            | AppMode::RenamePrompt
            | AppMode::NewSessionModal
            | AppMode::QuitConfirm => {
                self.draw_dashboard_mode(frame, th, tick);
            }
            AppMode::SessionView(id) | AppMode::SessionPicker(id) => {
                self.draw_session_view_mode(frame, th, tick, *id);
            }
            AppMode::AttachDriverPicker {
                target_session_id,
                drivers,
                // `restore_picker_selected` is consumed by the key
                // handler, not rendering — ignore it here.
                restore_picker_selected: _,
            } => {
                // Render the originating session view underneath so
                // the user still sees context, then overlay the
                // driver sub-picker. `drivers` is the snapshot
                // captured when the picker opened — no session lock
                // needed on the render path.
                let target = *target_session_id;
                let drivers = drivers.clone();
                self.draw_session_view_mode(frame, th, tick, target);
                self.draw_attach_driver_picker(frame, target, &drivers);
            }
            AppMode::Setup => {
                self.draw_setup_mode(frame, th, tick);
            }
            AppMode::McpConfirm => {
                // Render the dashboard underneath so the user has
                // context for what's being confirmed, then overlay
                // the confirmation modal.
                self.draw_dashboard_mode(frame, th, tick);
                self.draw_mcp_confirm(frame);
            }
        }
    }

    fn draw_editor_mode(&self, frame: &mut Frame, th: &crate::ui::theme::Theme, tick: u64) {
        let (main_area, cmd_area) = AppLayout::session_view(frame.area());

        if let Some(editor) = &self.editor {
            let panel = EditorPanel::new(editor, th, tick);
            frame.render_widget(panel, main_area);

            if let Some(msg) = &editor.message {
                let line = ratatui::text::Line::styled(
                    msg.clone(),
                    ratatui::style::Style::default().fg(th.status_warn),
                );
                frame.render_widget(line, cmd_area);
            } else if self.mode == AppMode::SendFilePrompt {
                let labels: Vec<String> = self
                    .sessions_lock()
                    .iter()
                    .map(|s| s.label.clone())
                    .collect();
                let bar = CommandBar::new(CommandBarMode::SendFile(labels), th);
                frame.render_widget(bar, cmd_area);
            } else {
                let bar = CommandBar::new(CommandBarMode::Editor, th);
                frame.render_widget(bar, cmd_area);
            }
        }
    }

    fn draw_dashboard_mode(&self, frame: &mut Frame, th: &crate::ui::theme::Theme, tick: u64) {
        let layout = AppLayout::new(frame.area());

        let session_dirs = self.session_dirs();
        let tree_panel = FileTreePanel::new(
            &self.file_tree,
            self.focus == PanelFocus::FileTree,
            &session_dirs,
            th,
            tick,
        )
        .with_scroll(self.file_tree_scroll)
        .with_git_status(self.git_status.as_ref());
        frame.render_widget(tree_panel, layout.file_tree);

        {
            // Snapshot the driver-attachment map before taking the
            // session lock so rendering holds exactly one lock at a
            // time. The map is small (driver count × attached ids)
            // so the clone is cheap.
            let attachments = self
                .attachment_map
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            let mgr = self.sessions_lock();
            let session_list = SessionListPanel::new(
                mgr.as_slice(),
                mgr.selected_index().unwrap_or(0),
                self.focus == PanelFocus::SessionList,
                th,
                tick,
            )
            .with_attachments(attachments);
            frame.render_widget(session_list, layout.main);
        }

        let show_banner = !self.setup_banner_dismissed && !self.setup_items.is_empty();
        let usage_area = if show_banner && layout.usage_graph.height > 1 {
            let banner = ratatui::text::Line::from(vec![
                ratatui::text::Span::styled(
                    " Setup needed ",
                    ratatui::style::Style::default()
                        .fg(th.selected_fg)
                        .bg(th.status_warn),
                ),
                ratatui::text::Span::styled(
                    format!(" {} item(s) — press S to configure", self.setup_items.len()),
                    ratatui::style::Style::default().fg(th.status_warn),
                ),
            ]);
            let banner_area = ratatui::layout::Rect {
                x: layout.usage_graph.x,
                y: layout.usage_graph.y,
                width: layout.usage_graph.width,
                height: 1,
            };
            frame.render_widget(banner, banner_area);
            ratatui::layout::Rect {
                x: layout.usage_graph.x,
                y: layout.usage_graph.y + 1,
                width: layout.usage_graph.width,
                height: layout.usage_graph.height - 1,
            }
        } else {
            layout.usage_graph
        };

        let usage_panel = UsageGraphPanel::new(th, tick).with_rate_limit(self.rate_limit.as_ref());
        frame.render_widget(usage_panel, usage_area);

        match &self.mode {
            AppMode::RenamePrompt => {
                let prompt = format!("Rename: {}_", self.input_buffer);
                let line = ratatui::text::Line::raw(prompt);
                frame.render_widget(line, layout.command_bar);
            }
            _ => {
                let bar_mode = match self.focus {
                    PanelFocus::SessionList => CommandBarMode::Dashboard,
                    PanelFocus::FileTree => CommandBarMode::FileTree,
                };
                let command_bar = CommandBar::new(bar_mode, th);
                frame.render_widget(command_bar, layout.command_bar);
            }
        }

        if self.show_help {
            self.draw_help_modal(frame);
        } else if self.mode == AppMode::NewSessionModal {
            self.draw_new_session_modal(frame);
        } else if self.mode == AppMode::QuitConfirm {
            self.draw_quit_confirm(frame);
        }
    }

    fn draw_session_view_mode(
        &self,
        frame: &mut Frame,
        th: &crate::ui::theme::Theme,
        tick: u64,
        id: usize,
    ) {
        let (main_area, cmd_area) = AppLayout::session_view(frame.area());

        let mgr = self.sessions_lock();
        let context_pct = if let Some(session) = mgr.get(id) {
            let view =
                SessionViewPanel::new(session, th, tick).with_scroll(self.session_view_scroll);
            frame.render_widget(view, main_area);
            session.context_percent
        } else {
            None
        };

        if matches!(self.mode, AppMode::SessionPicker(_)) {
            let picker = SessionPickerPanel::new(mgr.as_slice(), self.picker_selected, th);
            frame.render_widget(picker, main_area);
            drop(mgr);

            let command_bar = CommandBar::new(CommandBarMode::SessionPicker, th);
            frame.render_widget(command_bar, cmd_area);
        } else {
            drop(mgr);
            let usage = command_bar::UsageStats {
                context_pct,
                session_pct: self.rate_limit.as_ref().and_then(|r| r.session_pct),
                weekly_pct: self.rate_limit.as_ref().and_then(|r| r.weekly_pct),
            };
            let command_bar = CommandBar::new(CommandBarMode::SessionView, th).with_usage(usage);
            frame.render_widget(command_bar, cmd_area);
        }
    }

    fn draw_setup_mode(&self, frame: &mut Frame, th: &crate::ui::theme::Theme, _tick: u64) {
        let (main_area, cmd_area) = AppLayout::session_view(frame.area());
        self.draw_setup_screen(frame, main_area);

        let bar = CommandBar::new(CommandBarMode::Setup, th);
        frame.render_widget(bar, cmd_area);
    }

    fn draw_setup_screen(&self, frame: &mut Frame, area: Rect) {
        use ratatui::style::{Color, Style};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Paragraph};
        let th = &self.theme;

        let block = Block::default()
            .title(" Setup ")
            .borders(Borders::ALL)
            .border_style(th.border_focused());

        let inner = block.inner(area);
        frame.render_widget(block, area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), area, self.tick_count);
        }

        let mut lines = Vec::new();

        if self.setup_items.is_empty() {
            lines.push(Line::styled(
                "  All configurations are in place!",
                Style::default().fg(Color::Green),
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "  Press Esc to return.",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            lines.push(Line::styled(
                "  The following configurations are needed for full functionality:",
                Style::default().fg(Color::Yellow),
            ));
            lines.push(Line::raw(""));

            for (i, item) in self.setup_items.iter().enumerate() {
                let marker = if i == self.setup_selected {
                    " > "
                } else {
                    "   "
                };
                let (icon, color) = match item.status {
                    crate::setup::SetupStatus::Ok => ("OK", Color::Green),
                    crate::setup::SetupStatus::Missing => ("MISSING", Color::Red),
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(Color::Cyan)),
                    Span::styled(format!("[{icon}] "), Style::default().fg(color)),
                    Span::styled(&item.name, Style::default().fg(Color::White)),
                    Span::styled(
                        format!(" — {}", item.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }

            let missing_count = self
                .setup_items
                .iter()
                .filter(|i| i.status == crate::setup::SetupStatus::Missing)
                .count();

            if missing_count > 0 {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "  Press Enter or 'y' to fix — this will start a Claude session",
                    Style::default().fg(Color::Cyan),
                ));
                lines.push(Line::styled(
                    "  that configures the missing items for you.",
                    Style::default().fg(Color::Cyan),
                ));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn draw_new_session_modal(&self, frame: &mut Frame) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear};
        let th = &self.theme;

        let state = match &self.new_session {
            Some(s) => s,
            None => return,
        };

        let area = frame.area();
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 13u16.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" New Session ")
            .borders(Borders::ALL)
            .border_style(th.border_focused());
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let mut row = inner.y;
        let field_width = inner.width.saturating_sub(4);

        let type_focused = state.focused == 0;
        let type_style = if type_focused {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.dim)
        };
        let type_label = Line::styled("  Type:", type_style);
        frame.render_widget(type_label, Rect::new(inner.x, row, inner.width, 1));
        row += 1;
        let claude_selected = state.kind == SessionKind::Claude;
        let term_selected = state.kind == SessionKind::Terminal;
        let sel_style = Style::default().fg(th.text);
        let unsel_style = Style::default().fg(th.dim);
        let type_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if claude_selected {
                    "● Claude"
                } else {
                    "○ Claude"
                },
                if claude_selected {
                    sel_style
                } else {
                    unsel_style
                },
            ),
            Span::raw("   "),
            Span::styled(
                if term_selected {
                    "● Terminal"
                } else {
                    "○ Terminal"
                },
                if term_selected {
                    sel_style
                } else {
                    unsel_style
                },
            ),
        ]);
        frame.render_widget(type_line, Rect::new(inner.x, row, inner.width, 1));
        row += 2;

        let dir_focused = state.focused == 1;
        let dir_style = if dir_focused {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.dim)
        };
        let dir_label = Line::styled("  Directory:", dir_style);
        frame.render_widget(dir_label, Rect::new(inner.x, row, inner.width, 1));
        row += 1;

        let dir_text = if state.dir_input.is_empty() {
            format!("{} (default)", self.working_dir.display())
        } else if dir_focused {
            format!("{}█", state.dir_input)
        } else {
            state.dir_input.clone()
        };
        let dir_display = if dir_text.len() > field_width as usize {
            let skip = dir_text.len() - field_width as usize + 1;
            format!("  …{}", &dir_text[skip..])
        } else {
            format!("  > {dir_text}")
        };
        let cursor_style = if dir_focused {
            Style::default().fg(th.text)
        } else {
            Style::default().fg(th.dim)
        };
        let dir_line = Line::styled(dir_display, cursor_style);
        frame.render_widget(dir_line, Rect::new(inner.x, row, inner.width, 1));
        row += 2;

        let flags_focused = state.focused == 2;
        let flags_style = if flags_focused {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.dim)
        };
        let flags_label = Line::styled("  Flags:", flags_style);
        frame.render_widget(flags_label, Rect::new(inner.x, row, inner.width, 1));
        row += 1;

        let flags_text = if state.flags_input.is_empty() && !flags_focused {
            "(none)".to_string()
        } else if flags_focused {
            format!("{}█", state.flags_input)
        } else {
            state.flags_input.clone()
        };
        let flags_display = if flags_text.len() > field_width as usize {
            let skip = flags_text.len() - field_width as usize + 1;
            format!("  …{}", &flags_text[skip..])
        } else {
            format!("  > {flags_text}")
        };
        let fcursor_style = if flags_focused {
            Style::default().fg(th.text)
        } else {
            Style::default().fg(th.dim)
        };
        let flags_line = Line::styled(flags_display, fcursor_style);
        frame.render_widget(flags_line, Rect::new(inner.x, row, inner.width, 1));
        row += 2;

        if let Some(msg) = &state.status_message {
            let msg_display = if msg.len() + 2 > inner.width as usize {
                format!("  {}…", &msg[..inner.width as usize - 3])
            } else {
                format!("  {msg}")
            };
            let line = Line::styled(msg_display, Style::default().fg(th.status_warn));
            frame.render_widget(line, Rect::new(inner.x, row, inner.width, 1));
        } else {
            let help = Line::from(vec![
                Span::styled("  [Enter]", th.shortcut_key()),
                Span::styled(" Create ", th.shortcut_desc()),
                Span::styled("[Tab]", th.shortcut_key()),
                Span::styled(" Complete ", th.shortcut_desc()),
                Span::styled("[Esc]", th.shortcut_key()),
                Span::styled(" Cancel", th.shortcut_desc()),
            ]);
            frame.render_widget(help, Rect::new(inner.x, row, inner.width, 1));
        }
    }

    fn draw_quit_confirm(&self, frame: &mut Frame) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear};
        let th = &self.theme;

        let area = frame.area();
        let width = 50u16.min(area.width.saturating_sub(4));
        let height = 7u16.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Quit ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(th.status_warn));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let has_running = self
            .sessions_lock()
            .iter()
            .any(|s| !matches!(s.status, SessionStatus::Exited(_)));
        let msg = if has_running {
            "  Quit ccom? Running sessions will be killed."
        } else {
            "  Quit ccom?"
        };
        let line = Line::styled(msg, Style::default().fg(th.text));
        frame.render_widget(line, Rect::new(inner.x, inner.y + 1, inner.width, 1));

        let help = Line::from(vec![
            Span::styled("  [y]", Style::default().fg(th.status_warn)),
            Span::styled(" Yes  ", th.shortcut_desc()),
            Span::styled("[n/Esc]", th.shortcut_key()),
            Span::styled(" No", th.shortcut_desc()),
        ]);
        frame.render_widget(help, Rect::new(inner.x, inner.y + 3, inner.width, 1));
    }

    /// Phase 5: render the MCP write-tool confirmation modal. Shows
    /// the tool name and target session id so the user can make an
    /// informed allow/deny decision.
    fn draw_mcp_confirm(&self, frame: &mut Frame) {
        use crate::mcp::ConfirmTool;
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear};
        let th = &self.theme;

        let Some(req) = self.pending_confirm.as_ref() else {
            return;
        };

        let tool_name = match req.tool {
            ConfirmTool::SendPrompt => "send_prompt",
            ConfirmTool::KillSession => "kill_session",
            ConfirmTool::SpawnSession => "spawn_session",
        };

        // Look up the session label so the prompt is human-readable.
        let session_label = {
            let mgr = self.sessions_lock();
            mgr.get(req.session_id)
                .map(|s| s.label.clone())
                .unwrap_or_else(|| format!("id={}", req.session_id))
        };

        let area = frame.area();
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 9u16.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" MCP Confirmation ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(th.status_warn));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let header = Line::styled(
            "  An MCP tool is requesting your permission:",
            Style::default().fg(th.text),
        );
        frame.render_widget(header, Rect::new(inner.x, inner.y, inner.width, 1));

        let action = Line::from(vec![
            Span::styled("  Tool: ", Style::default().fg(th.dim)),
            Span::styled(tool_name.to_string(), Style::default().fg(th.status_warn)),
        ]);
        frame.render_widget(action, Rect::new(inner.x, inner.y + 2, inner.width, 1));

        let target = Line::from(vec![
            Span::styled("  Target: ", Style::default().fg(th.dim)),
            Span::styled(
                format!("session {} ({})", req.session_id, session_label),
                Style::default().fg(th.text),
            ),
        ]);
        frame.render_widget(target, Rect::new(inner.x, inner.y + 3, inner.width, 1));

        let help = Line::from(vec![
            Span::styled("  [y]", Style::default().fg(th.status_warn)),
            Span::styled(" Allow  ", th.shortcut_desc()),
            Span::styled("[n/Esc]", th.shortcut_key()),
            Span::styled(" Deny", th.shortcut_desc()),
        ]);
        frame.render_widget(help, Rect::new(inner.x, inner.y + 5, inner.width, 1));
    }

    /// Phase 6 Task 5: compact overlay listing live drivers. The
    /// selected row is drawn in the theme's selected style; all
    /// driver rows carry the `◆ ` prefix so the user knows they're
    /// picking a driver and not an arbitrary session.
    ///
    /// The `drivers` list is the snapshot captured when the picker
    /// opened (`open_attach_driver_picker`) and stored on the
    /// `AppMode::AttachDriverPicker` variant — this function takes
    /// no session lock (pr-review-phase-6-tasks-3-to-7.md §D2).
    fn draw_attach_driver_picker(
        &self,
        frame: &mut Frame,
        target_session_id: usize,
        drivers: &[(usize, String)],
    ) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear, Paragraph};
        let th = &self.theme;

        let area = frame.area();
        let width = 50u16.min(area.width.saturating_sub(4));
        let content_height = (drivers.len() as u16 + 4).max(5);
        let height = content_height.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let title = format!(" Attach session {target_session_id} to driver ");
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(th.border_focused());
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let mut lines: Vec<Line> = Vec::new();
        if drivers.is_empty() {
            lines.push(Line::styled(
                "  No active drivers.",
                Style::default().fg(th.status_warn),
            ));
        } else {
            for (i, (id, label)) in drivers.iter().enumerate() {
                let row_style = if i == self.picker_selected {
                    th.selected()
                } else {
                    Style::default().fg(th.text)
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {}", th.driver_icon()),
                        Style::default().fg(th.driver_color()),
                    ),
                    Span::styled(format!("{label} (id={id})"), row_style),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  [Enter]", th.shortcut_key()),
            Span::styled(" Attach  ", th.shortcut_desc()),
            Span::styled("[Esc]", th.shortcut_key()),
            Span::styled(" Cancel", th.shortcut_desc()),
        ]));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_help_modal(&self, frame: &mut Frame) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear, Paragraph};
        let th = &self.theme;

        let sections: &[(&str, &[(&str, &str)])] = &[
            (
                "Session Management",
                &[
                    ("n", "New session"),
                    ("Enter", "View selected session"),
                    ("a", "Approve tool request"),
                    ("d", "Deny tool request"),
                    ("c", "Send commit prompt"),
                    ("K", "Kill session"),
                    ("x", "Clear dead sessions"),
                    ("r", "Rename session"),
                ],
            ),
            (
                "Navigation",
                &[
                    ("↑/↓", "Navigate list"),
                    ("Tab", "Switch panel (sessions/files)"),
                    ("S", "Open setup screen"),
                ],
            ),
            (
                "File Tree",
                &[
                    ("Enter/→", "Expand directory"),
                    ("←", "Collapse directory"),
                    ("e", "Edit file"),
                    ("n", "New session in directory"),
                    ("R", "Refresh tree"),
                ],
            ),
            (
                "General",
                &[
                    ("t", "Cycle color theme"),
                    ("C-S-m", "Toggle mouse capture"),
                    ("?", "Toggle this help"),
                    ("q", "Quit"),
                    ("Ctrl+C", "Force quit"),
                ],
            ),
        ];

        let content_lines: u16 = sections
            .iter()
            .map(|(_, entries)| 1 + entries.len() as u16)
            .sum::<u16>()
            + (sections.len() as u16).saturating_sub(1);
        let height = (content_lines + 3).min(frame.area().height.saturating_sub(2));
        let width = 48u16.min(frame.area().width.saturating_sub(4));
        let area = frame.area();
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Keyboard Shortcuts ")
            .borders(Borders::ALL)
            .border_style(th.border_focused());
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let mut lines = Vec::new();
        for (i, (section, entries)) in sections.iter().enumerate() {
            if i > 0 {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                format!(" {section}"),
                Style::default().fg(th.status_warn),
            ));
            for (key, desc) in *entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("   {key:>10}"), th.shortcut_key()),
                    Span::styled(format!("  {desc}"), Style::default().fg(th.text)),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(" Press ? or Esc to close", th.shortcut_desc()));

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}
