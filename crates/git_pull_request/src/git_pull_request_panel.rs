use std::{collections::HashSet, ops::Range, sync::Arc};

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use editor::{Editor, EditorSettings, ui_scrollbar_settings_from_raw};
use git::{GitHostingProviderRegistry, ParsedGitRemote};
use gpui::{
    Action, AnyElement, App, AppContext, AsyncWindowContext, ClickEvent, Context, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Pixels, Render, SharedString, StatefulInteractiveElement, Styled, TaskExt,
    UniformListScrollHandle, WeakEntity, Window, actions, div, px, uniform_list,
};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::{RegisterSetting, Settings};
use util::rel_path::RelPath;
use ui::{
    Button, ButtonCommon, ButtonSize, ButtonStyle, Checkbox, Clickable, DiffStat, FluentBuilder,
    ToggleState,
    utils::{DateTimeType, FormatDistance},
};
use workspace::{
    Panel, Workspace,
    dock::{DockPosition, PanelEvent},
    ui::{
        ActiveTheme, Color, Icon, IconButton, IconName, IconSize, Label, LabelCommon, LabelSize,
        ScrollAxes, Scrollbars, WithScrollbar, h_flex,
        scrollbars::{ScrollbarVisibility, ShowScrollbar},
        v_flex,
    },
};

use crate::{
    git_pull_request_providers::{
        GitHubPullRequest, GitHubPullRequestFile, GitHubPullRequestFileStatus,
        fetch_pull_request_files, fetch_pull_requests,
    },
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
    pull_request_files: Vec<Arc<GitHubPullRequestFile>>,
    pull_requests: Vec<Arc<GitHubPullRequest>>,
    reviewed_files: HashSet<String>,
    scroll_handle: UniformListScrollHandle,
    selected_pull_request_idx: Option<usize>,
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
                pull_request_files: Vec::new(),
                pull_requests: Vec::new(),
                reviewed_files: HashSet::new(),
                scroll_handle,
                selected_pull_request_idx: None,
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

    fn load_pull_request_files(&mut self, pull_number: u32, cx: &mut Context<Self>) {
        let Some(git_remote) = self.get_git_remote(cx) else {
            return;
        };

        let client = cx.http_client();
        let credentials_provider = self.credentials_provider.clone();

        cx.spawn(async move |this, cx| -> anyhow::Result<()> {
            match fetch_pull_request_files(
                client,
                &git_remote,
                pull_number,
                credentials_provider,
                cx,
            )
            .await
            {
                Ok(files) => {
                    this.update(cx, |this, cx| {
                        this.pull_request_files = files.into_iter().map(Arc::new).collect();
                        cx.notify();
                    })?;
                }
                Err(err) => log::error!("{:?}", err),
            }
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

    fn handle_pull_request_click(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_pull_request_idx = Some(idx);
        self.pull_request_files.clear();
        self.reviewed_files.clear();

        if let Some(pull_request) = self.pull_requests.get(idx).cloned() {
            self.load_pull_request_files(pull_request.number, cx);
            GitPullRequestView::open(
                self.active_repository.clone(),
                pull_request,
                self.workspace.clone(),
                window,
                cx,
            );
        }

        cx.notify();
    }

    fn go_back(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.selected_pull_request_idx = None;
        self.pull_request_files.clear();
        self.reviewed_files.clear();
        cx.notify();
    }

    fn handle_file_click(
        &mut self,
        filename: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.selected_pull_request_idx else {
            return;
        };
        let Some(pull_request) = self.pull_requests.get(idx) else {
            return;
        };
        let pull_request_number = pull_request.number;

        let Ok(rel_path) = RelPath::unix(filename) else {
            return;
        };
        let path: Arc<RelPath> = rel_path.into();

        self.workspace
            .update(cx, |workspace, cx| {
                let view = workspace.active_pane().read(cx).items().find_map(|item| {
                    item.downcast::<GitPullRequestView>().filter(|view| {
                        view.read(cx).pull_request_number() == pull_request_number
                    })
                });
                if let Some(view) = view {
                    view.update(cx, |view, cx| {
                        view.move_to_path(path, window, cx);
                    });
                }
            })
            .ok();
    }

    fn toggle_file_reviewed(&mut self, filename: &str, cx: &mut Context<Self>) {
        if !self.reviewed_files.remove(filename) {
            self.reviewed_files.insert(filename.to_string());
        }
        cx.notify();
    }

    fn toggle_all_files_reviewed(&mut self, cx: &mut Context<Self>) {
        let all_reviewed = !self.pull_request_files.is_empty()
            && self
                .pull_request_files
                .iter()
                .all(|file| self.reviewed_files.contains(&file.filename));

        if all_reviewed {
            self.reviewed_files.clear();
        } else {
            self.reviewed_files = self
                .pull_request_files
                .iter()
                .map(|file| file.filename.clone())
                .collect();
        }
        cx.notify();
    }
}

impl Render for GitPullRequestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("git-pull-request-panel")
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .bg(cx.theme().colors().panel_background)
            .when(self.selected_pull_request_idx.is_none(), |this| {
                this.child(self.render_filter(cx))
                    .child(self.render_pull_requests(window, cx))
            })
            .when(self.selected_pull_request_idx.is_some(), |this| {
                this.child(self.render_pull_request_detail(window, cx))
            })
    }
}

impl GitPullRequestPanel {
    fn render_filter(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .p_2()
            .h(px(36.))
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

                        for idx in range {
                            if let Some(entry) = this.pull_requests.get(idx) {
                                items.push(this.render_pull_request_entry(idx, entry, cx))
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
        idx: usize,
        entry: &GitHubPullRequest,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::Name(format!("entry_{}", idx).into());

        let title = entry.title.as_str();
        let metadata = format!(
            "{} • {} • #{}",
            entry.user.login,
            FormatDistance::from_now(DateTimeType::Local(entry.created_at.into()))
                .add_suffix(true)
                .to_string(),
            entry.number
        );
        let is_selected = Some(idx) == self.selected_pull_request_idx;

        let title_label = Label::new(title).size(LabelSize::Default);
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
                    .child(h_flex().w_full().line_clamp(1).child(title_label))
                    .child(h_flex().w_full().justify_between().child(metadata_label)),
            )
            .when(is_selected, |this| {
                this.border_1()
                    .border_color(cx.theme().colors().border_selected)
            })
            .hover(|style| {
                let hover_color = cx.theme().colors().ghost_element_hover;
                style.bg(hover_color).border_color(hover_color)
            })
            .on_click({
                cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.handle_pull_request_click(idx, window, cx);
                })
            })
            .into_any_element()
    }
}

impl GitPullRequestPanel {
    fn render_pull_request_detail(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(idx) = self.selected_pull_request_idx else {
            return v_flex();
        };
        let Some(pull_request) = self.pull_requests.get(idx).cloned() else {
            return v_flex();
        };

        v_flex()
            .size_full()
            .child(self.render_detail_header(&pull_request, cx))
            .child(self.render_pr_info_block(&pull_request, cx))
            .child(
                div()
                    .id("pr-detail-content")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(self.render_changes(cx)),
            )
    }

    fn render_detail_header(
        &self,
        _pull_request: &GitHubPullRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .px_2()
            .h(px(24.))
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .cursor_pointer()
                    .child(
                        IconButton::new("back", IconName::ArrowLeft)
                            .icon_size(IconSize::Small)
                            .icon_color(Color::Muted)
                            .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
                    )
                    .child(
                        Label::new("All open PRs")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                IconButton::new("refresh", IconName::RotateCw)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.update_pull_requests(cx);
                    })),
            )
    }

    fn render_pr_info_block(
        &self,
        pull_request: &GitHubPullRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let metadata = format!(
            "{} · {} · #{}",
            pull_request.user.login,
            FormatDistance::from_now(DateTimeType::Local(pull_request.created_at.into()))
                .add_suffix(true)
                .to_string(),
            pull_request.number
        );

        v_flex()
            .px_3()
            .py_2()
            .gap_0p5()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                div().w_full().overflow_hidden().child(
                    Label::new(SharedString::from(format!(
                        "#{} — {}",
                        pull_request.number,
                        pull_request.title.clone()
                    )))
                    .size(LabelSize::Default),
                ),
            )
            .child(
                Label::new(SharedString::from(metadata))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn render_changes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.pull_request_files.clone();
        let file_count = files.len();
        let all_reviewed = file_count > 0
            && files
                .iter()
                .all(|file| self.reviewed_files.contains(&file.filename));
        let review_all_label = if all_reviewed {
            "Unreview All"
        } else {
            "Review All"
        };

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .px_3()
                    .py_1p5()
                    .justify_between()
                    .child(
                        Label::new(format!("{} Changes", file_count))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Button::new("review-all", review_all_label)
                            .size(ButtonSize::None)
                            .style(ButtonStyle::Transparent)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_all_files_reviewed(cx);
                            })),
                    ),
            )
            .children(
                files
                    .iter()
                    .enumerate()
                    .map(|(idx, file)| self.render_file_entry(idx, file, cx)),
            )
    }

    fn render_file_entry(
        &self,
        idx: usize,
        file: &GitHubPullRequestFile,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::Name(format!("file_entry_{}", idx).into());

        let path = std::path::Path::new(&file.filename);
        let file_name: SharedString = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file.filename.clone())
            .into();
        let parent_path: SharedString = path
            .parent()
            .and_then(|p| {
                let s = p.to_string_lossy().to_string();
                if s.is_empty() { None } else { Some(s) }
            })
            .unwrap_or_default()
            .into();

        let (status_icon, status_color) = match file.status {
            GitHubPullRequestFileStatus::Added => (IconName::SquarePlus, Color::Created),
            GitHubPullRequestFileStatus::Removed => (IconName::SquareMinus, Color::Deleted),
            _ => (IconName::SquareDot, Color::Modified),
        };

        let additions = file.additions;
        let deletions = file.deletions;
        let filename = file.filename.clone();
        let filename_for_click = filename.clone();
        let is_reviewed = self.reviewed_files.contains(&file.filename);
        let toggle_state = if is_reviewed {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let checkbox_id = ElementId::Name(format!("file_review_{}", idx).into());

        h_flex()
            .id(id)
            .w_full()
            .px_2()
            .py_1()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.handle_file_click(&filename_for_click, window, cx);
            }))
            .child(
                Icon::new(status_icon)
                    .size(IconSize::Small)
                    .color(status_color),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .child(Label::new(format!("{} ", file_name)).size(LabelSize::Small)),
                    )
                    .when(!parent_path.is_empty(), |this| {
                        this.child(
                            Label::new(parent_path)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate_start(),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1p5()
                    .child(DiffStat::new(
                        format!("diff-stat-{idx}"),
                        additions as usize,
                        deletions as usize,
                    ))
                    .child(
                        Checkbox::new(checkbox_id, toggle_state).on_click(cx.listener(
                            move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_file_reviewed(&filename, cx);
                            },
                        )),
                    ),
            )
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
