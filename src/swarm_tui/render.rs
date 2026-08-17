//! Drawing. Reads [`App`]; never writes it.
//!
//! [`draw`] is the screen router's other half: one arm per [`Screen`] variant,
//! so mounting a new screen is a variant plus an arm here.
//!
//! Colours stay inside the ANSI-16 set and collapse to the terminal default
//! when `NO_COLOR` is set, so the TUI is legible on a 16-colour console and on
//! a plain one.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use zeroclaw_config::traits::PropKind;
use zeroclaw_runtime::quickstart::FieldDescriptor;
use zeroclaw_runtime::swarm::board::BoxStatus;
use zeroclaw_runtime::swarm::store::{
    PersistedSwarm, SwarmBudget, SwarmBudgetLimits, SwarmPauseReason, SwarmSpend, SwarmStatus,
};

use super::state::{App, Modal, Screen};
use super::view::{BoxBadge, EditLine, LineKind, Mode, ViewState};
use super::wizard::WizardState;

/// Column widths on the dashboard, in cells.
const NAME_WIDTH: usize = 26;
const STATUS_WIDTH: usize = 12;
const BOXES_WIDTH: usize = 6;
const BUDGET_WIDTH: usize = 10;
/// `2026-08-16T21:35:58` — date and clock, without the fractional seconds and
/// offset an RFC-3339 stamp trails.
const TIMESTAMP_WIDTH: usize = 19;

/// The colours the TUI draws with. Every entry is an ANSI-16 name, and all of
/// them collapse to [`Color::Reset`] under `NO_COLOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub accent: Color,
    pub muted: Color,
    pub ok: Color,
    pub warn: Color,
    pub danger: Color,
}

impl Palette {
    /// Honour `NO_COLOR`: set and non-empty means "no colour at all", which is
    /// what the informal standard asks for.
    pub fn detect() -> Self {
        let disabled = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        if disabled {
            Self::plain()
        } else {
            Self::ansi()
        }
    }

    pub const fn ansi() -> Self {
        Self {
            accent: Color::Cyan,
            muted: Color::DarkGray,
            ok: Color::Green,
            warn: Color::Yellow,
            danger: Color::Red,
        }
    }

    pub const fn plain() -> Self {
        Self {
            accent: Color::Reset,
            muted: Color::Reset,
            ok: Color::Reset,
            warn: Color::Reset,
            danger: Color::Reset,
        }
    }
}

/// Draw the whole frame: the mounted screen, then any modal over it.
pub fn draw(frame: &mut Frame, app: &App, palette: Palette) {
    let area = frame.area();
    match app.screen() {
        Screen::Dashboard => draw_dashboard(frame, area, app, palette),
        Screen::SwarmView { swarm_id } => draw_swarm_view(frame, area, app, swarm_id, palette),
        Screen::Wizard => draw_wizard(frame, area, app, palette),
        Screen::Unsupported => draw_unsupported(frame, area, palette),
    }
    match app.modal() {
        Modal::None => {}
        Modal::ConfirmDelete { swarm_id, force } => {
            draw_confirm(frame, area, swarm_id, *force, palette);
        }
        Modal::Error { message } => draw_error(frame, area, message, palette),
    }
}

// ── Dashboard ────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            crate::t("cli-swarm-tui-title", "ZeroClaw swarms"),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );

    let header = format!(
        "{}{}{}{}{}",
        pad(&crate::t("cli-swarm-tui-col-name", "Name"), NAME_WIDTH),
        pad(
            &crate::t("cli-swarm-tui-col-status", "Status"),
            STATUS_WIDTH
        ),
        pad(&crate::t("cli-swarm-tui-col-boxes", "Boxes"), BOXES_WIDTH),
        pad(
            &crate::t("cli-swarm-tui-col-budget", "Budget"),
            BUDGET_WIDTH
        ),
        crate::t("cli-swarm-tui-col-updated", "Updated"),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            header,
            Style::default().fg(palette.muted),
        ))),
        rows[1],
    );

    let height = rows[2].height as usize;
    let (start, end) = window(app.selected(), app.row_count(), height);
    let mut items: Vec<ListItem> = Vec::new();
    for index in start..end {
        let selected = index == app.selected();
        let line = match app.swarms().get(index) {
            Some(swarm) => swarm_row(swarm, palette),
            None => Line::from(Span::styled(
                crate::t("cli-swarm-tui-new-row", "+ New swarm"),
                Style::default().fg(palette.accent),
            )),
        };
        items.push(ListItem::new(if selected { highlight(line) } else { line }));
    }
    frame.render_widget(List::new(items), rows[2]);

    let status = if app.busy() {
        crate::t("cli-swarm-tui-loading", "Working...")
    } else {
        app.status().to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(palette.muted),
        ))),
        rows[3],
    );
    frame.render_widget(
        hint_line(
            &crate::t(
                "cli-swarm-tui-keys-dashboard",
                "up/down move - enter open - d delete - r refresh - q quit",
            ),
            palette,
        ),
        rows[4],
    );
}

fn swarm_row(swarm: &PersistedSwarm, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::raw(pad(&swarm.spec.name, NAME_WIDTH)),
        Span::styled(
            pad(&status_label(swarm.run.status), STATUS_WIDTH),
            Style::default().fg(status_color(swarm.run.status, palette)),
        ),
        Span::raw(pad(&swarm.spec.boxes.len().to_string(), BOXES_WIDTH)),
        Span::raw(pad(&budget_label(swarm.spec.budget), BUDGET_WIDTH)),
        Span::styled(
            clip(&swarm.updated_at, TIMESTAMP_WIDTH),
            Style::default().fg(palette.muted),
        ),
    ])
}

// ── Live multi-box canvas ────────────────────────────────────────

fn draw_swarm_view(frame: &mut Frame, area: Rect, app: &App, swarm_id: &str, palette: Palette) {
    let Some(view) = app.view() else {
        frame.render_widget(
            Paragraph::new(crate::t(
                "cli-swarm-tui-detail-missing",
                "That swarm is gone.",
            )),
            area,
        );
        return;
    };
    let name = app
        .swarm(swarm_id)
        .map_or(swarm_id, |s| s.spec.name.as_str());

    let rows = Layout::vertical([
        Constraint::Length(1), // title + run status
        Constraint::Length(1), // budget bar
        Constraint::Min(3),    // grid
        Constraint::Length(3), // broadcast feed
        Constraint::Length(1), // input (chat)
        Constraint::Length(1), // hint
    ])
    .split(area);

    draw_view_title(frame, rows[0], name, view, palette);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            budget_bar(view.spent(), view.limits()),
            Style::default().fg(palette.muted),
        ))),
        rows[1],
    );
    draw_grid(frame, rows[2], view, palette);
    draw_feed(frame, rows[3], view, palette);
    draw_view_input(frame, rows[4], view, palette);
    frame.render_widget(hint_line(&view_hint(view.mode()), palette), rows[5]);
}

fn draw_view_title(frame: &mut Frame, area: Rect, name: &str, view: &ViewState, palette: Palette) {
    let status = status_detail(view.status());
    let line = Line::from(vec![
        Span::styled(
            format!("{name} "),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            status,
            Style::default().fg(status_color(view.status(), palette)),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// Split the grid area into rows then columns and draw one cell per box.
fn draw_grid(frame: &mut Frame, area: Rect, view: &ViewState, palette: Palette) {
    let boxes = view.boxes();
    if boxes.is_empty() {
        frame.render_widget(
            Paragraph::new(crate::t(
                "cli-swarm-tui-view-empty",
                "This swarm has no boxes.",
            )),
            area,
        );
        return;
    }
    let (rows, cols) = view.grid_shape();
    let row_n = u32::try_from(rows).unwrap_or(1).max(1);
    let col_n = u32::try_from(cols).unwrap_or(1).max(1);
    let row_areas = Layout::vertical(vec![Constraint::Ratio(1, row_n); rows]).split(area);
    for r in 0..rows {
        let col_areas =
            Layout::horizontal(vec![Constraint::Ratio(1, col_n); cols]).split(row_areas[r]);
        for c in 0..cols {
            let idx = r * cols + c;
            if idx < boxes.len() {
                draw_cell(frame, col_areas[c], view, idx, palette);
            }
        }
    }
}

fn draw_cell(frame: &mut Frame, area: Rect, view: &ViewState, idx: usize, palette: Palette) {
    let entry = &view.boxes()[idx];
    let focused = view.focus() == idx;
    let grabbed = focused && matches!(view.mode(), Mode::Move);
    let border = if grabbed {
        palette.warn
    } else if focused {
        palette.accent
    } else {
        palette.muted
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Span::styled(
            format!(" {} ", entry.box_id),
            Style::default().fg(border),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The editable role/job header (two lines), then badges, then the stream.
    // While editing, the header shows the in-progress buffers, not the last
    // committed values, so typing is visible.
    let editing = focused_edit(view, idx);
    let (role, job) = match (editing.is_some(), view.mode()) {
        (true, Mode::Edit { role, job, .. }) => (role.as_str(), job.as_str()),
        _ => (entry.role.as_str(), entry.job.as_str()),
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(header_line(
        &crate::t("cli-swarm-tui-cell-role", "role"),
        role,
        editing == Some(EditLine::Role),
        palette,
    ));
    lines.push(header_line(
        &crate::t("cli-swarm-tui-cell-job", "job"),
        job,
        editing == Some(EditLine::Job),
        palette,
    ));
    lines.push(badge_line(view.badge(&entry.box_id), palette));

    let height = inner.height as usize;
    let stream_room = height.saturating_sub(lines.len());
    lines.extend(stream_lines(view, &entry.box_id, stream_room, palette));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// The header row for one editable field: `label: value`, with a cursor and
/// accent when that line is the one being edited.
fn header_line(label: &str, value: &str, editing: bool, palette: Palette) -> Line<'static> {
    let shown = if value.is_empty() && !editing {
        crate::t("cli-swarm-tui-box-unassigned", "unassigned")
    } else {
        value.to_string()
    };
    let cursor = if editing { "_" } else { "" };
    let value_style = if editing {
        Style::default().fg(palette.accent)
    } else if value.is_empty() {
        Style::default().fg(palette.muted)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(palette.muted)),
        Span::styled(format!("{shown}{cursor}"), value_style),
    ])
}

/// The badge row: board status, current claim, the user-engaged flag, and the
/// latest context usage.
fn badge_line(badge: Option<&BoxBadge>, palette: Palette) -> Line<'static> {
    let Some(badge) = badge else {
        return Line::from(Span::styled(
            crate::t("cli-swarm-tui-badge-idle", "idle"),
            Style::default().fg(palette.muted),
        ));
    };
    let mut spans = vec![Span::styled(
        box_status_label(badge.status),
        Style::default().fg(box_status_color(badge.status, palette)),
    )];
    if let Some(claim) = &badge.claim {
        spans.push(Span::styled(
            format!(
                " {}",
                crate::ta(
                    "cli-swarm-tui-badge-claim",
                    &[("claim", claim.as_str())],
                    "claim"
                )
            ),
            Style::default().fg(palette.muted),
        ));
    }
    if badge.engaged {
        spans.push(Span::styled(
            format!(" {}", crate::t("cli-swarm-tui-badge-engaged", "(you)")),
            Style::default().fg(palette.warn),
        ));
    }
    if let Some((tokens, ceiling)) = badge.context {
        let ceiling = ceiling.map_or_else(|| "?".to_string(), |c| c.to_string());
        let ctx = crate::ta(
            "cli-swarm-tui-badge-ctx",
            &[
                ("tokens", tokens.to_string().as_str()),
                ("ceiling", ceiling.as_str()),
            ],
            "ctx",
        );
        spans.push(Span::styled(
            format!(" {ctx}"),
            Style::default().fg(palette.muted),
        ));
    }
    Line::from(spans)
}

/// The tail of a box's stream that fits `room` lines: a gap marker when the ring
/// dropped content, then the most recent finished lines, then the live partial.
fn stream_lines(
    view: &ViewState,
    box_id: &str,
    room: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    if room == 0 {
        return Vec::new();
    }
    let Some(stream) = view.stream(box_id) else {
        return vec![Line::from(Span::styled(
            crate::t("cli-swarm-tui-cell-waiting", "waiting…"),
            Style::default().fg(palette.muted),
        ))];
    };

    // Collect the renderable lines newest-last, then keep only the tail.
    let mut all: Vec<Line<'static>> = Vec::new();
    if stream.has_gap() {
        all.push(Line::from(Span::styled(
            crate::t("cli-swarm-tui-stream-gap", "… (stream gap)"),
            Style::default().fg(palette.warn),
        )));
    }
    for line in stream.lines() {
        all.push(Line::from(Span::styled(
            line.text.clone(),
            Style::default().fg(line_color(line.kind, palette)),
        )));
    }
    if let Some((kind, text)) = stream.partial() {
        all.push(Line::from(Span::styled(
            format!("{text}_"),
            Style::default().fg(line_color(kind, palette)),
        )));
    }
    if all.len() > room {
        all.split_off(all.len() - room)
    } else {
        all
    }
}

fn draw_feed(frame: &mut Frame, area: Rect, view: &ViewState, palette: Palette) {
    let mut lines = vec![Line::from(Span::styled(
        crate::t("cli-swarm-tui-feed-label", "Broadcast"),
        Style::default()
            .fg(palette.muted)
            .add_modifier(Modifier::BOLD),
    ))];
    let room = (area.height as usize).saturating_sub(1);
    let entries: Vec<&String> = view.feed().collect();
    let tail = entries.iter().rev().take(room).rev();
    for entry in tail {
        lines.push(Line::from(Span::styled(
            (*entry).clone(),
            Style::default().fg(palette.muted),
        )));
    }
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            crate::t("cli-swarm-tui-feed-empty", "no board activity yet"),
            Style::default().fg(palette.muted),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The chat input row, shown only while typing to a box.
fn draw_view_input(frame: &mut Frame, area: Rect, view: &ViewState, palette: Palette) {
    let Mode::Chat { buffer } = view.mode() else {
        return;
    };
    let box_id = view.focused_box().map_or("", |b| b.box_id.as_str());
    let prompt = crate::ta("cli-swarm-tui-chat-prompt", &[("box", box_id)], "chat");
    let line = Line::from(vec![
        Span::styled(format!("{prompt} "), Style::default().fg(palette.accent)),
        Span::raw(format!("{buffer}_")),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn view_hint(mode: &Mode) -> String {
    match mode {
        Mode::Nav => crate::t(
            "cli-swarm-tui-keys-view",
            "arrows focus - enter chat - e edit - m move - s/p/r/x run - esc back - q quit",
        ),
        Mode::Chat { .. } => crate::t(
            "cli-swarm-tui-keys-chat",
            "type message - enter send - 'resume' hands back - esc cancel",
        ),
        Mode::Edit { .. } => crate::t(
            "cli-swarm-tui-keys-edit",
            "type - tab role/job - enter save - esc cancel",
        ),
        Mode::Move => crate::t("cli-swarm-tui-keys-move", "arrows swap - enter/m/esc drop"),
    }
}

/// Which header line of cell `idx` is being edited, if the focused box is in
/// edit mode.
fn focused_edit(view: &ViewState, idx: usize) -> Option<EditLine> {
    if view.focus() != idx {
        return None;
    }
    match view.mode() {
        Mode::Edit { line, .. } => Some(*line),
        _ => None,
    }
}

// ── Wizard ───────────────────────────────────────────────────────

fn draw_wizard(frame: &mut Frame, area: Rect, app: &App, palette: Palette) {
    let Some(wizard) = app.wizard() else {
        frame.render_widget(
            Paragraph::new(crate::t(
                "cli-swarm-tui-wizard-loading",
                "Asking the daemon for the wizard steps...",
            )),
            area,
        );
        return;
    };
    let Some(step) = wizard.current_step() else {
        return;
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    let at = (wizard.current_index() + 1).to_string();
    let total = wizard.steps().len().to_string();
    let position = crate::ta(
        "cli-swarm-tui-wizard-title",
        &[
            ("step", at.as_str()),
            ("total", total.as_str()),
            ("title", step.title.as_str()),
        ],
        "New swarm",
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            position,
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(step.help.clone())
            .style(Style::default().fg(palette.muted))
            .wrap(Wrap { trim: true }),
        rows[1],
    );

    let mut lines: Vec<Line> = Vec::new();
    let editable = wizard.editable_fields();
    let mut answerable = 0usize;
    for field in wizard.visible_fields() {
        let advisory = WizardState::is_advisory(field);
        let focused = !advisory
            && editable
                .get(wizard.field_index())
                .is_some_and(|f| f.key == field.key);
        lines.extend(field_rows(wizard, field, focused, advisory, palette));
        if !advisory {
            answerable += 1;
        }
    }
    if answerable == 0 {
        lines.push(Line::from(Span::styled(
            crate::t(
                "cli-swarm-tui-wizard-advisory-only",
                "Nothing to answer here.",
            ),
            Style::default().fg(palette.muted),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[2]);

    let footer = if wizard.awaiting_submit() {
        Span::styled(
            crate::t("cli-swarm-tui-wizard-submitting", "Creating the swarm..."),
            Style::default().fg(palette.muted),
        )
    } else if let Some(error) = wizard.errors().first() {
        Span::styled(
            format!("{}: {}", error.field, error.message),
            Style::default().fg(palette.danger),
        )
    } else {
        Span::raw("")
    };
    frame.render_widget(Paragraph::new(Line::from(footer)), rows[3]);
    frame.render_widget(
        hint_line(
            &crate::t(
                "cli-swarm-tui-keys-wizard",
                "enter next - esc back - tab field - up/down choose - ctrl+c quit",
            ),
            palette,
        ),
        rows[4],
    );
}

/// One field: its label + value, then whatever the descriptor adds — help,
/// choices, and the daemon's rejection of it.
fn field_rows(
    wizard: &WizardState,
    field: &FieldDescriptor,
    focused: bool,
    advisory: bool,
    palette: Palette,
) -> Vec<Line<'static>> {
    let value = if advisory {
        field.default.clone().unwrap_or_default()
    } else if field.is_secret {
        "*".repeat(wizard.answer(&field.key).chars().count())
    } else {
        wizard.answer(&field.key).to_string()
    };
    let marker = if focused { "> " } else { "  " };
    let label_style = if advisory {
        Style::default().fg(palette.muted)
    } else if focused {
        Style::default()
            .fg(palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cursor = if focused && !is_picker(field) {
        "_"
    } else {
        ""
    };

    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{marker}{}", pad(&field.label, 18)), label_style),
        Span::raw(format!("{value}{cursor}")),
    ])];

    if advisory {
        lines.push(muted_row(
            &crate::t(
                "cli-swarm-tui-wizard-advisory",
                "config only - set it with `zeroclaw config`",
            ),
            palette,
        ));
    }
    if !field.help.is_empty() {
        lines.push(muted_row(&field.help, palette));
    }
    if let Some(variants) = field.enum_variants.as_ref().filter(|v| !v.is_empty()) {
        let choices = variants.join(", ");
        lines.push(muted_row(
            &crate::ta(
                "cli-swarm-tui-wizard-choices",
                &[("choices", choices.as_str())],
                "choices",
            ),
            palette,
        ));
    }
    for message in wizard.field_errors(&field.key) {
        lines.push(Line::from(Span::styled(
            format!("    {message}"),
            Style::default().fg(palette.danger),
        )));
    }
    lines
}

/// A closed-set control: up / down picks, typing does not.
fn is_picker(field: &FieldDescriptor) -> bool {
    !matches!(field.kind, PropKind::StringArray)
        && field.enum_variants.as_ref().is_some_and(|v| !v.is_empty())
}

// ── Degrade + modals ─────────────────────────────────────────────

fn draw_unsupported(frame: &mut Frame, area: Rect, palette: Palette) {
    let lines = vec![
        Line::from(Span::styled(
            crate::t(
                "cli-swarm-tui-unsupported-title",
                "This daemon has no swarms",
            ),
            Style::default()
                .fg(palette.warn)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::raw(crate::t(
            "cli-swarm-tui-unsupported-body",
            "The daemon behind this socket was built before swarms existed.",
        )),
        Line::raw(""),
        Line::from(Span::styled(
            crate::t("cli-swarm-tui-unsupported-keys", "any key to quit"),
            Style::default().fg(palette.muted),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn draw_confirm(frame: &mut Frame, area: Rect, swarm_id: &str, force: bool, palette: Palette) {
    let prompt = if force {
        crate::ta(
            "cli-swarm-tui-confirm-delete-force",
            &[("swarm_id", swarm_id)],
            "A live run holds this swarm. Delete anyway?",
        )
    } else {
        crate::ta(
            "cli-swarm-tui-confirm-delete",
            &[("swarm_id", swarm_id)],
            "Delete this swarm?",
        )
    };
    let accent = if force { palette.danger } else { palette.warn };
    modal(
        frame,
        area,
        &prompt,
        &crate::t("cli-swarm-tui-confirm-keys", "y delete - n cancel"),
        accent,
        palette,
    );
}

fn draw_error(frame: &mut Frame, area: Rect, message: &str, palette: Palette) {
    modal(
        frame,
        area,
        message,
        &crate::t("cli-swarm-tui-error-keys", "any key to dismiss"),
        palette.danger,
        palette,
    );
}

fn modal(frame: &mut Frame, area: Rect, body: &str, hint: &str, accent: Color, palette: Palette) {
    let box_area = centered(area, 60, 7);
    frame.render_widget(Clear, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(accent));
    let lines = vec![
        Line::raw(body.to_string()),
        Line::raw(""),
        Line::from(Span::styled(
            hint.to_string(),
            Style::default().fg(palette.muted),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        box_area,
    );
}

// ── Formatting helpers ───────────────────────────────────────────

/// One word, for a fixed-width column.
pub fn status_label(status: SwarmStatus) -> String {
    match status {
        SwarmStatus::Created => crate::t("cli-swarm-status-created", "created"),
        SwarmStatus::Running => crate::t("cli-swarm-status-running", "running"),
        SwarmStatus::Paused { .. } => crate::t("cli-swarm-status-paused", "paused"),
        SwarmStatus::Stopped => crate::t("cli-swarm-status-stopped", "stopped"),
        SwarmStatus::Completed => crate::t("cli-swarm-status-completed", "completed"),
    }
}

/// The status with the pause reason spelled out, where there is room for it.
fn status_detail(status: SwarmStatus) -> String {
    let SwarmStatus::Paused { reason } = status else {
        return status_label(status);
    };
    let reason = pause_label(reason);
    crate::ta(
        "cli-swarm-status-paused-detail",
        &[("reason", reason.as_str())],
        "paused",
    )
}

fn pause_label(reason: SwarmPauseReason) -> String {
    match reason {
        SwarmPauseReason::BudgetExhausted => {
            crate::t("cli-swarm-pause-budget-exhausted", "budget exhausted")
        }
        SwarmPauseReason::DaemonRestart => {
            crate::t("cli-swarm-pause-daemon-restart", "daemon restart")
        }
        SwarmPauseReason::UserRequested => {
            crate::t("cli-swarm-pause-user-requested", "user requested")
        }
    }
}

fn status_color(status: SwarmStatus, palette: Palette) -> Color {
    match status {
        SwarmStatus::Running => palette.ok,
        SwarmStatus::Paused { .. } => palette.warn,
        SwarmStatus::Stopped => palette.danger,
        SwarmStatus::Created | SwarmStatus::Completed => palette.muted,
    }
}

/// The budget as one word: the preset name, or `custom`.
pub fn budget_label(budget: SwarmBudget) -> String {
    match budget {
        SwarmBudget::Preset(preset) => preset.wire_name().to_string(),
        SwarmBudget::Custom(_) => crate::t("cli-swarm-budget-custom", "custom"),
    }
}

/// The swarm-level budget status bar: turns / tokens / cost spent against the
/// preset ceiling.
fn budget_bar(spent: SwarmSpend, limits: SwarmBudgetLimits) -> String {
    let turns = spent.turns.to_string();
    let max_turns = limits.max_turns.to_string();
    let tokens = spent.tokens.to_string();
    let max_tokens = limits.max_tokens.to_string();
    let cost = format!("{:.2}", spent.cost_usd);
    let max_cost = format!("{:.2}", limits.max_cost_usd);
    crate::ta(
        "cli-swarm-tui-budget-bar",
        &[
            ("turns", turns.as_str()),
            ("max_turns", max_turns.as_str()),
            ("tokens", tokens.as_str()),
            ("max_tokens", max_tokens.as_str()),
            ("cost", cost.as_str()),
            ("max_cost", max_cost.as_str()),
        ],
        "budget",
    )
}

/// A box's board status as one localized word.
fn box_status_label(status: BoxStatus) -> String {
    match status {
        BoxStatus::Idle => crate::t("cli-swarm-tui-badge-idle", "idle"),
        BoxStatus::Working => crate::t("cli-swarm-tui-badge-working", "working"),
        BoxStatus::Blocked => crate::t("cli-swarm-tui-badge-blocked", "blocked"),
        BoxStatus::Done => crate::t("cli-swarm-tui-badge-done", "done"),
    }
}

fn box_status_color(status: BoxStatus, palette: Palette) -> Color {
    match status {
        BoxStatus::Working => palette.ok,
        BoxStatus::Blocked => palette.danger,
        BoxStatus::Done => palette.accent,
        BoxStatus::Idle => palette.muted,
    }
}

/// The colour a coalesced stream line is tinted with.
fn line_color(kind: LineKind, palette: Palette) -> Color {
    match kind {
        LineKind::Message => Color::Reset,
        LineKind::Thought => palette.muted,
        LineKind::Tool => palette.accent,
        LineKind::System => palette.warn,
    }
}

fn muted_row(text: &str, palette: Palette) -> Line<'static> {
    Line::from(Span::styled(
        format!("    {text}"),
        Style::default().fg(palette.muted),
    ))
}

fn hint_line(text: &str, palette: Palette) -> Paragraph<'static> {
    Paragraph::new(Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(palette.muted),
    )))
}

fn highlight(line: Line<'static>) -> Line<'static> {
    line.patch_style(Style::default().add_modifier(Modifier::REVERSED))
}

/// Pad (or clip) to exactly `width` cells plus one separating space.
fn pad(text: &str, width: usize) -> String {
    let mut out = clip(text, width);
    let len = out.chars().count();
    for _ in len..width {
        out.push(' ');
    }
    out.push(' ');
    out
}

fn clip(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// The slice of a list to draw so the cursor stays on screen.
fn window(selected: usize, total: usize, height: usize) -> (usize, usize) {
    if height == 0 || total == 0 {
        return (0, 0);
    }
    if total <= height {
        return (0, total);
    }
    let start = selected.saturating_sub(height - 1).min(total - height);
    (start, start + height)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use zeroclaw_config::traits::PropKind;
    use zeroclaw_runtime::rpc::types::{SwarmStepShape, SwarmWizardStep};
    use zeroclaw_runtime::swarm::store::SwarmSpec;

    use crate::swarm_tui::state::{Input, Update};

    fn render(app: &App) -> String {
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, app, Palette::plain()))
            .expect("draw");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn swarm(id: &str, name: &str) -> PersistedSwarm {
        PersistedSwarm::new(SwarmSpec::new(
            id,
            name,
            "anthropic",
            "claude-sonnet-4",
            "balanced",
            "supervisor",
            "survey the field",
        ))
    }

    fn loaded() -> App {
        let mut app = App::new();
        app.on_update(Update::Swarms(vec![
            swarm("sw-1", "Research squad"),
            swarm("sw-2", "Triage squad"),
        ]));
        app
    }

    fn shapes() -> Vec<SwarmStepShape> {
        vec![
            SwarmStepShape {
                step: SwarmWizardStep::Provider,
                title: "Model provider".to_string(),
                help: "The provider every box talks to.".to_string(),
                fields: vec![FieldDescriptor {
                    key: "provider".to_string(),
                    label: "provider".to_string(),
                    help: String::new(),
                    kind: PropKind::AliasRef,
                    is_secret: false,
                    enum_variants: Some(vec!["anthropic".to_string(), "openai".to_string()]),
                    required: true,
                    default: None,
                }],
            },
            SwarmStepShape {
                step: SwarmWizardStep::Goal,
                title: "Goal".to_string(),
                help: "What this swarm is for.".to_string(),
                fields: vec![FieldDescriptor {
                    key: "goal".to_string(),
                    label: "goal".to_string(),
                    help: String::new(),
                    kind: PropKind::String,
                    is_secret: false,
                    enum_variants: None,
                    required: true,
                    default: None,
                }],
            },
        ]
    }

    #[test]
    fn the_dashboard_lists_every_swarm_and_the_new_row() {
        let rendered = render(&loaded());
        assert!(rendered.contains("Research squad"), "{rendered}");
        assert!(rendered.contains("Triage squad"));
        assert!(rendered.contains(&crate::t("cli-swarm-tui-new-row", "+ New swarm")));
        assert!(rendered.contains(&status_label(SwarmStatus::Created)));
        assert!(rendered.contains("standard"), "budget preset column");
    }

    #[test]
    fn an_empty_dashboard_still_offers_the_new_row() {
        let mut app = App::new();
        app.on_update(Update::Swarms(Vec::new()));
        let rendered = render(&app);
        assert!(rendered.contains(&crate::t("cli-swarm-tui-new-row", "+ New swarm")));
    }

    #[test]
    fn the_live_canvas_shows_a_cell_per_box_and_the_budget_bar() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        let rendered = render(&app);
        // One grid cell per roster box, titled by box id.
        assert!(rendered.contains("box-1"), "{rendered}");
        assert!(rendered.contains("box-4"));
        // The swarm-level budget status bar and the broadcast feed.
        assert!(rendered.contains(&crate::t("cli-swarm-tui-feed-label", "Broadcast")));
        assert!(
            rendered.contains(&status_label(SwarmStatus::Created)),
            "the run status is shown"
        );
    }

    /// Headless smoke: drive the reducer with a scripted stream and assert the
    /// rendered frame shows four streaming boxes with badges plus the budget
    /// bar — no TTY, no daemon.
    #[test]
    fn a_scripted_run_renders_four_streaming_boxes_with_badges() {
        use zeroclaw_runtime::rpc::types::{SessionUpdateEvent, SwarmBoardNotify, SwarmUpdate};
        use zeroclaw_runtime::swarm::board::{BoardEvent, BoxState, BoxStatus};

        let mut app = loaded();
        app.on_input(Input::Enter); // open the live view for sw-1
        app.on_update(Update::Subscribed {
            swarm_id: "sw-1".to_string(),
        });
        assert!(app.is_streaming(), "the view streams once subscribed");

        // Every box works its job and streams a line.
        for n in 1..=4 {
            let box_id = format!("box-{n}");
            app.on_update(Update::Board(Box::new(SwarmBoardNotify {
                swarm_id: "sw-1".to_string(),
                event: BoardEvent::Published {
                    box_id: box_id.clone(),
                    state: BoxState {
                        status: BoxStatus::Working,
                        claim: Some(format!("task-{n}")),
                        note: String::new(),
                    },
                },
            })));
            app.on_update(Update::Stream(Box::new(SwarmUpdate {
                swarm_id: "sw-1".to_string(),
                box_id: box_id.clone(),
                event: SessionUpdateEvent::AgentMessageChunk {
                    session_id: box_id,
                    text: format!("analyzing shard {n}\n"),
                },
            })));
        }
        // A live spend so the budget bar is not all zeros.
        app.on_update(Update::RunControl {
            swarm_id: "sw-1".to_string(),
            status: SwarmStatus::Running,
            spent: SwarmSpend {
                turns: 12,
                tokens: 3400,
                cost_usd: 1.5,
            },
        });

        let rendered = render(&app);
        for n in 1..=4 {
            assert!(
                rendered.contains(&format!("box-{n}")),
                "cell {n}: {rendered}"
            );
            assert!(
                rendered.contains(&format!("analyzing shard {n}")),
                "streamed line {n}: {rendered}"
            );
        }
        assert!(
            rendered.contains(&crate::t("cli-swarm-tui-badge-working", "working")),
            "working badge: {rendered}"
        );
        assert!(rendered.contains("task-1"), "claim badge: {rendered}");
        assert!(
            rendered.contains("12"),
            "spent turns in the budget bar: {rendered}"
        );
        assert!(
            rendered.contains(&status_label(SwarmStatus::Running)),
            "run status: {rendered}"
        );
    }

    #[test]
    fn the_chat_input_row_appears_while_typing_to_a_box() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        app.on_input(Input::Enter); // open chat on the focused box
        app.on_input(Input::Char('h'));
        app.on_input(Input::Char('i'));
        let rendered = render(&app);
        assert!(
            rendered.contains(&clip(
                &crate::ta("cli-swarm-tui-chat-prompt", &[("box", "box-1")], "chat"),
                8
            )),
            "the chat prompt names the box: {rendered}"
        );
        assert!(
            rendered.contains("hi"),
            "the typed message is shown: {rendered}"
        );
    }

    #[test]
    fn the_wizard_renders_the_daemons_step_and_choices() {
        let mut app = loaded();
        app.on_input(Input::Up);
        app.on_input(Input::Enter);
        app.on_update(Update::Fields {
            provider: None,
            steps: shapes(),
        });
        // Step 1 is the client-owned name step; step 2 is the daemon's first.
        app.on_input(Input::Enter);
        let rendered = render(&app);
        assert!(rendered.contains("Model provider"), "{rendered}");
        assert!(rendered.contains("The provider every box talks to."));
        assert!(rendered.contains("anthropic"));
        assert!(rendered.contains("openai"));
    }

    #[test]
    fn the_wizard_shows_the_loading_state_before_its_shapes_arrive() {
        let mut app = loaded();
        app.on_input(Input::Up);
        app.on_input(Input::Enter);
        let rendered = render(&app);
        assert!(rendered.contains(&clip(
            &crate::t("cli-swarm-tui-wizard-loading", "Asking the daemon"),
            20
        )));
    }

    #[test]
    fn a_rejected_field_is_drawn_next_to_it() {
        let mut app = loaded();
        app.on_input(Input::Up);
        app.on_input(Input::Enter);
        app.on_update(Update::Fields {
            provider: None,
            steps: shapes(),
        });
        for _ in 0..3 {
            app.on_input(Input::Enter);
        }
        app.on_update(Update::Validated(
            zeroclaw_runtime::rpc::types::SwarmValidateResult::Errors {
                errors: vec![zeroclaw_runtime::rpc::types::SwarmValidationError {
                    step: Some(SwarmWizardStep::Goal),
                    field: "goal".to_string(),
                    message: "state what this swarm is for".to_string(),
                }],
            },
        ));
        let rendered = render(&app);
        assert!(
            rendered.contains("state what this swarm is for"),
            "{rendered}"
        );
    }

    #[test]
    fn the_delete_confirmation_names_the_swarm_and_escalates_to_force() {
        let mut app = loaded();
        app.on_input(Input::Char('d'));
        let rendered = render(&app);
        assert!(rendered.contains("sw-1"), "{rendered}");

        app.on_input(Input::Char('y'));
        app.on_update(Update::Failed(crate::swarm_tui::client::RpcFailure {
            code: zeroclaw_api::jsonrpc::error_codes::SWARM_RUN_ACTIVE,
            message: "live run".to_string(),
        }));
        let rendered = render(&app);
        assert!(
            rendered.contains(&clip(
                &crate::ta(
                    "cli-swarm-tui-confirm-delete-force",
                    &[("swarm_id", "sw-1")],
                    "A live run holds this swarm. Delete anyway?"
                ),
                20
            )),
            "{rendered}"
        );
    }

    #[test]
    fn the_degrade_screen_explains_itself_instead_of_crashing() {
        let app = App::unsupported();
        let rendered = render(&app);
        assert!(rendered.contains(&clip(
            &crate::t(
                "cli-swarm-tui-unsupported-title",
                "This daemon has no swarms"
            ),
            20
        )));
    }

    #[test]
    fn an_error_modal_draws_the_daemons_message() {
        let mut app = loaded();
        app.on_update(Update::Failed(crate::swarm_tui::client::RpcFailure {
            code: -32602,
            message: "swarm not found".to_string(),
        }));
        let rendered = render(&app);
        assert!(rendered.contains("swarm not found"), "{rendered}");
    }

    #[test]
    fn a_paused_swarm_reads_short_in_a_column_and_long_in_the_pane() {
        let paused = SwarmStatus::Paused {
            reason: SwarmPauseReason::BudgetExhausted,
        };
        assert_eq!(
            status_label(paused),
            crate::t("cli-swarm-status-paused", "paused")
        );
        assert!(
            status_detail(paused).contains(&crate::t(
                "cli-swarm-pause-budget-exhausted",
                "budget exhausted"
            )),
            "the detail pane names why a swarm parked"
        );
    }

    #[test]
    fn no_color_collapses_the_palette_to_the_terminal_default() {
        assert_eq!(Palette::plain().accent, Color::Reset);
        assert_eq!(Palette::plain().danger, Color::Reset);
        assert_ne!(Palette::ansi().accent, Color::Reset);
    }

    #[test]
    fn the_window_keeps_the_cursor_on_screen() {
        assert_eq!(window(0, 3, 10), (0, 3));
        assert_eq!(window(9, 20, 5), (5, 10));
        assert_eq!(window(0, 20, 5), (0, 5));
        assert_eq!(window(19, 20, 5), (15, 20));
        assert_eq!(window(0, 0, 5), (0, 0));
    }

    #[test]
    fn padding_clips_rather_than_overflowing_a_column() {
        assert_eq!(pad("ab", 4), "ab   ");
        assert_eq!(pad("abcdef", 4), "abcd ");
    }
}
