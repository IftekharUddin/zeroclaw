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
use zeroclaw_runtime::swarm::store::{
    BoxSpec, PersistedSwarm, SwarmBudget, SwarmPauseReason, SwarmStatus,
};

use super::state::{App, Modal, Screen};
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
        Screen::Detail { swarm_id } => draw_detail(frame, area, app, swarm_id, palette),
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

// ── Detail ───────────────────────────────────────────────────────

fn draw_detail(frame: &mut Frame, area: Rect, app: &App, swarm_id: &str, palette: Palette) {
    let Some(swarm) = app.swarm(swarm_id) else {
        frame.render_widget(
            Paragraph::new(crate::t(
                "cli-swarm-tui-detail-missing",
                "That swarm is gone.",
            )),
            area,
        );
        return;
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            crate::ta(
                "cli-swarm-tui-detail-title",
                &[("name", swarm.spec.name.as_str())],
                "Swarm",
            ),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ))),
        rows[0],
    );

    let spent = &swarm.run.spent;
    let mut lines = vec![
        field_line("cli-swarm-tui-field-id", "Id", &swarm.spec.id, palette),
        field_line(
            "cli-swarm-tui-field-status",
            "Status",
            &status_detail(swarm.run.status),
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-provider",
            "Provider",
            &swarm.spec.provider,
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-model",
            "Model",
            &swarm.spec.model,
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-risk",
            "Risk profile",
            &swarm.spec.risk_profile,
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-budget",
            "Budget",
            &budget_detail(swarm.spec.budget),
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-spent",
            "Spent",
            &format!(
                "{} turns / {} tokens / {:.2} USD",
                spent.turns, spent.tokens, spent.cost_usd
            ),
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-channels",
            "Channels",
            &join_or_dash(&swarm.spec.channels),
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-role",
            "Role",
            &swarm.spec.role,
            palette,
        ),
        field_line(
            "cli-swarm-tui-field-goal",
            "Goal",
            &swarm.spec.goal,
            palette,
        ),
        Line::raw(""),
        Line::from(Span::styled(
            crate::t("cli-swarm-tui-roster", "Roster"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                "{}{}{}{}",
                pad(&crate::t("cli-swarm-tui-col-slot", "Slot"), 6),
                pad(&crate::t("cli-swarm-tui-col-box", "Box"), 12),
                pad(&crate::t("cli-swarm-tui-col-role", "Role"), 16),
                crate::t("cli-swarm-tui-col-job", "Job"),
            ),
            Style::default().fg(palette.muted),
        )),
    ];
    for entry in &swarm.spec.boxes {
        lines.push(box_line(entry, palette));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[1]);

    frame.render_widget(
        hint_line(
            &crate::t("cli-swarm-tui-keys-detail", "esc back - q quit"),
            palette,
        ),
        rows[2],
    );
}

fn box_line(entry: &BoxSpec, palette: Palette) -> Line<'static> {
    let unassigned = crate::t("cli-swarm-tui-box-unassigned", "unassigned");
    let role = if entry.role.is_empty() {
        unassigned.clone()
    } else {
        entry.role.clone()
    };
    let job = if entry.job.is_empty() {
        unassigned
    } else {
        entry.job.clone()
    };
    let dim = entry.role.is_empty() && entry.job.is_empty();
    let style = if dim {
        Style::default().fg(palette.muted)
    } else {
        Style::default()
    };
    Line::from(Span::styled(
        format!(
            "{}{}{}{}",
            pad(&entry.slot.to_string(), 6),
            pad(&entry.box_id, 12),
            pad(&role, 16),
            job
        ),
        style,
    ))
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

/// The budget with its ceilings spelled out, for the detail pane.
fn budget_detail(budget: SwarmBudget) -> String {
    let limits = budget.limits();
    format!(
        "{} ({} turns / {} tokens / {:.2} USD)",
        budget_label(budget),
        limits.max_turns,
        limits.max_tokens,
        limits.max_cost_usd
    )
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_string()
    } else {
        values.join(", ")
    }
}

fn field_line(key: &str, fallback: &str, value: &str, palette: Palette) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            pad(&crate::t(key, fallback), 14),
            Style::default().fg(palette.muted),
        ),
        Span::raw(value.to_string()),
    ])
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
    fn the_detail_pane_shows_the_spec_and_the_roster() {
        let mut app = loaded();
        app.on_input(Input::Enter);
        let rendered = render(&app);
        assert!(rendered.contains("claude-sonnet-4"), "{rendered}");
        assert!(rendered.contains("survey the field"));
        assert!(rendered.contains("box-1"));
        assert!(rendered.contains("box-4"));
        assert!(rendered.contains(&crate::t("cli-swarm-tui-roster", "Roster")));
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
