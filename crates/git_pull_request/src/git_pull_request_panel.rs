use std::{ops::Range, sync::Arc};

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use editor::{Editor, EditorSettings, ui_scrollbar_settings_from_raw};
use git::{GitHostingProviderRegistry, ParsedGitRemote};
use gpui::{
    Action, AnyElement, App, AppContext, AsyncWindowContext, ClickEvent, Context, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, StatefulInteractiveElement, Styled, TaskExt, UniformListScrollHandle,
    WeakEntity, Window, actions, px, uniform_list,
};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::{RegisterSetting, Settings};
use ui::utils::{DateTimeType, FormatDistance};
use workspace::{
    Panel, Workspace,
    dock::{DockPosition, PanelEvent},
    ui::{
        ActiveTheme, Color, Icon, IconName, IconSize, Label, LabelCommon, LabelSize, ScrollAxes,
        Scrollbars, WithScrollbar, h_flex,
        scrollbars::{ScrollbarVisibility, ShowScrollbar},
        v_flex,
    },
};

use crate::{
    git_pull_request_providers::{GitHubPullRequest, fetch_pull_requests},
    git_pull_request_view::GitPullRequestView,
};

actions!(git_pull_request_panel, [ToggleFocus, OpenPullRequestView]);

const GIT_PULL_REQUEST_PANEL_KEY: &str = "GitPullRequestPanel";

#[derive(Debug)]
pub enum Event {
    Focus,
}

pub struct GitPullRequestPanel {
    active_repository: Option<Entity<Repository>>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    filter_editor: Entity<Editor>,
    focus_handle: FocusHandle,
    project: Entity<Project>,
    // TODO: change GitHubPullRequest, use an abstraction
    pull_requests: Vec<Arc<GitHubPullRequest>>,
    scroll_handle: UniformListScrollHandle,
    workspace: WeakEntity<Workspace>,
}

impl GitPullRequestPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            GitPullRequestPanel::new(workspace, window, cx)
        })
    }

    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let active_repository = project.read(cx).active_repository(cx);

        let git_pull_request_panel = cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            cx.on_focus(&focus_handle, window, Self::focus_in).detach();

            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Search pull requests…", window, cx);
                editor
            });

            let scroll_handle = UniformListScrollHandle::new();

            cx.subscribe_in(
                &git_store,
                window,
                move |this, _git_store, event, _window, cx| match event {
                    GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::HeadChanged, true)
                    | GitStoreEvent::RepositoryAdded
                    | GitStoreEvent::RepositoryRemoved(_)
                    | GitStoreEvent::ActiveRepositoryChanged(_) => {
                        this.active_repository = this.project.read(cx).active_repository(cx);
                        this.update_pull_requests(cx);
                    }
                    _ => {}
                },
            )
            .detach();

            let credentials_provider = zed_credentials_provider::global(cx);

            let mut this = Self {
                active_repository,
                credentials_provider,
                filter_editor,
                focus_handle,
                project,
                pull_requests: Vec::new(),
                scroll_handle,
                workspace: workspace.weak_handle(),
            };

            this.update_pull_requests(cx);

            this
        });

        git_pull_request_panel
    }

    fn update_pull_requests(&mut self, cx: &mut Context<Self>) {
        let Some(git_remote) = self.get_git_remote(cx) else {
            return;
        };

        let client = cx.http_client();
        let credentials_provider = self.credentials_provider.clone();

        cx.spawn(async move |this, cx| -> anyhow::Result<()> {
            match fetch_pull_requests(client, &git_remote, credentials_provider, cx).await {
                Ok(pull_requests) => {
                    this.update(cx, |this, cx| {
                        this.pull_requests = pull_requests.into_iter().map(Arc::new).collect();
                        cx.notify();
                    })?;
                }
                Err(err) => log::error!("{:?}", err),
            };
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn get_git_remote(&self, cx: &Context<Self>) -> Option<ParsedGitRemote> {
        let Some(repository) = self.active_repository.as_ref() else {
            return None;
        };

        let Some(remote_url) = repository.read(cx).default_remote_url() else {
            return None;
        };

        let provider_registry = GitHostingProviderRegistry::global(cx);

        let Some((_, git_remote)) = git::parse_git_remote_url(provider_registry, &remote_url)
        else {
            return None;
        };

        Some(git_remote)
    }

    fn open_git_pull_request_view(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pull_request) = self.pull_requests.get(ix) {
            GitPullRequestView::open(
                self.active_repository.clone(),
                pull_request.clone(),
                self.workspace.clone(),
                window,
                cx,
            );
        }
    }
}

impl Render for GitPullRequestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("git-pull-request-panel")
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .bg(cx.theme().colors().panel_background)
            .child(
                v_flex()
                    .gap_2()
                    .size_full()
                    .child(self.render_filter(cx))
                    .child(self.render_pull_requests(window, cx)),
            )
    }
}

impl GitPullRequestPanel {
    fn render_filter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_2()
            .h(px(24.))
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(self.filter_editor.clone()),
            )
    }

    fn render_pull_requests(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                uniform_list(
                    "entries",
                    self.pull_requests.len(),
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        let mut items = Vec::with_capacity(range.end - range.start);

                        for ix in range {
                            if let Some(entry) = this.pull_requests.get(ix) {
                                items.push(this.render_pull_request_entry(ix, entry, cx))
                            }
                        }

                        items
                    }),
                )
                .px_1p5()
                .size_full()
                .flex_grow()
                .track_scroll(&self.scroll_handle),
            )
            .custom_scrollbars(
                Scrollbars::for_settings::<GitPullRequestPanelSettingsScrollbarProxy>()
                    .tracked_scroll_handle(&self.scroll_handle.clone())
                    .with_track_along(ScrollAxes::Horizontal, cx.theme().colors().panel_background)
                    .tracked_entity(cx.entity_id()),
                window,
                cx,
            )
    }

    fn render_pull_request_entry(
        &self,
        ix: usize,
        entry: &GitHubPullRequest,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::Name(format!("entry_{}", ix).into());

        let title = entry.title.as_str();
        let metadata = format!(
            "{} • {} • #{}",
            entry.user.login,
            FormatDistance::from_now(DateTimeType::Local(entry.created_at.into()))
                .add_suffix(true)
                .to_string(),
            entry.number
        );

        // TODO: single line??
        let title_label = Label::new(title).size(LabelSize::Default).single_line();
        let metadata_label = Label::new(metadata)
            .size(LabelSize::XSmall)
            .color(Color::Muted);

        h_flex()
            .id(id)
            .w_full()
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .child(
                v_flex()
                    .w_full()
                    .child(title_label)
                    .child(h_flex().w_full().justify_between().child(metadata_label)),
            )
            .hover(|style| {
                let hover_color = cx.theme().colors().ghost_element_hover;
                style.bg(hover_color).border_color(hover_color)
            })
            .on_click({
                cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.open_git_pull_request_view(ix, window, cx)
                })
            })
            .into_any_element()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, RegisterSetting)]
pub struct GitPullRequestPanelSettings {
    pub scrollbar: ScrollbarSettings,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ScrollbarSettings {
    /// When to show the scrollbar in the project panel.
    ///
    /// Default: inherits editor scrollbar settings
    pub show: Option<ShowScrollbar>,
}

#[derive(Default)]
pub(crate) struct GitPullRequestPanelSettingsScrollbarProxy;

impl ScrollbarVisibility for GitPullRequestPanelSettingsScrollbarProxy {
    fn visibility(&self, cx: &App) -> ShowScrollbar {
        GitPullRequestPanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl Settings for GitPullRequestPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        // TODO: change this panel
        let panel = content.outline_panel.as_ref().unwrap();
        Self {
            scrollbar: ScrollbarSettings {
                show: panel
                    .scrollbar
                    .unwrap()
                    .show
                    .map(ui_scrollbar_settings_from_raw),
            },
        }
    }
}

impl Focusable for GitPullRequestPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl GitPullRequestPanel {
    fn focus_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus_handle.contains_focused(window, cx) {
            cx.emit(Event::Focus);
        }
    }
}

impl EventEmitter<Event> for GitPullRequestPanel {}

impl EventEmitter<PanelEvent> for GitPullRequestPanel {}

impl Panel for GitPullRequestPanel {
    fn position(&self, _: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _: &mut Window, _cx: &mut Context<Self>) {
        // settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
        //     settings.project_panel.get_or_insert_default().dock = Some(position);
        // });
    }

    fn default_size(&self, _: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Git Pull Request Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn persistent_name() -> &'static str {
        "Git Pull Request Panel"
    }

    fn panel_key() -> &'static str {
        GIT_PULL_REQUEST_PANEL_KEY
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}
