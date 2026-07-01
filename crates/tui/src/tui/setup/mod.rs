//! Constitution-first setup wizard shell (#3404/#3794).
//!
//! This module owns the reusable setup shell: step ordering, navigation,
//! per-step status projection, and the v0.8.67 constitution checkpoint action.
//! Individual step contents can grow behind [`SetupWizardStep`] without
//! changing the navigation or commit contract.

use std::borrow::Cow;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Widget, Wrap},
};

use crate::config::{Config, has_api_key, has_api_key_for};
use crate::localization::{Locale, MessageId, tr};
use crate::palette;
use crate::prompts::CONSTITUTION_OVERRIDE_FILE;
use crate::tui::app::App;
use crate::tui::onboarding;
use crate::tui::views::{
    ActionHint, ModalKind, ModalView, ViewAction, ViewEvent, centered_modal_area,
    render_modal_footer, render_modal_surface,
};

use codewhale_config::{
    AutonomyPreference, ConstitutionChoice, ConstitutionSource, ConstitutionValidity,
    InheritedConfigFacts, RuntimePostureSource, SetupState, SetupStep, StepEntry, StepStatus,
    UserConstitution, UserConstitutionLoad,
};

/// Target lane for the once-per-version constitution checkpoint. The workspace
/// package remains 0.8.66 until release approval, so this cannot read
/// `CARGO_PKG_VERSION` yet.
pub const CONSTITUTION_CHECKPOINT_VERSION: &str = "0.8.67";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupCommitKind {
    BundledConstitution,
    DeferredConstitution,
}

pub trait SetupWizardStep {
    fn id(&self) -> SetupStep;
    fn title_id(&self) -> MessageId;
    fn why_id(&self) -> MessageId;
    fn required(&self) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StaticSetupStep {
    id: SetupStep,
    title_id: MessageId,
    why_id: MessageId,
    required: bool,
}

impl SetupWizardStep for StaticSetupStep {
    fn id(&self) -> SetupStep {
        self.id
    }

    fn title_id(&self) -> MessageId {
        self.title_id
    }

    fn why_id(&self) -> MessageId {
        self.why_id
    }

    fn required(&self) -> bool {
        self.required
    }
}

const STEP_SPECS: [StaticSetupStep; 8] = [
    StaticSetupStep {
        id: SetupStep::Language,
        title_id: MessageId::SetupStepLanguageTitle,
        why_id: MessageId::SetupStepLanguageWhy,
        required: true,
    },
    StaticSetupStep {
        id: SetupStep::ProviderModel,
        title_id: MessageId::SetupStepProviderModelTitle,
        why_id: MessageId::SetupStepProviderModelWhy,
        required: true,
    },
    StaticSetupStep {
        id: SetupStep::TrustSandbox,
        title_id: MessageId::SetupStepTrustSandboxTitle,
        why_id: MessageId::SetupStepTrustSandboxWhy,
        required: true,
    },
    StaticSetupStep {
        id: SetupStep::ToolsMcp,
        title_id: MessageId::SetupStepToolsMcpTitle,
        why_id: MessageId::SetupStepToolsMcpWhy,
        required: false,
    },
    StaticSetupStep {
        id: SetupStep::Hotbar,
        title_id: MessageId::SetupStepHotbarTitle,
        why_id: MessageId::SetupStepHotbarWhy,
        required: false,
    },
    StaticSetupStep {
        id: SetupStep::RemoteRuntime,
        title_id: MessageId::SetupStepRemoteRuntimeTitle,
        why_id: MessageId::SetupStepRemoteRuntimeWhy,
        required: false,
    },
    StaticSetupStep {
        id: SetupStep::Constitution,
        title_id: MessageId::SetupStepConstitutionTitle,
        why_id: MessageId::SetupStepConstitutionWhy,
        required: true,
    },
    StaticSetupStep {
        id: SetupStep::Verification,
        title_id: MessageId::SetupStepVerificationTitle,
        why_id: MessageId::SetupStepVerificationWhy,
        required: false,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupWizardView {
    state: SetupState,
    selected: usize,
    locale: Locale,
    facts: SetupRuntimeFacts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetupRuntimeFacts {
    provider: String,
    model: String,
    auth: String,
    health: String,
    provider_ready: bool,
    provider_result: String,
    work_intent: String,
    approval: String,
    shell: String,
    trust: String,
    sandbox: String,
    network: String,
    runtime_result: String,
}

impl Default for SetupRuntimeFacts {
    fn default() -> Self {
        Self {
            provider: "not loaded".to_string(),
            model: "not loaded".to_string(),
            auth: "not checked".to_string(),
            health: "not checked".to_string(),
            provider_ready: false,
            provider_result: "provider/model not loaded".to_string(),
            work_intent: "not loaded".to_string(),
            approval: "not loaded".to_string(),
            shell: "not loaded".to_string(),
            trust: "not loaded".to_string(),
            sandbox: "not configured".to_string(),
            network: "not configured".to_string(),
            runtime_result: "runtime posture not loaded".to_string(),
        }
    }
}

impl SetupRuntimeFacts {
    fn from_app_config(app: &App, config: &Config) -> Self {
        let provider_ready = has_api_key_for(config, app.api_provider);
        let model = app.model_display_label();
        let provider = app.api_provider.display_name().to_string();
        let auth = if provider_ready {
            "present or local runtime".to_string()
        } else {
            "missing for active provider".to_string()
        };
        let health = if provider_ready {
            "ready for first turn; live validation remains with /provider"
        } else {
            "needs key or local runtime before first turn"
        }
        .to_string();
        let provider_result = format!(
            "provider={}, model={}, auth={}, health={}",
            app.api_provider.as_str(),
            model,
            if provider_ready {
                "present/local"
            } else {
                "missing"
            },
            if provider_ready {
                "not checked"
            } else {
                "needs action"
            }
        );
        let shell = if app.allow_shell { "enabled" } else { "hidden" }.to_string();
        let trust = if app.trust_mode {
            "trusted workspace / writes allowed by posture"
        } else {
            "workspace trust not elevated"
        }
        .to_string();
        let sandbox = config
            .sandbox_mode
            .as_deref()
            .filter(|mode| !mode.trim().is_empty())
            .unwrap_or("default")
            .to_string();
        let network = config
            .network
            .as_ref()
            .map_or("prompt by default".to_string(), |policy| {
                format!("default {}", policy.default)
            });
        let runtime_result = format!(
            "intent={}, approval={}, shell={}, trust={}, sandbox={}, network={}",
            app.mode.as_setting(),
            app.approval_mode.label().to_ascii_lowercase(),
            if app.allow_shell { "enabled" } else { "hidden" },
            if app.trust_mode {
                "trusted"
            } else {
                "workspace"
            },
            sandbox,
            network
        );
        Self {
            provider,
            model,
            auth,
            health,
            provider_ready,
            provider_result,
            work_intent: app.mode.display_name().to_string(),
            approval: app.approval_mode.label().to_ascii_lowercase(),
            shell,
            trust,
            sandbox,
            network,
            runtime_result,
        }
    }
}

impl SetupWizardView {
    #[cfg(test)]
    #[must_use]
    pub fn new(state: SetupState, locale: Locale) -> Self {
        let selected = initial_step_index(&state);
        Self {
            state,
            selected,
            locale,
            facts: SetupRuntimeFacts::default(),
        }
    }

    #[must_use]
    pub fn new_for_app(app: &App, config: &Config) -> Self {
        Self::new_with_facts(
            load_setup_state_for_app(app, config),
            app.ui_locale,
            SetupRuntimeFacts::from_app_config(app, config),
        )
    }

    #[must_use]
    pub fn new_for_app_at(app: &App, config: &Config, step: SetupStep) -> Self {
        Self::new_at_with_facts(
            load_setup_state_for_app(app, config),
            app.ui_locale,
            step,
            SetupRuntimeFacts::from_app_config(app, config),
        )
    }

    #[cfg(test)]
    #[must_use]
    pub fn state(&self) -> &SetupState {
        &self.state
    }

    #[must_use]
    pub fn selected_step(&self) -> SetupStep {
        STEP_SPECS[self.selected].id()
    }

    fn selected_spec(&self) -> &'static dyn SetupWizardStep {
        &STEP_SPECS[self.selected]
    }

    fn new_with_facts(state: SetupState, locale: Locale, facts: SetupRuntimeFacts) -> Self {
        let selected = initial_step_index(&state);
        Self {
            state,
            selected,
            locale,
            facts,
        }
    }

    fn new_at_with_facts(
        state: SetupState,
        locale: Locale,
        step: SetupStep,
        facts: SetupRuntimeFacts,
    ) -> Self {
        Self {
            state,
            selected: step_index(step),
            locale,
            facts,
        }
    }

    fn move_next(&mut self) {
        self.selected = (self.selected + 1).min(STEP_SPECS.len().saturating_sub(1));
    }

    fn move_back(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn mark_selected(&mut self, status: StepStatus) {
        let spec = self.selected_spec();
        self.state.set_step(
            spec.id(),
            StepEntry::new(status, spec.required(), CONSTITUTION_CHECKPOINT_VERSION),
        );
    }

    fn commit_provider_model_review(&mut self) -> ViewAction {
        let status = if self.facts.provider_ready {
            StepStatus::Verified
        } else {
            StepStatus::NeedsAction
        };
        let mut state = self.state.clone();
        state.set_step(
            SetupStep::ProviderModel,
            StepEntry::new(status, true, CONSTITUTION_CHECKPOINT_VERSION)
                .with_result(self.facts.provider_result.clone()),
        );
        self.state = state.clone();
        self.move_next();
        let message_id = if status == StepStatus::Verified {
            MessageId::SetupProviderModelReviewed
        } else {
            MessageId::SetupProviderModelNeedsActionSaved
        };
        ViewAction::Emit(ViewEvent::SetupStateCommitRequested {
            state,
            message: tr(self.locale, message_id).to_string(),
        })
    }

    fn commit_runtime_posture_review(&mut self) -> ViewAction {
        let mut state = self.state.clone();
        state.runtime_posture_source = RuntimePostureSource::Confirmed;
        state.set_step(
            SetupStep::TrustSandbox,
            StepEntry::new(StepStatus::Verified, true, CONSTITUTION_CHECKPOINT_VERSION)
                .with_result(self.facts.runtime_result.clone()),
        );
        self.state = state.clone();
        self.move_next();
        ViewAction::Emit(ViewEvent::SetupStateCommitRequested {
            state,
            message: tr(self.locale, MessageId::SetupRuntimePostureReviewed).to_string(),
        })
    }

    fn commit_setup_report(&mut self) -> ViewAction {
        let mut state = self.state.clone();
        let status = if setup_report_ready(&state) {
            StepStatus::Verified
        } else {
            StepStatus::NeedsAction
        };
        state.set_step(
            SetupStep::Verification,
            StepEntry::new(status, false, CONSTITUTION_CHECKPOINT_VERSION)
                .with_result(setup_report_result(&state)),
        );
        self.state = state.clone();
        ViewAction::Emit(ViewEvent::SetupStateCommitRequested {
            state,
            message: tr(self.locale, MessageId::SetupReportRecorded).to_string(),
        })
    }

    fn commit_guided_constitution(&mut self) -> ViewAction {
        let constitution = guided_constitution_template(self.locale);
        let mut state = self.state.clone();
        state.complete_constitution_checkpoint(
            CONSTITUTION_CHECKPOINT_VERSION,
            ConstitutionChoice::GuidedCustom,
        );
        state.constitution_language = constitution.language.clone();
        state.constitution_source = ConstitutionSource::UserGlobal;
        state.constitution_validity = ConstitutionValidity::Valid;
        state.constitution_preview_hash = Some(constitution.preview_hash());
        state.constitution_preview_version =
            state.constitution_preview_version.saturating_add(1).max(1);
        let hash = state
            .constitution_preview_hash
            .as_deref()
            .unwrap_or("unknown");
        state.set_step(
            SetupStep::Constitution,
            StepEntry::new(StepStatus::Verified, true, CONSTITUTION_CHECKPOINT_VERSION)
                .with_result(format!("guided custom constitution preview_hash={hash}")),
        );
        self.state = state.clone();
        ViewAction::EmitAndClose(ViewEvent::SetupConstitutionCommitRequested {
            constitution,
            state,
            message: tr(self.locale, MessageId::SetupCheckpointDoneGuided).to_string(),
        })
    }

    fn commit_constitution(&self, kind: SetupCommitKind) -> ViewAction {
        let choice = match kind {
            SetupCommitKind::BundledConstitution => ConstitutionChoice::Bundled,
            SetupCommitKind::DeferredConstitution => ConstitutionChoice::Deferred,
        };
        let mut state = self.state.clone();
        state.complete_constitution_checkpoint(CONSTITUTION_CHECKPOINT_VERSION, choice);
        state.constitution_source = ConstitutionSource::Bundled;
        state.constitution_validity = ConstitutionValidity::Unknown;
        state.constitution_preview_hash = None;
        state.set_step(
            SetupStep::Constitution,
            StepEntry::new(StepStatus::Verified, true, CONSTITUTION_CHECKPOINT_VERSION)
                .with_result(match kind {
                    SetupCommitKind::BundledConstitution => "bundled/default constitution",
                    SetupCommitKind::DeferredConstitution => "checkpoint deferred; bundled applies",
                }),
        );
        let message_id = match kind {
            SetupCommitKind::BundledConstitution => MessageId::SetupCheckpointDoneBundled,
            SetupCommitKind::DeferredConstitution => MessageId::SetupCheckpointDeferred,
        };
        ViewAction::EmitAndClose(ViewEvent::SetupStateCommitRequested {
            state,
            message: tr(self.locale, message_id).to_string(),
        })
    }

    fn status_label(&self, status: StepStatus) -> Cow<'static, str> {
        tr(
            self.locale,
            match status {
                StepStatus::NotStarted => MessageId::SetupStatusNotStarted,
                StepStatus::Recommended => MessageId::SetupStatusRecommended,
                StepStatus::Optional => MessageId::SetupStatusOptional,
                StepStatus::Deferred => MessageId::SetupStatusDeferred,
                StepStatus::InProgress => MessageId::SetupStatusInProgress,
                StepStatus::NeedsAction => MessageId::SetupStatusNeedsAction,
                StepStatus::Verified => MessageId::SetupStatusVerified,
                StepStatus::Skipped => MessageId::SetupStatusSkipped,
                StepStatus::Failed => MessageId::SetupStatusFailed,
            },
        )
    }
}

impl ModalView for SetupWizardView {
    fn kind(&self) -> ModalKind {
        ModalKind::SetupWizard
    }

    fn handle_key(&mut self, key: KeyEvent) -> ViewAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ViewAction::Close,
            KeyCode::Left | KeyCode::Char('b') => {
                self.move_back();
                ViewAction::None
            }
            KeyCode::Right | KeyCode::Char('n') => {
                self.move_next();
                ViewAction::None
            }
            KeyCode::Up => {
                self.move_back();
                ViewAction::None
            }
            KeyCode::Down => {
                self.move_next();
                ViewAction::None
            }
            KeyCode::Char('s') => {
                self.mark_selected(StepStatus::Skipped);
                self.move_next();
                ViewAction::None
            }
            KeyCode::Char('r') => {
                self.mark_selected(StepStatus::NeedsAction);
                ViewAction::None
            }
            KeyCode::Char('g') if self.selected_step() == SetupStep::Constitution => {
                self.commit_guided_constitution()
            }
            KeyCode::Char('u') => self.commit_constitution(SetupCommitKind::BundledConstitution),
            KeyCode::Char('d') => self.commit_constitution(SetupCommitKind::DeferredConstitution),
            KeyCode::Enter if self.selected_step() == SetupStep::Constitution => {
                self.commit_constitution(SetupCommitKind::BundledConstitution)
            }
            KeyCode::Enter if self.selected_step() == SetupStep::ProviderModel => {
                self.commit_provider_model_review()
            }
            KeyCode::Enter if self.selected_step() == SetupStep::TrustSandbox => {
                self.commit_runtime_posture_review()
            }
            KeyCode::Enter if self.selected_step() == SetupStep::Verification => {
                self.commit_setup_report()
            }
            KeyCode::Enter => {
                self.move_next();
                ViewAction::None
            }
            _ => ViewAction::None,
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let popup_area = centered_modal_area(area, 92, 30, 56, 16);
        render_modal_surface(area, popup_area, buf);
        let progress = format!(
            "{} {}/{}",
            tr(self.locale, MessageId::SetupWizardProgress),
            self.selected + 1,
            STEP_SPECS.len()
        );
        let block = Block::default()
            .title(Line::from(Span::styled(
                format!(" {} ", tr(self.locale, MessageId::SetupWizardTitle)),
                Style::default()
                    .fg(palette::WHALE_ACCENT_PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )))
            .title_bottom(Line::from(Span::styled(
                format!(" {progress} "),
                Style::default()
                    .fg(palette::TEXT_MUTED)
                    .add_modifier(Modifier::BOLD),
            )))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(palette::BORDER_COLOR))
            .style(Style::default().bg(palette::DEEPSEEK_SLATE))
            .padding(Padding::new(2, 2, 1, 1));
        let inner = block.inner(popup_area);
        block.render(popup_area, buf);
        let mut hints = vec![
            ActionHint::new("B", tr(self.locale, MessageId::SetupActionBack).to_string()),
            ActionHint::new(
                "N",
                tr(self.locale, MessageId::SetupActionContinue).to_string(),
            ),
            ActionHint::new("S", tr(self.locale, MessageId::SetupActionSkip).to_string()),
            ActionHint::new(
                "R",
                tr(self.locale, MessageId::SetupActionRetry).to_string(),
            ),
        ];
        if self.selected_step() == SetupStep::Constitution {
            hints.push(ActionHint::new(
                "G",
                tr(self.locale, MessageId::SetupActionGuided).to_string(),
            ));
        }
        hints.extend([
            ActionHint::new(
                "U",
                tr(self.locale, MessageId::SetupActionUseBundled).to_string(),
            ),
            ActionHint::new(
                "D",
                tr(self.locale, MessageId::SetupActionDefer).to_string(),
            ),
            ActionHint::new(
                "Esc",
                tr(self.locale, MessageId::SetupActionCancel).to_string(),
            ),
        ]);
        let content_area = render_modal_footer(inner, buf, &hints);
        let spec = self.selected_spec();
        let mut lines = vec![
            Line::from(Span::styled(
                tr(self.locale, spec.title_id()).to_string(),
                Style::default()
                    .fg(palette::DEEPSEEK_SKY)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::raw(tr(self.locale, spec.why_id()).to_string())),
            Line::from(""),
        ];
        lines.extend(self.selected_step_detail_lines());
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            tr(self.locale, MessageId::SetupWizardWhy).to_string(),
            Style::default().fg(palette::TEXT_MUTED),
        )));
        lines.push(Line::from(""));
        for (idx, step) in STEP_SPECS.iter().enumerate() {
            let selected = idx == self.selected;
            let marker = if selected { ">" } else { " " };
            let style = if selected {
                Style::default()
                    .fg(palette::TEXT_PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(palette::TEXT_MUTED)
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::styled(tr(self.locale, step.title_id()).to_string(), style),
                Span::raw("  "),
                Span::styled(
                    self.status_label(self.state.status(step.id())).to_string(),
                    Style::default().fg(palette::WHALE_ACCENT_PRIMARY),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::raw(
            tr(self.locale, MessageId::SetupCheckpointLayerOrder).to_string(),
        )));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(content_area, buf);
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl SetupWizardView {
    fn selected_step_detail_lines(&self) -> Vec<Line<'static>> {
        match self.selected_step() {
            SetupStep::ProviderModel => self.provider_model_detail_lines(),
            SetupStep::TrustSandbox => self.runtime_posture_detail_lines(),
            SetupStep::Constitution => self.constitution_detail_lines(),
            SetupStep::Verification => self.verification_detail_lines(),
            _ => Vec::new(),
        }
    }

    fn provider_model_detail_lines(&self) -> Vec<Line<'static>> {
        vec![
            self.detail_row(MessageId::SetupCardRouteLabel, &self.facts.provider),
            self.detail_row(MessageId::SetupCardModelLabel, &self.facts.model),
            self.detail_row(MessageId::SetupCardAuthLabel, &self.facts.auth),
            self.detail_row(MessageId::SetupCardHealthLabel, &self.facts.health),
            Line::from(Span::styled(
                tr(
                    self.locale,
                    if self.facts.provider_ready {
                        MessageId::SetupProviderModelReadyHint
                    } else {
                        MessageId::SetupProviderModelNeedsActionHint
                    },
                )
                .to_string(),
                Style::default().fg(palette::TEXT_MUTED),
            )),
        ]
    }

    fn constitution_detail_lines(&self) -> Vec<Line<'static>> {
        let choice = constitution_choice_label(self.state.constitution_choice);
        let source = constitution_source_label(self.state.constitution_source);
        let validity = constitution_validity_label(self.state.constitution_validity);
        let preview = self
            .state
            .constitution_preview_hash
            .as_deref()
            .unwrap_or("not accepted yet")
            .to_string();
        vec![
            self.detail_row(MessageId::SetupConstitutionChoiceLabel, choice),
            self.detail_row(MessageId::SetupConstitutionSourceLabel, source),
            self.detail_row(MessageId::SetupConstitutionValidityLabel, validity),
            self.detail_row(MessageId::SetupConstitutionPreviewLabel, &preview),
            Line::from(Span::styled(
                tr(self.locale, MessageId::SetupConstitutionGuidedHint).to_string(),
                Style::default().fg(palette::TEXT_MUTED),
            )),
        ]
    }

    fn runtime_posture_detail_lines(&self) -> Vec<Line<'static>> {
        vec![
            self.detail_row(MessageId::SetupCardIntentLabel, &self.facts.work_intent),
            self.detail_row(MessageId::SetupCardApprovalLabel, &self.facts.approval),
            self.detail_row(MessageId::SetupCardShellLabel, &self.facts.shell),
            self.detail_row(MessageId::SetupCardTrustLabel, &self.facts.trust),
            self.detail_row(MessageId::SetupCardSandboxLabel, &self.facts.sandbox),
            self.detail_row(MessageId::SetupCardNetworkLabel, &self.facts.network),
            Line::from(Span::styled(
                tr(self.locale, MessageId::SetupRuntimePostureBoundary).to_string(),
                Style::default().fg(palette::TEXT_MUTED),
            )),
            Line::from(Span::styled(
                tr(self.locale, MessageId::SetupRuntimePostureReviewHint).to_string(),
                Style::default().fg(palette::TEXT_MUTED),
            )),
        ]
    }

    fn verification_detail_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![
            self.detail_row(
                MessageId::SetupReportFirstRunLabel,
                &self.ready_label(self.state.first_run_ready()),
            ),
            self.detail_row(
                MessageId::SetupReportUpdateLabel,
                &self.ready_label(self.state.update_ready(CONSTITUTION_CHECKPOINT_VERSION)),
            ),
            self.detail_row(
                MessageId::SetupReportSourceLabel,
                &self.state_source_label(),
            ),
            Line::from(""),
            Line::from(Span::styled(
                tr(self.locale, MessageId::SetupReportRowsLabel).to_string(),
                Style::default()
                    .fg(palette::TEXT_MUTED)
                    .add_modifier(Modifier::BOLD),
            )),
        ];

        for spec in STEP_SPECS {
            let step = spec.id();
            let entry = self.state.steps.get(&step);
            let required = entry.map_or(spec.required(), |entry| entry.required);
            let required_label = if required {
                tr(self.locale, MessageId::SetupReportRequired)
            } else {
                tr(self.locale, MessageId::SetupReportOptional)
            };
            let mut value = format!(
                "{} ({})",
                self.status_label(self.state.status(step)),
                required_label
            );
            if let Some(version) = entry.and_then(|entry| entry.version.as_deref()) {
                value.push_str(&format!(" · {version}"));
            }
            if let Some(result) = entry.and_then(|entry| entry.result.as_deref()) {
                value.push_str(&format!(" · {result}"));
            }
            lines.push(self.detail_row(spec.title_id(), &value));
        }

        lines.push(Line::from(""));
        let next_action = tr(self.locale, self.next_action_id()).to_string();
        lines.push(self.detail_row(MessageId::SetupReportNextActionLabel, &next_action));
        lines
    }

    fn ready_label(&self, ready: bool) -> String {
        if ready {
            tr(self.locale, MessageId::SetupReportReady).to_string()
        } else {
            tr(self.locale, MessageId::SetupStatusNeedsAction).to_string()
        }
    }

    fn state_source_label(&self) -> String {
        if self.state.inherited {
            tr(self.locale, MessageId::SetupReportInherited).to_string()
        } else {
            tr(self.locale, MessageId::SetupReportPersisted).to_string()
        }
    }

    fn next_action_id(&self) -> MessageId {
        if !self.state.update_ready(CONSTITUTION_CHECKPOINT_VERSION) {
            return MessageId::SetupReportNextActionConstitution;
        }
        if !matches!(
            self.state.status(SetupStep::ProviderModel),
            StepStatus::Verified | StepStatus::NeedsAction
        ) {
            return MessageId::SetupReportNextActionProvider;
        }
        if !self.state.runtime_posture_source.is_reviewed() {
            return MessageId::SetupReportNextActionRuntime;
        }
        if !self.state.first_run_ready() {
            return MessageId::SetupReportNextActionRequired;
        }
        MessageId::SetupReportNextActionNone
    }

    fn detail_row(&self, label: MessageId, value: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("{} ", tr(self.locale, label)),
                Style::default()
                    .fg(palette::TEXT_MUTED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(value.to_string()),
        ])
    }
}

fn setup_report_ready(state: &SetupState) -> bool {
    state.first_run_ready() || state.update_ready(CONSTITUTION_CHECKPOINT_VERSION)
}

fn setup_report_result(state: &SetupState) -> String {
    format!(
        "first_run={}, update={}, constitution={:?}, posture={:?}",
        if state.first_run_ready() {
            "ready"
        } else {
            "needs_action"
        },
        if state.update_ready(CONSTITUTION_CHECKPOINT_VERSION) {
            "ready"
        } else {
            "needs_action"
        },
        state.constitution_choice,
        state.runtime_posture_source
    )
}

#[must_use]
fn guided_constitution_template(locale: Locale) -> UserConstitution {
    match locale {
        Locale::ZhHans => UserConstitution {
            language: Some(locale.tag().to_string()),
            about: Some("希望 CodeWhale 成为稳健、重证据的工作台用户。".to_string()),
            working_style: vec![
                "保持改动聚焦，并简短说明重要取舍。".to_string(),
                "在重要场景用命令、测试、截图或引用给出具体验证。".to_string(),
                "保护密钥、用户文件、Git 历史、生产系统、成本、隐私和时间。".to_string(),
            ],
            priorities: vec![
                "当前用户请求和实时工具证据优先于记忆、陈旧交接和猜测。".to_string(),
                "遇到破坏性、高成本、凭据、发布、法律或安全风险操作时先询问。".to_string(),
            ],
            autonomy_preference: AutonomyPreference::Balanced,
            notes: Some(
                "这是用户全局常驻指导。自由文本只作为建议，不会改变审批、沙箱、Shell、网络、信任或 MCP 权限。"
                    .to_string(),
            ),
            ..UserConstitution::default()
        },
        _ => UserConstitution {
            language: Some(locale.tag().to_string()),
            about: Some(
                "A CodeWhale user who wants a calm, evidence-first coding workbench.".to_string(),
            ),
            working_style: vec![
                "Keep changes scoped and explain important tradeoffs briefly.".to_string(),
                "Prefer concrete verification with commands, tests, screenshots, or citations when they matter.".to_string(),
                "Protect secrets, user files, git history, production systems, cost, privacy, and time.".to_string(),
            ],
            priorities: vec![
                "Current user requests and live tool evidence outrank memory, stale handoffs, and guesses.".to_string(),
                "Ask before destructive, high-cost, credential, publishing, legal, or security-risk actions.".to_string(),
            ],
            autonomy_preference: AutonomyPreference::Balanced,
            notes: Some(
                "Use this as user-global standing guidance. Freeform principles are advisory and do not change runtime approval, sandbox, shell, network, trust, or MCP permissions."
                    .to_string(),
            ),
            ..UserConstitution::default()
        },
    }
}

fn constitution_choice_label(choice: ConstitutionChoice) -> &'static str {
    match choice {
        ConstitutionChoice::Unset => "unset",
        ConstitutionChoice::Bundled => "bundled/default",
        ConstitutionChoice::GuidedCustom => "guided custom",
        ConstitutionChoice::ExpertOverride => "expert override",
        ConstitutionChoice::Deferred => "deferred",
    }
}

fn constitution_source_label(source: ConstitutionSource) -> &'static str {
    match source {
        ConstitutionSource::Bundled => "bundled",
        ConstitutionSource::UserGlobal => "user-global constitution.json",
        ConstitutionSource::ExpertOverride => "expert full Markdown override",
    }
}

fn constitution_validity_label(validity: ConstitutionValidity) -> &'static str {
    match validity {
        ConstitutionValidity::Unknown => "unknown",
        ConstitutionValidity::Valid => "valid",
        ConstitutionValidity::Invalid => "invalid",
        ConstitutionValidity::Empty => "empty",
        ConstitutionValidity::Unreadable => "unreadable",
    }
}

pub fn persist_user_constitution_choice(
    constitution: &UserConstitution,
    state: &SetupState,
) -> anyhow::Result<()> {
    let constitution_path = UserConstitution::path()?;
    let setup_state_path = SetupState::path()?;
    let mut transaction = codewhale_config::persistence::SetupTransaction::new();
    transaction.stage_json(constitution_path, &constitution.bounded())?;
    transaction.stage_json(setup_state_path, state)?;
    transaction.commit()
}

#[must_use]
pub fn should_open_update_checkpoint(app: &App, config: &Config) -> bool {
    let state = load_setup_state_for_app(app, config);
    state.needs_constitution_checkpoint(CONSTITUTION_CHECKPOINT_VERSION)
}

#[must_use]
pub fn load_setup_state_for_app(app: &App, config: &Config) -> SetupState {
    if let Ok(Some(state)) = SetupState::load() {
        return state;
    }
    SetupState::derive_inherited(&inherited_facts_for_app(app, config))
}

#[must_use]
fn inherited_facts_for_app(app: &App, config: &Config) -> InheritedConfigFacts {
    let user_constitution = UserConstitution::load().ok();
    let user_constitution_validity = user_constitution.as_ref().map_or(
        ConstitutionValidity::Unknown,
        UserConstitutionLoad::validity,
    );
    let has_user_constitution = user_constitution
        .as_ref()
        .is_some_and(|loaded| !matches!(loaded, UserConstitutionLoad::Missing));
    InheritedConfigFacts {
        language: Some(app.ui_locale.tag().to_string()),
        has_provider_route: !config.default_model().trim().is_empty(),
        has_credentials_or_local_runtime: has_api_key(config),
        trust_chosen: app.trust_mode || !onboarding::needs_trust(&app.workspace),
        has_expert_override: expert_override_path().is_some_and(|path| path.exists()),
        has_user_constitution,
        user_constitution_validity,
    }
}

fn expert_override_path() -> Option<std::path::PathBuf> {
    codewhale_config::codewhale_home()
        .ok()
        .map(|home| home.join(Path::new(CONSTITUTION_OVERRIDE_FILE)))
}

#[must_use]
fn initial_step_index(state: &SetupState) -> usize {
    if state.needs_constitution_checkpoint(CONSTITUTION_CHECKPOINT_VERSION) {
        return step_index(SetupStep::Constitution);
    }
    STEP_SPECS
        .iter()
        .position(|step| {
            step.required()
                && !matches!(
                    state.status(step.id()),
                    StepStatus::Verified
                        | StepStatus::NeedsAction
                        | StepStatus::Deferred
                        | StepStatus::Optional
                        | StepStatus::Skipped
                )
        })
        .unwrap_or_else(|| step_index(SetupStep::Verification))
}

#[must_use]
fn step_index(step: SetupStep) -> usize {
    STEP_SPECS
        .iter()
        .position(|spec| spec.id() == step)
        .expect("all setup-state steps should have wizard specs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn wizard_resumes_at_constitution_checkpoint_when_update_incomplete() {
        let state = SetupState::default();

        let view = SetupWizardView::new(state, Locale::En);

        assert_eq!(view.selected_step(), SetupStep::Constitution);
    }

    #[test]
    fn bundled_constitution_commit_marks_checkpoint_complete() {
        let mut view = SetupWizardView::new(SetupState::default(), Locale::En);

        let action = view.handle_key(key(KeyCode::Enter));

        let ViewAction::EmitAndClose(ViewEvent::SetupStateCommitRequested { state, message }) =
            action
        else {
            panic!("expected setup-state commit event");
        };
        assert_eq!(
            state.constitution_checkpoint_completed_for.as_deref(),
            Some(CONSTITUTION_CHECKPOINT_VERSION)
        );
        assert_eq!(state.constitution_choice, ConstitutionChoice::Bundled);
        assert_eq!(state.status(SetupStep::Constitution), StepStatus::Verified);
        assert!(message.contains("Constitution checkpoint complete"));
    }

    #[test]
    fn cancel_closes_without_commit_event() {
        let mut view = SetupWizardView::new(SetupState::default(), Locale::En);

        let action = view.handle_key(key(KeyCode::Esc));

        assert!(matches!(action, ViewAction::Close));
    }

    #[test]
    fn skip_and_retry_update_staged_state_without_committing() {
        let mut view = SetupWizardView::new(SetupState::default(), Locale::En);

        let action = view.handle_key(key(KeyCode::Char('s')));

        assert!(matches!(action, ViewAction::None));
        assert_eq!(
            view.state().status(SetupStep::Constitution),
            StepStatus::Skipped
        );
        assert_eq!(view.selected_step(), SetupStep::Verification);

        let action = view.handle_key(key(KeyCode::Char('r')));

        assert!(matches!(action, ViewAction::None));
        assert_eq!(
            view.state().status(SetupStep::Verification),
            StepStatus::NeedsAction
        );
    }

    #[test]
    fn completed_checkpoint_resumes_to_first_required_gap() {
        let mut state = SetupState::default();
        state.complete_constitution_checkpoint(
            CONSTITUTION_CHECKPOINT_VERSION,
            ConstitutionChoice::Bundled,
        );

        let view = SetupWizardView::new(state, Locale::En);

        assert_eq!(view.selected_step(), SetupStep::Language);
    }

    #[test]
    fn zh_hans_checkpoint_copy_is_localized() {
        assert_ne!(
            tr(Locale::ZhHans, MessageId::SetupWizardTitle),
            tr(Locale::En, MessageId::SetupWizardTitle)
        );
        assert_ne!(
            tr(Locale::ZhHans, MessageId::SetupCheckpointDoneBundled),
            tr(Locale::En, MessageId::SetupCheckpointDoneBundled)
        );
    }

    #[test]
    fn guided_constitution_commit_emits_structured_payload() {
        let mut view = SetupWizardView::new(SetupState::default(), Locale::En);

        let action = view.handle_key(key(KeyCode::Char('g')));

        let ViewAction::EmitAndClose(ViewEvent::SetupConstitutionCommitRequested {
            constitution,
            state,
            message,
        }) = action
        else {
            panic!("expected guided constitution commit event");
        };
        assert_eq!(constitution.language.as_deref(), Some("en"));
        assert_eq!(
            constitution.autonomy_preference,
            AutonomyPreference::Balanced
        );
        assert_eq!(state.constitution_choice, ConstitutionChoice::GuidedCustom);
        assert_eq!(state.constitution_source, ConstitutionSource::UserGlobal);
        assert_eq!(state.constitution_validity, ConstitutionValidity::Valid);
        assert_eq!(
            state.constitution_preview_hash.as_deref(),
            Some(constitution.preview_hash().as_str())
        );
        assert_eq!(state.status(SetupStep::Constitution), StepStatus::Verified);
        assert_eq!(state.runtime_posture_source, RuntimePostureSource::Unset);
        assert!(message.contains("Guided user-global constitution saved"));
    }

    #[test]
    fn guided_constitution_key_is_contextual_to_constitution_step() {
        let mut view = SetupWizardView::new_at_with_facts(
            SetupState::default(),
            Locale::En,
            SetupStep::ProviderModel,
            SetupRuntimeFacts::default(),
        );

        let action = view.handle_key(key(KeyCode::Char('g')));

        assert!(matches!(action, ViewAction::None));
        assert_eq!(view.selected_step(), SetupStep::ProviderModel);
        assert_eq!(view.state().constitution_choice, ConstitutionChoice::Unset);
    }

    #[test]
    fn guided_constitution_template_localizes_content() {
        let english = guided_constitution_template(Locale::En).render_body();
        let zh_hans = guided_constitution_template(Locale::ZhHans).render_body();

        assert!(english.contains("evidence-first coding workbench"));
        assert!(zh_hans.contains("重证据"));
        assert_ne!(english, zh_hans);
    }

    #[test]
    fn persist_user_constitution_choice_writes_constitution_and_state() {
        let _guard = crate::test_support::lock_test_env();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let _home = crate::test_support::EnvVarGuard::set("CODEWHALE_HOME", tmp.path());
        let constitution = guided_constitution_template(Locale::En);
        let mut state = SetupState::default();
        state.complete_constitution_checkpoint(
            CONSTITUTION_CHECKPOINT_VERSION,
            ConstitutionChoice::GuidedCustom,
        );
        state.constitution_source = ConstitutionSource::UserGlobal;
        state.constitution_validity = ConstitutionValidity::Valid;
        state.constitution_preview_hash = Some(constitution.preview_hash());
        state.set_step(
            SetupStep::Constitution,
            StepEntry::new(StepStatus::Verified, true, CONSTITUTION_CHECKPOINT_VERSION),
        );

        persist_user_constitution_choice(&constitution, &state).expect("persist constitution");

        let loaded_constitution = UserConstitution::load().expect("load constitution");
        assert!(matches!(
            loaded_constitution,
            UserConstitutionLoad::Loaded(_)
        ));
        let loaded_state = SetupState::load()
            .expect("load setup state")
            .expect("setup state");
        assert_eq!(
            loaded_state.constitution_choice,
            ConstitutionChoice::GuidedCustom
        );
        assert_eq!(
            loaded_state
                .constitution_checkpoint_completed_for
                .as_deref(),
            Some(CONSTITUTION_CHECKPOINT_VERSION)
        );
    }

    #[test]
    fn provider_model_review_records_ready_route_and_continues() {
        let facts = SetupRuntimeFacts {
            provider: "DeepSeek".to_string(),
            model: "deepseek-v4-pro".to_string(),
            auth: "present".to_string(),
            health: "ready".to_string(),
            provider_ready: true,
            provider_result:
                "provider=deepseek, model=deepseek-v4-pro, auth=present/local, health=not checked"
                    .to_string(),
            ..SetupRuntimeFacts::default()
        };
        let mut view = SetupWizardView::new_at_with_facts(
            SetupState::default(),
            Locale::En,
            SetupStep::ProviderModel,
            facts,
        );

        let action = view.handle_key(key(KeyCode::Enter));

        let ViewAction::Emit(ViewEvent::SetupStateCommitRequested { state, message }) = action
        else {
            panic!("expected setup-state commit event");
        };
        assert_eq!(state.status(SetupStep::ProviderModel), StepStatus::Verified);
        assert_eq!(view.selected_step(), SetupStep::TrustSandbox);
        assert!(message.contains("Provider/model readiness recorded"));
    }

    #[test]
    fn provider_model_review_records_missing_auth_as_needs_action() {
        let facts = SetupRuntimeFacts {
            provider_ready: false,
            provider_result:
                "provider=deepseek, model=deepseek-v4-pro, auth=missing, health=needs action"
                    .to_string(),
            ..SetupRuntimeFacts::default()
        };
        let mut view = SetupWizardView::new_at_with_facts(
            SetupState::default(),
            Locale::En,
            SetupStep::ProviderModel,
            facts,
        );

        let action = view.handle_key(key(KeyCode::Enter));

        let ViewAction::Emit(ViewEvent::SetupStateCommitRequested { state, message }) = action
        else {
            panic!("expected setup-state commit event");
        };
        assert_eq!(
            state.status(SetupStep::ProviderModel),
            StepStatus::NeedsAction
        );
        assert!(message.contains("needs action"));
    }

    #[test]
    fn runtime_posture_review_confirms_without_config_mutation() {
        let facts = SetupRuntimeFacts {
            runtime_result: "intent=agent, approval=suggest, shell=enabled, trust=workspace, sandbox=default, network=prompt by default".to_string(),
            ..SetupRuntimeFacts::default()
        };
        let mut view = SetupWizardView::new_at_with_facts(
            SetupState::default(),
            Locale::En,
            SetupStep::TrustSandbox,
            facts,
        );

        let action = view.handle_key(key(KeyCode::Enter));

        let ViewAction::Emit(ViewEvent::SetupStateCommitRequested { state, message }) = action
        else {
            panic!("expected setup-state commit event");
        };
        assert_eq!(state.status(SetupStep::TrustSandbox), StepStatus::Verified);
        assert_eq!(
            state.runtime_posture_source,
            RuntimePostureSource::Confirmed
        );
        assert!(message.contains("Runtime posture reviewed"));
        assert_eq!(view.selected_step(), SetupStep::ToolsMcp);
    }

    #[test]
    fn verification_report_records_needs_action_until_checkpoint_complete() {
        let mut view = SetupWizardView::new_at_with_facts(
            SetupState::default(),
            Locale::En,
            SetupStep::Verification,
            SetupRuntimeFacts::default(),
        );

        let action = view.handle_key(key(KeyCode::Enter));

        let ViewAction::Emit(ViewEvent::SetupStateCommitRequested { state, message }) = action
        else {
            panic!("expected setup-state commit event");
        };
        assert_eq!(
            state.status(SetupStep::Verification),
            StepStatus::NeedsAction
        );
        assert!(
            state
                .steps
                .get(&SetupStep::Verification)
                .and_then(|entry| entry.result.as_deref())
                .is_some_and(|result| result.contains("update=needs_action"))
        );
        assert!(message.contains("Setup report recorded"));
    }

    #[test]
    fn verification_report_records_ready_after_bundled_checkpoint() {
        let mut state = SetupState::default();
        state.complete_constitution_checkpoint(
            CONSTITUTION_CHECKPOINT_VERSION,
            ConstitutionChoice::Bundled,
        );
        let mut view = SetupWizardView::new_at_with_facts(
            state,
            Locale::En,
            SetupStep::Verification,
            SetupRuntimeFacts::default(),
        );

        let action = view.handle_key(key(KeyCode::Enter));

        let ViewAction::Emit(ViewEvent::SetupStateCommitRequested { state, .. }) = action else {
            panic!("expected setup-state commit event");
        };
        assert_eq!(state.status(SetupStep::Verification), StepStatus::Verified);
        assert!(
            state
                .steps
                .get(&SetupStep::Verification)
                .and_then(|entry| entry.result.as_deref())
                .is_some_and(|result| result.contains("update=ready"))
        );
    }

    #[test]
    fn verification_detail_lines_show_next_action() {
        let view = SetupWizardView::new_at_with_facts(
            SetupState::default(),
            Locale::En,
            SetupStep::Verification,
            SetupRuntimeFacts::default(),
        );

        let text = lines_to_text(view.verification_detail_lines());

        assert!(text.contains("First-run:"));
        assert!(text.contains("Update checkpoint:"));
        assert!(text.contains("Complete the constitution checkpoint"));
    }

    fn lines_to_text(lines: Vec<Line<'static>>) -> String {
        lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
