use crate::agent::{Agent, AgentEvent, Detail, NoticeLevel, StepView, TaskOutcome, Verb};
use crate::commands::{find_command, ArgKind};
use crate::config::Config;
use crate::inputline::InputState;
use crate::memory::{MemLevel, Memory};
use crate::perms::{Grant, Mode, PermEngine};
use crate::providers::openai::OpenAiProvider;
use crate::providers::{Msg, Provider as _, Role, ToolCall};
use crate::sysprompt;
use crate::tools::Ctl;

use crate::transcript::{
    Block, Expand, Hit, PermAskBlock, ResumedItem, ResumedSession, StepBlock, StepItem, StepsGroup,
    PERM_OPTIONS,
};
use crate::ui_text::clean;
use crate::uirender;
use anyhow::Context as _;
use crossterm::event::{EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const ESCALATION_WINDOW: Duration = Duration::from_millis(1200);

/// Internal application messages, separate from agent events.
pub enum AppMsg {
    /// $EDITOR session finished - resume drawing
    EditorDone,
}

struct SessionSaveRequest {
    model: String,
    last_prompt_tokens: u64,
    messages: Vec<Msg>,
}

pub struct App {
    pub blocks: Vec<Block>,
    pub steps_group_idx: Option<usize>,
    pub active_ask: Option<usize>,
    pub input: InputState,
    pub scroll_from_end: usize,
    /// transcript row count at the previous frame - keeps manual scroll
    /// anchored while new content arrives
    pub scroll_total_seen: usize,
    pub running: bool,
    pub started_at: Option<Instant>,
    pub thinking_since: Option<Instant>,
    pub mode: Mode,
    pub model_name: String,
    pub ctx_used: u64,
    pub ctx_window: u64,
    pub escalation: Option<Instant>,
    pub quit: bool,
    pub hitmap: Vec<Hit>,
    /// keyboard focus over expandable transcript elements: (block_idx, step_idx)
    /// (step_idx == usize::MAX means a group/thought header)
    pub focus: Option<(usize, usize)>,
    pub transcript_area: ratatui::layout::Rect,
    pub palette_sel: usize,
    pub models_cache: Vec<String>,
    pub cfg: Arc<Config>,
    pub agent: Arc<Agent<OpenAiProvider>>,
    pub memory: Memory,
    history_path: PathBuf,
    /// Ordered background persistence. The worker owns the current session
    /// identity so an older snapshot can never overtake a newer one.
    session_tx: Option<std::sync::mpsc::Sender<SessionSaveRequest>>,
    session_worker: Option<std::thread::JoinHandle<()>>,
    live_narration: String,
    live_thought: String,
    /// slot of the most recent folded Narration pushed this turn, so Finished
    /// can promote it to a visible answer if the final turn ended with no prose
    /// after its last tool call (i.e. the model wrote the answer then ran a verify)
    last_said: Option<(usize, usize)>,
    /// whether the most recent (final) turn left any prose after its last tool
    /// call - gates promotion of a folded Narration to a visible answer
    final_turn_had_post_prose: bool,
    /// action currently executing, shown in present tense until its final step arrives
    pub live_step: Option<(String, String)>,
    file_index: Arc<Mutex<Vec<String>>>,
    ctl_tx: Option<mpsc::UnboundedSender<Ctl>>,
    app_tx: mpsc::UnboundedSender<AppMsg>,
    app_rx: mpsc::UnboundedReceiver<AppMsg>,
    /// true while an external $EDITOR owns the terminal
    suspended: bool,
    ev_tx: mpsc::UnboundedSender<AgentEvent>,
    ev_rx: mpsc::UnboundedReceiver<AgentEvent>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<Config>,
        agent: Arc<Agent<OpenAiProvider>>,
        memory: Memory,
        history_path: PathBuf,
        sessions_dir: PathBuf,
        resume: Option<(Option<crate::session::SessionRef>, String)>,
    ) -> Self {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (app_tx, app_rx) = mpsc::unbounded_channel();
        let initial_session = resume.as_ref().and_then(|(session, _)| session.clone());
        let (session_tx, session_worker) = spawn_session_saver(
            sessions_dir.clone(),
            cfg.project_root.clone(),
            initial_session,
            ev_tx.clone(),
        );
        let mut input = InputState::new();
        for entry in load_history(&history_path) {
            input.push_history(&entry);
        }
        let restored_context_tokens = agent.context_tokens();
        let mut app = Self {
            blocks: Vec::new(),
            steps_group_idx: None,
            active_ask: None,
            input,
            scroll_from_end: 0,
            scroll_total_seen: 0,
            running: false,
            started_at: None,
            thinking_since: None,
            mode: Mode::Build,
            model_name: cfg.model.clone(),
            ctx_used: restored_context_tokens,
            ctx_window: cfg.context_window,
            escalation: None,
            quit: false,
            hitmap: Vec::new(),
            focus: None,
            transcript_area: Default::default(),
            palette_sel: 0,
            models_cache: cfg.models.clone(),
            cfg: cfg.clone(),
            agent,
            memory,
            history_path,
            session_tx: Some(session_tx),
            session_worker: Some(session_worker),
            live_narration: String::new(),
            live_thought: String::new(),
            last_said: None,
            final_turn_had_post_prose: false,
            live_step: None,
            file_index: Arc::new(Mutex::new(Vec::new())),
            ctl_tx: None,
            app_tx,
            app_rx,
            suspended: false,
            ev_tx,
            ev_rx,
        };
        if let Some((session, note)) = resume {
            if session.is_some() {
                app.blocks.push(Block::Resumed(ResumedSession {
                    label: note,
                    items: resumed_items(&app.agent.snapshot_convo()),
                    expanded: false,
                }));
            } else {
                app.push_notice(note);
            }
        }
        app
    }

    pub async fn run_app(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> anyhow::Result<()> {
        self.spawn_index_rebuild();
        self.agent.refresh_memory_layer();
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(120));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if self.quit {
                break;
            }
            // while $EDITOR owns the terminal, Few must not draw anything
            if !self.suspended {
                terminal.draw(|f| uirender::draw(f, self))?;
            }
            tokio::select! {
                maybe_ev = events.next(), if !self.suspended => {
                    match maybe_ev {
                        Some(Ok(ev)) => self.on_term_event(ev).await,
                        Some(Err(e)) => {
                            self.push_notice(format!("input error: {e}"));
                        }
                        None => break,
                    }
                }
                Some(ae) = self.ev_rx.recv() => self.on_agent_event(ae),
                Some(msg) = self.app_rx.recv() => match msg {
                    AppMsg::EditorDone => {
                        // $EDITOR returned: re-take the terminal and force a full
                        // redraw. ratatui diffs against its own buffer, which still
                        // holds the pre-editor frame, so without a clear the
                        // (already cleared) alternate screen would keep missing rows.
                        self.suspended = false;
                        let _ = terminal.clear();
                    }
                },
                _ = tick.tick() => {
                    if let Some(t) = self.escalation {
                        if t.elapsed() > ESCALATION_WINDOW {
                            self.escalation = None;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn absorb_models(&mut self, list: Vec<String>) {
        let mut merged: Vec<String> = Vec::new();
        if !self.model_name.is_empty() {
            merged.push(self.model_name.clone());
        }
        let rest = list.into_iter().chain(self.models_cache.iter().cloned());
        for m in rest {
            if !merged.contains(&m) {
                merged.push(m);
            }
        }
        self.models_cache = merged;
    }

    async fn on_term_event(&mut self, ev: crossterm::event::Event) {
        match ev {
            crossterm::event::Event::Key(k) => {
                if k.kind == KeyEventKind::Press {
                    self.on_key(k).await;
                }
            }
            crossterm::event::Event::Mouse(m) => self.on_mouse(m),
            crossterm::event::Event::Paste(text) => {
                // Drop a trailing newline from pasted text (copies from a terminal
                // or chat UI usually end with one) so no empty input line appears.
                let pasted = clean(&text).trim_end_matches('\n').to_string();
                self.input.insert_str(&pasted);
                self.after_edit();
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        match m.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_from_end = self.scroll_from_end.saturating_add(3)
            }
            MouseEventKind::ScrollDown => {
                self.scroll_from_end = self.scroll_from_end.saturating_sub(3)
            }
            MouseEventKind::Up(MouseButton::Left) => self.on_click(m.row),
            // hover highlights clickable (expandable) rows so the user can see
            // what responds to a click
            MouseEventKind::Moved => {
                // only highlight rows that actually expand on click (the group
                // header, steps with detail, and thought/said lines) - not
                // remembered lines or detail-less steps, which are no-ops
                let rel = m.row.saturating_sub(self.transcript_area.y) as usize;
                self.focus = self.hitmap.get(rel).copied().and_then(|h| match h {
                    Hit::Step(bi, si) => {
                        let t = (bi, si);
                        if self.expandable_targets().contains(&t) {
                            Some(t)
                        } else {
                            None
                        }
                    }
                    Hit::Block(bi) if matches!(self.blocks.get(bi), Some(Block::Resumed(_))) => {
                        Some((bi, usize::MAX))
                    }
                    _ => None,
                });
            }
            _ => {}
        }
    }

    pub fn on_click(&mut self, row: u16) {
        let rel = row.saturating_sub(self.transcript_area.y) as usize;
        let Some(hit) = self.hitmap.get(rel).copied() else {
            return;
        };
        match hit {
            Hit::Nothing => {}
            Hit::Step(bi, usize::MAX) => {
                self.focus = Some((bi, usize::MAX));
                if let Some(Block::Steps(g)) = self.blocks.get_mut(bi) {
                    g.expanded = !g.expanded;
                }
            }
            Hit::Step(bi, si) => {
                let t = (bi, si);
                // Only focus and expand rows that actually respond to a click.
                if !self.expandable_targets().contains(&t) {
                    return;
                }
                self.focus = Some(t);
                if let Some(Block::Steps(g)) = self.blocks.get_mut(bi) {
                    match g.steps.get_mut(si) {
                        // Full is visited only when Shown actually truncates
                        // detail; short content folds on the second click.
                        Some(StepItem::Step(s)) => {
                            s.expand = s.next_expand(self.cfg.diff_lines.max(10));
                        }
                        Some(StepItem::Thought { expand, .. })
                        | Some(StepItem::Narration { expand, .. }) => {
                            *expand = expand.next_binary();
                        }
                        None => {}
                    }
                }
            }
            Hit::PermOption(bi, opt) => {
                if self.active_ask == Some(bi) {
                    self.resolve_ask(opt);
                }
            }
            Hit::Block(bi) => {
                self.focus = Some((bi, usize::MAX));
                if let Some(Block::Resumed(resumed)) = self.blocks.get_mut(bi) {
                    resumed.expanded = !resumed.expanded;
                }
            }
        }
    }

    /// Expandable transcript elements in visual order. A step is listed when
    /// its detail actually respects Expand (diffs and captured output).
    pub fn expandable_targets(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (bi, block) in self.blocks.iter().enumerate() {
            match block {
                Block::Steps(g) => {
                    out.push((bi, usize::MAX));
                    for (si, item) in g.steps.iter().enumerate() {
                        match item {
                            StepItem::Step(step) => {
                                if matches!(
                                    step.view.detail,
                                    Some(Detail::Diff { .. }) | Some(Detail::Output { .. })
                                ) {
                                    out.push((bi, si));
                                }
                            }
                            StepItem::Thought { .. } | StepItem::Narration { .. } => {
                                out.push((bi, si));
                            }
                        }
                    }
                }
                Block::Resumed(_) => out.push((bi, usize::MAX)),
                _ => {}
            }
        }
        out
    }

    pub fn move_focus(&mut self, forward: bool) {
        let targets = self.expandable_targets();
        if targets.is_empty() {
            return;
        }
        let next = match self.focus {
            None => {
                if forward {
                    0
                } else {
                    targets.len() - 1
                }
            }
            Some(cur) => {
                let pos = targets.iter().position(|&t| t == cur).unwrap_or(0);
                if forward {
                    (pos + 1) % targets.len()
                } else {
                    (pos + targets.len() - 1) % targets.len()
                }
            }
        };
        self.focus = Some(targets[next]);
    }

    pub fn toggle_focused(&mut self) -> bool {
        let Some((bi, si)) = self.focus else {
            return false;
        };
        match self.blocks.get_mut(bi) {
            Some(Block::Steps(g)) if si == usize::MAX => {
                g.expanded = !g.expanded;
                true
            }
            Some(Block::Steps(g)) => match g.steps.get_mut(si) {
                Some(StepItem::Step(s)) => {
                    s.expand = s.next_expand(self.cfg.diff_lines.max(10));
                    true
                }
                Some(StepItem::Thought { expand, .. })
                | Some(StepItem::Narration { expand, .. }) => {
                    *expand = expand.next_binary();
                    true
                }
                None => false,
            },
            Some(Block::Resumed(resumed)) if si == usize::MAX => {
                resumed.expanded = !resumed.expanded;
                true
            }
            _ => false,
        }
    }

    async fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.ctrl_c_ladder();
            return;
        }

        // transcript cursor: Alt+Up/Down to move, Space toggles, typing clears
        if k.modifiers.contains(KeyModifiers::ALT) && matches!(k.code, KeyCode::Up | KeyCode::Down)
        {
            self.move_focus(k.code == KeyCode::Down);
            return;
        }
        if self.focus.is_some() {
            match k.code {
                KeyCode::Char(' ') => {
                    if !self.toggle_focused() {
                        self.focus = None;
                    }
                    return;
                }
                KeyCode::Esc => {
                    self.focus = None;
                    return;
                }
                KeyCode::Char(_)
                | KeyCode::Enter
                | KeyCode::Backspace
                | KeyCode::Tab
                | KeyCode::BackTab => {
                    self.focus = None;
                    // fall through: the key keeps its normal meaning
                }
                _ => {}
            }
        }

        if self.active_ask.is_some() {
            self.on_ask_key(k.code);
            return;
        }

        let text = self.input.text();
        let shift_tab = k.code == KeyCode::BackTab
            || (k.code == KeyCode::Tab && k.modifiers.contains(KeyModifiers::SHIFT));
        if shift_tab {
            self.cycle_mode();
            return;
        }
        if text.starts_with('/') && !text.contains('\n') {
            match k.code {
                KeyCode::Esc => self.input.clear(),
                KeyCode::Up => self.palette_sel = self.palette_sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => {
                    let len = uirender::current_palette(self)
                        .map(|v| v.len())
                        .unwrap_or(0);
                    if len > 0 {
                        self.palette_sel = (self.palette_sel + 1).min(len - 1);
                    }
                }
                KeyCode::Enter => {
                    self.pick_palette().await;
                    return;
                }
                KeyCode::Backspace => {
                    self.input.backspace();
                    self.palette_sel = 0;
                }
                KeyCode::Char(ch) => {
                    self.input.insert_str(&ch.to_string());
                    self.palette_sel = 0;
                }
                _ => {}
            }
            return;
        }

        if self.input.menu_items().is_some() {
            match k.code {
                KeyCode::Esc => self.input.completion = None,
                KeyCode::Tab | KeyCode::Down => self.input.cycle_menu(true),
                KeyCode::Up => self.input.cycle_menu(false),
                KeyCode::Right | KeyCode::Enter => self.input.accept_selected(),
                KeyCode::Char(ch) => {
                    self.input.insert_str(&ch.to_string());
                    self.after_edit();
                }
                KeyCode::Backspace => {
                    self.input.backspace();
                    self.after_edit();
                }
                _ => {}
            }
            return;
        }

        match k.code {
            KeyCode::PageUp => {
                self.scroll_from_end += self.transcript_area.height as usize;
            }
            KeyCode::PageDown => {
                self.scroll_from_end = self
                    .scroll_from_end
                    .saturating_sub(self.transcript_area.height as usize);
            }
            KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.newline()
            }
            KeyCode::Enter => {
                if k.modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                {
                    self.input.newline();
                } else {
                    self.submit().await;
                }
            }
            KeyCode::Tab => {
                let index = self.file_index.lock().unwrap();
                self.input.update_completion(&index);
                drop(index);
                // With no candidate Tab is intentionally a no-op, so typing a
                // path never changes the permission mode by accident.
                if self.input.completion.is_some() {
                    self.input.accept_selected();
                }
            }
            KeyCode::Esc => self.input.completion = None,
            KeyCode::Char(ch) => {
                self.input.insert_str(&ch.to_string());
                self.after_edit();
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.after_edit();
            }
            KeyCode::Delete => self.input.delete_forward(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up => self.input.history_prev(),
            KeyCode::Down => self.input.history_next(),
            _ => {}
        }
    }

    fn after_edit(&mut self) {
        let text = self.input.text();
        if text.starts_with('/') && !text.contains('\n') {
            self.palette_sel = 0;
            return;
        }
        let index = self.file_index.lock().unwrap();
        self.input.update_completion(&index);
    }

    fn ctrl_c_ladder(&mut self) {
        let now = Instant::now();
        let quick = self
            .escalation
            .map(|t| now.duration_since(t) <= ESCALATION_WINDOW)
            .unwrap_or(false);
        self.escalation = Some(now);

        if self.running {
            if quick {
                if let Some(tx) = &self.ctl_tx {
                    let _ = tx.send(Ctl::HardAbort);
                }
            } else {
                let (ack, _rx) = oneshot::channel();
                if let Some(tx) = &self.ctl_tx {
                    let _ = tx.send(Ctl::SoftInterrupt { ack });
                }
                self.push_notice("^C interrupt requested".into());
            }
        } else if quick {
            self.quit = true;
        } else {
            self.push_notice("^C press again to exit".into());
        }
    }

    fn on_ask_key(&mut self, code: KeyCode) {
        let Some(bi) = self.active_ask else { return };
        let selected = match self.blocks.get(bi) {
            Some(Block::PermAsk(a)) if a.resolved.is_none() => a.selected,
            _ => {
                self.active_ask = None;
                return;
            }
        };
        match code {
            KeyCode::Up => {
                if let Some(Block::PermAsk(a)) = self.blocks.get_mut(bi) {
                    a.selected = a.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Tab => {
                if let Some(Block::PermAsk(a)) = self.blocks.get_mut(bi) {
                    a.selected = (a.selected + 1).min(PERM_OPTIONS.len() - 1);
                }
            }
            KeyCode::Char(c @ '1'..='4') => {
                let opt = (c as u8 - b'1') as usize;
                self.resolve_ask(opt);
            }
            KeyCode::Enter => self.resolve_ask(selected),
            KeyCode::Esc => self.resolve_ask(3),
            _ => {}
        }
    }

    fn resolve_ask(&mut self, opt: usize) {
        let Some(bi) = self.active_ask else { return };
        let grant = match opt {
            3 => None,
            2 => Some(Grant::Always),
            1 => Some(Grant::Session),
            _ => Some(Grant::Once),
        };
        if let Some(Block::PermAsk(a)) = self.blocks.get_mut(bi) {
            a.resolved = Some(PERM_OPTIONS[opt.min(PERM_OPTIONS.len() - 1)]);
            let id = a.id;
            if let Some(tx) = &self.ctl_tx {
                let _ = tx.send(Ctl::PermChoice { id, grant });
            }
        }
        self.active_ask = None;
    }

    async fn pick_palette(&mut self) {
        let items = match uirender::current_palette(self) {
            Some(items) => items,
            None => return,
        };
        if items.is_empty() {
            return;
        }
        self.palette_sel = self.palette_sel.min(items.len() - 1);
        let item = items[self.palette_sel].clone();

        let text = self.input.text();
        if item.starts_with('/') && !text.contains(' ') {
            match find_command(&item) {
                Some(cmd) if cmd.arg_kind == ArgKind::None => {
                    self.execute_command(&item).await;
                    return;
                }
                Some(_) => {
                    self.input.set_text(&format!("{item} "));
                    self.palette_sel = 0;
                    return;
                }
                None => {}
            }
        }
        let base = text.split(' ').next().unwrap_or("").to_owned();
        let full = if base.is_empty() {
            item.clone()
        } else {
            format!("{base} {item}")
        };
        self.input.clear();
        self.execute_command(&full).await;
    }

    async fn execute_command(&mut self, cmd: &str) {
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        let Some(name) = parts.first() else { return };
        let rest = parts[1..].join(" ");
        match *name {
            "/exit" => self.quit = true,
            "/mode" => match parse_mode(&rest) {
                Some(m) => {
                    self.apply_mode(m);
                    self.push_notice(format!("mode · {}", label_mode(m)));
                }
                None => self.input.set_text("/mode "),
            },
            "/model" => {
                if rest.is_empty() || rest == "list" {
                    self.push_notice("fetching models…".into());
                    let mut list = self.agent.provider.list_models().await.unwrap_or_default();
                    for m in &self.cfg.models {
                        if !list.contains(m) {
                            list.insert(0, m.clone());
                        }
                    }
                    self.absorb_models(list);
                    self.input.set_text("/model ");
                } else {
                    self.agent.provider.set_model(&rest);
                    self.model_name = rest.clone();
                    self.push_notice(format!("model · {rest}"));
                }
            }
            "/memory" => match rest.as_str() {
                "view project" | "" => self.memory_view(MemLevel::Project),
                "view persistent" | "persistent" => self.memory_view(MemLevel::Persistent),
                "edit project" => self.memory_edit(MemLevel::Project),
                "edit persistent" => self.memory_edit(MemLevel::Persistent),
                _ => self.input.set_text("/memory "),
            },
            other => {
                self.push_notice_level(format!("unknown command: {other}"), NoticeLevel::Error);
            }
        }
        self.scroll_from_end = 0;
    }

    fn apply_mode(&mut self, m: Mode) {
        self.mode = m;
        PermEngine::lock(&self.agent.perms).set_mode(m);
        self.agent.set_mode_directive(sysprompt::mode_directive(m));
    }

    /// Cycle the agent mode (Build -> Plan -> Auto -> Build) from Shift+Tab.
    fn cycle_mode(&mut self) {
        let next = match self.mode {
            Mode::Build => Mode::Plan,
            Mode::Plan => Mode::Auto,
            Mode::Auto => Mode::Build,
        };
        self.apply_mode(next);
    }

    fn memory_view(&mut self, level: MemLevel) {
        let path = self.memory.level_path(level).to_path_buf();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let entries = Memory::entries(&text);
        let display = match level {
            MemLevel::Project => self.memory.display_project_path(),
            MemLevel::Persistent => self.memory.display_persistent_path(),
        };
        let mut out = format!("{} · {}\n", level.label(), display);
        if entries.is_empty() {
            out += "  (empty)\n";
        } else {
            for e in entries {
                out += &format!("  - {e}\n");
            }
        }
        self.blocks.push(Block::MemoryView {
            text: clean(out.trim_end()),
        });
    }

    fn memory_edit(&mut self, level: MemLevel) {
        let path = self.memory.level_path(level).to_path_buf();
        let _ = self.memory.ensure_file(level);
        // hand the terminal over to $EDITOR: stop drawing until it returns
        self.suspended = true;
        let agent = Arc::clone(&self.agent);
        let tx = self.ev_tx.clone();
        let atx = self.app_tx.clone();
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || run_editor_blocking(&path)).await;
            agent.refresh_memory_layer();
            let msg = match res {
                Ok(Ok(())) => "memory updated".to_owned(),
                Ok(Err(e)) => format!("editor failed: {e:#}"),
                Err(e) => format!("editor join failed: {e}"),
            };
            let _ = tx.send(AgentEvent::Notice {
                text: msg,
                level: crate::agent::NoticeLevel::Info,
            });
            // always restore drawing, even when the editor failed
            let _ = atx.send(AppMsg::EditorDone);
        });
    }

    fn create_steps_group(&mut self) -> usize {
        // Expanded by default: while a task runs the transcript shows the step
        // headlines with a live "N steps" counter. The one task-wide group
        // collapses to a compact "> N steps" header only at Finished.
        self.blocks.push(Block::Steps(StepsGroup {
            steps: Vec::new(),
            expanded: true,
            outcome: None,
        }));
        self.blocks.len() - 1
    }

    fn push_notice(&mut self, text: String) {
        self.push_notice_level(text, NoticeLevel::Info);
    }

    fn push_notice_level(&mut self, text: String, level: NoticeLevel) {
        self.blocks.push(Block::Notice { text, level });
    }

    fn on_agent_event(&mut self, ae: AgentEvent) {
        match ae {
            AgentEvent::ThinkingStarted => {
                self.live_thought.clear();
                self.thinking_since = Some(Instant::now());
            }
            AgentEvent::ThoughtDelta { text } => {
                self.live_thought.push_str(&text);
            }
            AgentEvent::ThinkingFinished { .. } => {
                self.thinking_since = None;
                let text = clean(&std::mem::take(&mut self.live_thought));
                if !text.is_empty() {
                    self.flush_narration(false);
                    self.push_step_item(StepItem::Thought {
                        text,
                        expand: Expand::Collapsed,
                    });
                }
            }
            AgentEvent::AssistantDelta { text } => {
                // accumulate intermediate prose; it is folded into the current
                // step group as a collapsed Narration item once the turn seals
                self.live_narration.push_str(&clean(&text));
            }
            AgentEvent::TurnClosed => {
                // Remember whether this turn left any prose after its last tool
                // call: only then is the concluding answer auto-visible. The
                // final-turn promotion (if needed) happens at Finished.
                //
                // The step group is intentionally NOT collapsed or detached here.
                // Per the UX spec the whole task collapses into ONE line on
                // completion - not several nested sub-groups - so a task is a
                // single group that stays expanded through every turn and only
                // collapses to "> N steps" at Finished.
                self.final_turn_had_post_prose = !self.live_narration.trim().is_empty();
                self.flush_narration(true);
            }
            AgentEvent::Step(view) => {
                self.live_step = None;
                self.flush_narration(false);
                // A memory write surfaces outside the step group, so a completed
                // task cannot hide a durable fact behind its collapsed summary.
                if let Some(facts) = memory_facts(&self.memory, &self.cfg.project_root, &view) {
                    for fact in facts {
                        self.blocks.push(Block::Remembered(clean(&fact)));
                    }
                    return;
                }
                self.push_step_item(StepItem::Step(StepBlock {
                    view: sanitize_step(view),
                    expand: Expand::Collapsed,
                }));
            }
            AgentEvent::Notice { text, level } => {
                self.blocks.push(Block::Notice {
                    text: clean(&text),
                    level,
                });
            }
            AgentEvent::AssistantText(text) => {
                // the concluding answer is shown verbatim, not folded into steps
                self.blocks.push(Block::Assistant(clean(&text)));
            }
            AgentEvent::Usage { prompt_tokens, .. } => {
                if prompt_tokens > 0 {
                    self.ctx_used = prompt_tokens;
                }
            }
            AgentEvent::PermAsk(view) => {
                self.blocks.push(Block::PermAsk(PermAskBlock {
                    id: view.id,
                    verb: clean(&view.verb),
                    target: clean(&view.target),
                    cap_label: clean(view.cap_label),
                    sensitive: view.sensitive,
                    selected: 0,
                    resolved: None,
                }));
                self.active_ask = Some(self.blocks.len() - 1);
            }
            AgentEvent::StepStarted(view) => {
                let v = sanitize_step_start(view);
                self.live_step = Some((v.0.to_owned(), v.1));
            }
            AgentEvent::Finished(outcome) => {
                self.flush_narration(true);
                // The final turn's answer: if it was written before the last
                // verify step it sits folded as a Narration - promote it to a
                // visible top-level answer so it is not hidden. Narration from
                // earlier turns stays folded inside its own group.
                if !self.final_turn_had_post_prose {
                    if let Some((gi, si)) = self.last_said.take() {
                        if let Some(Block::Steps(g)) = self.blocks.get_mut(gi) {
                            if let Some(StepItem::Narration { text, .. }) = g.steps.get(si) {
                                let t = text.clone();
                                g.steps.remove(si);
                                self.blocks.push(Block::Assistant(t));
                            }
                        }
                    }
                }
                self.last_said = None;
                self.final_turn_had_post_prose = false;
                self.thinking_since = None;
                self.live_thought.clear();
                self.live_step = None;
                if let Some(gi) = self.steps_group_idx {
                    let mut dropped = false;
                    if let Some(Block::Steps(g)) = self.blocks.get_mut(gi) {
                        g.outcome = Some(outcome.clone());
                        g.expanded = false;
                        dropped = g.steps.is_empty() && outcome == TaskOutcome::Done;
                    }
                    if dropped && self.blocks.len() == gi + 1 {
                        self.blocks.remove(gi);
                        // keep the keyboard focus pointing at a real element
                        if let Some((bi, _)) = self.focus {
                            if bi == gi {
                                self.focus = None;
                            }
                        }
                    }
                }
                self.steps_group_idx = None;
                self.running = false;
                self.started_at = None;
                self.escalation = None;
                self.ctl_tx = None;
                self.active_ask = None;
                self.save_session();
                self.spawn_index_rebuild();
            }
        }
    }

    fn push_step_item(&mut self, item: StepItem) {
        let gi = match self.steps_group_idx {
            Some(gi) => gi,
            None => self.create_steps_group_and_set(),
        };
        if let Some(Block::Steps(g)) = self.blocks.get_mut(gi) {
            g.steps.push(item);
        }
    }

    /// Flush accumulated assistant prose. Intermediate narration (mid-turn) is
    /// folded into the step group as a collapsed item; the concluding answer is
    /// shown verbatim as its own block so the agent is never left silent.
    fn flush_narration(&mut self, final_answer: bool) {
        let text = std::mem::take(&mut self.live_narration);
        if text.trim().is_empty() {
            return;
        }
        if final_answer {
            // the turn's concluding answer is shown verbatim, never folded away
            self.blocks.push(Block::Assistant(text));
        } else {
            // interim prose is folded into the current step group as a collapsed
            // Narration; remember its slot so Finished can promote it to a
            // visible top-level answer if the final turn ended without post-step prose
            self.push_step_item(StepItem::Narration {
                text: text.clone(),
                expand: Expand::Collapsed,
            });
            if let Some(gi) = self.steps_group_idx {
                if let Some(Block::Steps(g)) = self.blocks.get(gi) {
                    self.last_said = Some((gi, g.steps.len() - 1));
                }
            }
        }
    }

    fn create_steps_group_and_set(&mut self) -> usize {
        let gi = self.create_steps_group();
        self.steps_group_idx = Some(gi);
        gi
    }

    async fn submit(&mut self) {
        let text = self.input.text();
        let mentioned_paths = self.input.mentioned_paths();
        if text.trim().is_empty() {
            return;
        }
        self.input.clear();

        if text.starts_with('/') && !text.contains('\n') {
            self.execute_command(&text).await;
            return;
        }

        {
            let mut perms = PermEngine::lock(&self.agent.perms);
            for path in mentioned_paths {
                let path = crate::paths::resolve_under(&self.cfg.project_root, &path);
                perms.grant_implicit_read(&path);
            }
        }

        if self.running {
            if let Some(tx) = &self.ctl_tx {
                let _ = tx.send(Ctl::QueuedUser(text));
                self.push_notice("queued · applied at the next safe point".into());
            }
            return;
        }

        append_history_line(&self.history_path, &text);
        self.start_task(text);
    }

    fn start_task(&mut self, text: String) {
        self.input.push_history(&text);
        // display goes through the sanitizer; the agent receives raw text
        self.blocks.push(Block::User(clean(&text)));
        let gi = self.create_steps_group();
        self.steps_group_idx = Some(gi);
        self.running = true;
        self.started_at = Some(Instant::now());
        self.scroll_from_end = 0;

        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        self.ctl_tx = Some(ctl_tx);
        let agent = Arc::clone(&self.agent);
        let ev = self.ev_tx.clone();
        tokio::spawn(async move {
            let outcome: TaskOutcome = agent.run(text, ev, ctl_rx).await;
            let _ = outcome;
        });
    }

    fn save_session(&self) {
        let convo = self.agent.snapshot_convo();
        if convo.is_empty() {
            return;
        }
        if let Some(tx) = &self.session_tx {
            let _ = tx.send(SessionSaveRequest {
                model: self.agent.provider.model_name(),
                last_prompt_tokens: self.agent.context_tokens(),
                messages: convo,
            });
        }
    }

    fn spawn_index_rebuild(&mut self) {
        let root = self.cfg.project_root.clone();
        let project_detected = self.cfg.project_detected;
        let idx = Arc::clone(&self.file_index);
        tokio::spawn(async move {
            let files = build_file_index(root, project_detected).await;
            *idx.lock().unwrap() = files;
        });
    }
}

fn parse_mode(s: &str) -> Option<Mode> {
    match s.trim() {
        "plan" => Some(Mode::Plan),
        "build" => Some(Mode::Build),
        "auto" | "auto-approve" => Some(Mode::Auto),
        _ => None,
    }
}

fn resumed_items(messages: &[Msg]) -> Vec<ResumedItem> {
    let mut items = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User if !msg.content.starts_with("[few ") => {
                let text = clean(&msg.content);
                if !text.trim().is_empty() {
                    items.push(ResumedItem::User(text));
                }
            }
            Role::Assistant => {
                let text = clean(&msg.content);
                if !text.trim().is_empty() {
                    items.push(ResumedItem::Assistant(text));
                }
                for call in &msg.tool_calls {
                    items.push(ResumedItem::Step(clean(&resumed_tool_label(call))));
                }
            }
            Role::System | Role::Tool | Role::User => {}
        }
    }
    items
}

fn resumed_tool_label(call: &ToolCall) -> String {
    let arg = call.primary_arg();
    let verb = match call.name.as_str() {
        "read" => "read",
        "write" if call.arguments.get("delete").and_then(|v| v.as_bool()) == Some(true) => {
            "deleted"
        }
        "write" | "edit" => "wrote",
        "shell" => "ran",
        other => other,
    };
    if arg.is_empty() {
        verb.to_owned()
    } else {
        format!("{verb} {arg}")
    }
}

/// If a step mutates a known memory file, return the recorded `- fact`
/// lines so they can be shown as `remembered:` entries instead of a generic
/// write/edit step. Every added fact line is surfaced - the diff already
/// contains only what changed, so re-reading the file (which now includes the
/// fact) must not be used to filter additions out.
fn memory_facts(memory: &Memory, root: &std::path::Path, view: &StepView) -> Option<Vec<String>> {
    match view.verb {
        Verb::Wrote | Verb::Deleted | Verb::Renamed => {}
        _ => return None,
    }
    let candidate = if std::path::Path::new(&view.arg).is_absolute() {
        std::path::PathBuf::from(&view.arg)
    } else {
        root.join(&view.arg)
    };
    let _ = memory.path_level(&candidate)?;
    let Detail::Diff { lines, .. } = view.detail.as_ref()? else {
        return None;
    };
    let mut facts = Vec::new();
    for l in lines {
        if l.sign == '+' {
            if let Some(fact) = l.text.trim().strip_prefix("- ") {
                let fact = fact.trim().to_string();
                if !fact.is_empty() {
                    facts.push(fact);
                }
            }
        }
    }
    if facts.is_empty() {
        None
    } else {
        Some(facts)
    }
}

/// Sanitize step data at the UI boundary - tool output and file paths are
/// untrusted rendering input.
fn sanitize_step(mut view: StepView) -> StepView {
    view.arg = clean(&view.arg);
    view.detail = view.detail.map(|d| match d {
        Detail::Output {
            text,
            total_bytes,
            truncated,
        } => Detail::Output {
            text: clean(&text),
            total_bytes,
            truncated,
        },
        Detail::Message(m) => Detail::Message(clean(&m)),
        other => other,
    });
    view
}

fn sanitize_step_start(view: crate::agent::StepStartView) -> (&'static str, String) {
    (view.verb.doing(), clean(&view.arg))
}

pub fn label_mode(m: Mode) -> &'static str {
    match m {
        Mode::Plan => "plan",
        Mode::Build => "build",
        Mode::Auto => "auto-approve",
    }
}

fn escape_history(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape_history(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn load_history(path: &PathBuf) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<String> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(unescape_history)
        .collect();
    if lines.len() > 1000 {
        lines[lines.len() - 1000..].to_vec()
    } else {
        lines
    }
}

fn append_history_line(path: &std::path::Path, entry: &str) {
    use std::io::Write;
    if let Ok(mut f) = crate::fsutil::open_private_append(path) {
        let _ = writeln!(f, "{}", escape_history(entry));
    }
}

fn spawn_session_saver(
    dir: PathBuf,
    root: PathBuf,
    current: Option<crate::session::SessionRef>,
    ev: mpsc::UnboundedSender<AgentEvent>,
) -> (
    std::sync::mpsc::Sender<SessionSaveRequest>,
    std::thread::JoinHandle<()>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || session_saver_loop(rx, dir, root, current, ev));
    (tx, worker)
}

impl Drop for App {
    fn drop(&mut self) {
        // Closing the sender lets the worker drain every queued snapshot. Join
        // it so a fast `/exit` cannot terminate the process mid-save.
        self.session_tx.take();
        if let Some(worker) = self.session_worker.take() {
            let _ = worker.join();
        }
    }
}

fn session_saver_loop(
    rx: std::sync::mpsc::Receiver<SessionSaveRequest>,
    dir: PathBuf,
    root: PathBuf,
    mut current: Option<crate::session::SessionRef>,
    ev: mpsc::UnboundedSender<AgentEvent>,
) {
    for request in rx {
        match crate::session::save(
            &dir,
            &root,
            &request.model,
            current.as_ref(),
            request.last_prompt_tokens,
            request.messages,
        ) {
            Ok(saved) => current = Some(saved),
            Err(error) => {
                let _ = ev.send(AgentEvent::Notice {
                    text: format!("failed saving session: {error}"),
                    level: NoticeLevel::Error,
                });
            }
        }
    }
}

async fn build_file_index(root: PathBuf, recursive: bool) -> Vec<String> {
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        if !recursive {
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten().take(20_000) {
                    if entry.path().is_file() {
                        out.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            out.sort();
            return out;
        }
        let walker = ignore::WalkBuilder::new(&root).build();
        for entry in walker.flatten() {
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
            if is_file {
                if let Ok(rel) = entry.path().strip_prefix(&root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
            if out.len() >= 20_000 {
                break;
            }
        }
        out.sort();
        out
    })
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod file_index_tests {
    use super::*;

    #[tokio::test]
    async fn non_project_index_is_shallow() {
        let root = std::env::temp_dir().join(format!("few-index-shallow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("top.txt"), "top").unwrap();
        std::fs::write(root.join("nested/private.txt"), "private").unwrap();

        let shallow = build_file_index(root.clone(), false).await;
        assert_eq!(shallow, vec!["top.txt"]);
        let recursive = build_file_index(root.clone(), true).await;
        assert!(recursive.contains(&"nested/private.txt".to_owned()));
        let _ = std::fs::remove_dir_all(&root);
    }
}

fn run_editor_blocking(path: &PathBuf) -> anyhow::Result<()> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".into()
            } else {
                "vi".into()
            }
        });

    crate::tui::suspend()?;
    let result = std::process::Command::new(editor).arg(path).status();
    crate::tui::resume()?;
    result.context("editor could not be started").map(|_| ())
}

#[cfg(test)]
mod history_escape_tests {
    use super::*;

    #[test]
    fn multiline_history_roundtrips() {
        let entry = "first line\ncode: let x = \n 1;\nlast";
        let esc = escape_history(entry);
        assert!(!esc.contains('\n'), "escaped entry is one file line");
        assert_eq!(unescape_history(&esc), entry);
        // a literal backslash survives the roundtrip
        let bs = "path\\to\\file";
        assert_eq!(unescape_history(&escape_history(bs)), bs);
    }

    #[test]
    fn resumed_history_keeps_dialogue_and_summarizes_tools() {
        let messages = vec![
            Msg::user("Create hello.py"),
            Msg {
                role: Role::Assistant,
                tool_calls: vec![
                    ToolCall::parse(
                        "w1".into(),
                        "write".into(),
                        r#"{"path":"hello.py","content":"print('hi')"}"#.into(),
                    ),
                    ToolCall::parse(
                        "s1".into(),
                        "shell".into(),
                        r#"{"command":"python3 hello.py"}"#.into(),
                    ),
                ],
                ..Default::default()
            },
            Msg::tool_result("w1", "write", "large raw output is omitted"),
            Msg::tool_result("s1", "shell", "hi"),
            Msg::assistant("Done."),
            Msg::user("[few verify] internal feedback"),
        ];

        let items = resumed_items(&messages);
        assert_eq!(items.len(), 4);
        assert!(matches!(&items[0], ResumedItem::User(s) if s == "Create hello.py"));
        assert!(matches!(&items[1], ResumedItem::Step(s) if s == "wrote hello.py"));
        assert!(matches!(&items[2], ResumedItem::Step(s) if s == "ran python3 hello.py"));
        assert!(matches!(&items[3], ResumedItem::Assistant(s) if s == "Done."));
    }

    #[cfg(unix)]
    #[test]
    fn history_file_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!("few-history-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("state/history.txt");

        append_history_line(&path, "private prompt");

        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_saver_keeps_request_order() {
        let root = std::env::temp_dir().join(format!("few-session-worker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("project");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&project).unwrap();
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let worker_sessions = sessions.clone();
        let worker_project = project.clone();
        let worker = std::thread::spawn(move || {
            session_saver_loop(request_rx, worker_sessions, worker_project, None, event_tx);
        });

        request_tx
            .send(SessionSaveRequest {
                model: "m".into(),
                last_prompt_tokens: 1,
                messages: vec![Msg::user("older")],
            })
            .unwrap();
        request_tx
            .send(SessionSaveRequest {
                model: "m".into(),
                last_prompt_tokens: 2,
                messages: vec![Msg::user("newer")],
            })
            .unwrap();
        drop(request_tx);
        worker.join().unwrap();

        let (_, saved) = crate::session::load_latest(&sessions, &project)
            .unwrap()
            .unwrap();
        assert_eq!(saved.last_prompt_tokens, 2);
        assert_eq!(saved.messages[0].content, "newer");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod memory_step_tests {
    use super::*;
    use crate::agent::{AgentEvent, Detail, StepView, Verb};
    use crate::config::Config;
    use crate::memory::Memory;
    use crate::perms::{PermEngine, Policy};
    use crate::providers::openai::OpenAiProvider;
    use std::sync::{Arc, Mutex};

    fn app_with(root: std::path::PathBuf) -> App {
        app_with_context(root, 0)
    }

    fn app_with_context(root: std::path::PathBuf, restored_tokens: u64) -> App {
        app_with_context_and_detection(root, restored_tokens, true)
    }

    fn app_with_context_and_detection(
        root: std::path::PathBuf,
        restored_tokens: u64,
        project_detected: bool,
    ) -> App {
        let cfg = Arc::new(Config {
            model: "m".into(),
            context_window: 1000,
            project_root: root.clone(),
            project_config_path: root.join(".few/config.toml"),
            project_detected,
            ..Default::default()
        });
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            Default::default(),
            Policy::Ask,
            Policy::Ask,
            project_detected,
        )));
        let memory = Memory::new(&root, &root.join(".data"));
        let provider = OpenAiProvider::new("http://127.0.0.1:9/v1", None, "m").unwrap();
        let agent = Arc::new(Agent::new(
            provider,
            cfg.clone(),
            perms,
            memory.clone(),
            Default::default(),
        ));
        if restored_tokens > 0 {
            agent.restore_convo(vec![Msg::user("restored")], restored_tokens);
        }
        App::new(
            cfg,
            agent,
            memory,
            root.join("hist"),
            root.join("sessions"),
            None,
        )
    }

    #[test]
    fn restored_context_usage_initializes_status_state() {
        let root =
            std::env::temp_dir().join(format!("few-resume-context-{}-usage", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let app = app_with_context(root.clone(), 12_345);
        assert_eq!(app.ctx_used, 12_345);
        assert_eq!(app.agent.context_tokens(), 12_345);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn tab_completes_while_shift_tab_cycles_mode() {
        let root = std::env::temp_dir().join(format!("few-tab-mode-{}-keys", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = app_with(root.clone());
        *app.file_index.lock().unwrap() = vec!["src/main.rs".into()];

        app.input.set_text("src/ma");
        app.after_edit();
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.text(), "src/main.rs");
        assert_eq!(app.mode, Mode::Build, "completion must not change mode");

        app.input.set_text("no-match");
        app.after_edit();
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(app.mode, Mode::Build, "bare Tab with no match is a no-op");

        app.input.set_text("src/ma");
        app.after_edit();
        app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(app.input.text(), "src/ma");
        assert!(
            app.input.completion.is_some(),
            "mode cycling must preserve the active completion"
        );

        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.mode, Mode::Auto);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn accepted_completion_grants_only_non_sensitive_read() {
        let root =
            std::env::temp_dir().join(format!("few-completion-grant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let mut app = app_with_context_and_detection(root.clone(), 0, false);
        app.running = true;
        *app.file_index.lock().unwrap() = vec!["notes.txt".into()];
        app.input.set_text("Read not");
        app.after_edit();
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        app.submit().await;
        let notes = root.join("notes.txt");
        assert_eq!(
            PermEngine::lock(&app.agent.perms)
                .check(crate::perms::Capability::FsRead, Some(&notes)),
            crate::perms::Check::Allowed
        );

        let mut sensitive = app_with_context_and_detection(root.clone(), 0, false);
        sensitive.running = true;
        *sensitive.file_index.lock().unwrap() = vec![".env".into()];
        sensitive.input.set_text("Read .e");
        sensitive.after_edit();
        sensitive
            .on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        sensitive.submit().await;
        let env = root.join(".env");
        assert_eq!(
            PermEngine::lock(&sensitive.agent.perms)
                .check(crate::perms::Capability::FsRead, Some(&env)),
            crate::perms::Check::Ask { sensitive: true }
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_write_becomes_remembered_not_step() {
        let root = std::env::temp_dir().join(format!("few-mem-{}-{}", std::process::id(), "a"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = app_with(root);
        let mem_path = app.memory.project_path.to_string_lossy().to_string();

        app.on_agent_event(AgentEvent::Step(StepView {
            verb: Verb::Wrote,
            arg: mem_path.clone(),
            detail: Some(Detail::Diff {
                lines: vec![crate::diffgen::DiffLine {
                    sign: '+',
                    text: "- calc.py adds two numbers".into(),
                }],
                capped_at: None,
            }),
        }));

        // The memory write surfaces as a top-level `remembered:` block, not as
        // a generic wrote step or part of a collapsible steps summary.
        let remembered: Vec<String> = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Remembered(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        assert!(
            remembered.iter().any(|f| f.contains("adds two numbers")),
            "memory write must surface as a visible top-level remembered block"
        );
        assert!(
            !app.blocks.iter().any(|b| match b {
                Block::Steps(g) => g.steps.iter().any(|s| {
                    matches!(s, StepItem::Step(st) if matches!(st.view.verb, Verb::Wrote) && st.view.arg == mem_path)
                }),
                _ => false,
            }),
            "memory write must not be a generic wrote step"
        );

        // a non-memory file write stays a normal step
        app.on_agent_event(AgentEvent::Step(StepView {
            verb: Verb::Wrote,
            arg: "src/main.rs".into(),
            detail: None,
        }));
        assert!(
            app.blocks.iter().any(|b| matches!(b, Block::Steps(g) if g
                    .steps
                    .iter()
                    .any(|s| matches!(s, StepItem::Step(_))))),
            "non-memory write stays a step"
        );
    }

    #[test]
    fn memory_write_shows_every_added_fact_line() {
        let root = std::env::temp_dir().join(format!("few-mem-{}-{}", std::process::id(), "b"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = app_with(root.clone());
        let mem_path = app.memory.project_path.to_string_lossy().to_string();
        let _ = std::fs::create_dir_all(app.memory.project_path.parent().unwrap());
        std::fs::write(
            &app.memory.project_path,
            "# Few memory\n\n- already known fact\n",
        )
        .unwrap();

        app.on_agent_event(AgentEvent::Step(StepView {
            verb: Verb::Wrote,
            arg: mem_path.clone(),
            detail: Some(Detail::Diff {
                lines: vec![
                    crate::diffgen::DiffLine {
                        sign: '+',
                        text: "- already known fact".into(),
                    },
                    crate::diffgen::DiffLine {
                        sign: '+',
                        text: "- brand new fact".into(),
                    },
                ],
                capped_at: None,
            }),
        }));

        let remembered: Vec<String> = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Remembered(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        // Every added `- fact` line is surfaced immediately as `remembered:`
        // (the diff already carries only what changed, so we never filter by
        // the now-updated file content).
        assert!(
            remembered.iter().any(|f| f.contains("brand new fact")),
            "new fact shown"
        );
        assert!(
            remembered.iter().any(|f| f.contains("already known")),
            "added fact line shown even if the same text was already in the file"
        );
    }

    #[test]
    fn memory_write_surfaces_fact_even_after_file_updated() {
        // Mirrors the real run: the agent writes the memory file (so it already
        // contains the fact) and then reports the write via a relative arg + a
        // diff. The memory-fact display must not read the now-updated file to
        // filter the addition out.
        let root = std::env::temp_dir().join(format!("few-mem-{}-{}", std::process::id(), "c"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = app_with(root.clone());
        let mem_path = app.memory.project_path.clone();
        std::fs::create_dir_all(mem_path.parent().unwrap()).unwrap();
        std::fs::write(
            &mem_path,
            "# Few memory\n\n- fact: greet.py is a greeting module\n",
        )
        .unwrap();

        app.on_agent_event(AgentEvent::Step(StepView {
            verb: Verb::Wrote,
            arg: ".few/memory/project.md".into(),
            detail: Some(Detail::Diff {
                lines: vec![crate::diffgen::DiffLine {
                    sign: '+',
                    text: "- fact: greet.py is a greeting module".into(),
                }],
                capped_at: None,
            }),
        }));

        let remembered: Vec<String> = app
            .blocks
            .iter()
            .filter_map(|b| match b {
                Block::Remembered(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        assert!(
            remembered.iter().any(|f| f.contains("greet.py")),
            "memory write must surface as top-level remembered even when the file already holds the fact"
        );
        assert!(
            !app.blocks.iter().any(|b| match b {
                Block::Steps(g) => g.steps.iter().any(|s| {
                    matches!(s, StepItem::Step(st) if matches!(st.view.verb, Verb::Wrote) && st.view.arg == ".few/memory/project.md")
                }),
                _ => false,
            }),
            "memory write must not be a generic wrote step"
        );
    }

    #[test]
    fn final_answer_shows_as_visible_block() {
        let root = std::env::temp_dir().join(format!("few-ans-{}-{}", std::process::id(), "c"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = app_with(root);

        app.on_agent_event(AgentEvent::AssistantDelta {
            text: "Готово: создал greet.py и записал факт в память".into(),
        });
        app.on_agent_event(AgentEvent::TurnClosed);

        assert!(
            app.blocks
                .iter()
                .any(|b| matches!(b, Block::Assistant(f) if f.contains("Готово"))),
            "final answer must be a visible Assistant block, not folded away"
        );
        assert!(
            !app.blocks.iter().any(|b| matches!(b, Block::Steps(g) if g
                    .steps
                    .iter()
                    .any(|s| matches!(s, StepItem::Narration { .. })))),
            "final answer must not be a collapsed Narration inside steps"
        );
    }

    #[tokio::test]
    async fn answer_before_final_step_is_promoted_to_visible() {
        let root = std::env::temp_dir().join(format!("few-ans2-{}-{}", std::process::id(), "c"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let mut app = app_with(root);

        // model writes the answer, then runs a verification step - the answer
        // prose is folded as a Narration at that step, so Finished must promote
        // it to a visible top-level answer rather than hiding it
        app.on_agent_event(AgentEvent::AssistantDelta {
            text: "Файл уже существует и корректен".into(),
        });
        app.on_agent_event(AgentEvent::Step(StepView {
            verb: Verb::Ran,
            arg: "python3 -m py_compile greet.py".into(),
            detail: None,
        }));
        app.on_agent_event(AgentEvent::TurnClosed);
        app.on_agent_event(AgentEvent::Finished(TaskOutcome::Done));

        assert!(
            app.blocks
                .iter()
                .any(|b| matches!(b, Block::Assistant(f) if f.contains("Файл уже существует"))),
            "answer written before the final step must be promoted to a visible Assistant block"
        );
        assert!(
            !app.blocks.iter().any(|b| matches!(b, Block::Steps(g) if g
                .steps
                .iter()
                .any(|s| matches!(s, StepItem::Narration { text, .. } if text.contains("Файл уже существует"))))),
            "promoted answer must not remain a collapsed Narration"
        );
    }
}
