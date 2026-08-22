use crate::app::App;
use crate::commands::{arg_options, filter_commands, find_command};
use crate::theme;
use crate::transcript::{Block, Expand, Hit, Level, PERM_OPTIONS};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

type Seg = (String, Style);

fn wrap_segments(segs: &[Seg], width: usize) -> Vec<Vec<Seg>> {
    let mut rows: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut row_w = 0usize;

    fn push_char(rows: &mut Vec<Vec<Seg>>, row_w: &mut usize, c: char, st: Style, width: usize) {
        let cw = c.width().unwrap_or(0);
        if *row_w + cw > width {
            rows.push(Vec::new());
            *row_w = 0;
        }
        let row = rows.last_mut().unwrap();
        match row.last_mut() {
            Some((t, s)) if *s == st && !t.ends_with(' ') => t.push(c),
            _ => row.push((c.to_string(), st)),
        }
        *row_w += cw;
    }

    for (text, st) in segs {
        for c in text.chars() {
            if c == '\n' {
                rows.push(Vec::new());
                row_w = 0;
                continue;
            }
            push_char(&mut rows, &mut row_w, c, *st, width);
        }
    }

    while rows.len() > 1 && rows.last().map(|r| r.is_empty()).unwrap_or(false) {
        rows.pop();
    }
    if rows.iter().all(|r| r.is_empty()) {
        rows = vec![vec![(String::new(), theme::normal())]];
    }
    rows
}

fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let width = area.width as usize;

    let input_h = app.input.lines_count().clamp(1, 6) as u16;
    let busy_rows: u16 = if app.running { 1 } else { 0 };
    let pal_items = palette_items(app);
    let pal_rows = pal_items
        .as_ref()
        .map(|v| v.len().min(8) as u16)
        .unwrap_or(0);

    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(busy_rows),
        Constraint::Length(pal_rows),
        Constraint::Length(input_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let transcript = chunks[0];
    app.transcript_area = transcript;

    let all_rows = build_rows(app, width.max(4));
    let total = all_rows.len();
    let h = transcript.height as usize;
    let offset = app.scroll_from_end.min(total.saturating_sub(h));
    let start = total
        .saturating_sub(offset + h)
        .min(total.saturating_sub(h));
    let end = (start + h).min(total);
    let visible = &all_rows[start..end];

    let mut hits = vec![Hit::Nothing; h];
    let mut lines: Vec<Line> = Vec::with_capacity(h);
    for (i, (segs, hit)) in visible.iter().enumerate() {
        hits[i] = *hit;
        lines.push(Line::from(
            segs.iter()
                .map(|(t, s)| Span::styled(t.clone(), *s))
                .collect::<Vec<_>>(),
        ));
    }
    app.hitmap = hits;
    f.render_widget(Paragraph::new(lines), transcript);

    if busy_rows > 0 {
        let spin = ['|', '/', '-', '\\'][app.spinner_tick % 4];
        let mut text = if let Some(t) = app.thinking_since {
            format!("{spin} thinking · {}", t.elapsed().as_secs())
        } else {
            let secs = app.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            format!("{spin} working · {secs}s")
        };
        if !app.input.is_empty() {
            text += " · typed text queued";
        }
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, theme::dim()))),
            chunks[1],
        );
    }

    if let (Some(items), true) = (&pal_items, pal_rows > 0) {
        render_palette(f, chunks[2], items, app.palette_sel);
    }

    render_input(f, chunks[3], app);

    f.render_widget(
        Paragraph::new(Line::from(Span::styled("─".repeat(width), theme::dim()))),
        chunks[4],
    );

    render_status(f, chunks[5], app);
}

fn render_status(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let pct = app
        .ctx_used
        .checked_mul(100)
        .and_then(|n| n.checked_div(app.ctx_window))
        .unwrap_or(if app.ctx_window > 0 { 100 } else { 0 });
    let spans = vec![
        Span::styled("model:", theme::dim()),
        Span::raw(format!(" {}    ", app.model_name)),
        Span::styled("mode:", theme::dim()),
        Span::raw(format!(" {}    ", mode_label(app.mode))),
        Span::styled("ctx:", theme::dim()),
        Span::raw(format!(
            " {} / {} ({pct}%)",
            human_tokens(app.ctx_used),
            human_tokens(app.ctx_window)
        )),
    ];
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn mode_label(m: crate::perms::Mode) -> &'static str {
    match m {
        crate::perms::Mode::Plan => "plan",
        crate::perms::Mode::Build => "auto",
        crate::perms::Mode::Auto => "auto-approve",
    }
}

fn human_tokens(n: u64) -> String {
    fn one_decimal(v: f64) -> String {
        let s = format!("{v:.1}");
        if let Some(stripped) = s.strip_suffix(".0") {
            stripped.to_owned()
        } else {
            s
        }
    }
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{}k", one_decimal(n as f64 / 1000.0))
    } else {
        format!("{}M", one_decimal(n as f64 / 1_000_000.0))
    }
}

fn render_input(f: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let height = area.height as usize;
    let width = area.width as usize;

    if app.input.is_empty() && !app.running {
        let ph = format!("> {}", placeholder_text());
        let wrapped = wrap_segments(&[(ph, theme::dim())], width);
        let take: Vec<Line> = wrapped
            .into_iter()
            .take(height)
            .map(|row| {
                Line::from(
                    row.into_iter()
                        .map(|(t, s)| Span::styled(t, s))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        f.render_widget(Paragraph::new(take), area);
        return;
    }

    let styled_lines = build_styled_input_lines(app);
    let (cur_line, cur_col) = app.input.cursor_line_col();

    let shown_from = cur_line.saturating_sub(height - 1);
    let window: Vec<&Vec<Seg>> = styled_lines.iter().skip(shown_from).take(height).collect();
    let lines: Vec<Line> = window
        .iter()
        .map(|row| {
            Line::from(
                row.iter()
                    .map(|(t, s)| Span::styled(t.clone(), *s))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);

    let vis_line = cur_line - shown_from;
    if vis_line < height {
        let line_chars: Vec<char> = {
            let mut out = Vec::new();
            for (i, ch) in app.input.text.iter().enumerate() {
                if i >= app.input.cursor {
                    break;
                }
                if *ch == '\n' {
                    out.clear();
                    continue;
                }
                out.push(*ch);
            }
            out
        };
        let prefix = if cur_line == 0 {
            "> ".to_owned()
        } else {
            "  ".to_owned()
        };
        let x = area.x as usize + str_width(&prefix) + display_col(&line_chars, cur_col);
        let y = area.y as usize + vis_line;
        if y < area.y as usize + height {
            f.set_cursor_position(ratatui::prelude::Position::new(x as u16, y as u16));
        }
    }
}

fn build_styled_input_lines(app: &App) -> Vec<Vec<Seg>> {
    let mention_style = theme::blue_dim();
    let mut lines: Vec<Vec<Seg>> = vec![Vec::new()];
    let mut line_idx = 0usize;

    let push_seg = |lines: &mut Vec<Vec<Seg>>, li: usize, text: String, st: Style| {
        let row = &mut lines[li];
        match row.last_mut() {
            Some((t, s)) if *s == st => t.push_str(&text),
            _ => row.push((text, st)),
        }
    };

    let first_prefix = "> ";
    let cont_prefix = "  ";
    push_seg(&mut lines, 0, first_prefix.to_owned(), theme::normal());

    let mentions = &app.input.mentions;
    for (gi, ch) in app.input.text.iter().enumerate() {
        if *ch == '\n' {
            line_idx += 1;
            lines.push(Vec::new());
            push_seg(
                &mut lines,
                line_idx,
                cont_prefix.to_owned(),
                theme::normal(),
            );
            continue;
        }
        let in_mention = mentions.iter().any(|(a, b)| gi >= *a && gi < *b);
        push_seg(
            &mut lines,
            line_idx,
            ch.to_string(),
            if in_mention {
                mention_style
            } else {
                theme::normal()
            },
        );
    }

    let (cur_line, _) = app.input.cursor_line_col();
    if let Some(tail) = app.input.ghost_tail() {
        push_seg(&mut lines, cur_line, tail, theme::dim());
    }

    lines
}

fn display_col(chars: &[char], upto: usize) -> usize {
    chars[..upto.min(chars.len())]
        .iter()
        .map(|c| c.width().unwrap_or(0))
        .sum()
}

fn placeholder_text() -> &'static str {
    "ask keiko anything · / commands · shift+enter newline"
}

fn render_palette(f: &mut Frame, area: ratatui::layout::Rect, items: &[String], sel: usize) {
    if area.height == 0 || items.is_empty() {
        return;
    }
    let max = area.height as usize;
    let sel = sel.min(items.len().saturating_sub(1));
    let start = sel.saturating_sub(max - 1);
    let window: Vec<Line> = items
        .iter()
        .enumerate()
        .skip(start)
        .take(max)
        .map(|(i, label)| {
            let style = if i == sel {
                theme::normal()
            } else {
                theme::dim()
            };
            let marker = if i == sel { "> " } else { "  " };
            Line::from(Span::styled(format!("{marker}{label}"), style))
        })
        .collect();
    f.render_widget(Paragraph::new(window), area);
}

pub fn current_palette(app: &App) -> Option<Vec<String>> {
    let items = palette_items(app)?;
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn palette_items(app: &App) -> Option<Vec<String>> {
    let text = app.input.text();
    if !text.starts_with('/') || text.contains('\n') {
        return None;
    }
    if text.contains(' ') {
        let cmd = find_command(&text)?;
        let after = text.split_once(' ').map(|x| x.1).unwrap_or("");
        let opts: Vec<String> = match cmd.arg_kind {
            crate::commands::ArgKind::Models => merge_models(app),
            kind => arg_options(kind, &[]),
        };
        let lowered = after.to_lowercase();
        let filtered: Vec<String> = opts
            .into_iter()
            .filter(|o| o.to_lowercase().starts_with(&lowered))
            .collect();
        return if filtered.is_empty() {
            None
        } else {
            Some(filtered)
        };
    }
    let typed = text.trim_start_matches('/');
    let names = filter_commands(typed);
    if names.is_empty() {
        None
    } else {
        Some(names.into_iter().map(|n| format!("/{n}")).collect())
    }
}

fn merge_models(app: &App) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in std::iter::once(app.model_name.clone()).chain(app.models_cache.iter().cloned()) {
        if !m.is_empty() && !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn build_rows(app: &App, width: usize) -> Vec<(Vec<Seg>, Hit)> {
    let mut rows: Vec<(Vec<Seg>, Hit)> = Vec::new();
    let cap = app.cfg.diff_lines.max(10);

    // reverse video marks the keyboard-focused element
    let focused = |bi: usize, si: usize| -> bool { app.focus == Some((bi, si)) };
    let mark = |s: ratatui::style::Style, on: bool| -> ratatui::style::Style {
        if on {
            s.add_modifier(ratatui::style::Modifier::REVERSED)
        } else {
            s
        }
    };

    for (bi, block) in app.blocks.iter().enumerate() {
        if !rows.is_empty() {
            rows.push((vec![], Hit::Nothing));
        }
        match block {
            Block::User(text) | Block::Assistant(text) | Block::LiveAssistant(text) => {
                push_wrapped_text(&mut rows, text, theme::normal(), width, Hit::Nothing);
            }
            Block::Thought {
                dur_ms,
                text,
                expand,
            } => {
                let secs = dur_ms / 1000;
                let marker = if *expand == Expand::Collapsed {
                    '>'
                } else {
                    'v'
                };
                rows.push((
                    vec![(
                        format!("{marker} thought {secs}s"),
                        mark(theme::dim(), focused(bi, usize::MAX)),
                    )],
                    Hit::Block(bi),
                ));
                if *expand != Expand::Collapsed {
                    push_indented(&mut rows, text, theme::dim(), width, 2);
                }
            }
            Block::Remembered(line) => {
                rows.push((
                    vec![(format!("remembered: {line}"), theme::blue_dim())],
                    Hit::Nothing,
                ));
            }
            Block::Notice { text, level } => {
                let style = match level {
                    Level::Info => theme::dim(),
                    Level::Error => theme::red(),
                    Level::Warn => theme::amber(),
                };
                push_wrapped_text(&mut rows, text, style, width, Hit::Nothing);
            }
            Block::MemoryView { text } => {
                for l in text.lines() {
                    let style = if l.starts_with(' ') {
                        theme::dim()
                    } else {
                        theme::blue_dim()
                    };
                    rows.push((vec![(l.to_owned(), style)], Hit::Nothing));
                }
            }
            Block::PermAsk(ask) => match &ask.resolved {
                Some(choice) => {
                    let text = format!("✓ {} {} ({})", ask.verb, ask.target, choice);
                    rows.push((vec![(text, theme::dim())], Hit::Nothing));
                }
                None => {
                    let header = format!(
                        "? {} {}{}  [{}]",
                        ask.verb,
                        ask.target,
                        if ask.sensitive { " (sensitive)" } else { "" },
                        ask.cap_label
                    );
                    rows.push((vec![(header, theme::amber())], Hit::Nothing));
                    for (oi, opt) in PERM_OPTIONS.iter().enumerate() {
                        let style = if oi == ask.selected {
                            theme::amber()
                        } else {
                            theme::dim()
                        };
                        let marker = if oi == ask.selected { ">" } else { " " };
                        rows.push((
                            vec![(format!("  {marker} {} {opt}", oi + 1), style)],
                            Hit::PermOption(bi, oi),
                        ));
                    }
                }
            },
            Block::Steps(group) => {
                let marker = if group.expanded { 'v' } else { '>' };
                rows.push((
                    vec![(
                        format!("{marker} {}", group.summary()),
                        mark(theme::dim(), focused(bi, usize::MAX)),
                    )],
                    Hit::Step(bi, usize::MAX),
                ));
                if group.expanded {
                    for (si, step) in group.steps.iter().enumerate() {
                        let style = match step.view.verb.word() {
                            "failed" | "error" => theme::red(),
                            "denied" => theme::amber(),
                            _ => theme::dim(),
                        };
                        rows.push((
                            vec![(
                                format!("  {}", step.headline()),
                                mark(style, focused(bi, si)),
                            )],
                            Hit::Step(bi, si),
                        ));
                        if step.expand != Expand::Collapsed {
                            for dr in step.detail_rows(cap) {
                                let dstyle = if dr.starts_with('+') {
                                    theme::green()
                                } else if dr.starts_with('-') {
                                    theme::red()
                                } else {
                                    theme::dim()
                                };
                                rows.push((vec![(format!("    {dr}"), dstyle)], Hit::Step(bi, si)));
                            }
                        }
                    }
                }
            }
        }
    }

    rows
}

fn push_wrapped_text(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    style: Style,
    width: usize,
    hit: Hit,
) {
    for segs in wrap_segments(&[(text.to_owned(), style)], width) {
        rows.push((segs, hit));
    }
}

fn push_indented(
    rows: &mut Vec<(Vec<Seg>, Hit)>,
    text: &str,
    style: Style,
    width: usize,
    indent: usize,
) {
    let pad = " ".repeat(indent);
    for l in text.lines() {
        let segs = vec![(format!("{pad}{l}"), style)];
        for r in wrap_segments(&segs, width) {
            rows.push((r, Hit::Nothing));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::app::App;
    use crate::config::Config;
    use crate::memory::Memory;
    use crate::perms::{Mode, PermEngine};
    use crate::providers::openai::OpenAiProvider;
    use crate::transcript::{StepBlock, StepsGroup};
    use std::sync::{Arc, Mutex};

    fn test_app(tag: &str) -> App {
        let root = std::env::temp_dir().join(format!("keiko-ui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let cfg = Arc::new(Config {
            model: "test-model".into(),
            context_window: 200_000,
            project_root: root.clone(),
            project_config_path: root.join(".keiko/config.toml"),
            ..Default::default()
        });
        let perms = Arc::new(Mutex::new(PermEngine::new(
            root.clone(),
            vec![],
            Default::default(),
        )));
        let memory = Memory::new(&root, &root.join(".data"));
        let provider = OpenAiProvider::new("http://127.0.0.1:9/v1", None, "test-model").unwrap();
        let agent = Arc::new(Agent::new(
            provider,
            cfg.clone(),
            perms,
            memory.clone(),
            Default::default(),
        ));
        let mut app = App::new(
            cfg,
            agent,
            memory,
            root.join("hist.txt"),
            root.join("sessions"),
            None,
        );
        app.model_name = "sonnet-5".into();
        app.mode = Mode::Build;
        app.ctx_used = 12_400;
        app
    }

    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        let mut rows = Vec::new();
        for y in 0..h {
            let mut line = String::new();
            for x in 0..w {
                line += buf[(x, y)].symbol();
            }
            rows.push(line.trim_end().to_owned());
        }
        rows
    }

    #[test]
    fn status_bar_and_placeholder_contract() {
        let mut app = test_app("status");
        let rows = render(&mut app, 80, 10);
        let status = &rows[9];
        assert!(status.contains("model:"), "{status:?}");
        assert!(status.contains("sonnet-5"), "{status:?}");
        assert!(status.contains("mode:"), "{status:?}");
        assert!(status.contains("auto"), "{status:?}");
        assert!(status.contains("ctx:"), "{status:?}");
        assert!(status.contains("12.4k / 200k (6%)"), "{status:?}");
        let sep = char::from_u32(0x2500).unwrap().to_string().repeat(80);
        assert_eq!(rows[8], sep, "thin separator above status bar");
        let joined = rows.join("\n");
        assert!(joined.contains("ask keiko anything"), "{joined:?}");
    }

    #[test]
    fn collapsed_steps_summary_with_error_count() {
        let mut app = test_app("steps");
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![
                StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Read,
                        arg: "README.md".into(),
                        detail: None,
                    },
                    expand: Expand::Collapsed,
                },
                StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Failed,
                        arg: "cargo test".into(),
                        detail: None,
                    },
                    expand: Expand::Collapsed,
                },
                StepBlock {
                    view: crate::agent::StepView {
                        verb: crate::agent::Verb::Ran,
                        arg: "cargo build".into(),
                        detail: None,
                    },
                    expand: Expand::Collapsed,
                },
            ],
            expanded: false,
            outcome: Some(crate::agent::TaskOutcome::Done),
        }));
        let rows = render(&mut app, 60, 12);
        let joined = rows.join("\n");
        assert!(joined.contains("> 3 steps · 1 error"), "{joined:?}");
        assert!(
            !joined.contains("README.md"),
            "collapsed group must hide step args"
        );
    }

    fn thought_block(dur_ms: u64) -> Block {
        Block::Thought {
            dur_ms,
            text: "considering options".into(),
            expand: Expand::Collapsed,
        }
    }

    fn step_with_diff(arg: &str) -> StepBlock {
        StepBlock {
            view: crate::agent::StepView {
                verb: crate::agent::Verb::Wrote,
                arg: arg.into(),
                detail: Some(crate::agent::Detail::Diff {
                    lines: vec![crate::diffgen::DiffLine {
                        sign: '+',
                        text: "new line".into(),
                    }],
                    capped_at: None,
                }),
            },
            expand: Expand::Collapsed,
        }
    }

    #[test]
    fn expandable_targets_in_visual_order() {
        let mut app = test_app("targets");
        app.blocks.push(thought_block(1500));
        let mut group = StepsGroup {
            steps: vec![step_with_diff("a.txt")],
            expanded: false,
            outcome: None,
        };
        group.steps.push(StepBlock {
            view: crate::agent::StepView {
                verb: crate::agent::Verb::Ran,
                arg: "cargo fmt".into(),
                detail: Some(crate::agent::Detail::Message("ok".into())),
            },
            expand: Expand::Collapsed,
        });
        app.blocks.push(Block::Steps(group));

        // thought header, group header, diff-capable step only
        // (Message detail does not respect Expand, so it is not a target)
        assert_eq!(
            app.expandable_targets(),
            vec![(0, usize::MAX), (1, usize::MAX), (1, 0)]
        );
    }

    #[test]
    fn focus_moves_wraps_and_space_toggles() {
        let mut app = test_app("focus");
        app.blocks.push(thought_block(1500));
        app.blocks.push(Block::Steps(StepsGroup {
            steps: vec![step_with_diff("a.txt")],
            expanded: false,
            outcome: None,
        }));

        app.move_focus(true);
        assert_eq!(app.focus, Some((0, usize::MAX)));
        app.move_focus(false);
        assert_eq!(app.focus, Some((1, 0)), "wraps backwards to last target");

        app.focus = Some((1, usize::MAX));
        assert!(app.toggle_focused());
        match &app.blocks[1] {
            Block::Steps(g) => assert!(g.expanded, "space on group headline expands it"),
            _ => panic!("expected steps block"),
        }

        app.focus = Some((1, 0));
        assert!(app.toggle_focused());
        match &app.blocks[1] {
            Block::Steps(g) => {
                assert_eq!(g.steps[0].expand, Expand::Shown);
            }
            _ => panic!("expected steps block"),
        }
    }

    #[test]
    fn mouse_click_on_thought_toggles() {
        let mut app = test_app("thought-click");
        app.blocks.push(thought_block(1500));
        render(&mut app, 60, 12); // builds the hitmap
        assert_eq!(app.hitmap[0], Hit::Block(0));
        app.on_click(0); // first transcript row is the thought header
        match &app.blocks[0] {
            Block::Thought { expand, .. } => {
                assert!(*expand != Expand::Collapsed, "click must unfold thought");
            }
            _ => panic!("expected thought block"),
        }
    }
}
