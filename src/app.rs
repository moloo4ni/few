use crate::agent::{Agent, AgentEvent, Detail, StepView, TaskOutcome};
use crate::commands::{find_command, ArgKind};
use crate::config::Config;
use crate::inputline::InputState;
use crate::memory::{MemLevel, Memory};
use crate::perms::{Grant, Mode, PermEngine};
use crate::providers::openai::OpenAiProvider;
use crate::providers::Provider as _;
use crate::sysprompt;
use crate::tools::Ctl;
use crate::transcript::{
    classify_notice, Block, Expand, Hit, Level, PermAskBlock, StepBlock, StepsGroup, PERM_OPTIONS,
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
    sessions_dir: PathBuf,
    /// session identity, updated by background saves (serialized by the mutex)
    session: Arc<std::sync::Mutex<Option<crate::session::SessionRef>>>,
    live_text_idx: Option<usize>,
    live_thought: String,
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
        let mut input = InputState::new();
        for entry in load_history(&history_path) {
            input.push_history(&entry);
        }
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
            ctx_used: 0,
            ctx_window: cfg.context_window,
            escalation: None,
            quit: false,
            hitmap: Vec::new(),
            focus: None,
            transcript_area: Default::default(),
            palette_sel: 0,
            models_cache: cfg.models.clone(),
            cfg,
            agent,
            memory,
            history_path,
            sessions_dir,
            session: Arc::new(std::sync::Mutex::new(
                resume.as_ref().and_then(|(r, _)| r.clone()),
            )),
            live_text_idx: None,
            live_thought: String::new(),
            live_step: None,
            file_index: Arc::new(Mutex::new(Vec::new())),
            ctl_tx: None,
            app_tx,
            app_rx,
            suspended: false,
            ev_tx,
            ev_rx,
        };
        if let Some((_, note)) = resume {
            app.push_notice(note);
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
            // while $EDITOR owns the terminal, Keiko must not draw anything
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
                    AppMsg::EditorDone => self.suspended = false,
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
                self.input.insert_str(&clean(&text));
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
            Hit::Block(bi) => {
                // only thought headers are toggleable via Hit::Block
                if let Some(Block::Thought { expand, .. }) = self.blocks.get_mut(bi) {
                    *expand = expand.next();
                }
            }
            Hit::Step(bi, usize::MAX) => {
                if let Some(Block::Steps(g)) = self.blocks.get_mut(bi) {
                    g.expanded = !g.expanded;
                }
            }
            Hit::Step(bi, si) => {
                if let Some(Block::Steps(g)) = self.blocks.get_mut(bi) {
                    if let Some(s) = g.steps.get_mut(si) {
                        s.expand = s.expand.next();
                    }
                }
            }
            Hit::PermOption(bi, opt) => {
                if self.active_ask == Some(bi) {
                    self.resolve_ask(opt);
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
                Block::Thought { .. } => out.push((bi, usize::MAX)),
                Block::Steps(g) => {
                    out.push((bi, usize::MAX));
                    for (si, s) in g.steps.iter().enumerate() {
                        if matches!(
                            s.view.detail,
                            Some(Detail::Diff { .. }) | Some(Detail::Output { .. })
                        ) {
                            out.push((bi, si));
                        }
                    }
                }
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
            Some(Block::Thought { expand, .. }) if si == usize::MAX => {
                *expand = expand.next();
                true
            }
            Some(Block::Steps(g)) if si == usize::MAX => {
                g.expanded = !g.expanded;
                true
            }
            Some(Block::Steps(g)) => match g.steps.get_mut(si) {
                Some(s) => {
                    s.expand = s.expand.next();
                    true
                }
                None => false,
            },
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
                KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace | KeyCode::Tab => {
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
                self.push_notice_level(format!("unknown command: {other}"), Level::Error);
            }
        }
        self.scroll_from_end = 0;
    }

    fn apply_mode(&mut self, m: Mode) {
        self.mode = m;
        PermEngine::lock(&self.agent.perms).set_mode(m);
        self.agent.set_mode_directive(sysprompt::mode_directive(m));
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
        let _ = self.memory.ensure_files();
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
            let _ = tx.send(AgentEvent::Notice(msg));
            // always restore drawing, even when the editor failed
            let _ = atx.send(AppMsg::EditorDone);
        });
    }

    fn create_steps_group(&mut self) -> usize {
        self.blocks.push(Block::Steps(StepsGroup {
            steps: Vec::new(),
            expanded: true,
            outcome: None,
        }));
        self.blocks.len() - 1
    }

    fn push_notice(&mut self, text: String) {
        self.push_notice_level(text, Level::Info);
    }

    fn push_notice_level(&mut self, text: String, level: Level) {
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
            AgentEvent::ThinkingFinished { dur_ms } => {
                self.thinking_since = None;
                let text = clean(&std::mem::take(&mut self.live_thought));
                if !text.is_empty() {
                    self.blocks.push(Block::Thought {
                        dur_ms,
                        text,
                        expand: Expand::Collapsed,
                    });
                }
            }
            AgentEvent::AssistantDelta { text } => {
                // per-delta cleaning: a control sequence split across chunk
                // boundaries could survive, but assistant text is the least
                // hostile source; complete strings are cleaned fully elsewhere
                let text = clean(&text);
                let open = match self.live_text_idx.and_then(|i| self.blocks.get_mut(i)) {
                    Some(Block::LiveAssistant(existing)) => {
                        existing.push_str(&text);
                        true
                    }
                    _ => false,
                };
                if !open {
                    self.blocks.push(Block::LiveAssistant(text));
                    self.live_text_idx = Some(self.blocks.len() - 1);
                }
            }
            AgentEvent::TurnClosed => {
                self.seal_live_blocks();
            }
            AgentEvent::Step(view) => {
                self.live_step = None;
                let gi = match self.steps_group_idx {
                    Some(gi) => gi,
                    None => self.create_steps_group_and_set(),
                };
                if let Some(Block::Steps(g)) = self.blocks.get_mut(gi) {
                    g.steps.push(StepBlock {
                        view: sanitize_step(view),
                        expand: Expand::Collapsed,
                    });
                }
            }
            AgentEvent::Remembered { line } => {
                self.blocks.push(Block::Remembered(clean(&line)));
            }
            AgentEvent::Notice(text) => {
                let level = classify_notice(&text);
                self.blocks.push(Block::Notice {
                    text: clean(&text),
                    level,
                });
            }
            AgentEvent::AssistantText(text) => {
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
                self.seal_live_blocks();
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

    fn seal_live_blocks(&mut self) {
        if let Some(i) = self.live_text_idx.take() {
            let sealed = match self.blocks.get_mut(i) {
                Some(Block::LiveAssistant(text)) => Some(std::mem::take(text)),
                _ => None,
            };
            if let Some(text) = sealed {
                self.blocks[i] = Block::Assistant(text);
            }
        }
        self.thinking_since = None;
    }

    fn create_steps_group_and_set(&mut self) -> usize {
        let gi = self.create_steps_group();
        self.steps_group_idx = Some(gi);
        gi
    }

    async fn submit(&mut self) {
        let text = self.input.text();
        if text.trim().is_empty() {
            return;
        }
        self.input.clear();

        if text.starts_with('/') && !text.contains('\n') {
            self.execute_command(&text).await;
            return;
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

    fn save_session(&mut self) {
        let convo = self.agent.snapshot_convo();
        if convo.is_empty() {
            return;
        }
        let root = self.cfg.project_root.clone();
        let model = self.agent.provider.model_name();
        let dir = self.sessions_dir.clone();
        let session = Arc::clone(&self.session);
        let ev = self.ev_tx.clone();
        // serialization + disk IO happen off the render loop; the shared
        // mutex also serializes overlapping saves so an older snapshot can
        // never overwrite a newer one
        tokio::task::spawn_blocking(move || {
            let prev = session.lock().ok().and_then(|g| g.clone());
            match crate::session::save(&dir, &root, &model, prev.as_ref(), convo) {
                Ok(r) => {
                    if let Ok(mut g) = session.lock() {
                        *g = Some(r);
                    }
                }
                Err(e) => {
                    let _ = ev.send(AgentEvent::Notice(format!("failed saving session: {e}")));
                }
            }
        });
    }

    fn spawn_index_rebuild(&mut self) {
        let root = self.cfg.project_root.clone();
        let idx = Arc::clone(&self.file_index);
        tokio::spawn(async move {
            let files = build_file_index(root).await;
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

fn append_history_line(path: &PathBuf, entry: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", escape_history(entry));
    }
}

async fn build_file_index(root: PathBuf) -> Vec<String> {
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
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
}
