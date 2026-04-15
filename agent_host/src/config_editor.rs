use agent_frame::config::MemorySystem;
use agent_host::config::{LATEST_CONFIG_VERSION, load_server_config_file};
use anyhow::{Context, Result, anyhow, bail};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{Map, Number, Value, json};
use std::fs;
use std::io::{self, IsTerminal, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const TOOLING_FIELD_COUNT: usize = 5;
const MAIN_AGENT_FIELD_COUNT: usize = 15;
const RUNTIME_FIELD_COUNT: usize = 2;
const SANDBOX_FIELD_COUNT: usize = 3;

pub fn run_config_editor(path: &Path) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("`partyclaw config` requires an interactive terminal");
    }

    let value = load_or_create_json_document(path)?;
    let mut app = ConfigEditorApp::new(path.to_path_buf(), value);
    let mut tui = TuiSession::enter()?;

    loop {
        tui.draw(|frame| app.render(frame))?;
        if app.should_quit {
            break;
        }
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Err(error) = app.handle_key(key) {
            app.show_error(error);
        }
    }

    Ok(())
}

struct TuiSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TuiSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("failed to initialize terminal")?;
        terminal.hide_cursor().context("failed to hide cursor")?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, draw_fn: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal
            .draw(draw_fn)
            .context("failed to render config editor")?;
        Ok(())
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

struct ConfigEditorApp {
    path: PathBuf,
    value: Value,
    dirty: bool,
    current_page: PageKind,
    main_focus: MainFocus,
    screen: ScreenState,
    modal: Option<ModalState>,
    status: StatusMessage,
    models_selected: usize,
    tooling_selected: usize,
    main_agent_selected: usize,
    runtime_selected: usize,
    sandbox_selected: usize,
    channels_selected: usize,
    should_quit: bool,
}

impl ConfigEditorApp {
    fn new(path: PathBuf, value: Value) -> Self {
        Self {
            path,
            value,
            dirty: false,
            current_page: PageKind::Overview,
            main_focus: MainFocus::Sections,
            screen: ScreenState::Main,
            modal: None,
            status: StatusMessage::info(
                "Up/Down choose a section, Right enters it, Left returns to the section list",
            ),
            models_selected: 0,
            tooling_selected: 0,
            main_agent_selected: 0,
            runtime_selected: 0,
            sandbox_selected: 0,
            channels_selected: 0,
            should_quit: false,
        }
    }

    fn render(&self, frame: &mut Frame<'_>) {
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(4),
            ])
            .split(frame.area());

        self.render_header(frame, layout[0]);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(40)])
            .split(layout[1]);

        self.render_nav(frame, body[0]);
        self.render_content(frame, body[1]);
        self.render_footer(frame, layout[2]);

        if let Some(modal) = &self.modal {
            self.render_modal(frame, modal);
        }
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = match &self.screen {
            ScreenState::Main => self.current_page.title().to_string(),
            ScreenState::ModelTypeWizard(_) => "Choose Model Type".to_string(),
            ScreenState::ModelForm(form) => {
                if form.existing_alias.is_some() {
                    "Edit Model".to_string()
                } else {
                    "Add Model".to_string()
                }
            }
            ScreenState::ChannelKindWizard(_) => "Choose Channel Type".to_string(),
            ScreenState::ChannelForm(form) => {
                if form.existing_index.is_some() {
                    "Edit Channel".to_string()
                } else {
                    "Add Channel".to_string()
                }
            }
        };

        let status_style = match self.status.level {
            StatusLevel::Info => Style::default().fg(Color::Cyan),
            StatusLevel::Success => Style::default().fg(Color::Green),
            StatusLevel::Warning => Style::default().fg(Color::Yellow),
            StatusLevel::Error => Style::default().fg(Color::Red),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" partyclaw config - {title} "))
            .border_style(Style::default().fg(Color::Blue));

        let lines = vec![
            Line::from(vec![
                Span::styled(
                    if self.dirty { "modified " } else { "saved " },
                    if self.dirty {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Green)
                    },
                ),
                Span::raw(self.path.display().to_string()),
            ]),
            Line::from(Span::styled(self.status.text.clone(), status_style)),
        ];
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_nav(&self, frame: &mut Frame<'_>, area: Rect) {
        let items = PageKind::ALL
            .iter()
            .map(|page| ListItem::new(page.title()))
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(self.current_page.index()));
        let focused = self.main_focus == MainFocus::Sections;
        let widget = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Sections ")
                    .border_style(focus_border_style(focused)),
            )
            .highlight_style(list_highlight_style(focused))
            .highlight_symbol(list_highlight_symbol(focused));
        frame.render_stateful_widget(widget, area, &mut state);
    }

    fn render_content(&self, frame: &mut Frame<'_>, area: Rect) {
        match &self.screen {
            ScreenState::Main => self.render_main_page(frame, area),
            ScreenState::ModelTypeWizard(state) => {
                self.render_model_type_wizard(frame, area, state)
            }
            ScreenState::ModelForm(form) => self.render_model_form(frame, area, form),
            ScreenState::ChannelKindWizard(state) => {
                self.render_channel_kind_wizard(frame, area, state)
            }
            ScreenState::ChannelForm(form) => self.render_channel_form(frame, area, form),
        }
    }

    fn render_main_page(&self, frame: &mut Frame<'_>, area: Rect) {
        match self.current_page {
            PageKind::Overview => self.render_overview(frame, area),
            PageKind::Models => self.render_models_page(frame, area),
            PageKind::Tooling => self.render_tooling_page(frame, area),
            PageKind::MainAgent => self.render_main_agent_page(frame, area),
            PageKind::Runtime => self.render_runtime_page(frame, area),
            PageKind::Sandbox => self.render_sandbox_page(frame, area),
            PageKind::Channels => self.render_channels_page(frame, area),
        }
    }

    fn render_overview(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Overview ")
            .border_style(Style::default().fg(Color::Blue));

        let lines = vec![
            Line::from(format!("Version: {}", current_version(&self.value))),
            Line::from(format!("Models: {}", self.model_aliases().len())),
            Line::from(format!("Channels: {}", self.channels().len())),
            Line::from(""),
            Line::from("Pages"),
            Line::from("  Models: browse, add, edit, delete model definitions"),
            Line::from("  Tooling: route helper tools to specific model aliases"),
            Line::from("  Main Agent: adjust defaults for the foreground agent"),
            Line::from("  Runtime: service-level counters and polling settings"),
            Line::from("  Sandbox: choose subprocess / bubblewrap"),
            Line::from("  Channels: configure Telegram, DingTalk, or command-line channels"),
            Line::from(""),
            Line::from("Global keys"),
            Line::from("  Up / Down: move in the focused pane"),
            Line::from("  Right / Enter: enter the selected section"),
            Line::from("  Left: return to the section list"),
            Line::from("  s: save    v: validate    b: bootstrap latest skeleton"),
            Line::from("  q: quit    ?: help"),
        ];

        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_models_page(&self, frame: &mut Frame<'_>, area: Rect) {
        let focused = self.main_focus == MainFocus::Content;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(5)])
            .split(area);

        let aliases = self.model_aliases();
        let list_items = if aliases.is_empty() {
            vec![ListItem::new("No models configured. Press a to add one.")]
        } else {
            aliases
                .iter()
                .map(|alias| {
                    let ty = nested_string(&self.value, &["models", alias.as_str(), "type"])
                        .unwrap_or("unknown");
                    let model_name =
                        nested_string(&self.value, &["models", alias.as_str(), "model"])
                            .unwrap_or("unset");
                    ListItem::new(Line::from(vec![
                        Span::styled(alias.clone(), Style::default().add_modifier(Modifier::BOLD)),
                        Span::raw(format!("  [{ty}]  {model_name}")),
                    ]))
                })
                .collect()
        };

        let mut state = ListState::default();
        if !aliases.is_empty() {
            state.select(Some(self.models_selected.min(aliases.len() - 1)));
        }

        let list = List::new(list_items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Models ")
                    .border_style(focus_border_style(focused)),
            )
            .highlight_style(list_highlight_style(focused))
            .highlight_symbol(list_highlight_symbol(focused));
        frame.render_stateful_widget(list, sections[0], &mut state);

        let help_text = if let Some(alias) =
            aliases.get(self.models_selected.min(aliases.len().saturating_sub(1)))
        {
            model_summary_text(&self.value, alias)
        } else {
            "a 新增模型，Enter/e 编辑当前模型，d 删除当前模型。".to_string()
        };
        frame.render_widget(
            Paragraph::new(help_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Help ")
                        .border_style(focus_border_style(focused)),
                )
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_tooling_page(&self, frame: &mut Frame<'_>, area: Rect) {
        let fields = vec![
            (
                "web_search",
                top_level_tooling_value(&self.value, "web_search"),
            ),
            ("image", top_level_tooling_value(&self.value, "image")),
            (
                "image_gen",
                top_level_tooling_value(&self.value, "image_gen"),
            ),
            ("pdf", top_level_tooling_value(&self.value, "pdf")),
            (
                "audio_input",
                top_level_tooling_value(&self.value, "audio_input"),
            ),
        ];
        render_field_list(
            frame,
            area,
            " Tooling ",
            &fields,
            self.tooling_selected.min(TOOLING_FIELD_COUNT - 1),
            "Enter edits the selected routing target. Blank clears the field.",
            self.main_focus == MainFocus::Content,
        );
    }

    fn render_main_agent_page(&self, frame: &mut Frame<'_>, area: Rect) {
        let fields = vec![
            (
                "global_install_root",
                nested_string(&self.value, &["main_agent", "global_install_root"])
                    .unwrap_or("/opt")
                    .to_string(),
            ),
            (
                "token_estimation_cache.template.hf",
                nested_string(
                    &self.value,
                    &["main_agent", "token_estimation_cache", "template", "hf"],
                )
                .unwrap_or("template-cache/hf")
                .to_string(),
            ),
            (
                "token_estimation_cache.tokenizer.hf",
                nested_string(
                    &self.value,
                    &["main_agent", "token_estimation_cache", "tokenizer", "hf"],
                )
                .unwrap_or("tokenizer-cache/hf")
                .to_string(),
            ),
            (
                "language",
                nested_string(&self.value, &["main_agent", "language"])
                    .unwrap_or("zh-CN")
                    .to_string(),
            ),
            (
                "memory_system",
                nested_string(&self.value, &["main_agent", "memory_system"])
                    .unwrap_or("layered")
                    .to_string(),
            ),
            (
                "time_awareness.emit_system_date_on_user_message",
                bool_string(nested_bool(
                    &self.value,
                    &[
                        "main_agent",
                        "time_awareness",
                        "emit_system_date_on_user_message",
                    ],
                    false,
                )),
            ),
            (
                "time_awareness.emit_idle_time_gap_hint",
                bool_string(nested_bool(
                    &self.value,
                    &["main_agent", "time_awareness", "emit_idle_time_gap_hint"],
                    true,
                )),
            ),
            (
                "enable_context_compression",
                bool_string(nested_bool(
                    &self.value,
                    &["main_agent", "enable_context_compression"],
                    true,
                )),
            ),
            (
                "context_compaction.trigger_ratio",
                nested_number_string(
                    &self.value,
                    &["main_agent", "context_compaction", "trigger_ratio"],
                )
                .unwrap_or_else(|| "0.9".to_string()),
            ),
            (
                "context_compaction.token_limit_override",
                nested_number_string(
                    &self.value,
                    &["main_agent", "context_compaction", "token_limit_override"],
                )
                .unwrap_or_default(),
            ),
            (
                "context_compaction.recent_fidelity_target_ratio",
                nested_number_string(
                    &self.value,
                    &[
                        "main_agent",
                        "context_compaction",
                        "recent_fidelity_target_ratio",
                    ],
                )
                .unwrap_or_else(|| "0.18".to_string()),
            ),
            (
                "idle_compaction.enabled",
                bool_string(nested_bool(
                    &self.value,
                    &["main_agent", "idle_compaction", "enabled"],
                    false,
                )),
            ),
            (
                "idle_compaction.poll_interval_seconds",
                nested_number_string(
                    &self.value,
                    &["main_agent", "idle_compaction", "poll_interval_seconds"],
                )
                .unwrap_or_else(|| "15".to_string()),
            ),
            (
                "idle_compaction.min_ratio",
                nested_number_string(&self.value, &["main_agent", "idle_compaction", "min_ratio"])
                    .unwrap_or_else(|| "0.5".to_string()),
            ),
            (
                "timeout_observation_compaction.enabled",
                bool_string(nested_bool(
                    &self.value,
                    &["main_agent", "timeout_observation_compaction", "enabled"],
                    true,
                )),
            ),
        ];
        render_field_list(
            frame,
            area,
            " Main Agent ",
            &fields,
            self.main_agent_selected.min(MAIN_AGENT_FIELD_COUNT - 1),
            "Enter edits values. Space toggles booleans on the highlighted row.",
            self.main_focus == MainFocus::Content,
        );
    }

    fn render_runtime_page(&self, frame: &mut Frame<'_>, area: Rect) {
        let fields = vec![
            (
                "max_global_sub_agents",
                nested_number_string(&self.value, &["max_global_sub_agents"])
                    .unwrap_or_else(|| "4".to_string()),
            ),
            (
                "cron_poll_interval_seconds",
                nested_number_string(&self.value, &["cron_poll_interval_seconds"])
                    .unwrap_or_else(|| "5".to_string()),
            ),
        ];
        render_field_list(
            frame,
            area,
            " Runtime ",
            &fields,
            self.runtime_selected.min(RUNTIME_FIELD_COUNT - 1),
            "Service-wide limits and background polling intervals.",
            self.main_focus == MainFocus::Content,
        );
    }

    fn render_sandbox_page(&self, frame: &mut Frame<'_>, area: Rect) {
        let fields = vec![
            (
                "mode",
                nested_string(&self.value, &["sandbox", "mode"])
                    .unwrap_or("subprocess")
                    .to_string(),
            ),
            (
                "bubblewrap_binary",
                nested_string(&self.value, &["sandbox", "bubblewrap_binary"])
                    .unwrap_or("bwrap")
                    .to_string(),
            ),
            (
                "map_docker_socket",
                bool_string(nested_bool(
                    &self.value,
                    &["sandbox", "map_docker_socket"],
                    false,
                )),
            ),
        ];
        render_field_list(
            frame,
            area,
            " Sandbox ",
            &fields,
            self.sandbox_selected.min(SANDBOX_FIELD_COUNT - 1),
            "Enter edits values. The mode row opens a choice list.",
            self.main_focus == MainFocus::Content,
        );
    }

    fn render_channels_page(&self, frame: &mut Frame<'_>, area: Rect) {
        let focused = self.main_focus == MainFocus::Content;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(5)])
            .split(area);

        let channels = self.channels();
        let list_items = if channels.is_empty() {
            vec![ListItem::new("No channels configured. Press a to add one.")]
        } else {
            channels
                .iter()
                .map(|channel| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            channel.id.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!("  [{}]", channel.kind)),
                    ]))
                })
                .collect()
        };

        let mut state = ListState::default();
        if !channels.is_empty() {
            state.select(Some(self.channels_selected.min(channels.len() - 1)));
        }

        let list = List::new(list_items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Channels ")
                    .border_style(focus_border_style(focused)),
            )
            .highlight_style(list_highlight_style(focused))
            .highlight_symbol(list_highlight_symbol(focused));
        frame.render_stateful_widget(list, sections[0], &mut state);

        let help_text = if let Some(channel) =
            channels.get(self.channels_selected.min(channels.len().saturating_sub(1)))
        {
            channel_summary_text(channel)
        } else {
            "a 新增频道，Enter/e 编辑当前频道，d 删除当前频道。".to_string()
        };
        frame.render_widget(
            Paragraph::new(help_text)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Help ")
                        .border_style(focus_border_style(focused)),
                )
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_model_form(&self, frame: &mut Frame<'_>, area: Rect, form: &ModelFormState) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(9)])
            .split(area);
        let fields = form.fields();
        render_field_list(
            frame,
            sections[0],
            " Model Form ",
            &fields,
            form.selected.min(fields.len().saturating_sub(1)),
            "Up/Down move, Enter edit, Space toggle, s save, Esc cancel.",
            true,
        );
        let selected = form.selected_field();
        frame.render_widget(
            Paragraph::new(model_field_guide_text(form, selected))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Field Guide ")
                        .border_style(Style::default().fg(Color::Blue)),
                )
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_channel_form(&self, frame: &mut Frame<'_>, area: Rect, form: &ChannelFormState) {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(8)])
            .split(area);
        let fields = form.fields();
        render_field_list(
            frame,
            sections[0],
            " Channel Form ",
            &fields,
            form.selected.min(fields.len().saturating_sub(1)),
            "Up/Down move, Enter edit, s save, Esc cancel.",
            true,
        );
        let selected = form.selected_field();
        frame.render_widget(
            Paragraph::new(channel_field_guide_text(form, selected))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Field Guide ")
                        .border_style(Style::default().fg(Color::Blue)),
                )
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_model_type_wizard(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        state: &ModelTypeWizardState,
    ) {
        let sections = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(area);
        let items = ModelTypeWizardState::OPTIONS
            .iter()
            .map(|option| ListItem::new(*option))
            .collect::<Vec<_>>();
        let mut list_state = ListState::default();
        list_state.select(Some(
            state.selected.min(ModelTypeWizardState::OPTIONS.len() - 1),
        ));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Model Types ")
                    .border_style(Style::default().fg(Color::Blue)),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, sections[0], &mut list_state);
        frame.render_widget(
            Paragraph::new(model_type_wizard_text(state.selected_type()))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Type Guide ")
                        .border_style(Style::default().fg(Color::Blue)),
                )
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_channel_kind_wizard(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        state: &ChannelKindWizardState,
    ) {
        let sections = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(area);
        let items = ChannelKindWizardState::OPTIONS
            .iter()
            .map(|option| ListItem::new(*option))
            .collect::<Vec<_>>();
        let mut list_state = ListState::default();
        list_state.select(Some(
            state
                .selected
                .min(ChannelKindWizardState::OPTIONS.len() - 1),
        ));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Channel Types ")
                    .border_style(Style::default().fg(Color::Blue)),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, sections[0], &mut list_state);
        frame.render_widget(
            Paragraph::new(channel_kind_wizard_text(state.selected_kind()))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Type Guide ")
                        .border_style(Style::default().fg(Color::Blue)),
                )
                .wrap(Wrap { trim: false }),
            sections[1],
        );
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = self.footer_lines();
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Keys ")
                    .border_style(Style::default().fg(Color::Blue)),
            ),
            area,
        );
    }

    fn footer_lines(&self) -> Vec<Line<'static>> {
        match (&self.screen, &self.current_page, self.main_focus) {
            (ScreenState::Main, PageKind::Models, MainFocus::Sections) => vec![
                Line::from(
                    "Focus: Sections  Up/Down choose section  Right/Enter open Models  s save  v validate  b bootstrap  q quit",
                ),
                Line::from("Models page: Up/Down choose model  a add  Enter/e edit  d delete"),
            ],
            (ScreenState::Main, PageKind::Channels, MainFocus::Sections) => vec![
                Line::from(
                    "Focus: Sections  Up/Down choose section  Right/Enter open Channels  s save  v validate  b bootstrap  q quit",
                ),
                Line::from("Channels page: Up/Down choose channel  a add  Enter/e edit  d delete"),
            ],
            (ScreenState::Main, _, MainFocus::Sections) => vec![
                Line::from(
                    "Focus: Sections  Up/Down choose section  Right/Enter open page  s save  v validate  b bootstrap  q quit",
                ),
                Line::from(self.page_action_hint()),
            ],
            (ScreenState::Main, PageKind::Models, MainFocus::Content) => vec![
                Line::from(
                    "Focus: Models  Up/Down choose model  Enter/e edit  a add  d delete  Left return",
                ),
                Line::from("File: s save config  v validate  b bootstrap latest skeleton  q quit"),
            ],
            (ScreenState::Main, PageKind::Channels, MainFocus::Content) => vec![
                Line::from(
                    "Focus: Channels  Up/Down choose channel  Enter/e edit  a add  d delete  Left return",
                ),
                Line::from("File: s save config  v validate  b bootstrap latest skeleton  q quit"),
            ],
            (ScreenState::Main, _, MainFocus::Content) => vec![
                Line::from(self.page_focus_hint()),
                Line::from("File: s save config  v validate  b bootstrap latest skeleton  q quit"),
            ],
            (ScreenState::ModelTypeWizard(_), _, _) => vec![
                Line::from(
                    "Choose model type: Up/Down move  Enter/Right continue  Esc/Left cancel",
                ),
                Line::from(
                    "Next step: after choosing a type, fill in the model form and press s to save",
                ),
            ],
            (ScreenState::ModelForm(_), _, _) => vec![
                Line::from(
                    "Model form: Up/Down move field  Enter edit  Space toggle bool  s save model  Esc cancel",
                ),
                Line::from(
                    "Tip: `capabilities` opens a multi-select picker. Changing `type` updates provider-specific fields.",
                ),
            ],
            (ScreenState::ChannelKindWizard(_), _, _) => vec![
                Line::from(
                    "Choose channel type: Up/Down move  Enter/Right continue  Esc/Left cancel",
                ),
                Line::from("Next step: fill in the channel form and press s to save"),
            ],
            (ScreenState::ChannelForm(_), _, _) => vec![
                Line::from(
                    "Channel form: Up/Down move field  Enter edit  s save channel  Esc cancel",
                ),
                Line::from(
                    "Telegram needs bot_token_env, DingTalk needs client env vars, and command_line keeps the local shell defaults.",
                ),
            ],
        }
    }

    fn page_action_hint(&self) -> &'static str {
        match self.current_page {
            PageKind::Overview => {
                "Overview: this page is read-only. Enter a section on the left to start editing."
            }
            PageKind::Models => {
                "Models: press Right/Enter to open. Then use a add, Enter/e edit, d delete."
            }
            PageKind::Tooling => {
                "Tooling: press Right/Enter to open. Then Enter edits the selected routing target."
            }
            PageKind::MainAgent => {
                "Main Agent: press Right/Enter to open. Then Enter edits values and Space toggles booleans."
            }
            PageKind::Runtime => {
                "Runtime: press Right/Enter to open. Then Enter edits the selected field."
            }
            PageKind::Sandbox => {
                "Sandbox: press Right/Enter to open. Then Enter edits the selected field."
            }
            PageKind::Channels => {
                "Channels: press Right/Enter to open. Then use a add, Enter/e edit, d delete."
            }
        }
    }

    fn page_focus_hint(&self) -> &'static str {
        match self.current_page {
            PageKind::Overview => {
                "Focus: Overview  Left return to sections  s save  v validate  b bootstrap  q quit"
            }
            PageKind::Models => {
                "Focus: Models  Up/Down choose model  Enter/e edit  a add  d delete  Left return"
            }
            PageKind::Tooling => {
                "Focus: Tooling  Up/Down choose field  Enter edit target  Left return"
            }
            PageKind::MainAgent => {
                "Focus: Main Agent  Up/Down choose field  Enter edit  Space toggle bool  Left return"
            }
            PageKind::Runtime => "Focus: Runtime  Up/Down choose field  Enter edit  Left return",
            PageKind::Sandbox => "Focus: Sandbox  Up/Down choose field  Enter edit  Left return",
            PageKind::Channels => {
                "Focus: Channels  Up/Down choose channel  Enter/e edit  a add  d delete  Left return"
            }
        }
    }

    fn render_modal(&self, frame: &mut Frame<'_>, modal: &ModalState) {
        let size = centered_rect(70, 35, frame.area());
        frame.render_widget(Clear, size);
        match modal {
            ModalState::TextInput(state) => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", state.title))
                    .border_style(Style::default().fg(Color::Yellow));
                let lines = vec![
                    Line::from(state.help.as_str()),
                    Line::from(""),
                    render_text_input_line(state),
                    Line::from(""),
                    Line::from(
                        "Left/Right move cursor, Enter confirm, Esc cancel, Backspace delete",
                    ),
                ];
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(block)
                        .wrap(Wrap { trim: false }),
                    size,
                );
            }
            ModalState::Select(state) => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", state.title))
                    .border_style(Style::default().fg(Color::Yellow));
                let items = state
                    .options
                    .iter()
                    .map(|item| ListItem::new(item.clone()))
                    .collect::<Vec<_>>();
                let mut list_state = ListState::default();
                list_state.select(Some(
                    state.selected.min(state.options.len().saturating_sub(1)),
                ));
                let list = List::new(items)
                    .block(block)
                    .highlight_style(
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol(">> ");
                frame.render_stateful_widget(list, size, &mut list_state);
            }
            ModalState::MultiSelect(state) => {
                let sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(8), Constraint::Length(5)])
                    .split(size);
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", state.title))
                    .border_style(Style::default().fg(Color::Yellow));
                let items = state
                    .options
                    .iter()
                    .map(|item| {
                        let mark = if item.checked { "[x]" } else { "[ ]" };
                        ListItem::new(format!("{mark} {}", item.label))
                    })
                    .collect::<Vec<_>>();
                let mut list_state = ListState::default();
                if !state.options.is_empty() {
                    list_state.select(Some(
                        state.selected.min(state.options.len().saturating_sub(1)),
                    ));
                }
                let list = List::new(items)
                    .block(block)
                    .highlight_style(
                        Style::default()
                            .bg(Color::Yellow)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol(">> ");
                frame.render_stateful_widget(list, sections[0], &mut list_state);
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(state.help.as_str()),
                        Line::from(""),
                        Line::from("Up/Down move, Space toggle, Enter confirm, Esc cancel"),
                    ])
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(" Help ")
                            .border_style(Style::default().fg(Color::Yellow)),
                    )
                    .wrap(Wrap { trim: false }),
                    sections[1],
                );
            }
            ModalState::Confirm(state) => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", state.title))
                    .border_style(Style::default().fg(Color::Yellow));
                let yes_style = if state.selected_yes {
                    Style::default()
                        .bg(Color::Yellow)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let no_style = if state.selected_yes {
                    Style::default()
                } else {
                    Style::default()
                        .bg(Color::Yellow)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD)
                };
                let lines = vec![
                    Line::from(state.message.as_str()),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("  Yes  ", yes_style),
                        Span::raw("    "),
                        Span::styled("  No  ", no_style),
                    ]),
                    Line::from(""),
                    Line::from("Left/Right or h/l switch, Enter confirm, Esc cancel"),
                ];
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(block)
                        .wrap(Wrap { trim: false }),
                    size,
                );
            }
            ModalState::Message(state) => {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", state.title))
                    .border_style(Style::default().fg(Color::Yellow));
                let lines = vec![
                    Line::from(state.message.as_str()),
                    Line::from(""),
                    Line::from("Press Enter, Esc, or q to close."),
                ];
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(block)
                        .wrap(Wrap { trim: false }),
                    size,
                );
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }

        match &self.screen {
            ScreenState::Main => self.handle_main_key(key),
            ScreenState::ModelTypeWizard(_) => self.handle_model_type_wizard_key(key),
            ScreenState::ModelForm(_) => self.handle_model_form_key(key),
            ScreenState::ChannelKindWizard(_) => self.handle_channel_kind_wizard_key(key),
            ScreenState::ChannelForm(_) => self.handle_channel_form_key(key),
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.handle_global_key(key)? {
            return Ok(());
        }

        if self.main_focus == MainFocus::Sections {
            return self.handle_sections_key(key);
        }

        match self.current_page {
            PageKind::Overview => self.handle_overview_key(key),
            PageKind::Models => self.handle_models_key(key),
            PageKind::Tooling => self.handle_tooling_key(key),
            PageKind::MainAgent => self.handle_main_agent_key(key),
            PageKind::Runtime => self.handle_runtime_key(key),
            PageKind::Sandbox => self.handle_sandbox_key(key),
            PageKind::Channels => self.handle_channels_key(key),
        }
    }

    fn handle_global_key(&mut self, key: KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Left => {
                if self.main_focus == MainFocus::Content {
                    self.main_focus = MainFocus::Sections;
                    self.status = StatusMessage::info(
                        "Section list focused. Use Up/Down to choose a page, Right to enter it.",
                    );
                    return Ok(true);
                }
            }
            KeyCode::Right | KeyCode::Enter => {
                if self.main_focus == MainFocus::Sections {
                    self.main_focus = MainFocus::Content;
                    self.status = StatusMessage::info(format!(
                        "{} focused. Use Up/Down inside the page, Left to return to sections.",
                        self.current_page.title()
                    ));
                    return Ok(true);
                }
            }
            KeyCode::Char('s') => {
                self.save()?;
                return Ok(true);
            }
            KeyCode::Char('v') => {
                let summary = validate_json_document(&self.value)?;
                self.status = StatusMessage::success(summary);
                return Ok(true);
            }
            KeyCode::Char('b') => {
                self.modal = Some(ModalState::Confirm(ConfirmState {
                    title: "Bootstrap Latest Skeleton".to_string(),
                    message: "Replace the current config with the latest empty skeleton?"
                        .to_string(),
                    selected_yes: false,
                    action: ConfirmAction::BootstrapLatest,
                }));
                return Ok(true);
            }
            KeyCode::Char('?') => {
                self.modal = Some(ModalState::Message(MessageState {
                    title: "Help".to_string(),
                    message: "Use Up and Down to move in the focused pane. On the left section list, Right or Enter opens the selected page. On the page itself, Left returns to the section list. Save with s, validate with v, and quit with q.".to_string(),
                }));
                return Ok(true);
            }
            KeyCode::Char('q') => {
                if self.dirty {
                    self.modal = Some(ModalState::Confirm(ConfirmState {
                        title: "Discard Unsaved Changes".to_string(),
                        message: "Exit the editor and discard unsaved changes?".to_string(),
                        selected_yes: false,
                        action: ConfirmAction::ExitDiscard,
                    }));
                } else {
                    self.should_quit = true;
                }
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_sections_key(&mut self, key: KeyEvent) -> Result<()> {
        let current = self.current_page.index();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.current_page = PageKind::from_index(current.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let next = (current + 1).min(PageKind::ALL.len().saturating_sub(1));
                self.current_page = PageKind::from_index(next);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_overview_key(&mut self, _key: KeyEvent) -> Result<()> {
        Ok(())
    }

    fn handle_models_key(&mut self, key: KeyEvent) -> Result<()> {
        let aliases = self.model_aliases();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.models_selected = self.models_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !aliases.is_empty() {
                    self.models_selected = (self.models_selected + 1).min(aliases.len() - 1);
                }
            }
            KeyCode::Char('a') => {
                self.screen = ScreenState::ModelTypeWizard(ModelTypeWizardState::default());
                self.status =
                    StatusMessage::info("Choose a model type, then fill in the model sheet.");
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(alias) = aliases.get(self.models_selected) {
                    self.screen =
                        ScreenState::ModelForm(ModelFormState::from_existing(&self.value, alias));
                    self.status = StatusMessage::info("Editing model. Press s to save changes.");
                }
            }
            KeyCode::Char('d') => {
                if let Some(alias) = aliases.get(self.models_selected) {
                    self.modal = Some(ModalState::Confirm(ConfirmState {
                        title: "Delete Model".to_string(),
                        message: format!("Delete model `{alias}`? References are not auto-fixed."),
                        selected_yes: false,
                        action: ConfirmAction::DeleteModel(alias.clone()),
                    }));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_tooling_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.tooling_selected = self.tooling_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.tooling_selected = (self.tooling_selected + 1).min(TOOLING_FIELD_COUNT - 1);
            }
            KeyCode::Enter => {
                let field = ToolingField::from_index(self.tooling_selected);
                self.open_text_input(
                    field.title(),
                    "Examples: sonar_pro, gpt54:self, or blank to clear",
                    top_level_tooling_value(&self.value, field.key()),
                    InputTarget::Tooling(field),
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_main_agent_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.main_agent_selected = self.main_agent_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.main_agent_selected =
                    (self.main_agent_selected + 1).min(MAIN_AGENT_FIELD_COUNT - 1);
            }
            KeyCode::Char(' ') => {
                self.toggle_main_agent_bool(MainAgentField::from_index(self.main_agent_selected));
            }
            KeyCode::Enter => {
                let field = MainAgentField::from_index(self.main_agent_selected);
                if field.is_bool() {
                    self.toggle_main_agent_bool(field);
                } else if field == MainAgentField::MemorySystem {
                    self.modal = Some(ModalState::Select(SelectState {
                        title: "Main Agent Memory System".to_string(),
                        options: vec!["layered".to_string(), "claude_code".to_string()],
                        selected: current_memory_system_index(&self.value),
                        target: SelectTarget::MainAgentMemorySystem,
                    }));
                } else {
                    self.open_text_input(
                        field.title(),
                        field.help(),
                        self.main_agent_field_value(field),
                        InputTarget::MainAgent(field),
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_runtime_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.runtime_selected = self.runtime_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.runtime_selected = (self.runtime_selected + 1).min(RUNTIME_FIELD_COUNT - 1);
            }
            KeyCode::Enter => {
                let field = RuntimeField::from_index(self.runtime_selected);
                self.open_text_input(
                    field.title(),
                    field.help(),
                    self.runtime_field_value(field),
                    InputTarget::Runtime(field),
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_sandbox_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.sandbox_selected = self.sandbox_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.sandbox_selected = (self.sandbox_selected + 1).min(SANDBOX_FIELD_COUNT - 1);
            }
            KeyCode::Enter => {
                let field = SandboxField::from_index(self.sandbox_selected);
                match field {
                    SandboxField::Mode => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Sandbox Mode".to_string(),
                            options: vec!["subprocess".to_string(), "bubblewrap".to_string()],
                            selected: current_sandbox_mode_index(&self.value),
                            target: SelectTarget::SandboxMode,
                        }));
                    }
                    SandboxField::BubblewrapBinary => {
                        self.open_text_input(
                            field.title(),
                            field.help(),
                            self.sandbox_field_value(field),
                            InputTarget::Sandbox(field),
                        );
                    }
                    SandboxField::MapDockerSocket => {
                        self.toggle_sandbox_bool(field);
                    }
                }
            }
            KeyCode::Char(' ') => {
                let field = SandboxField::from_index(self.sandbox_selected);
                if field.is_bool() {
                    self.toggle_sandbox_bool(field);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_channels_key(&mut self, key: KeyEvent) -> Result<()> {
        let channels = self.channels();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.channels_selected = self.channels_selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !channels.is_empty() {
                    self.channels_selected = (self.channels_selected + 1).min(channels.len() - 1);
                }
            }
            KeyCode::Char('a') => {
                self.screen = ScreenState::ChannelKindWizard(ChannelKindWizardState::default());
                self.status =
                    StatusMessage::info("Choose a channel type, then fill in the channel sheet.");
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(channel) = channels.get(self.channels_selected) {
                    self.screen = ScreenState::ChannelForm(ChannelFormState::from_existing(
                        self.channels_selected,
                        channel,
                    ));
                    self.status = StatusMessage::info("Editing channel. Press s to save changes.");
                }
            }
            KeyCode::Char('d') => {
                if let Some(channel) = channels.get(self.channels_selected) {
                    self.modal = Some(ModalState::Confirm(ConfirmState {
                        title: "Delete Channel".to_string(),
                        message: format!("Delete channel `{}`?", channel.id),
                        selected_yes: false,
                        action: ConfirmAction::DeleteChannel(self.channels_selected),
                    }));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_model_form_key(&mut self, key: KeyEvent) -> Result<()> {
        let ScreenState::ModelForm(_) = &self.screen else {
            return Ok(());
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.selected = form.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    let field_count = form.fields().len();
                    form.selected = (form.selected + 1).min(field_count.saturating_sub(1));
                }
            }
            KeyCode::Char(' ') => {
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.toggle_field(form.selected_field());
                }
            }
            KeyCode::Enter => {
                let (field, model_type, current_value) = match &self.screen {
                    ScreenState::ModelForm(form) => {
                        let field = form.selected_field();
                        (field, form.model_type.clone(), form.field_value(field))
                    }
                    _ => unreachable!(),
                };
                match field {
                    ModelFormField::ModelType => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Model Type".to_string(),
                            options: vec![
                                "openrouter".to_string(),
                                "openrouter-resp".to_string(),
                                "codex-subscription".to_string(),
                            ],
                            selected: model_type_index(&model_type),
                            target: SelectTarget::ModelForm(field),
                        }));
                    }
                    ModelFormField::RetryMode => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Retry Mode".to_string(),
                            options: vec!["no".to_string(), "random".to_string()],
                            selected: retry_mode_index(&current_value),
                            target: SelectTarget::ModelForm(field),
                        }));
                    }
                    ModelFormField::TokenTemplateSource => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Token Template Source".to_string(),
                            options: vec![
                                "builtin".to_string(),
                                "local".to_string(),
                                "huggingface".to_string(),
                            ],
                            selected: token_template_source_index(&current_value),
                            target: SelectTarget::ModelForm(field),
                        }));
                    }
                    ModelFormField::TokenTokenizerSource => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Token Tokenizer Source".to_string(),
                            options: vec![
                                "tiktoken".to_string(),
                                "local".to_string(),
                                "huggingface".to_string(),
                            ],
                            selected: token_tokenizer_source_index(&current_value),
                            target: SelectTarget::ModelForm(field),
                        }));
                    }
                    ModelFormField::TokenTokenizerEncoding => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Tiktoken Encoding".to_string(),
                            options: vec![
                                "auto".to_string(),
                                "o200k_base".to_string(),
                                "cl100k_base".to_string(),
                                "o200k_harmony".to_string(),
                            ],
                            selected: token_tokenizer_encoding_index(&current_value),
                            target: SelectTarget::ModelForm(field),
                        }));
                    }
                    ModelFormField::Capabilities => {
                        self.open_model_capabilities_picker();
                    }
                    field if field.is_bool() => {
                        if let ScreenState::ModelForm(form) = &mut self.screen {
                            form.toggle_field(field);
                        }
                    }
                    _ => {
                        self.open_text_input(
                            field.title(),
                            field.help(),
                            current_value,
                            InputTarget::ModelForm(field),
                        );
                    }
                }
            }
            KeyCode::Esc => {
                self.screen = ScreenState::Main;
                self.status = StatusMessage::warning("Model edits cancelled.");
            }
            KeyCode::Char('s') => {
                let form = match &self.screen {
                    ScreenState::ModelForm(form) => form.clone(),
                    _ => unreachable!(),
                };
                let alias = self.apply_model_form(&form)?;
                self.screen = ScreenState::Main;
                self.current_page = PageKind::Models;
                self.models_selected = self
                    .model_aliases()
                    .iter()
                    .position(|value| value == &alias)
                    .unwrap_or(0);
                self.status = StatusMessage::success(format!("Saved model `{alias}`."));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_channel_form_key(&mut self, key: KeyEvent) -> Result<()> {
        let ScreenState::ChannelForm(_) = &self.screen else {
            return Ok(());
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let ScreenState::ChannelForm(form) = &mut self.screen {
                    form.selected = form.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let field_count = match &self.screen {
                    ScreenState::ChannelForm(form) => form.fields().len(),
                    _ => 0,
                };
                if field_count > 0 {
                    if let ScreenState::ChannelForm(form) = &mut self.screen {
                        form.selected = (form.selected + 1).min(field_count - 1);
                    }
                }
            }
            KeyCode::Enter => {
                let (field, kind, current_value) = match &self.screen {
                    ScreenState::ChannelForm(form) => {
                        let field = form.selected_field();
                        (field, form.kind.clone(), form.field_value(field))
                    }
                    _ => unreachable!(),
                };
                match field {
                    ChannelFormField::Kind => {
                        self.modal = Some(ModalState::Select(SelectState {
                            title: "Channel Kind".to_string(),
                            options: vec![
                                "command_line".to_string(),
                                "telegram".to_string(),
                                "dingtalk".to_string(),
                            ],
                            selected: match kind.as_str() {
                                "telegram" => 1,
                                "dingtalk" => 2,
                                _ => 0,
                            },
                            target: SelectTarget::ChannelKind,
                        }));
                    }
                    _ => {
                        self.open_text_input(
                            field.title(),
                            field.help(),
                            current_value,
                            InputTarget::ChannelForm(field),
                        );
                    }
                }
            }
            KeyCode::Esc => {
                self.screen = ScreenState::Main;
                self.status = StatusMessage::warning("Channel edits cancelled.");
            }
            KeyCode::Char('s') => {
                let form = match &self.screen {
                    ScreenState::ChannelForm(form) => form.clone(),
                    _ => unreachable!(),
                };
                let id = self.apply_channel_form(&form)?;
                self.screen = ScreenState::Main;
                self.current_page = PageKind::Channels;
                self.channels_selected = self
                    .channels()
                    .iter()
                    .position(|channel| channel.id == id)
                    .unwrap_or(0);
                self.status = StatusMessage::success(format!("Saved channel `{id}`."));
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_model_type_wizard_key(&mut self, key: KeyEvent) -> Result<()> {
        let ScreenState::ModelTypeWizard(_) = &self.screen else {
            return Ok(());
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let ScreenState::ModelTypeWizard(state) = &mut self.screen {
                    state.selected = state.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let ScreenState::ModelTypeWizard(state) = &mut self.screen {
                    state.selected = (state.selected + 1)
                        .min(ModelTypeWizardState::OPTIONS.len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                let model_type = match &self.screen {
                    ScreenState::ModelTypeWizard(state) => state.selected_type().to_string(),
                    _ => unreachable!(),
                };
                self.screen = ScreenState::ModelForm(ModelFormState::new_with_type(&model_type));
                self.status = StatusMessage::info(
                    "Fill in the model sheet and press s to save the new model.",
                );
            }
            KeyCode::Esc | KeyCode::Left => {
                self.screen = ScreenState::Main;
                self.status = StatusMessage::warning("Model creation cancelled.");
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_channel_kind_wizard_key(&mut self, key: KeyEvent) -> Result<()> {
        let ScreenState::ChannelKindWizard(_) = &self.screen else {
            return Ok(());
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let ScreenState::ChannelKindWizard(state) = &mut self.screen {
                    state.selected = state.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let ScreenState::ChannelKindWizard(state) = &mut self.screen {
                    state.selected = (state.selected + 1)
                        .min(ChannelKindWizardState::OPTIONS.len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                let kind = match &self.screen {
                    ScreenState::ChannelKindWizard(state) => state.selected_kind().to_string(),
                    _ => unreachable!(),
                };
                self.screen = ScreenState::ChannelForm(ChannelFormState::new_with_kind(&kind));
                self.status = StatusMessage::info(
                    "Fill in the channel sheet and press s to save the new channel.",
                );
            }
            KeyCode::Esc | KeyCode::Left => {
                self.screen = ScreenState::Main;
                self.status = StatusMessage::warning("Channel creation cancelled.");
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(modal) = self.modal.as_mut() else {
            return Ok(());
        };
        match modal {
            ModalState::TextInput(state) => match key.code {
                KeyCode::Esc => {
                    self.modal = None;
                }
                KeyCode::Backspace => {
                    delete_before_cursor(&mut state.value, &mut state.cursor);
                }
                KeyCode::Left => {
                    state.cursor = state.cursor.saturating_sub(1);
                }
                KeyCode::Right => {
                    state.cursor = (state.cursor + 1).min(char_count(&state.value));
                }
                KeyCode::Enter => {
                    let target = state.target;
                    let value = state.value.clone();
                    self.modal = None;
                    self.apply_text_input(target, value)?;
                }
                KeyCode::Char(ch) => {
                    if key.modifiers != KeyModifiers::CONTROL {
                        insert_at_cursor(&mut state.value, &mut state.cursor, ch);
                    }
                }
                _ => {}
            },
            ModalState::Select(state) => match key.code {
                KeyCode::Esc => {
                    self.modal = None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.selected = state.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !state.options.is_empty() {
                        state.selected = (state.selected + 1).min(state.options.len() - 1);
                    }
                }
                KeyCode::Enter => {
                    let target = state.target;
                    let selected = state.selected;
                    self.modal = None;
                    self.apply_select(target, selected)?;
                }
                _ => {}
            },
            ModalState::MultiSelect(state) => match key.code {
                KeyCode::Esc => {
                    self.modal = None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.selected = state.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !state.options.is_empty() {
                        state.selected = (state.selected + 1).min(state.options.len() - 1);
                    }
                }
                KeyCode::Char(' ') => {
                    if let Some(option) = state.options.get_mut(state.selected) {
                        option.checked = !option.checked;
                    }
                }
                KeyCode::Enter => {
                    let target = state.target;
                    let selected = state
                        .options
                        .iter()
                        .filter(|option| option.checked)
                        .map(|option| option.label.clone())
                        .collect::<Vec<_>>();
                    self.modal = None;
                    self.apply_multi_select(target, selected);
                }
                _ => {}
            },
            ModalState::Confirm(state) => match key.code {
                KeyCode::Esc => {
                    self.modal = None;
                }
                KeyCode::Left | KeyCode::Char('h') | KeyCode::Right | KeyCode::Char('l') => {
                    state.selected_yes = !state.selected_yes;
                }
                KeyCode::Enter => {
                    let action = state.action.clone();
                    let confirmed = state.selected_yes;
                    self.modal = None;
                    if confirmed {
                        self.apply_confirm(action)?;
                    }
                }
                _ => {}
            },
            ModalState::Message(_) => match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                    self.modal = None;
                }
                _ => {}
            },
        }
        Ok(())
    }

    fn apply_text_input(&mut self, target: InputTarget, value: String) -> Result<()> {
        match target {
            InputTarget::Tooling(field) => self.set_tooling_value(field, &value),
            InputTarget::MainAgent(field) => self.set_main_agent_value(field, &value),
            InputTarget::Runtime(field) => self.set_runtime_value(field, &value),
            InputTarget::Sandbox(field) => self.set_sandbox_value(field, &value),
            InputTarget::ModelForm(field) => {
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.set_field_value(field, value);
                }
                Ok(())
            }
            InputTarget::ChannelForm(field) => {
                if let ScreenState::ChannelForm(form) = &mut self.screen {
                    form.set_field_value(field, value);
                }
                Ok(())
            }
        }
    }

    fn apply_select(&mut self, target: SelectTarget, selected: usize) -> Result<()> {
        match target {
            SelectTarget::SandboxMode => {
                let modes = ["subprocess", "bubblewrap"];
                let value = modes
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid sandbox mode selection"))?;
                self.set_sandbox_value(SandboxField::Mode, value)
            }
            SelectTarget::MainAgentMemorySystem => {
                let systems = ["layered", "claude_code"];
                let value = systems
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid memory system selection"))?;
                self.set_main_agent_value(MainAgentField::MemorySystem, value)
            }
            SelectTarget::ModelForm(ModelFormField::ModelType) => {
                let types = ["openrouter", "openrouter-resp", "codex-subscription"];
                let value = types
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid model type selection"))?;
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.apply_model_type_defaults(value);
                }
                Ok(())
            }
            SelectTarget::ModelForm(ModelFormField::RetryMode) => {
                let modes = ["no", "random"];
                let value = modes
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid retry mode selection"))?;
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.retry_mode = (*value).to_string();
                    form.selected = form
                        .selected
                        .min(form.visible_fields().len().saturating_sub(1));
                }
                Ok(())
            }
            SelectTarget::ModelForm(ModelFormField::TokenTemplateSource) => {
                let sources = ["builtin", "local", "huggingface"];
                let value = sources
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid token template source selection"))?;
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.token_template_source = (*value).to_string();
                    form.selected = form
                        .selected
                        .min(form.visible_fields().len().saturating_sub(1));
                }
                Ok(())
            }
            SelectTarget::ModelForm(ModelFormField::TokenTokenizerSource) => {
                let sources = ["tiktoken", "local", "huggingface"];
                let value = sources
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid token tokenizer source selection"))?;
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.token_tokenizer_source = (*value).to_string();
                    form.selected = form
                        .selected
                        .min(form.visible_fields().len().saturating_sub(1));
                }
                Ok(())
            }
            SelectTarget::ModelForm(ModelFormField::TokenTokenizerEncoding) => {
                let encodings = ["auto", "o200k_base", "cl100k_base", "o200k_harmony"];
                let value = encodings
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid token tokenizer encoding selection"))?;
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.token_tokenizer_encoding = (*value).to_string();
                }
                Ok(())
            }
            SelectTarget::ChannelKind => {
                let kinds = ["command_line", "telegram", "dingtalk"];
                let value = kinds
                    .get(selected)
                    .ok_or_else(|| anyhow!("invalid channel kind selection"))?;
                if let ScreenState::ChannelForm(form) = &mut self.screen {
                    form.kind = (*value).to_string();
                    form.selected = 0;
                }
                Ok(())
            }
            SelectTarget::ModelForm(_) => Ok(()),
        }
    }

    fn apply_confirm(&mut self, action: ConfirmAction) -> Result<()> {
        match action {
            ConfirmAction::ExitDiscard => {
                self.should_quit = true;
            }
            ConfirmAction::BootstrapLatest => {
                self.value = latest_server_config_skeleton();
                self.dirty = true;
                self.screen = ScreenState::Main;
                self.current_page = PageKind::Overview;
                self.status = StatusMessage::warning(
                    "Replaced config with the latest empty skeleton. Save to persist it.",
                );
            }
            ConfirmAction::DeleteModel(alias) => {
                self.delete_model(&alias)?;
                let aliases = self.model_aliases();
                if aliases.is_empty() {
                    self.models_selected = 0;
                } else {
                    self.models_selected = self.models_selected.min(aliases.len() - 1);
                }
                self.status = StatusMessage::success(format!("Deleted model `{alias}`."));
            }
            ConfirmAction::DeleteChannel(index) => {
                self.delete_channel(index)?;
                let channels = self.channels();
                if channels.is_empty() {
                    self.channels_selected = 0;
                } else {
                    self.channels_selected = self.channels_selected.min(channels.len() - 1);
                }
                self.status = StatusMessage::success("Deleted channel.");
            }
        }
        Ok(())
    }

    fn apply_multi_select(&mut self, target: MultiSelectTarget, selected: Vec<String>) {
        match target {
            MultiSelectTarget::ModelCapabilities => {
                if let ScreenState::ModelForm(form) = &mut self.screen {
                    form.set_capabilities(selected);
                }
            }
        }
    }

    fn open_text_input(&mut self, title: &str, help: &str, value: String, target: InputTarget) {
        self.modal = Some(ModalState::TextInput(TextInputState {
            title: title.to_string(),
            help: help.to_string(),
            cursor: char_count(&value),
            value,
            target,
        }));
    }

    fn open_model_capabilities_picker(&mut self) {
        let ScreenState::ModelForm(form) = &self.screen else {
            return;
        };
        let selected = form.capabilities_list();
        let options = model_capability_options(&selected)
            .into_iter()
            .map(|label| MultiSelectOption {
                checked: selected.iter().any(|item| item == &label),
                label,
            })
            .collect::<Vec<_>>();
        self.modal = Some(ModalState::MultiSelect(MultiSelectState {
            title: "Model Capabilities".to_string(),
            help: "Choose every capability this model should advertise for routing and validation."
                .to_string(),
            options,
            selected: 0,
            target: MultiSelectTarget::ModelCapabilities,
        }));
    }

    fn set_tooling_value(&mut self, field: ToolingField, raw: &str) -> Result<()> {
        let tooling = ensure_nested_object_mut(&mut self.value, &["tooling"]);
        set_optional_trimmed_string(tooling, field.key(), raw);
        self.dirty = true;
        self.status = StatusMessage::success(format!("Updated tooling.{}.", field.key()));
        Ok(())
    }

    fn toggle_main_agent_bool(&mut self, field: MainAgentField) {
        let value = !self.main_agent_bool_value(field);
        let target = match field {
            MainAgentField::TimeAwarenessEmitSystemDateOnUserMessage
            | MainAgentField::TimeAwarenessEmitIdleTimeGapHint => {
                ensure_nested_object_mut(&mut self.value, &["main_agent", "time_awareness"])
            }
            MainAgentField::EnableContextCompression => {
                ensure_nested_object_mut(&mut self.value, &["main_agent"])
            }
            MainAgentField::IdleCompactionEnabled => {
                ensure_nested_object_mut(&mut self.value, &["main_agent", "idle_compaction"])
            }
            MainAgentField::TimeoutObservationCompactionEnabled => ensure_nested_object_mut(
                &mut self.value,
                &["main_agent", "timeout_observation_compaction"],
            ),
            _ => return,
        };
        set_bool(target, field.key(), value);
        self.dirty = true;
        self.status = StatusMessage::success(format!("Set {} to {}.", field.key(), value));
    }

    fn set_main_agent_value(&mut self, field: MainAgentField, raw: &str) -> Result<()> {
        match field {
            MainAgentField::GlobalInstallRoot
            | MainAgentField::TokenEstimationTemplateHfCache
            | MainAgentField::TokenEstimationTokenizerHfCache
            | MainAgentField::Language
            | MainAgentField::MemorySystem => match field {
                MainAgentField::MemorySystem => {
                    let main_agent = ensure_nested_object_mut(&mut self.value, &["main_agent"]);
                    let memory_system = parse_memory_system(raw)?;
                    set_string(main_agent, field.key(), memory_system.as_config_value());
                }
                MainAgentField::TokenEstimationTemplateHfCache => {
                    let cache = ensure_nested_object_mut(
                        &mut self.value,
                        &["main_agent", "token_estimation_cache", "template"],
                    );
                    set_optional_trimmed_string(cache, field.key(), raw);
                }
                MainAgentField::TokenEstimationTokenizerHfCache => {
                    let cache = ensure_nested_object_mut(
                        &mut self.value,
                        &["main_agent", "token_estimation_cache", "tokenizer"],
                    );
                    set_optional_trimmed_string(cache, field.key(), raw);
                }
                _ => {
                    let main_agent = ensure_nested_object_mut(&mut self.value, &["main_agent"]);
                    set_optional_trimmed_string(main_agent, field.key(), raw);
                }
            },
            MainAgentField::TimeAwarenessEmitSystemDateOnUserMessage
            | MainAgentField::TimeAwarenessEmitIdleTimeGapHint
            | MainAgentField::EnableContextCompression
            | MainAgentField::IdleCompactionEnabled
            | MainAgentField::TimeoutObservationCompactionEnabled => {
                return Ok(());
            }
            MainAgentField::ContextTriggerRatio => {
                let value = parse_f64(raw)?.ok_or_else(|| anyhow!("value must not be empty"))?;
                let section = ensure_nested_object_mut(
                    &mut self.value,
                    &["main_agent", "context_compaction"],
                );
                set_f64(section, field.key(), value)?;
            }
            MainAgentField::ContextTokenLimitOverride => {
                let section = ensure_nested_object_mut(
                    &mut self.value,
                    &["main_agent", "context_compaction"],
                );
                match parse_u64(raw)? {
                    Some(value) => set_u64(section, field.key(), value),
                    None => {
                        section.remove(field.key());
                    }
                }
            }
            MainAgentField::ContextRecentFidelityTargetRatio => {
                let value = parse_f64(raw)?.ok_or_else(|| anyhow!("value must not be empty"))?;
                let section = ensure_nested_object_mut(
                    &mut self.value,
                    &["main_agent", "context_compaction"],
                );
                set_f64(section, field.key(), value)?;
            }
            MainAgentField::IdleCompactionPollIntervalSeconds => {
                let value = parse_u64(raw)?.ok_or_else(|| anyhow!("value must not be empty"))?;
                let section =
                    ensure_nested_object_mut(&mut self.value, &["main_agent", "idle_compaction"]);
                set_u64(section, field.key(), value);
            }
            MainAgentField::IdleCompactionMinRatio => {
                let value = parse_f64(raw)?.ok_or_else(|| anyhow!("value must not be empty"))?;
                let section =
                    ensure_nested_object_mut(&mut self.value, &["main_agent", "idle_compaction"]);
                set_f64(section, field.key(), value)?;
            }
        }
        self.dirty = true;
        self.status = StatusMessage::success(format!("Updated {}.", field.title()));
        Ok(())
    }

    fn set_runtime_value(&mut self, field: RuntimeField, raw: &str) -> Result<()> {
        let value = parse_u64(raw)?.ok_or_else(|| anyhow!("value must not be empty"))?;
        let root = root_object_mut(&mut self.value);
        set_u64(root, field.key(), value);
        self.dirty = true;
        self.status = StatusMessage::success(format!("Updated {}.", field.title()));
        Ok(())
    }

    fn set_sandbox_value(&mut self, field: SandboxField, raw: &str) -> Result<()> {
        let sandbox = ensure_nested_object_mut(&mut self.value, &["sandbox"]);
        match field {
            SandboxField::Mode => set_optional_trimmed_string(sandbox, field.key(), raw),
            SandboxField::BubblewrapBinary => {
                set_optional_trimmed_string(sandbox, field.key(), raw)
            }
            SandboxField::MapDockerSocket => {
                let parsed = parse_bool(raw, field.title())?;
                set_bool(sandbox, field.key(), parsed);
            }
        }
        self.dirty = true;
        self.status = StatusMessage::success(format!("Updated sandbox {}.", field.key()));
        Ok(())
    }

    fn toggle_sandbox_bool(&mut self, field: SandboxField) {
        if !field.is_bool() {
            return;
        }
        let value = !nested_bool(&self.value, &["sandbox", field.key()], false);
        let sandbox = ensure_nested_object_mut(&mut self.value, &["sandbox"]);
        set_bool(sandbox, field.key(), value);
        self.dirty = true;
        self.status = StatusMessage::success(format!("Set sandbox {} to {}.", field.key(), value));
    }

    fn apply_model_form(&mut self, form: &ModelFormState) -> Result<String> {
        let alias = trim_non_empty(&form.alias, "model alias")?.to_string();
        let existing_alias = form.existing_alias.as_deref();
        {
            let models = ensure_nested_object_mut(&mut self.value, &["models"]);
            if models.contains_key(&alias) && existing_alias != Some(alias.as_str()) {
                bail!("model alias `{alias}` already exists");
            }
        }

        let mut model = form.raw.clone();
        set_string(&mut model, "type", &form.model_type);
        set_string(
            &mut model,
            "api_endpoint",
            &trim_non_empty(&form.api_endpoint, "api_endpoint")?,
        );
        set_string(
            &mut model,
            "model",
            &trim_non_empty(&form.model_name, "model name")?,
        );
        set_optional_trimmed_string(&mut model, "api_key_env", &form.api_key_env);
        set_optional_trimmed_string(
            &mut model,
            "chat_completions_path",
            &form.chat_completions_path,
        );
        set_optional_trimmed_string(&mut model, "codex_home", &form.codex_home);
        set_optional_trimmed_string(
            &mut model,
            "auth_credentials_store_mode",
            &form.auth_credentials_store_mode,
        );
        set_optional_trimmed_string(&mut model, "description", &form.description);
        set_optional_trimmed_string(&mut model, "image_tool_model", &form.image_tool_model);
        set_optional_trimmed_string(&mut model, "web_search", &form.model_web_search);
        set_bool(
            &mut model,
            "supports_vision_input",
            form.supports_vision_input,
        );
        set_bool(&mut model, "agent_model_enabled", form.agent_model_enabled);
        if let Some(value) = parse_f64(&form.timeout_seconds)? {
            set_f64(&mut model, "timeout_seconds", value)?;
        } else {
            model.remove("timeout_seconds");
        }
        set_retry_mode_object(
            &mut model,
            &form.retry_mode,
            &form.retry_max_retries,
            &form.retry_random_mean,
        )?;
        if let Some(value) = parse_u64(&form.context_window_tokens)? {
            set_u64(&mut model, "context_window_tokens", value);
        } else {
            model.remove("context_window_tokens");
        }
        set_token_estimation_object(
            &mut model,
            &form.token_template_source,
            &form.token_template_path,
            &form.token_template_repo,
            &form.token_template_revision,
            &form.token_template_file,
            &form.token_template_field,
            &form.token_template_cache_dir,
            &form.token_tokenizer_source,
            &form.token_tokenizer_encoding,
            &form.token_tokenizer_path,
            &form.token_tokenizer_repo,
            &form.token_tokenizer_revision,
            &form.token_tokenizer_file,
            &form.token_tokenizer_cache_dir,
        )?;

        let capabilities = parse_csv_items(&form.capabilities)
            .into_iter()
            .map(Value::String)
            .collect::<Vec<_>>();
        if capabilities.is_empty() {
            model.remove("capabilities");
        } else {
            model.insert("capabilities".to_string(), Value::Array(capabilities));
        }

        match model.get_mut("native_web_search") {
            Some(Value::Object(native)) => {
                set_bool(native, "enabled", form.native_web_search_enabled);
            }
            _ if form.native_web_search_enabled => {
                model.insert(
                    "native_web_search".to_string(),
                    json!({
                        "enabled": true,
                        "payload": {}
                    }),
                );
            }
            _ => {}
        }

        let models = ensure_nested_object_mut(&mut self.value, &["models"]);
        if let Some(existing_alias) = existing_alias
            && existing_alias != alias
        {
            models.remove(existing_alias);
        }
        models.insert(alias.clone(), Value::Object(model));

        update_backend_membership(
            &mut self.value,
            existing_alias,
            &alias,
            "agent_frame",
            form.agent_frame_enabled,
        );

        self.dirty = true;
        Ok(alias)
    }

    fn apply_channel_form(&mut self, form: &ChannelFormState) -> Result<String> {
        let id = trim_non_empty(&form.id, "channel id")?.to_string();
        let mut channel = form.raw.clone();
        set_string(&mut channel, "kind", &form.kind);
        set_string(&mut channel, "id", &id);

        if form.kind == "command_line" {
            set_optional_trimmed_string(&mut channel, "prompt", &form.prompt);
            channel.remove("bot_token");
            channel.remove("bot_token_env");
            channel.remove("client_id");
            channel.remove("client_id_env");
            channel.remove("client_secret");
            channel.remove("client_secret_env");
            channel.remove("api_base_url");
            channel.remove("commands");
        } else if form.kind == "telegram" {
            set_optional_trimmed_string(&mut channel, "bot_token_env", &form.bot_token_env);
            channel.remove("bot_token");
            channel.remove("client_id");
            channel.remove("client_id_env");
            channel.remove("client_secret");
            channel.remove("client_secret_env");
            channel.remove("commands");
            channel.remove("prompt");
        } else {
            set_optional_trimmed_string(&mut channel, "client_id_env", &form.client_id_env);
            set_optional_trimmed_string(&mut channel, "client_secret_env", &form.client_secret_env);
            set_optional_trimmed_string(&mut channel, "api_base_url", &form.api_base_url);
            channel.remove("client_id");
            channel.remove("client_secret");
            channel.remove("bot_token");
            channel.remove("bot_token_env");
            channel.remove("commands");
            channel.remove("prompt");
        }

        let channels = ensure_array_mut(&mut self.value, &["channels"]);
        let item = Value::Object(channel);
        if let Some(index) = form.existing_index {
            let duplicate = channels.iter().enumerate().any(|(other_index, value)| {
                other_index != index && value.get("id").and_then(Value::as_str) == Some(id.as_str())
            });
            if duplicate {
                bail!("another channel already uses id `{id}`");
            }
            if index >= channels.len() {
                bail!("channel index out of bounds");
            }
            channels[index] = item;
        } else {
            if channels
                .iter()
                .any(|value| value.get("id").and_then(Value::as_str) == Some(id.as_str()))
            {
                bail!("channel id `{id}` already exists");
            }
            channels.push(item);
        }

        self.dirty = true;
        Ok(id)
    }

    fn delete_model(&mut self, alias: &str) -> Result<()> {
        let models = ensure_nested_object_mut(&mut self.value, &["models"]);
        if models.remove(alias).is_none() {
            bail!("model `{alias}` does not exist");
        }
        remove_backend_alias(&mut self.value, "agent_frame", alias);
        self.dirty = true;
        Ok(())
    }

    fn delete_channel(&mut self, index: usize) -> Result<()> {
        let channels = ensure_array_mut(&mut self.value, &["channels"]);
        if index >= channels.len() {
            bail!("channel index out of bounds");
        }
        channels.remove(index);
        self.dirty = true;
        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        save_json_document(&self.path, &self.value)?;
        self.dirty = false;
        self.status = StatusMessage::success(format!("Saved {}", self.path.display()));
        Ok(())
    }

    fn show_error(&mut self, error: anyhow::Error) {
        self.modal = Some(ModalState::Message(MessageState {
            title: "Error".to_string(),
            message: format!("{error:#}"),
        }));
        self.status = StatusMessage::error("The last action failed. See the dialog for details.");
    }

    fn model_aliases(&self) -> Vec<String> {
        let mut aliases = nested_object(&self.value, &["models"])
            .map(|models| models.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        aliases.sort();
        aliases
    }

    fn channels(&self) -> Vec<ChannelSummary> {
        nested_array(&self.value, &["channels"])
            .map(|channels| {
                channels
                    .iter()
                    .filter_map(|value| {
                        let raw = value.as_object()?.clone();
                        let id = raw.get("id").and_then(Value::as_str).unwrap_or("unnamed");
                        let kind = raw.get("kind").and_then(Value::as_str).unwrap_or("unknown");
                        Some(ChannelSummary {
                            id: id.to_string(),
                            kind: kind.to_string(),
                            raw,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    fn main_agent_field_value(&self, field: MainAgentField) -> String {
        match field {
            MainAgentField::GlobalInstallRoot => {
                nested_string(&self.value, &["main_agent", "global_install_root"])
                    .unwrap_or("/opt")
                    .to_string()
            }
            MainAgentField::TokenEstimationTemplateHfCache => nested_string(
                &self.value,
                &["main_agent", "token_estimation_cache", "template", "hf"],
            )
            .unwrap_or("template-cache/hf")
            .to_string(),
            MainAgentField::TokenEstimationTokenizerHfCache => nested_string(
                &self.value,
                &["main_agent", "token_estimation_cache", "tokenizer", "hf"],
            )
            .unwrap_or("tokenizer-cache/hf")
            .to_string(),
            MainAgentField::Language => nested_string(&self.value, &["main_agent", "language"])
                .unwrap_or("zh-CN")
                .to_string(),
            MainAgentField::MemorySystem => {
                nested_string(&self.value, &["main_agent", "memory_system"])
                    .unwrap_or("layered")
                    .to_string()
            }
            MainAgentField::TimeAwarenessEmitSystemDateOnUserMessage => bool_string(nested_bool(
                &self.value,
                &[
                    "main_agent",
                    "time_awareness",
                    "emit_system_date_on_user_message",
                ],
                false,
            )),
            MainAgentField::TimeAwarenessEmitIdleTimeGapHint => bool_string(nested_bool(
                &self.value,
                &["main_agent", "time_awareness", "emit_idle_time_gap_hint"],
                true,
            )),
            MainAgentField::EnableContextCompression => bool_string(nested_bool(
                &self.value,
                &["main_agent", "enable_context_compression"],
                true,
            )),
            MainAgentField::ContextTriggerRatio => nested_number_string(
                &self.value,
                &["main_agent", "context_compaction", "trigger_ratio"],
            )
            .unwrap_or_else(|| "0.9".to_string()),
            MainAgentField::ContextTokenLimitOverride => nested_number_string(
                &self.value,
                &["main_agent", "context_compaction", "token_limit_override"],
            )
            .unwrap_or_default(),
            MainAgentField::ContextRecentFidelityTargetRatio => nested_number_string(
                &self.value,
                &[
                    "main_agent",
                    "context_compaction",
                    "recent_fidelity_target_ratio",
                ],
            )
            .unwrap_or_else(|| "0.18".to_string()),
            MainAgentField::IdleCompactionEnabled => bool_string(nested_bool(
                &self.value,
                &["main_agent", "idle_compaction", "enabled"],
                false,
            )),
            MainAgentField::IdleCompactionPollIntervalSeconds => nested_number_string(
                &self.value,
                &["main_agent", "idle_compaction", "poll_interval_seconds"],
            )
            .unwrap_or_else(|| "15".to_string()),
            MainAgentField::IdleCompactionMinRatio => {
                nested_number_string(&self.value, &["main_agent", "idle_compaction", "min_ratio"])
                    .unwrap_or_else(|| "0.5".to_string())
            }
            MainAgentField::TimeoutObservationCompactionEnabled => bool_string(nested_bool(
                &self.value,
                &["main_agent", "timeout_observation_compaction", "enabled"],
                true,
            )),
        }
    }

    fn main_agent_bool_value(&self, field: MainAgentField) -> bool {
        match field {
            MainAgentField::TimeAwarenessEmitSystemDateOnUserMessage => nested_bool(
                &self.value,
                &[
                    "main_agent",
                    "time_awareness",
                    "emit_system_date_on_user_message",
                ],
                false,
            ),
            MainAgentField::TimeAwarenessEmitIdleTimeGapHint => nested_bool(
                &self.value,
                &["main_agent", "time_awareness", "emit_idle_time_gap_hint"],
                true,
            ),
            MainAgentField::EnableContextCompression => nested_bool(
                &self.value,
                &["main_agent", "enable_context_compression"],
                true,
            ),
            MainAgentField::IdleCompactionEnabled => nested_bool(
                &self.value,
                &["main_agent", "idle_compaction", "enabled"],
                false,
            ),
            MainAgentField::TimeoutObservationCompactionEnabled => nested_bool(
                &self.value,
                &["main_agent", "timeout_observation_compaction", "enabled"],
                true,
            ),
            _ => false,
        }
    }

    fn runtime_field_value(&self, field: RuntimeField) -> String {
        nested_number_string(&self.value, &[field.key()])
            .unwrap_or_else(|| field.default().to_string())
    }

    fn sandbox_field_value(&self, field: SandboxField) -> String {
        nested_string(&self.value, &["sandbox", field.key()])
            .unwrap_or(field.default())
            .to_string()
    }
}

#[derive(Clone)]
struct ChannelSummary {
    id: String,
    kind: String,
    raw: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageKind {
    Overview,
    Models,
    Tooling,
    MainAgent,
    Runtime,
    Sandbox,
    Channels,
}

impl PageKind {
    const ALL: [Self; 7] = [
        Self::Overview,
        Self::Models,
        Self::Tooling,
        Self::MainAgent,
        Self::Runtime,
        Self::Sandbox,
        Self::Channels,
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Models => "Models",
            Self::Tooling => "Tooling",
            Self::MainAgent => "Main Agent",
            Self::Runtime => "Runtime",
            Self::Sandbox => "Sandbox",
            Self::Channels => "Channels",
        }
    }

    fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|page| *page == self)
            .expect("page present")
    }

    fn from_index(index: usize) -> Self {
        Self::ALL.get(index).copied().unwrap_or(Self::Overview)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainFocus {
    Sections,
    Content,
}

enum ScreenState {
    Main,
    ModelTypeWizard(ModelTypeWizardState),
    ModelForm(ModelFormState),
    ChannelKindWizard(ChannelKindWizardState),
    ChannelForm(ChannelFormState),
}

struct StatusMessage {
    level: StatusLevel,
    text: String,
}

impl StatusMessage {
    fn info(text: impl Into<String>) -> Self {
        Self {
            level: StatusLevel::Info,
            text: text.into(),
        }
    }

    fn success(text: impl Into<String>) -> Self {
        Self {
            level: StatusLevel::Success,
            text: text.into(),
        }
    }

    fn warning(text: impl Into<String>) -> Self {
        Self {
            level: StatusLevel::Warning,
            text: text.into(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            level: StatusLevel::Error,
            text: text.into(),
        }
    }
}

enum StatusLevel {
    Info,
    Success,
    Warning,
    Error,
}

enum ModalState {
    TextInput(TextInputState),
    Select(SelectState),
    MultiSelect(MultiSelectState),
    Confirm(ConfirmState),
    Message(MessageState),
}

struct TextInputState {
    title: String,
    help: String,
    value: String,
    cursor: usize,
    target: InputTarget,
}

#[derive(Clone, Default)]
struct ModelTypeWizardState {
    selected: usize,
}

impl ModelTypeWizardState {
    const OPTIONS: [&str; 3] = ["openrouter", "openrouter-resp", "codex-subscription"];

    fn selected_type(&self) -> &'static str {
        Self::OPTIONS
            .get(self.selected)
            .copied()
            .unwrap_or(Self::OPTIONS[0])
    }
}

#[derive(Clone, Default)]
struct ChannelKindWizardState {
    selected: usize,
}

impl ChannelKindWizardState {
    const OPTIONS: [&str; 3] = ["telegram", "dingtalk", "command_line"];

    fn selected_kind(&self) -> &'static str {
        Self::OPTIONS
            .get(self.selected)
            .copied()
            .unwrap_or(Self::OPTIONS[0])
    }
}

struct SelectState {
    title: String,
    options: Vec<String>,
    selected: usize,
    target: SelectTarget,
}

struct MultiSelectState {
    title: String,
    help: String,
    options: Vec<MultiSelectOption>,
    selected: usize,
    target: MultiSelectTarget,
}

struct MultiSelectOption {
    label: String,
    checked: bool,
}

struct ConfirmState {
    title: String,
    message: String,
    selected_yes: bool,
    action: ConfirmAction,
}

struct MessageState {
    title: String,
    message: String,
}

#[derive(Clone)]
enum ConfirmAction {
    ExitDiscard,
    BootstrapLatest,
    DeleteModel(String),
    DeleteChannel(usize),
}

#[derive(Clone, Copy)]
enum InputTarget {
    Tooling(ToolingField),
    MainAgent(MainAgentField),
    Runtime(RuntimeField),
    Sandbox(SandboxField),
    ModelForm(ModelFormField),
    ChannelForm(ChannelFormField),
}

#[derive(Clone, Copy)]
enum SelectTarget {
    SandboxMode,
    MainAgentMemorySystem,
    ModelForm(ModelFormField),
    ChannelKind,
}

#[derive(Clone, Copy)]
enum MultiSelectTarget {
    ModelCapabilities,
}

#[derive(Clone, Copy)]
enum ToolingField {
    WebSearch,
    Image,
    ImageGen,
    Pdf,
    AudioInput,
}

impl ToolingField {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::WebSearch,
            1 => Self::Image,
            2 => Self::ImageGen,
            3 => Self::Pdf,
            _ => Self::AudioInput,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::Image => "image",
            Self::ImageGen => "image_gen",
            Self::Pdf => "pdf",
            Self::AudioInput => "audio_input",
        }
    }

    fn title(self) -> &'static str {
        self.key()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MainAgentField {
    GlobalInstallRoot,
    TokenEstimationTemplateHfCache,
    TokenEstimationTokenizerHfCache,
    Language,
    MemorySystem,
    TimeAwarenessEmitSystemDateOnUserMessage,
    TimeAwarenessEmitIdleTimeGapHint,
    EnableContextCompression,
    ContextTriggerRatio,
    ContextTokenLimitOverride,
    ContextRecentFidelityTargetRatio,
    IdleCompactionEnabled,
    IdleCompactionPollIntervalSeconds,
    IdleCompactionMinRatio,
    TimeoutObservationCompactionEnabled,
}

impl MainAgentField {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::GlobalInstallRoot,
            1 => Self::TokenEstimationTemplateHfCache,
            2 => Self::TokenEstimationTokenizerHfCache,
            3 => Self::Language,
            4 => Self::MemorySystem,
            5 => Self::TimeAwarenessEmitSystemDateOnUserMessage,
            6 => Self::TimeAwarenessEmitIdleTimeGapHint,
            7 => Self::EnableContextCompression,
            8 => Self::ContextTriggerRatio,
            9 => Self::ContextTokenLimitOverride,
            10 => Self::ContextRecentFidelityTargetRatio,
            11 => Self::IdleCompactionEnabled,
            12 => Self::IdleCompactionPollIntervalSeconds,
            13 => Self::IdleCompactionMinRatio,
            _ => Self::TimeoutObservationCompactionEnabled,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::GlobalInstallRoot => "global_install_root",
            Self::TokenEstimationTemplateHfCache => "hf",
            Self::TokenEstimationTokenizerHfCache => "hf",
            Self::Language => "language",
            Self::MemorySystem => "memory_system",
            Self::TimeAwarenessEmitSystemDateOnUserMessage => "emit_system_date_on_user_message",
            Self::TimeAwarenessEmitIdleTimeGapHint => "emit_idle_time_gap_hint",
            Self::EnableContextCompression => "enable_context_compression",
            Self::ContextTriggerRatio => "trigger_ratio",
            Self::ContextTokenLimitOverride => "token_limit_override",
            Self::ContextRecentFidelityTargetRatio => "recent_fidelity_target_ratio",
            Self::IdleCompactionEnabled => "enabled",
            Self::IdleCompactionPollIntervalSeconds => "poll_interval_seconds",
            Self::IdleCompactionMinRatio => "min_ratio",
            Self::TimeoutObservationCompactionEnabled => "enabled",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::GlobalInstallRoot => "main_agent.global_install_root",
            Self::TokenEstimationTemplateHfCache => "token_estimation_cache.template.hf",
            Self::TokenEstimationTokenizerHfCache => "token_estimation_cache.tokenizer.hf",
            Self::Language => "main_agent.language",
            Self::MemorySystem => "main_agent.memory_system",
            Self::TimeAwarenessEmitSystemDateOnUserMessage => {
                "time_awareness.emit_system_date_on_user_message"
            }
            Self::TimeAwarenessEmitIdleTimeGapHint => "time_awareness.emit_idle_time_gap_hint",
            Self::EnableContextCompression => "main_agent.enable_context_compression",
            Self::ContextTriggerRatio => "context_compaction.trigger_ratio",
            Self::ContextTokenLimitOverride => "context_compaction.token_limit_override",
            Self::ContextRecentFidelityTargetRatio => {
                "context_compaction.recent_fidelity_target_ratio"
            }
            Self::IdleCompactionEnabled => "idle_compaction.enabled",
            Self::IdleCompactionPollIntervalSeconds => "idle_compaction.poll_interval_seconds",
            Self::IdleCompactionMinRatio => "idle_compaction.min_ratio",
            Self::TimeoutObservationCompactionEnabled => "timeout_observation_compaction.enabled",
        }
    }

    fn is_bool(self) -> bool {
        matches!(
            self,
            Self::TimeAwarenessEmitSystemDateOnUserMessage
                | Self::TimeAwarenessEmitIdleTimeGapHint
                | Self::EnableContextCompression
                | Self::IdleCompactionEnabled
                | Self::TimeoutObservationCompactionEnabled
        )
    }

    fn help(self) -> &'static str {
        match self {
            Self::GlobalInstallRoot => "Directory used for global installs",
            Self::TokenEstimationTemplateHfCache => {
                "HuggingFace chat template cache root, relative paths resolve under workdir"
            }
            Self::TokenEstimationTokenizerHfCache => {
                "HuggingFace tokenizer cache root, relative paths resolve under workdir"
            }
            Self::Language => "Language tag such as zh-CN or en-US",
            Self::MemorySystem => "Choose layered memory or Claude-style PARTCLAW memory",
            Self::TimeAwarenessEmitSystemDateOnUserMessage => {
                "Prefix every user turn with the current local system date and time"
            }
            Self::TimeAwarenessEmitIdleTimeGapHint => {
                "Add a system tip after long idle gaps before the next user turn"
            }
            Self::EnableContextCompression => "Toggle context compaction",
            Self::ContextTriggerRatio => "Float between 0 and 1",
            Self::ContextTokenLimitOverride => "Optional integer, blank clears it",
            Self::ContextRecentFidelityTargetRatio => "Float between 0 and 1",
            Self::IdleCompactionEnabled => "Toggle idle compaction",
            Self::IdleCompactionPollIntervalSeconds => "Positive integer seconds",
            Self::IdleCompactionMinRatio => "Float between 0 and 1",
            Self::TimeoutObservationCompactionEnabled => "Toggle timeout observation compaction",
        }
    }
}

#[derive(Clone, Copy)]
enum RuntimeField {
    MaxGlobalSubAgents,
    CronPollIntervalSeconds,
}

impl RuntimeField {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::MaxGlobalSubAgents,
            _ => Self::CronPollIntervalSeconds,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::MaxGlobalSubAgents => "max_global_sub_agents",
            Self::CronPollIntervalSeconds => "cron_poll_interval_seconds",
        }
    }

    fn title(self) -> &'static str {
        self.key()
    }

    fn help(self) -> &'static str {
        match self {
            Self::MaxGlobalSubAgents => "Positive integer limit for global subagents",
            Self::CronPollIntervalSeconds => "Positive integer seconds between cron polls",
        }
    }

    fn default(self) -> &'static str {
        match self {
            Self::MaxGlobalSubAgents => "4",
            Self::CronPollIntervalSeconds => "5",
        }
    }
}

#[derive(Clone, Copy)]
enum SandboxField {
    Mode,
    BubblewrapBinary,
    MapDockerSocket,
}

impl SandboxField {
    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Mode,
            1 => Self::BubblewrapBinary,
            _ => Self::MapDockerSocket,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::BubblewrapBinary => "bubblewrap_binary",
            Self::MapDockerSocket => "map_docker_socket",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Mode => "sandbox.mode",
            Self::BubblewrapBinary => "sandbox.bubblewrap_binary",
            Self::MapDockerSocket => "sandbox.map_docker_socket",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Mode => "Choose subprocess or bubblewrap",
            Self::BubblewrapBinary => "Executable name for bubblewrap, typically bwrap",
            Self::MapDockerSocket => {
                "Linux bubblewrap only: bind /run/docker.sock into the sandbox"
            }
        }
    }

    fn default(self) -> &'static str {
        match self {
            Self::Mode => "subprocess",
            Self::BubblewrapBinary => "bwrap",
            Self::MapDockerSocket => "false",
        }
    }

    fn is_bool(self) -> bool {
        matches!(self, Self::MapDockerSocket)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModelFormField {
    Alias,
    ModelType,
    ModelName,
    ApiEndpoint,
    ApiKeyEnv,
    ChatCompletionsPath,
    CodexHome,
    AuthCredentialsStoreMode,
    TimeoutSeconds,
    RetryMode,
    RetryMaxRetries,
    RetryRandomMean,
    ContextWindowTokens,
    TokenTemplateSource,
    TokenTemplatePath,
    TokenTemplateRepo,
    TokenTemplateRevision,
    TokenTemplateFile,
    TokenTemplateField,
    TokenTemplateCacheDir,
    TokenTokenizerSource,
    TokenTokenizerEncoding,
    TokenTokenizerPath,
    TokenTokenizerRepo,
    TokenTokenizerRevision,
    TokenTokenizerFile,
    TokenTokenizerCacheDir,
    Capabilities,
    SupportsVisionInput,
    AgentModelEnabled,
    ImageToolModel,
    ModelWebSearch,
    NativeWebSearchEnabled,
    Description,
    AgentFrameEnabled,
}

impl ModelFormField {
    fn title(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::ModelType => "type",
            Self::ModelName => "model",
            Self::ApiEndpoint => "api_endpoint",
            Self::ApiKeyEnv => "api_key_env",
            Self::ChatCompletionsPath => "chat_completions_path",
            Self::CodexHome => "codex_home",
            Self::AuthCredentialsStoreMode => "auth_credentials_store_mode",
            Self::TimeoutSeconds => "timeout_seconds",
            Self::RetryMode => "retry_mode.mode",
            Self::RetryMaxRetries => "retry_mode.max_retries",
            Self::RetryRandomMean => "retry_mode.retry_random_mean",
            Self::ContextWindowTokens => "context_window_tokens",
            Self::TokenTemplateSource => "token_estimation.template.source",
            Self::TokenTemplatePath => "token_estimation.template.path",
            Self::TokenTemplateRepo => "token_estimation.template.repo",
            Self::TokenTemplateRevision => "token_estimation.template.revision",
            Self::TokenTemplateFile => "token_estimation.template.file",
            Self::TokenTemplateField => "token_estimation.template.field",
            Self::TokenTemplateCacheDir => "token_estimation.template.cache_dir",
            Self::TokenTokenizerSource => "token_estimation.tokenizer.source",
            Self::TokenTokenizerEncoding => "token_estimation.tokenizer.encoding",
            Self::TokenTokenizerPath => "token_estimation.tokenizer.path",
            Self::TokenTokenizerRepo => "token_estimation.tokenizer.repo",
            Self::TokenTokenizerRevision => "token_estimation.tokenizer.revision",
            Self::TokenTokenizerFile => "token_estimation.tokenizer.file",
            Self::TokenTokenizerCacheDir => "token_estimation.tokenizer.cache_dir",
            Self::Capabilities => "capabilities",
            Self::SupportsVisionInput => "supports_vision_input",
            Self::AgentModelEnabled => "agent_model_enabled",
            Self::ImageToolModel => "image_tool_model",
            Self::ModelWebSearch => "web_search",
            Self::NativeWebSearchEnabled => "native_web_search.enabled",
            Self::Description => "description",
            Self::AgentFrameEnabled => "agent.agent_frame.available_models",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Alias => "Unique key under models",
            Self::ModelType => "Select openrouter, openrouter-resp, or codex-subscription",
            Self::ModelName => "Upstream model name",
            Self::ApiEndpoint => "Base upstream endpoint URL",
            Self::ApiKeyEnv => "Environment variable name for API auth",
            Self::ChatCompletionsPath => "Usually /chat/completions or /responses",
            Self::CodexHome => "Required for codex-subscription models",
            Self::AuthCredentialsStoreMode => "file, keyring, auto, or ephemeral",
            Self::TimeoutSeconds => "Floating-point number",
            Self::RetryMode => "Select no or random",
            Self::RetryMaxRetries => "Integer retry attempt count",
            Self::RetryRandomMean => "Mean random delay in seconds",
            Self::ContextWindowTokens => "Integer token window size",
            Self::TokenTemplateSource => "Select builtin, local, or huggingface prompt template",
            Self::TokenTemplatePath => "Path to tokenizer_config.json or a direct template file",
            Self::TokenTemplateRepo => "HuggingFace model repo id",
            Self::TokenTemplateRevision => "HuggingFace revision, branch, tag, or commit",
            Self::TokenTemplateFile => "HuggingFace template metadata filename",
            Self::TokenTemplateField => "JSON field containing the chat template",
            Self::TokenTemplateCacheDir => "Optional HuggingFace cache directory",
            Self::TokenTokenizerSource => "Select tiktoken, local, or huggingface tokenizer",
            Self::TokenTokenizerEncoding => {
                "Select auto, o200k_base, cl100k_base, or o200k_harmony"
            }
            Self::TokenTokenizerPath => "Path to tokenizer.json",
            Self::TokenTokenizerRepo => "HuggingFace model repo id",
            Self::TokenTokenizerRevision => "HuggingFace revision, branch, tag, or commit",
            Self::TokenTokenizerFile => "HuggingFace tokenizer filename",
            Self::TokenTokenizerCacheDir => "Optional HuggingFace cache directory",
            Self::Capabilities => "Pick one or more capability names",
            Self::SupportsVisionInput => "Toggle for image input support",
            Self::AgentModelEnabled => "Toggle whether it can act as a chat agent",
            Self::ImageToolModel => "Alias or self for image input tooling",
            Self::ModelWebSearch => "Per-model fallback search alias",
            Self::NativeWebSearchEnabled => "Toggle native provider web search",
            Self::Description => "Human-readable description",
            Self::AgentFrameEnabled => {
                "Include this alias under agent.agent_frame.available_models"
            }
        }
    }

    fn is_bool(self) -> bool {
        matches!(
            self,
            Self::SupportsVisionInput
                | Self::AgentModelEnabled
                | Self::NativeWebSearchEnabled
                | Self::AgentFrameEnabled
        )
    }
}

fn token_estimation_object(raw: &Map<String, Value>) -> Option<&Map<String, Value>> {
    raw.get("token_estimation").and_then(Value::as_object)
}

fn token_estimation_section<'a>(
    raw: &'a Map<String, Value>,
    section: &str,
) -> Option<&'a Map<String, Value>> {
    token_estimation_object(raw)?
        .get(section)
        .and_then(Value::as_object)
}

fn token_estimation_root_str<'a>(raw: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    token_estimation_object(raw)?
        .get(key)
        .and_then(Value::as_str)
}

fn token_estimation_section_str<'a>(
    raw: &'a Map<String, Value>,
    section: &str,
    key: &str,
) -> Option<&'a str> {
    token_estimation_section(raw, section)?
        .get(key)
        .and_then(Value::as_str)
}

fn token_estimation_source(raw: &Map<String, Value>, section: &str, default: &str) -> String {
    token_estimation_section_str(raw, section, "source")
        .or_else(|| {
            (token_estimation_root_str(raw, "source") == Some("huggingface"))
                .then_some("huggingface")
        })
        .unwrap_or(default)
        .to_string()
}

fn token_estimation_section_or_root_str(
    raw: &Map<String, Value>,
    section: &str,
    key: &str,
    default: &str,
) -> String {
    token_estimation_section_str(raw, section, key)
        .or_else(|| token_estimation_root_str(raw, key))
        .unwrap_or(default)
        .to_string()
}

#[derive(Clone)]
struct ModelFormState {
    existing_alias: Option<String>,
    raw: Map<String, Value>,
    selected: usize,
    alias: String,
    model_type: String,
    model_name: String,
    api_endpoint: String,
    api_key_env: String,
    chat_completions_path: String,
    codex_home: String,
    auth_credentials_store_mode: String,
    timeout_seconds: String,
    retry_mode: String,
    retry_max_retries: String,
    retry_random_mean: String,
    context_window_tokens: String,
    token_template_source: String,
    token_template_path: String,
    token_template_repo: String,
    token_template_revision: String,
    token_template_file: String,
    token_template_field: String,
    token_template_cache_dir: String,
    token_tokenizer_source: String,
    token_tokenizer_encoding: String,
    token_tokenizer_path: String,
    token_tokenizer_repo: String,
    token_tokenizer_revision: String,
    token_tokenizer_file: String,
    token_tokenizer_cache_dir: String,
    capabilities: String,
    supports_vision_input: bool,
    agent_model_enabled: bool,
    image_tool_model: String,
    model_web_search: String,
    native_web_search_enabled: bool,
    description: String,
    agent_frame_enabled: bool,
}

impl ModelFormState {
    fn new_with_type(model_type: &str) -> Self {
        let defaults = default_model_type_profile(model_type);
        Self {
            existing_alias: None,
            raw: Map::new(),
            selected: 0,
            alias: String::new(),
            model_type: defaults.model_type.to_string(),
            model_name: String::new(),
            api_endpoint: defaults.api_endpoint.to_string(),
            api_key_env: defaults.api_key_env.to_string(),
            chat_completions_path: defaults.chat_completions_path.to_string(),
            codex_home: defaults.codex_home.to_string(),
            auth_credentials_store_mode: defaults.auth_credentials_store_mode.to_string(),
            timeout_seconds: "120".to_string(),
            retry_mode: "no".to_string(),
            retry_max_retries: "2".to_string(),
            retry_random_mean: "8".to_string(),
            context_window_tokens: "128000".to_string(),
            token_template_source: "builtin".to_string(),
            token_template_path: String::new(),
            token_template_repo: String::new(),
            token_template_revision: "main".to_string(),
            token_template_file: "tokenizer_config.json".to_string(),
            token_template_field: "chat_template".to_string(),
            token_template_cache_dir: String::new(),
            token_tokenizer_source: "tiktoken".to_string(),
            token_tokenizer_encoding: "auto".to_string(),
            token_tokenizer_path: String::new(),
            token_tokenizer_repo: String::new(),
            token_tokenizer_revision: "main".to_string(),
            token_tokenizer_file: "tokenizer.json".to_string(),
            token_tokenizer_cache_dir: String::new(),
            capabilities: defaults.capabilities.to_string(),
            supports_vision_input: defaults.supports_vision_input,
            agent_model_enabled: defaults.agent_model_enabled,
            image_tool_model: String::new(),
            model_web_search: String::new(),
            native_web_search_enabled: false,
            description: String::new(),
            agent_frame_enabled: true,
        }
    }

    fn from_existing(value: &Value, alias: &str) -> Self {
        let raw = nested_object(value, &["models", alias])
            .cloned()
            .unwrap_or_default();
        let agent_frame_enabled = backend_has_alias(value, "agent_frame", alias);
        Self {
            existing_alias: Some(alias.to_string()),
            raw: raw.clone(),
            selected: 0,
            alias: alias.to_string(),
            model_type: raw
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("openrouter")
                .to_string(),
            model_name: raw
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            api_endpoint: raw
                .get("api_endpoint")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            api_key_env: raw
                .get("api_key_env")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            chat_completions_path: raw
                .get("chat_completions_path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            codex_home: raw
                .get("codex_home")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            auth_credentials_store_mode: raw
                .get("auth_credentials_store_mode")
                .and_then(Value::as_str)
                .unwrap_or("auto")
                .to_string(),
            timeout_seconds: raw
                .get("timeout_seconds")
                .and_then(value_to_string)
                .unwrap_or_else(|| "120".to_string()),
            retry_mode: raw
                .get("retry_mode")
                .and_then(Value::as_object)
                .and_then(|retry| retry.get("mode"))
                .and_then(Value::as_str)
                .unwrap_or("no")
                .to_string(),
            retry_max_retries: raw
                .get("retry_mode")
                .and_then(Value::as_object)
                .and_then(|retry| retry.get("max_retries"))
                .and_then(value_to_string)
                .unwrap_or_else(|| "2".to_string()),
            retry_random_mean: raw
                .get("retry_mode")
                .and_then(Value::as_object)
                .and_then(|retry| retry.get("retry_random_mean"))
                .and_then(value_to_string)
                .unwrap_or_else(|| "8".to_string()),
            context_window_tokens: raw
                .get("context_window_tokens")
                .and_then(value_to_string)
                .unwrap_or_else(|| "128000".to_string()),
            token_template_source: token_estimation_source(&raw, "template", "builtin"),
            token_template_path: raw
                .get("token_estimation")
                .and_then(Value::as_object)
                .and_then(|token| token.get("template"))
                .and_then(Value::as_object)
                .and_then(|template| template.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            token_template_repo: token_estimation_section_or_root_str(&raw, "template", "repo", ""),
            token_template_revision: token_estimation_section_or_root_str(
                &raw, "template", "revision", "main",
            ),
            token_template_file: token_estimation_section_str(&raw, "template", "file")
                .unwrap_or("tokenizer_config.json")
                .to_string(),
            token_template_field: token_estimation_section_str(&raw, "template", "field")
                .unwrap_or("chat_template")
                .to_string(),
            token_template_cache_dir: token_estimation_section_or_root_str(
                &raw,
                "template",
                "cache_dir",
                "",
            ),
            token_tokenizer_source: token_estimation_source(&raw, "tokenizer", "tiktoken"),
            token_tokenizer_encoding: raw
                .get("token_estimation")
                .and_then(Value::as_object)
                .and_then(|token| token.get("tokenizer"))
                .and_then(Value::as_object)
                .and_then(|tokenizer| tokenizer.get("encoding"))
                .and_then(Value::as_str)
                .unwrap_or("auto")
                .to_string(),
            token_tokenizer_path: raw
                .get("token_estimation")
                .and_then(Value::as_object)
                .and_then(|token| token.get("tokenizer"))
                .and_then(Value::as_object)
                .and_then(|tokenizer| tokenizer.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            token_tokenizer_repo: token_estimation_section_or_root_str(
                &raw,
                "tokenizer",
                "repo",
                "",
            ),
            token_tokenizer_revision: token_estimation_section_or_root_str(
                &raw,
                "tokenizer",
                "revision",
                "main",
            ),
            token_tokenizer_file: token_estimation_section_str(&raw, "tokenizer", "file")
                .unwrap_or("tokenizer.json")
                .to_string(),
            token_tokenizer_cache_dir: token_estimation_section_or_root_str(
                &raw,
                "tokenizer",
                "cache_dir",
                "",
            ),
            capabilities: raw
                .get("capabilities")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
            supports_vision_input: raw
                .get("supports_vision_input")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            agent_model_enabled: raw
                .get("agent_model_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            image_tool_model: raw
                .get("image_tool_model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            model_web_search: raw
                .get("web_search")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            native_web_search_enabled: raw
                .get("native_web_search")
                .and_then(Value::as_object)
                .and_then(|native| native.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            description: raw
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            agent_frame_enabled,
        }
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        self.visible_fields()
            .into_iter()
            .map(|field| (field.title(), self.field_value(field)))
            .collect()
    }

    fn visible_fields(&self) -> Vec<ModelFormField> {
        let mut fields = vec![
            ModelFormField::Alias,
            ModelFormField::ModelType,
            ModelFormField::ModelName,
            ModelFormField::ApiEndpoint,
            ModelFormField::ApiKeyEnv,
            ModelFormField::ChatCompletionsPath,
            ModelFormField::TimeoutSeconds,
            ModelFormField::RetryMode,
            ModelFormField::ContextWindowTokens,
            ModelFormField::TokenTemplateSource,
            ModelFormField::TokenTokenizerSource,
            ModelFormField::Capabilities,
            ModelFormField::SupportsVisionInput,
            ModelFormField::AgentModelEnabled,
            ModelFormField::ImageToolModel,
            ModelFormField::ModelWebSearch,
            ModelFormField::NativeWebSearchEnabled,
            ModelFormField::Description,
            ModelFormField::AgentFrameEnabled,
        ];
        if self.model_type == "codex-subscription" {
            fields.insert(6, ModelFormField::CodexHome);
            fields.insert(7, ModelFormField::AuthCredentialsStoreMode);
        }
        if self.retry_mode == "random"
            && let Some(index) = fields
                .iter()
                .position(|field| *field == ModelFormField::RetryMode)
        {
            fields.insert(index + 1, ModelFormField::RetryMaxRetries);
            fields.insert(index + 2, ModelFormField::RetryRandomMean);
        }
        if self.token_template_source == "local"
            && let Some(index) = fields
                .iter()
                .position(|field| *field == ModelFormField::TokenTemplateSource)
        {
            fields.insert(index + 1, ModelFormField::TokenTemplatePath);
            fields.insert(index + 2, ModelFormField::TokenTemplateField);
        } else if self.token_template_source == "huggingface"
            && let Some(index) = fields
                .iter()
                .position(|field| *field == ModelFormField::TokenTemplateSource)
        {
            fields.insert(index + 1, ModelFormField::TokenTemplateRepo);
            fields.insert(index + 2, ModelFormField::TokenTemplateRevision);
            fields.insert(index + 3, ModelFormField::TokenTemplateFile);
            fields.insert(index + 4, ModelFormField::TokenTemplateField);
            fields.insert(index + 5, ModelFormField::TokenTemplateCacheDir);
        }
        if self.token_tokenizer_source == "tiktoken"
            && let Some(index) = fields
                .iter()
                .position(|field| *field == ModelFormField::TokenTokenizerSource)
        {
            fields.insert(index + 1, ModelFormField::TokenTokenizerEncoding);
        } else if self.token_tokenizer_source == "local"
            && let Some(index) = fields
                .iter()
                .position(|field| *field == ModelFormField::TokenTokenizerSource)
        {
            fields.insert(index + 1, ModelFormField::TokenTokenizerPath);
        } else if self.token_tokenizer_source == "huggingface"
            && let Some(index) = fields
                .iter()
                .position(|field| *field == ModelFormField::TokenTokenizerSource)
        {
            fields.insert(index + 1, ModelFormField::TokenTokenizerRepo);
            fields.insert(index + 2, ModelFormField::TokenTokenizerRevision);
            fields.insert(index + 3, ModelFormField::TokenTokenizerFile);
            fields.insert(index + 4, ModelFormField::TokenTokenizerCacheDir);
        }
        fields
    }

    fn selected_field(&self) -> ModelFormField {
        self.visible_fields()
            .get(self.selected)
            .copied()
            .unwrap_or(ModelFormField::Alias)
    }

    fn capabilities_list(&self) -> Vec<String> {
        parse_csv_items(&self.capabilities)
    }

    fn set_capabilities(&mut self, items: Vec<String>) {
        self.capabilities = items.join(", ");
    }

    fn toggle_field(&mut self, field: ModelFormField) {
        match field {
            ModelFormField::SupportsVisionInput => {
                self.supports_vision_input = !self.supports_vision_input;
            }
            ModelFormField::AgentModelEnabled => {
                self.agent_model_enabled = !self.agent_model_enabled;
            }
            ModelFormField::NativeWebSearchEnabled => {
                self.native_web_search_enabled = !self.native_web_search_enabled;
            }
            ModelFormField::AgentFrameEnabled => {
                self.agent_frame_enabled = !self.agent_frame_enabled;
            }
            _ => {}
        }
    }

    fn field_value(&self, field: ModelFormField) -> String {
        match field {
            ModelFormField::Alias => self.alias.clone(),
            ModelFormField::ModelType => self.model_type.clone(),
            ModelFormField::ModelName => self.model_name.clone(),
            ModelFormField::ApiEndpoint => self.api_endpoint.clone(),
            ModelFormField::ApiKeyEnv => self.api_key_env.clone(),
            ModelFormField::ChatCompletionsPath => self.chat_completions_path.clone(),
            ModelFormField::CodexHome => self.codex_home.clone(),
            ModelFormField::AuthCredentialsStoreMode => self.auth_credentials_store_mode.clone(),
            ModelFormField::TimeoutSeconds => self.timeout_seconds.clone(),
            ModelFormField::RetryMode => self.retry_mode.clone(),
            ModelFormField::RetryMaxRetries => self.retry_max_retries.clone(),
            ModelFormField::RetryRandomMean => self.retry_random_mean.clone(),
            ModelFormField::ContextWindowTokens => self.context_window_tokens.clone(),
            ModelFormField::TokenTemplateSource => self.token_template_source.clone(),
            ModelFormField::TokenTemplatePath => self.token_template_path.clone(),
            ModelFormField::TokenTemplateRepo => self.token_template_repo.clone(),
            ModelFormField::TokenTemplateRevision => self.token_template_revision.clone(),
            ModelFormField::TokenTemplateFile => self.token_template_file.clone(),
            ModelFormField::TokenTemplateField => self.token_template_field.clone(),
            ModelFormField::TokenTemplateCacheDir => self.token_template_cache_dir.clone(),
            ModelFormField::TokenTokenizerSource => self.token_tokenizer_source.clone(),
            ModelFormField::TokenTokenizerEncoding => self.token_tokenizer_encoding.clone(),
            ModelFormField::TokenTokenizerPath => self.token_tokenizer_path.clone(),
            ModelFormField::TokenTokenizerRepo => self.token_tokenizer_repo.clone(),
            ModelFormField::TokenTokenizerRevision => self.token_tokenizer_revision.clone(),
            ModelFormField::TokenTokenizerFile => self.token_tokenizer_file.clone(),
            ModelFormField::TokenTokenizerCacheDir => self.token_tokenizer_cache_dir.clone(),
            ModelFormField::Capabilities => self.capabilities.clone(),
            ModelFormField::SupportsVisionInput => bool_string(self.supports_vision_input),
            ModelFormField::AgentModelEnabled => bool_string(self.agent_model_enabled),
            ModelFormField::ImageToolModel => self.image_tool_model.clone(),
            ModelFormField::ModelWebSearch => self.model_web_search.clone(),
            ModelFormField::NativeWebSearchEnabled => bool_string(self.native_web_search_enabled),
            ModelFormField::Description => self.description.clone(),
            ModelFormField::AgentFrameEnabled => bool_string(self.agent_frame_enabled),
        }
    }

    fn set_field_value(&mut self, field: ModelFormField, value: String) {
        match field {
            ModelFormField::Alias => self.alias = value,
            ModelFormField::ModelType => self.model_type = value,
            ModelFormField::ModelName => self.model_name = value,
            ModelFormField::ApiEndpoint => self.api_endpoint = value,
            ModelFormField::ApiKeyEnv => self.api_key_env = value,
            ModelFormField::ChatCompletionsPath => self.chat_completions_path = value,
            ModelFormField::CodexHome => self.codex_home = value,
            ModelFormField::AuthCredentialsStoreMode => self.auth_credentials_store_mode = value,
            ModelFormField::TimeoutSeconds => self.timeout_seconds = value,
            ModelFormField::RetryMode => self.retry_mode = value,
            ModelFormField::RetryMaxRetries => self.retry_max_retries = value,
            ModelFormField::RetryRandomMean => self.retry_random_mean = value,
            ModelFormField::ContextWindowTokens => self.context_window_tokens = value,
            ModelFormField::TokenTemplateSource => self.token_template_source = value,
            ModelFormField::TokenTemplatePath => self.token_template_path = value,
            ModelFormField::TokenTemplateRepo => self.token_template_repo = value,
            ModelFormField::TokenTemplateRevision => self.token_template_revision = value,
            ModelFormField::TokenTemplateFile => self.token_template_file = value,
            ModelFormField::TokenTemplateField => self.token_template_field = value,
            ModelFormField::TokenTemplateCacheDir => self.token_template_cache_dir = value,
            ModelFormField::TokenTokenizerSource => self.token_tokenizer_source = value,
            ModelFormField::TokenTokenizerEncoding => self.token_tokenizer_encoding = value,
            ModelFormField::TokenTokenizerPath => self.token_tokenizer_path = value,
            ModelFormField::TokenTokenizerRepo => self.token_tokenizer_repo = value,
            ModelFormField::TokenTokenizerRevision => self.token_tokenizer_revision = value,
            ModelFormField::TokenTokenizerFile => self.token_tokenizer_file = value,
            ModelFormField::TokenTokenizerCacheDir => self.token_tokenizer_cache_dir = value,
            ModelFormField::Capabilities => self.capabilities = value,
            ModelFormField::ImageToolModel => self.image_tool_model = value,
            ModelFormField::ModelWebSearch => self.model_web_search = value,
            ModelFormField::Description => self.description = value,
            _ => {}
        }
    }

    fn apply_model_type_defaults(&mut self, model_type: &str) {
        let defaults = default_model_type_profile(model_type);
        self.model_type = defaults.model_type.to_string();
        self.api_endpoint = defaults.api_endpoint.to_string();
        self.api_key_env = defaults.api_key_env.to_string();
        self.chat_completions_path = defaults.chat_completions_path.to_string();
        if defaults.codex_home.is_empty() {
            self.codex_home.clear();
        } else if self.codex_home.trim().is_empty() {
            self.codex_home = defaults.codex_home.to_string();
        }
        if defaults.auth_credentials_store_mode.is_empty() {
            self.auth_credentials_store_mode.clear();
        } else {
            self.auth_credentials_store_mode = defaults.auth_credentials_store_mode.to_string();
        }
        self.selected = self
            .selected
            .min(self.visible_fields().len().saturating_sub(1));
    }

    #[cfg(test)]
    fn preview_json(&self) -> Value {
        let mut model = self.raw.clone();
        set_optional_trimmed_string(&mut model, "type", &self.model_type);
        set_optional_trimmed_string(&mut model, "api_endpoint", &self.api_endpoint);
        set_optional_trimmed_string(&mut model, "model", &self.model_name);
        set_optional_trimmed_string(&mut model, "api_key_env", &self.api_key_env);
        set_optional_trimmed_string(
            &mut model,
            "chat_completions_path",
            &self.chat_completions_path,
        );
        set_optional_trimmed_string(&mut model, "codex_home", &self.codex_home);
        set_optional_trimmed_string(
            &mut model,
            "auth_credentials_store_mode",
            &self.auth_credentials_store_mode,
        );
        set_optional_trimmed_string(&mut model, "description", &self.description);
        set_optional_trimmed_string(&mut model, "image_tool_model", &self.image_tool_model);
        set_optional_trimmed_string(&mut model, "web_search", &self.model_web_search);
        set_bool(
            &mut model,
            "supports_vision_input",
            self.supports_vision_input,
        );
        set_bool(&mut model, "agent_model_enabled", self.agent_model_enabled);
        let _ = set_retry_mode_object(
            &mut model,
            &self.retry_mode,
            &self.retry_max_retries,
            &self.retry_random_mean,
        );
        let _ = set_token_estimation_object(
            &mut model,
            &self.token_template_source,
            &self.token_template_path,
            &self.token_template_repo,
            &self.token_template_revision,
            &self.token_template_file,
            &self.token_template_field,
            &self.token_template_cache_dir,
            &self.token_tokenizer_source,
            &self.token_tokenizer_encoding,
            &self.token_tokenizer_path,
            &self.token_tokenizer_repo,
            &self.token_tokenizer_revision,
            &self.token_tokenizer_file,
            &self.token_tokenizer_cache_dir,
        );
        if !self.capabilities.trim().is_empty() {
            model.insert(
                "capabilities".to_string(),
                Value::Array(
                    parse_csv_items(&self.capabilities)
                        .into_iter()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        if let Some(Value::Object(native)) = model.get_mut("native_web_search") {
            set_bool(native, "enabled", self.native_web_search_enabled);
        } else if self.native_web_search_enabled {
            model.insert(
                "native_web_search".to_string(),
                json!({"enabled": true, "payload": {}}),
            );
        }
        Value::Object(model)
    }
}

#[derive(Clone, Copy)]
enum ChannelFormField {
    Kind,
    Id,
    Prompt,
    BotTokenEnv,
    ClientIdEnv,
    ClientSecretEnv,
    ApiBaseUrl,
}

impl ChannelFormField {
    fn title(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Id => "id",
            Self::Prompt => "prompt",
            Self::BotTokenEnv => "bot_token_env",
            Self::ClientIdEnv => "client_id_env",
            Self::ClientSecretEnv => "client_secret_env",
            Self::ApiBaseUrl => "api_base_url",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::Kind => "Choose command_line, telegram, or dingtalk",
            Self::Id => "Stable channel identifier",
            Self::Prompt => "CLI prompt text such as you> ",
            Self::BotTokenEnv => "Environment variable for Telegram bot token",
            Self::ClientIdEnv => "Environment variable for DingTalk client id",
            Self::ClientSecretEnv => "Environment variable for DingTalk client secret",
            Self::ApiBaseUrl => "DingTalk API base URL",
        }
    }
}

#[derive(Clone)]
struct ChannelFormState {
    existing_index: Option<usize>,
    raw: Map<String, Value>,
    selected: usize,
    kind: String,
    id: String,
    prompt: String,
    bot_token_env: String,
    client_id_env: String,
    client_secret_env: String,
    api_base_url: String,
}

impl ChannelFormState {
    fn new_with_kind(kind: &str) -> Self {
        Self {
            existing_index: None,
            raw: Map::new(),
            selected: 0,
            kind: kind.to_string(),
            id: String::new(),
            prompt: "you> ".to_string(),
            bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            client_id_env: "DINGTALK_CLIENT_ID".to_string(),
            client_secret_env: "DINGTALK_CLIENT_SECRET".to_string(),
            api_base_url: "https://api.dingtalk.com".to_string(),
        }
    }

    fn from_existing(index: usize, channel: &ChannelSummary) -> Self {
        let raw = channel.raw.clone();
        Self {
            existing_index: Some(index),
            raw: raw.clone(),
            selected: 0,
            kind: channel.kind.clone(),
            id: channel.id.clone(),
            prompt: raw
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("you> ")
                .to_string(),
            bot_token_env: raw
                .get("bot_token_env")
                .and_then(Value::as_str)
                .unwrap_or("TELEGRAM_BOT_TOKEN")
                .to_string(),
            client_id_env: raw
                .get("client_id_env")
                .and_then(Value::as_str)
                .unwrap_or("DINGTALK_CLIENT_ID")
                .to_string(),
            client_secret_env: raw
                .get("client_secret_env")
                .and_then(Value::as_str)
                .unwrap_or("DINGTALK_CLIENT_SECRET")
                .to_string(),
            api_base_url: raw
                .get("api_base_url")
                .and_then(Value::as_str)
                .unwrap_or("https://api.dingtalk.com")
                .to_string(),
        }
    }

    fn fields(&self) -> Vec<(&'static str, String)> {
        if self.kind == "command_line" {
            vec![
                ("kind", self.kind.clone()),
                ("id", self.id.clone()),
                ("prompt", self.prompt.clone()),
            ]
        } else if self.kind == "telegram" {
            vec![
                ("kind", self.kind.clone()),
                ("id", self.id.clone()),
                ("bot_token_env", self.bot_token_env.clone()),
            ]
        } else {
            vec![
                ("kind", self.kind.clone()),
                ("id", self.id.clone()),
                ("client_id_env", self.client_id_env.clone()),
                ("client_secret_env", self.client_secret_env.clone()),
                ("api_base_url", self.api_base_url.clone()),
            ]
        }
    }

    fn selected_field(&self) -> ChannelFormField {
        if self.kind == "command_line" {
            match self.selected {
                0 => ChannelFormField::Kind,
                1 => ChannelFormField::Id,
                _ => ChannelFormField::Prompt,
            }
        } else if self.kind == "telegram" {
            match self.selected {
                0 => ChannelFormField::Kind,
                1 => ChannelFormField::Id,
                _ => ChannelFormField::BotTokenEnv,
            }
        } else {
            match self.selected {
                0 => ChannelFormField::Kind,
                1 => ChannelFormField::Id,
                2 => ChannelFormField::ClientIdEnv,
                3 => ChannelFormField::ClientSecretEnv,
                _ => ChannelFormField::ApiBaseUrl,
            }
        }
    }

    fn field_value(&self, field: ChannelFormField) -> String {
        match field {
            ChannelFormField::Kind => self.kind.clone(),
            ChannelFormField::Id => self.id.clone(),
            ChannelFormField::Prompt => self.prompt.clone(),
            ChannelFormField::BotTokenEnv => self.bot_token_env.clone(),
            ChannelFormField::ClientIdEnv => self.client_id_env.clone(),
            ChannelFormField::ClientSecretEnv => self.client_secret_env.clone(),
            ChannelFormField::ApiBaseUrl => self.api_base_url.clone(),
        }
    }

    fn set_field_value(&mut self, field: ChannelFormField, value: String) {
        match field {
            ChannelFormField::Kind => self.kind = value,
            ChannelFormField::Id => self.id = value,
            ChannelFormField::Prompt => self.prompt = value,
            ChannelFormField::BotTokenEnv => self.bot_token_env = value,
            ChannelFormField::ClientIdEnv => self.client_id_env = value,
            ChannelFormField::ClientSecretEnv => self.client_secret_env = value,
            ChannelFormField::ApiBaseUrl => self.api_base_url = value,
        }
    }
}

struct ModelTypeProfile {
    model_type: &'static str,
    api_endpoint: &'static str,
    api_key_env: &'static str,
    chat_completions_path: &'static str,
    codex_home: &'static str,
    auth_credentials_store_mode: &'static str,
    capabilities: &'static str,
    supports_vision_input: bool,
    agent_model_enabled: bool,
}

fn default_model_type_profile(model_type: &str) -> ModelTypeProfile {
    match model_type {
        "openrouter-resp" => ModelTypeProfile {
            model_type: "openrouter-resp",
            api_endpoint: "https://openrouter.ai/api/v1",
            api_key_env: "OPENROUTER_API_KEY",
            chat_completions_path: "/responses",
            codex_home: "",
            auth_credentials_store_mode: "",
            capabilities: "chat",
            supports_vision_input: true,
            agent_model_enabled: true,
        },
        "codex-subscription" => ModelTypeProfile {
            model_type: "codex-subscription",
            api_endpoint: "https://chatgpt.com/backend-api/codex",
            api_key_env: "OPENAI_API_KEY",
            chat_completions_path: "/responses",
            codex_home: "~/.codex",
            auth_credentials_store_mode: "auto",
            capabilities: "chat",
            supports_vision_input: true,
            agent_model_enabled: true,
        },
        _ => ModelTypeProfile {
            model_type: "openrouter",
            api_endpoint: "https://openrouter.ai/api/v1",
            api_key_env: "OPENROUTER_API_KEY",
            chat_completions_path: "/chat/completions",
            codex_home: "",
            auth_credentials_store_mode: "",
            capabilities: "chat",
            supports_vision_input: false,
            agent_model_enabled: true,
        },
    }
}

fn model_type_wizard_text(model_type: &str) -> String {
    match model_type {
        "openrouter-resp" => [
            "OpenRouter Responses",
            "",
            "Use this for OpenRouter models that speak the Responses API.",
            "Recommended for multimodal models and image generation routes.",
            "",
            "Defaults",
            "  api_endpoint: https://openrouter.ai/api/v1",
            "  chat_completions_path: /responses",
            "  api_key_env: OPENROUTER_API_KEY",
        ]
        .join("\n"),
        "codex-subscription" => [
            "Codex Subscription",
            "",
            "Use this for the ChatGPT/Codex subscription-backed model path.",
            "This usually needs a valid codex home directory.",
            "",
            "Defaults",
            "  api_endpoint: https://chatgpt.com/backend-api/codex",
            "  chat_completions_path: /responses",
            "  api_key_env: OPENAI_API_KEY",
            "  codex_home: ~/.codex",
        ]
        .join("\n"),
        _ => [
            "OpenRouter Chat Completions",
            "",
            "Use this for normal OpenRouter chat models.",
            "It is the safest default for most text-only agent models.",
            "",
            "Defaults",
            "  api_endpoint: https://openrouter.ai/api/v1",
            "  chat_completions_path: /chat/completions",
            "  api_key_env: OPENROUTER_API_KEY",
        ]
        .join("\n"),
    }
}

fn channel_kind_wizard_text(kind: &str) -> String {
    match kind {
        "command_line" => [
            "Command Line Channel",
            "",
            "Adds a local CLI entrypoint.",
            "You only need an id and an optional prompt string.",
        ]
        .join("\n"),
        "dingtalk" => [
            "DingTalk Channel",
            "",
            "Adds a DingTalk Stream bot entrypoint.",
            "You need environment variables for the DingTalk client id and client secret.",
            "",
            "Built in automatically",
            "  Stream websocket subscriptions",
            "  sessionWebhook-based replies",
            "  default api.dingtalk.com endpoint",
        ]
        .join("\n"),
        _ => [
            "Telegram Channel",
            "",
            "Adds a Telegram bot entrypoint.",
            "You only need an id and the environment variable name holding the bot token.",
            "",
            "Built in automatically",
            "  telegram command list",
            "  default polling settings",
            "  default api.telegram.org endpoint",
        ]
        .join("\n"),
    }
}

fn model_field_guide_text(form: &ModelFormState, field: ModelFormField) -> String {
    let (what_to_write, example, note) = match field {
        ModelFormField::Alias => (
            "A short stable key used everywhere else in config.",
            "gpt54 or sonar_pro",
            "Avoid spaces. This becomes the model alias under models.",
        ),
        ModelFormField::ModelType => (
            "Pick the upstream adapter shape for this model.",
            form.model_type.as_str(),
            "OpenRouter uses /chat/completions, Responses models use /responses, and Codex subscription uses the ChatGPT backend.",
        ),
        ModelFormField::ModelName => (
            "The real upstream model identifier sent to the provider.",
            "gpt-5.4 or anthropic/claude-opus-4.6",
            "This is not the local alias; it is the provider-side model name.",
        ),
        ModelFormField::ApiEndpoint => (
            "The base URL of the provider endpoint.",
            "https://openrouter.ai/api/v1",
            "Do not include the final /chat/completions or /responses path here.",
        ),
        ModelFormField::ApiKeyEnv => (
            "The env var name that stores the API key.",
            "OPENROUTER_API_KEY",
            "Leave blank only if this provider authenticates some other way.",
        ),
        ModelFormField::ChatCompletionsPath => (
            "The request path appended to api_endpoint.",
            "/chat/completions or /responses",
            "Match this to the provider adapter. ZGent models require the default chat completions path.",
        ),
        ModelFormField::CodexHome => (
            "The Codex home directory used by subscription-backed models.",
            "~/.codex",
            "Usually only needed for codex-subscription models.",
        ),
        ModelFormField::AuthCredentialsStoreMode => (
            "How auth credentials should be stored or discovered.",
            "auto, file, keyring, ephemeral",
            "Most setups should stay on auto.",
        ),
        ModelFormField::TimeoutSeconds => (
            "Request timeout in seconds.",
            "120 or 300",
            "This accepts floating-point numbers.",
        ),
        ModelFormField::RetryMode => (
            "How failed upstream requests should be retried.",
            "no or random",
            "Use no to return failures immediately, or random to wait before retrying.",
        ),
        ModelFormField::RetryMaxRetries => (
            "Maximum number of retry attempts after the first failure.",
            "2",
            "Only used when retry_mode.mode is random.",
        ),
        ModelFormField::RetryRandomMean => (
            "Mean wait time in seconds for random retry delay.",
            "8",
            "Only used when retry_mode.mode is random.",
        ),
        ModelFormField::ContextWindowTokens => (
            "Approximate context window size of the upstream model.",
            "128000 or 262144",
            "Used to size prompt compaction and budgeting.",
        ),
        ModelFormField::TokenTemplateSource => (
            "Which chat template renderer to use for local token estimates.",
            "builtin, local, or huggingface",
            "builtin is the default. huggingface downloads only tokenizer_config.json by default.",
        ),
        ModelFormField::TokenTemplatePath => (
            "Local path to tokenizer_config.json or a direct Jinja chat template.",
            "./tokenizers/qwen/tokenizer_config.json",
            "Relative paths are resolved from the config file directory.",
        ),
        ModelFormField::TokenTemplateRepo => (
            "HuggingFace repo id for the chat template metadata.",
            "Qwen/Qwen2.5-Coder-7B-Instruct",
            "Only small tokenizer metadata files are fetched; model weights are never requested.",
        ),
        ModelFormField::TokenTemplateRevision => (
            "HuggingFace branch, tag, or commit for the template file.",
            "main or a commit hash",
            "Pinned commits are best for stable production estimates.",
        ),
        ModelFormField::TokenTemplateFile => (
            "HuggingFace filename containing the chat template.",
            "tokenizer_config.json",
            "Allowed files are limited to tokenizer/template metadata, not weights.",
        ),
        ModelFormField::TokenTemplateField => (
            "JSON field containing the chat template.",
            "chat_template",
            "Used for local JSON and HuggingFace tokenizer_config.json templates.",
        ),
        ModelFormField::TokenTemplateCacheDir => (
            "Optional HuggingFace cache directory for template metadata.",
            "./hf-cache",
            "Leave blank to use the normal HF_HOME/default HuggingFace cache.",
        ),
        ModelFormField::TokenTokenizerSource => (
            "Which tokenizer implementation to use for local token estimates.",
            "tiktoken, local, or huggingface",
            "huggingface downloads only tokenizer.json by default. tiktoken uses the configured encoding.",
        ),
        ModelFormField::TokenTokenizerEncoding => (
            "Which tiktoken encoding to use.",
            "auto, o200k_base, cl100k_base, o200k_harmony",
            "auto chooses from the model name. Use an explicit value to override the heuristic.",
        ),
        ModelFormField::TokenTokenizerPath => (
            "Local path to HuggingFace tokenizer.json.",
            "./tokenizers/qwen/tokenizer.json",
            "Relative paths are resolved from the config file directory.",
        ),
        ModelFormField::TokenTokenizerRepo => (
            "HuggingFace repo id for tokenizer.json.",
            "Qwen/Qwen2.5-Coder-7B-Instruct",
            "Only tokenizer metadata is fetched; model weights are never requested.",
        ),
        ModelFormField::TokenTokenizerRevision => (
            "HuggingFace branch, tag, or commit for tokenizer.json.",
            "main or a commit hash",
            "Pinned commits are best for stable production estimates.",
        ),
        ModelFormField::TokenTokenizerFile => (
            "HuggingFace tokenizer filename.",
            "tokenizer.json",
            "Keep this as tokenizer.json unless the repo stores a compatible tokenizer file elsewhere.",
        ),
        ModelFormField::TokenTokenizerCacheDir => (
            "Optional HuggingFace cache directory for tokenizer metadata.",
            "./hf-cache",
            "Leave blank to use the normal HF_HOME/default HuggingFace cache.",
        ),
        ModelFormField::Capabilities => (
            "Pick every capability this model can serve from the checklist.",
            "chat, web_search, image_in, image_out, pdf, audio_in",
            "Press Enter here to open the multi-select picker, then Space toggles each item.",
        ),
        ModelFormField::SupportsVisionInput => (
            "Whether the model accepts image inputs.",
            "true / false",
            "Turn this on for multimodal chat models.",
        ),
        ModelFormField::AgentModelEnabled => (
            "Whether the model can be chosen as an interactive agent model.",
            "true / false",
            "Turn this off for helper-only models such as dedicated search or image generation routes.",
        ),
        ModelFormField::ImageToolModel => (
            "Optional image-tool override used when this model handles image inputs.",
            "self or another alias",
            "Use self when the same upstream model should handle its own image input tooling.",
        ),
        ModelFormField::ModelWebSearch => (
            "Optional per-model fallback web search route.",
            "sonar_pro",
            "Useful when the provider cannot do native web search.",
        ),
        ModelFormField::NativeWebSearchEnabled => (
            "Whether provider-native web search should be turned on.",
            "true / false",
            "Only enable this when the upstream provider actually supports it.",
        ),
        ModelFormField::Description => (
            "Short human-readable note for this model.",
            "Claude Opus 4.6 via OpenRouter",
            "This is shown to operators and helps explain intent.",
        ),
        ModelFormField::AgentFrameEnabled => (
            "Whether this alias should be selectable by the agent_frame backend.",
            "true / false",
            "Turn this on when foreground sessions may run through agent_frame.",
        ),
    };
    format!(
        "Field: {}\n\nWhat to write\n{}\n\nExample\n{}\n\nNotes\n{}",
        field.title(),
        what_to_write,
        example,
        note
    )
}

fn channel_field_guide_text(form: &ChannelFormState, field: ChannelFormField) -> String {
    let (what_to_write, example, note) = match field {
        ChannelFormField::Kind => (
            "Which integration should be created for this channel.",
            form.kind.as_str(),
            "Telegram manages bot commands internally. DingTalk uses Stream mode credentials. Command-line only needs a prompt.",
        ),
        ChannelFormField::Id => (
            "A stable unique channel id.",
            "telegram-main, dingtalk-main, or local-cli",
            "This id is how the runtime tracks the channel internally.",
        ),
        ChannelFormField::Prompt => (
            "The terminal prompt shown for the local CLI channel.",
            "you> ",
            "Only used by command_line channels.",
        ),
        ChannelFormField::BotTokenEnv => (
            "The environment variable that holds the Telegram bot token.",
            "TELEGRAM_BOT_TOKEN",
            "Telegram commands, polling defaults, and API base URL are handled automatically unless an older config already overrides them.",
        ),
        ChannelFormField::ClientIdEnv => (
            "The environment variable that holds the DingTalk client id.",
            "DINGTALK_CLIENT_ID",
            "Only used by dingtalk channels. This is the app Client ID from DingTalk developer console.",
        ),
        ChannelFormField::ClientSecretEnv => (
            "The environment variable that holds the DingTalk client secret.",
            "DINGTALK_CLIENT_SECRET",
            "Only used by dingtalk channels. Keep the secret out of the JSON config.",
        ),
        ChannelFormField::ApiBaseUrl => (
            "The DingTalk API base URL.",
            "https://api.dingtalk.com",
            "Usually keep the default unless you need a special endpoint for testing.",
        ),
    };
    format!(
        "Field: {}\n\nWhat to write\n{}\n\nExample\n{}\n\nNotes\n{}",
        field.title(),
        what_to_write,
        example,
        note
    )
}

fn render_text_input_line(state: &TextInputState) -> Line<'static> {
    let cursor = state.cursor.min(char_count(&state.value));
    let byte_index = char_to_byte_index(&state.value, cursor);
    let before = state.value[..byte_index].to_string();
    let after = state.value[byte_index..].to_string();
    let focused = after
        .chars()
        .next()
        .map(|ch| ch.to_string())
        .unwrap_or_else(|| " ".to_string());
    let remainder = after.chars().skip(1).collect::<String>();
    Line::from(vec![
        Span::styled(before, Style::default().fg(Color::White)),
        Span::styled(
            focused,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(remainder, Style::default().fg(Color::White)),
    ])
}

fn char_count(value: &str) -> usize {
    value.chars().count()
}

fn char_to_byte_index(value: &str, cursor: usize) -> usize {
    value
        .char_indices()
        .nth(cursor)
        .map(|(index, _)| index)
        .unwrap_or_else(|| value.len())
}

fn insert_at_cursor(value: &mut String, cursor: &mut usize, ch: char) {
    let index = char_to_byte_index(value, *cursor);
    value.insert(index, ch);
    *cursor += 1;
}

fn delete_before_cursor(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let end = char_to_byte_index(value, *cursor);
    let start = char_to_byte_index(value, *cursor - 1);
    value.replace_range(start..end, "");
    *cursor -= 1;
}

fn render_field_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    fields: &[(&str, String)],
    selected: usize,
    help: &str,
    focused: bool,
) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(4)])
        .split(area);
    let items = fields
        .iter()
        .map(|(label, value)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{label:<38}"), Style::default().fg(Color::Cyan)),
                Span::raw(value.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !fields.is_empty() {
        state.select(Some(selected.min(fields.len() - 1)));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(focus_border_style(focused)),
        )
        .highlight_style(list_highlight_style(focused))
        .highlight_symbol(list_highlight_symbol(focused));
    frame.render_stateful_widget(list, sections[0], &mut state);
    frame.render_widget(
        Paragraph::new(help)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Help ")
                    .border_style(focus_border_style(focused)),
            )
            .wrap(Wrap { trim: false }),
        sections[1],
    );
}

fn focus_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Blue)
    }
}

fn list_highlight_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    }
}

fn list_highlight_symbol(focused: bool) -> &'static str {
    if focused { ">> " } else { "   " }
}

fn centered_rect(percent_x: u16, percent_y: u16, rect: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(rect);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn current_version(value: &Value) -> String {
    nested_string(value, &["version"])
        .unwrap_or(LATEST_CONFIG_VERSION)
        .to_string()
}

fn top_level_tooling_value(value: &Value, key: &str) -> String {
    nested_string(value, &["tooling", key])
        .unwrap_or("")
        .to_string()
}

fn model_summary_text(value: &Value, alias: &str) -> String {
    let model = nested_object(value, &["models", alias]);
    let model_type = model
        .and_then(|item| item.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let upstream = model
        .and_then(|item| item.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("unset");
    let capabilities = model
        .and_then(|item| item.get("capabilities"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "none".to_string());
    format!(
        "当前模型: {alias}\n类型: {model_type}\n上游模型: {upstream}\n能力: {capabilities}\nagent_frame 可用: {}",
        bool_string(backend_has_alias(value, "agent_frame", alias))
    )
}

fn backend_has_alias(value: &Value, backend: &str, alias: &str) -> bool {
    nested_array(value, &["agent", backend, "available_models"])
        .map(|entries| entries.iter().any(|entry| entry.as_str() == Some(alias)))
        .unwrap_or(false)
}

fn update_backend_membership(
    value: &mut Value,
    old_alias: Option<&str>,
    new_alias: &str,
    backend: &str,
    enabled: bool,
) {
    let entries = ensure_array_mut(value, &["agent", backend, "available_models"]);
    entries.retain(|entry| {
        let Some(entry_alias) = entry.as_str() else {
            return true;
        };
        Some(entry_alias) != old_alias && entry_alias != new_alias
    });
    if enabled {
        entries.push(Value::String(new_alias.to_string()));
    }
}

fn remove_backend_alias(value: &mut Value, backend: &str, alias: &str) {
    let entries = ensure_array_mut(value, &["agent", backend, "available_models"]);
    entries.retain(|entry| entry.as_str() != Some(alias));
}

fn model_type_index(model_type: &str) -> usize {
    match model_type {
        "openrouter" => 0,
        "openrouter-resp" => 1,
        "codex-subscription" => 2,
        _ => 0,
    }
}

fn retry_mode_index(retry_mode: &str) -> usize {
    match retry_mode {
        "random" => 1,
        _ => 0,
    }
}

fn token_template_source_index(source: &str) -> usize {
    match source {
        "local" => 1,
        "huggingface" => 2,
        _ => 0,
    }
}

fn token_tokenizer_source_index(source: &str) -> usize {
    match source {
        "local" => 1,
        "huggingface" => 2,
        _ => 0,
    }
}

fn token_tokenizer_encoding_index(encoding: &str) -> usize {
    match encoding {
        "o200k_base" => 1,
        "cl100k_base" => 2,
        "o200k_harmony" => 3,
        _ => 0,
    }
}

fn current_sandbox_mode_index(value: &Value) -> usize {
    match nested_string(value, &["sandbox", "mode"]).unwrap_or("subprocess") {
        "subprocess" | "disabled" => 0,
        "bubblewrap" => 1,
        _ => 0,
    }
}

fn current_memory_system_index(value: &Value) -> usize {
    match nested_string(value, &["main_agent", "memory_system"]).unwrap_or("layered") {
        "claude_code" => 1,
        _ => 0,
    }
}

fn parse_memory_system(raw: &str) -> Result<MemorySystem> {
    match raw.trim() {
        "layered" => Ok(MemorySystem::Layered),
        "claude_code" => Ok(MemorySystem::ClaudeCode),
        other => Err(anyhow!(
            "invalid memory system `{other}`; expected `layered` or `claude_code`"
        )),
    }
}

fn parse_bool(raw: &str, field_name: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(anyhow!(
            "invalid boolean `{other}` for {field_name}; expected true or false"
        )),
    }
}

trait MemorySystemConfigValue {
    fn as_config_value(self) -> &'static str;
}

impl MemorySystemConfigValue for MemorySystem {
    fn as_config_value(self) -> &'static str {
        match self {
            MemorySystem::Layered => "layered",
            MemorySystem::ClaudeCode => "claude_code",
        }
    }
}

fn channel_summary_text(channel: &ChannelSummary) -> String {
    let bot_token_env = channel
        .raw
        .get("bot_token_env")
        .and_then(Value::as_str)
        .unwrap_or("TELEGRAM_BOT_TOKEN");
    let client_id_env = channel
        .raw
        .get("client_id_env")
        .and_then(Value::as_str)
        .unwrap_or("DINGTALK_CLIENT_ID");
    let client_secret_env = channel
        .raw
        .get("client_secret_env")
        .and_then(Value::as_str)
        .unwrap_or("DINGTALK_CLIENT_SECRET");
    let api_base_url = channel
        .raw
        .get("api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("https://api.dingtalk.com");
    let prompt = channel
        .raw
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("you> ");
    match channel.kind.as_str() {
        "telegram" => format!(
            "当前频道: {}\n类型: telegram\nbot_token_env: {}\n内建命令列表和默认 polling 会自动处理。",
            channel.id, bot_token_env
        ),
        "dingtalk" => format!(
            "当前频道: {}\n类型: dingtalk\nclient_id_env: {}\nclient_secret_env: {}\napi_base_url: {}\n使用 Stream 模式收消息，并通过 sessionWebhook 回复当前会话。",
            channel.id, client_id_env, client_secret_env, api_base_url
        ),
        "command_line" => format!(
            "当前频道: {}\n类型: command_line\nprompt: {}",
            channel.id, prompt
        ),
        _ => format!("当前频道: {}\n类型: {}", channel.id, channel.kind),
    }
}

fn load_or_create_json_document(path: &Path) -> Result<Value> {
    if path.exists() {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        return parse_json_document(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()));
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let value = empty_object();
    save_json_document(path, &value)?;
    Ok(value)
}

fn parse_json_document(raw: &str) -> Result<Value> {
    if raw.trim().is_empty() {
        return Ok(empty_object());
    }
    serde_json::from_str(raw).context("invalid JSON")
}

fn save_json_document(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::write(path, format_json_document(value)?)
        .with_context(|| format!("failed to write config file {}", path.display()))
}

fn format_json_document(value: &Value) -> Result<String> {
    let mut raw = serde_json::to_string_pretty(value).context("failed to serialize JSON")?;
    raw.push('\n');
    Ok(raw)
}

fn validate_json_document(value: &Value) -> Result<String> {
    let temp_path = std::env::temp_dir().join(format!("partyclaw-config-{}.json", Uuid::new_v4()));
    save_json_document(&temp_path, value)?;
    let result = load_server_config_file(&temp_path);
    let _ = fs::remove_file(&temp_path);
    let config = result?;
    Ok(format!(
        "Validation passed: version {}, {} model(s), {} channel(s)",
        config.version,
        config.models.len(),
        config.channels.len()
    ))
}

fn latest_server_config_skeleton() -> Value {
    json!({
        "version": LATEST_CONFIG_VERSION,
        "models": {},
        "agent": {
            "agent_frame": {
                "available_models": []
            }
        },
        "tooling": {},
        "main_agent": {
            "global_install_root": if cfg!(target_os = "windows") {
                "C:/ClawPartyPrograms"
            } else {
                "/opt"
            },
            "language": "zh-CN",
            "memory_system": "layered",
            "token_estimation_cache": {
                "template": {
                    "hf": "template-cache/hf"
                },
                "tokenizer": {
                    "hf": "tokenizer-cache/hf"
                }
            },
            "time_awareness": {
                "emit_system_date_on_user_message": false,
                "emit_idle_time_gap_hint": true
            },
            "enable_context_compression": true,
            "context_compaction": {
                "trigger_ratio": 0.9,
                "token_limit_override": Value::Null,
                "recent_fidelity_target_ratio": 0.18
            },
            "idle_compaction": {
                "enabled": false,
                "poll_interval_seconds": 15,
                "min_ratio": 0.5
            },
            "timeout_observation_compaction": {
                "enabled": true
            }
        },
        "sandbox": {
            "mode": "subprocess",
            "bubblewrap_binary": "bwrap",
            "map_docker_socket": false
        },
        "max_global_sub_agents": 4,
        "cron_poll_interval_seconds": 5,
        "channels": []
    })
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn root_object_mut(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = empty_object();
    }
    value.as_object_mut().expect("root object")
}

fn ensure_nested_object_mut<'a>(value: &'a mut Value, path: &[&str]) -> &'a mut Map<String, Value> {
    let mut current = root_object_mut(value);
    for key in path {
        let entry = current
            .entry((*key).to_string())
            .or_insert_with(empty_object);
        if !entry.is_object() {
            *entry = empty_object();
        }
        current = entry.as_object_mut().expect("section object");
    }
    current
}

fn ensure_array_mut<'a>(value: &'a mut Value, path: &[&str]) -> &'a mut Vec<Value> {
    assert!(!path.is_empty());
    let (prefix, last) = path.split_at(path.len() - 1);
    let object = ensure_nested_object_mut(value, prefix);
    let entry = object
        .entry(last[0].to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = Value::Array(Vec::new());
    }
    entry.as_array_mut().expect("array section")
}

fn nested_value<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn nested_object<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Map<String, Value>> {
    nested_value(value, path)?.as_object()
}

fn nested_array<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Vec<Value>> {
    nested_value(value, path)?.as_array()
}

fn nested_string<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    nested_value(value, path)?.as_str()
}

fn nested_bool(value: &Value, path: &[&str], default: bool) -> bool {
    nested_value(value, path)
        .and_then(Value::as_bool)
        .unwrap_or(default)
}

fn nested_number_string(value: &Value, path: &[&str]) -> Option<String> {
    let value = nested_value(value, path)?;
    match value {
        Value::Number(number) => Some(number.to_string()),
        Value::String(string) => Some(string.clone()),
        _ => None,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn set_string(object: &mut Map<String, Value>, key: &str, value: &str) {
    object.insert(key.to_string(), Value::String(value.to_string()));
}

fn set_optional_trimmed_string(object: &mut Map<String, Value>, key: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        object.remove(key);
    } else {
        set_string(object, key, value);
    }
}

fn set_bool(object: &mut Map<String, Value>, key: &str, value: bool) {
    object.insert(key.to_string(), Value::Bool(value));
}

fn set_u64(object: &mut Map<String, Value>, key: &str, value: u64) {
    object.insert(key.to_string(), Value::Number(Number::from(value)));
}

fn set_f64(object: &mut Map<String, Value>, key: &str, value: f64) -> Result<()> {
    let number = Number::from_f64(value).ok_or_else(|| anyhow!("invalid floating-point value"))?;
    object.insert(key.to_string(), Value::Number(number));
    Ok(())
}

fn set_retry_mode_object(
    object: &mut Map<String, Value>,
    mode: &str,
    max_retries: &str,
    retry_random_mean: &str,
) -> Result<()> {
    let mut retry_mode = Map::new();
    match mode {
        "random" => {
            retry_mode.insert("mode".to_string(), Value::String("random".to_string()));
            let max_retries = parse_u64(max_retries)?
                .ok_or_else(|| anyhow!("retry_mode.max_retries must not be empty"))?;
            let retry_random_mean = parse_f64(retry_random_mean)?
                .ok_or_else(|| anyhow!("retry_mode.retry_random_mean must not be empty"))?;
            set_u64(&mut retry_mode, "max_retries", max_retries);
            set_f64(&mut retry_mode, "retry_random_mean", retry_random_mean)?;
        }
        _ => {
            retry_mode.insert("mode".to_string(), Value::String("no".to_string()));
        }
    }
    object.insert("retry_mode".to_string(), Value::Object(retry_mode));
    Ok(())
}

fn set_token_estimation_object(
    object: &mut Map<String, Value>,
    template_source: &str,
    template_path: &str,
    template_repo: &str,
    template_revision: &str,
    template_file: &str,
    template_field: &str,
    template_cache_dir: &str,
    tokenizer_source: &str,
    tokenizer_encoding: &str,
    tokenizer_path: &str,
    tokenizer_repo: &str,
    tokenizer_revision: &str,
    tokenizer_file: &str,
    tokenizer_cache_dir: &str,
) -> Result<()> {
    let template_source = template_source.trim();
    let tokenizer_source = tokenizer_source.trim();
    let template_is_default = template_source.is_empty() || template_source == "builtin";
    let tokenizer_is_default = (tokenizer_source.is_empty() || tokenizer_source == "tiktoken")
        && (tokenizer_encoding.trim().is_empty() || tokenizer_encoding.trim() == "auto");
    if template_is_default && tokenizer_is_default {
        object.remove("token_estimation");
        return Ok(());
    }

    let mut token_estimation = Map::new();
    match template_source {
        "" | "builtin" => {}
        "local" => {
            let path = trim_non_empty(template_path, "token_estimation.template.path")?;
            let field = if template_field.trim().is_empty() {
                "chat_template"
            } else {
                template_field.trim()
            };
            token_estimation.insert(
                "template".to_string(),
                json!({
                    "source": "local",
                    "path": path,
                    "field": field
                }),
            );
        }
        "huggingface" => {
            let repo = trim_non_empty(template_repo, "token_estimation.template.repo")?;
            let revision = if template_revision.trim().is_empty() {
                "main"
            } else {
                template_revision.trim()
            };
            let file = if template_file.trim().is_empty() {
                "tokenizer_config.json"
            } else {
                template_file.trim()
            };
            let field = if template_field.trim().is_empty() {
                "chat_template"
            } else {
                template_field.trim()
            };
            let mut template = json!({
                "source": "huggingface",
                "repo": repo,
                "revision": revision,
                "file": file,
                "field": field
            });
            if let Some(object) = template.as_object_mut() {
                set_optional_trimmed_string(object, "cache_dir", template_cache_dir);
            }
            token_estimation.insert("template".to_string(), template);
        }
        _ if !template_is_default => {
            bail!("token_estimation.template.source must be builtin, local, or huggingface");
        }
        _ => {}
    }

    match tokenizer_source {
        "" | "tiktoken" => {
            let encoding = if tokenizer_encoding.trim().is_empty() {
                "auto"
            } else {
                tokenizer_encoding.trim()
            };
            if !matches!(
                encoding,
                "auto" | "o200k_base" | "cl100k_base" | "o200k_harmony"
            ) {
                bail!(
                    "token_estimation.tokenizer.encoding must be auto, o200k_base, cl100k_base, or o200k_harmony"
                );
            }
            if encoding != "auto" {
                token_estimation.insert(
                    "tokenizer".to_string(),
                    json!({
                        "source": "tiktoken",
                        "encoding": encoding
                    }),
                );
            }
        }
        "local" => {
            let path = trim_non_empty(tokenizer_path, "token_estimation.tokenizer.path")?;
            token_estimation.insert(
                "tokenizer".to_string(),
                json!({
                    "source": "local",
                    "path": path
                }),
            );
        }
        "huggingface" => {
            let repo = trim_non_empty(tokenizer_repo, "token_estimation.tokenizer.repo")?;
            let revision = if tokenizer_revision.trim().is_empty() {
                "main"
            } else {
                tokenizer_revision.trim()
            };
            let file = if tokenizer_file.trim().is_empty() {
                "tokenizer.json"
            } else {
                tokenizer_file.trim()
            };
            let mut tokenizer = json!({
                "source": "huggingface",
                "repo": repo,
                "revision": revision,
                "file": file
            });
            if let Some(object) = tokenizer.as_object_mut() {
                set_optional_trimmed_string(object, "cache_dir", tokenizer_cache_dir);
            }
            token_estimation.insert("tokenizer".to_string(), tokenizer);
        }
        _ => bail!("token_estimation.tokenizer.source must be tiktoken, local, or huggingface"),
    }

    object.insert(
        "token_estimation".to_string(),
        Value::Object(token_estimation),
    );
    Ok(())
}

fn parse_u64(raw: &str) -> Result<Option<u64>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .with_context(|| format!("`{trimmed}` is not a valid integer"))
}

fn parse_f64(raw: &str) -> Result<Option<f64>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<f64>()
        .map(Some)
        .with_context(|| format!("`{trimmed}` is not a valid number"))
}

fn trim_non_empty<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{label} must not be empty");
    }
    Ok(trimmed)
}

fn parse_csv_items(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn model_capability_options(current: &[String]) -> Vec<String> {
    let mut options = vec![
        "chat".to_string(),
        "web_search".to_string(),
        "image_in".to_string(),
        "image_out".to_string(),
        "pdf".to_string(),
        "audio_in".to_string(),
    ];
    for item in current {
        if !options.iter().any(|known| known == item) {
            options.push(item.clone());
        }
    }
    options
}

fn bool_string(value: bool) -> String {
    if value {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LATEST_CONFIG_VERSION, ModelFormState, format_json_document, latest_server_config_skeleton,
        load_or_create_json_document, model_capability_options, parse_json_document,
        validate_json_document,
    };
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn apply_backend_membership(value: &mut serde_json::Value, alias: &str) {
        super::update_backend_membership(value, None, alias, "agent_frame", true);
    }

    #[test]
    fn missing_config_file_is_created_as_empty_object() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");

        let value = load_or_create_json_document(&config_path).unwrap();

        assert_eq!(value, json!({}));
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            "{ }\n".replace(' ', "")
        );
    }

    #[test]
    fn blank_config_file_loads_as_empty_object() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(&config_path, "\n").unwrap();

        let value = load_or_create_json_document(&config_path).unwrap();

        assert_eq!(value, json!({}));
    }

    #[test]
    fn latest_skeleton_uses_latest_version() {
        let skeleton = latest_server_config_skeleton();
        assert_eq!(skeleton["version"], json!(LATEST_CONFIG_VERSION));
        assert_eq!(skeleton["main_agent"]["memory_system"], json!("layered"));
        assert_eq!(
            skeleton["main_agent"]["token_estimation_cache"]["template"]["hf"],
            json!("template-cache/hf")
        );
        assert_eq!(
            skeleton["main_agent"]["token_estimation_cache"]["tokenizer"]["hf"],
            json!("tokenizer-cache/hf")
        );
        assert_eq!(skeleton["sandbox"]["mode"], json!("subprocess"));
        assert_eq!(skeleton["sandbox"]["map_docker_socket"], json!(false));
        assert_eq!(
            skeleton["main_agent"]["time_awareness"]["emit_system_date_on_user_message"],
            json!(false)
        );
    }

    #[test]
    fn validation_accepts_minimal_valid_config() {
        let value = json!({
            "version": LATEST_CONFIG_VERSION,
            "models": {
                "main": {
                    "type": "openrouter",
                    "api_endpoint": "https://example.com/v1",
                    "model": "demo-model",
                    "capabilities": ["chat"]
                }
            },
            "agent": {
                "agent_frame": {
                    "available_models": ["main"]
                }
            },
            "main_agent": {
                "global_install_root": "/opt",
                "language": "zh-CN",
                "memory_system": "claude_code",
                "time_awareness": {
                    "emit_system_date_on_user_message": true,
                    "emit_idle_time_gap_hint": true
                },
                "enable_context_compression": true,
                "context_compaction": {
                    "trigger_ratio": 0.9,
                    "token_limit_override": null,
                    "recent_fidelity_target_ratio": 0.18
                },
                "idle_compaction": {
                    "enabled": false,
                    "poll_interval_seconds": 15,
                    "min_ratio": 0.5
                },
                "timeout_observation_compaction": {
                    "enabled": true
                }
            },
            "sandbox": {
                "mode": "subprocess",
                "bubblewrap_binary": "bwrap",
                "map_docker_socket": false
            },
            "max_global_sub_agents": 4,
            "cron_poll_interval_seconds": 5,
            "channels": [
                {
                    "kind": "command_line",
                    "id": "cli"
                }
            ]
        });

        let summary = validate_json_document(&value).unwrap();

        assert!(summary.contains(&format!("version {LATEST_CONFIG_VERSION}")));
        assert!(summary.contains("1 model(s)"));
        assert!(summary.contains("1 channel(s)"));
    }

    #[test]
    fn format_json_document_appends_trailing_newline() {
        let formatted = format_json_document(&json!({"a": 1})).unwrap();
        assert!(formatted.ends_with('\n'));
    }

    #[test]
    fn parse_json_document_rejects_invalid_json() {
        let err = parse_json_document("{").unwrap_err();
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn model_form_preview_keeps_boolean_fields() {
        let mut form = ModelFormState::new_with_type("openrouter");
        form.alias = "demo".to_string();
        form.model_name = "gpt-5.4".to_string();
        form.supports_vision_input = true;
        form.agent_model_enabled = true;
        form.agent_frame_enabled = true;
        let preview = form.preview_json();
        assert_eq!(preview["supports_vision_input"], json!(true));
        assert_eq!(preview["agent_model_enabled"], json!(true));
    }

    #[test]
    fn backend_membership_helper_adds_alias_once() {
        let mut value = json!({});
        apply_backend_membership(&mut value, "demo");
        apply_backend_membership(&mut value, "demo");
        let entries = value["agent"]["agent_frame"]["available_models"]
            .as_array()
            .unwrap();
        assert_eq!(entries, &vec![json!("demo")]);
    }

    #[test]
    fn model_capability_options_keep_unknown_entries() {
        let options = model_capability_options(&["chat".to_string(), "custom_cap".to_string()]);
        assert!(options.iter().any(|item| item == "chat"));
        assert!(options.iter().any(|item| item == "custom_cap"));
    }
}
