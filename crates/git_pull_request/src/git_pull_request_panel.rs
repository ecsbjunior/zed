use std::{
    collections::{BTreeMap, HashSet},
    ops::Range,
    sync::Arc,
};

use anyhow::Result;
use credentials_provider::CredentialsProvider;
use editor::{Editor, EditorSettings, ui_scrollbar_settings_from_raw};
use file_icons::FileIcons;
use git::{GitHostingProviderRegistry, ParsedGitRemote};
use gpui::{
    Action, AnyElement, App, AppContext, AsyncWindowContext, ClickEvent, Context, ElementId,
    Entity, EventEmitter, FocusHandle, Focusable, FontWeight, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, TaskExt,
    UniformListScrollHandle, WeakEntity, Window, actions, div, px, rems, uniform_list,
};
use project::{
    Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::{RegisterSetting, Settings};
use ui::{
    Button, ButtonCommon, ButtonSize, Checkbox, Clickable, DiffStat, ElevationIndex, FluentBuilder,
    ToggleState, Tooltip,
    utils::{DateTimeType, FormatDistance},
};
use util::rel_path::RelPath;
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
        fetch_pull_request_files, fetch_pull_requests, set_pull_request_file_viewed,
    },
    git_pull_request_view::GitPullRequestView,
};

actions!(git_pull_request_panel, [ToggleFocus, OpenPullRequestView]);

const GIT_PULL_REQUEST_PANEL_KEY: &str = "GitPullRequestPanel";
const TREE_INDENT: f32 = 16.0;
const TREE_GUIDE_OFFSET: f32 = 7.0;

#[derive(Debug)]
pub enum Event {
    Focus,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum PrFilesViewMode {
    #[default]
    Flat,
    Tree,
}

enum PrFileTreeEntry {
    Directory {
        path: String,
        name: String,
        depth: usize,
        is_collapsed: bool,
    },
    File {
        idx: usize,
        depth: usize,
    },
}

pub struct GitPullRequestPanel {
    active_repository: Option<Entity<Repository>>,
    collapsed_dirs: HashSet<String>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    files_view_mode: PrFilesViewMode,
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
                collapsed_dirs: HashSet::new(),
                credentials_provider,
                files_view_mode: PrFilesViewMode::default(),
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

    fn handle_file_click(&mut self, filename: &str, window: &mut Window, cx: &mut Context<Self>) {
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
                    item.downcast::<GitPullRequestView>()
                        .filter(|view| view.read(cx).pull_request_number() == pull_request_number)
                });
                if let Some(view) = view {
                    view.update(cx, |view, cx| {
                        view.move_to_path(path, window, cx);
                    });
                }
            })
            .ok();
    }

    pub fn toggle_file_reviewed(&mut self, filename: &str, cx: &mut Context<Self>) {
        let viewed = if self.reviewed_files.remove(filename) {
            false
        } else {
            self.reviewed_files.insert(filename.to_string());
            true
        };
        self.sync_files_viewed(vec![(filename.to_string(), viewed)], cx);
        cx.notify();
    }

    fn sync_files_viewed(
        &self,
        updates: Vec<(String, bool)>,
        cx: &mut Context<Self>,
    ) {
        if updates.is_empty() {
            return;
        }
        let Some(idx) = self.selected_pull_request_idx else {
            return;
        };
        let Some(pull_request) = self.pull_requests.get(idx) else {
            return;
        };
        let node_id = pull_request.node_id.clone();
        if node_id.is_empty() {
            return;
        }
        let client = cx.http_client();
        let credentials = self.credentials_provider.clone();

        cx.spawn(async move |_, cx| -> anyhow::Result<()> {
            for (path, viewed) in updates {
                if let Err(err) = set_pull_request_file_viewed(
                    client.clone(),
                    node_id.clone(),
                    path.clone(),
                    viewed,
                    credentials.clone(),
                    cx,
                )
                .await
                {
                    log::error!("failed to sync viewed state for {path:?}: {err:?}");
                }
            }
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn is_file_reviewed(&self, filename: &str) -> bool {
        self.reviewed_files.contains(filename)
    }

    fn directory_review_state(&self, dir_path: &str) -> ToggleState {
        let prefix = format!("{}/", dir_path);
        let mut total = 0;
        let mut reviewed = 0;
        for file in &self.pull_request_files {
            if file.filename.starts_with(&prefix) {
                total += 1;
                if self.reviewed_files.contains(&file.filename) {
                    reviewed += 1;
                }
            }
        }
        if total == 0 || reviewed == 0 {
            ToggleState::Unselected
        } else if reviewed == total {
            ToggleState::Selected
        } else {
            ToggleState::Indeterminate
        }
    }

    fn toggle_directory_reviewed(&mut self, dir_path: &str, cx: &mut Context<Self>) {
        let prefix = format!("{}/", dir_path);
        let files_in_dir: Vec<String> = self
            .pull_request_files
            .iter()
            .filter(|file| file.filename.starts_with(&prefix))
            .map(|file| file.filename.clone())
            .collect();
        let all_reviewed = !files_in_dir.is_empty()
            && files_in_dir
                .iter()
                .all(|filename| self.reviewed_files.contains(filename));

        let mut updates = Vec::with_capacity(files_in_dir.len());
        if all_reviewed {
            for filename in &files_in_dir {
                if self.reviewed_files.remove(filename) {
                    updates.push((filename.clone(), false));
                }
            }
        } else {
            for filename in files_in_dir {
                if self.reviewed_files.insert(filename.clone()) {
                    updates.push((filename, true));
                }
            }
        }
        self.sync_files_viewed(updates, cx);
        cx.notify();
    }

    fn build_tree_entries(&self) -> Vec<PrFileTreeEntry> {
        #[derive(Default)]
        struct DirNode {
            children: BTreeMap<String, DirNode>,
            files: Vec<usize>,
        }

        let mut root = DirNode::default();
        for (idx, file) in self.pull_request_files.iter().enumerate() {
            let parts: Vec<&str> = file.filename.split('/').collect();
            let dir_parts: &[&str] = if parts.len() > 1 {
                &parts[..parts.len() - 1]
            } else {
                &[]
            };
            let mut node = &mut root;
            for part in dir_parts {
                node = node.children.entry((*part).to_string()).or_default();
            }
            node.files.push(idx);
        }

        let mut entries = Vec::new();
        fn walk(
            node: &DirNode,
            path: &str,
            depth: usize,
            collapsed: &HashSet<String>,
            entries: &mut Vec<PrFileTreeEntry>,
        ) {
            for (name, child) in &node.children {
                let mut display_parts = vec![name.clone()];
                let mut path_parts = vec![name.clone()];
                let mut cursor = child;
                while cursor.files.is_empty() && cursor.children.len() == 1 {
                    let Some((child_name, only_child)) = cursor.children.iter().next() else {
                        break;
                    };
                    display_parts.push(child_name.clone());
                    path_parts.push(child_name.clone());
                    cursor = only_child;
                }
                let display_name = display_parts.join("/");
                let segment = path_parts.join("/");
                let full_path = if path.is_empty() {
                    segment
                } else {
                    format!("{}/{}", path, segment)
                };
                let is_collapsed = collapsed.contains(&full_path);
                entries.push(PrFileTreeEntry::Directory {
                    path: full_path.clone(),
                    name: display_name,
                    depth,
                    is_collapsed,
                });
                if !is_collapsed {
                    walk(cursor, &full_path, depth + 1, collapsed, entries);
                }
            }
            for &file_idx in &node.files {
                entries.push(PrFileTreeEntry::File {
                    idx: file_idx,
                    depth,
                });
            }
        }
        walk(&root, "", 0, &self.collapsed_dirs, &mut entries);
        entries
    }

    fn toggle_files_view_mode(&mut self, cx: &mut Context<Self>) {
        self.files_view_mode = match self.files_view_mode {
            PrFilesViewMode::Flat => PrFilesViewMode::Tree,
            PrFilesViewMode::Tree => PrFilesViewMode::Flat,
        };
        cx.notify();
    }

    fn toggle_directory(&mut self, dir: String, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(&dir) {
            self.collapsed_dirs.insert(dir);
        }
        cx.notify();
    }

    fn toggle_all_files_reviewed(&mut self, cx: &mut Context<Self>) {
        let all_reviewed = !self.pull_request_files.is_empty()
            && self
                .pull_request_files
                .iter()
                .all(|file| self.reviewed_files.contains(&file.filename));

        let mut updates = Vec::with_capacity(self.pull_request_files.len());
        if all_reviewed {
            for file in &self.pull_request_files {
                if self.reviewed_files.remove(&file.filename) {
                    updates.push((file.filename.clone(), false));
                }
            }
        } else {
            for file in &self.pull_request_files {
                if self.reviewed_files.insert(file.filename.clone()) {
                    updates.push((file.filename.clone(), true));
                }
            }
        }
        self.sync_files_viewed(updates, cx);
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
            .h(px(24.))
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .w_full()
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

        let title = format!("#{} — {}", entry.number, entry.title.as_str());
        let metadata = format!(
            "{} · {} on ",
            entry.user.login,
            FormatDistance::from_now(DateTimeType::Local(entry.created_at.into()))
                .add_suffix(true)
                .to_string()
        );
        let is_selected = Some(idx) == self.selected_pull_request_idx;

        let title_label = Label::new(title).size(LabelSize::Default);
        let metadata_label = Label::new(metadata)
            .size(LabelSize::XSmall)
            .color(Color::Muted);
        let head_ref_label = Label::new(SharedString::from(entry.head.ref_name.clone()))
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .weight(FontWeight::BOLD);

        h_flex()
            .id(id)
            .w_full()
            .px_2()
            .py_1()
            .cursor_pointer()
            .child(
                v_flex()
                    .w_full()
                    .child(h_flex().w_full().line_clamp(1).child(title_label))
                    .child(
                        h_flex()
                            .w_full()
                            .line_clamp(1)
                            .child(metadata_label)
                            .child(head_ref_label),
                    ),
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
            .p_2()
            .h(px(24.))
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
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
        let title = format!("#{} — {}", pull_request.number, pull_request.title.as_str());
        let metadata = format!(
            "{} · {} on ",
            pull_request.user.login,
            FormatDistance::from_now(DateTimeType::Local(pull_request.created_at.into()))
                .add_suffix(true)
                .to_string()
        );

        let title_label = Label::new(title).size(LabelSize::Default);
        let metadata_label = Label::new(metadata)
            .size(LabelSize::XSmall)
            .color(Color::Muted);
        let head_ref_label = Label::new(SharedString::from(pull_request.head.ref_name.clone()))
            .size(LabelSize::XSmall)
            .color(Color::Muted)
            .weight(FontWeight::BOLD);

        v_flex()
            .px_2()
            .py_1()
            .w_full()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(h_flex().w_full().overflow_hidden().child(title_label))
            .child(
                h_flex()
                    .w_full()
                    .overflow_hidden()
                    .child(metadata_label)
                    .child(head_ref_label),
            )
    }

    fn render_changes(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.pull_request_files.clone();
        let file_count = files.len();
        let reviewed_count = files
            .iter()
            .filter(|file| self.reviewed_files.contains(&file.filename))
            .count();
        let review_all_state = if file_count == 0 || reviewed_count == 0 {
            ToggleState::Unselected
        } else if reviewed_count == file_count {
            ToggleState::Selected
        } else {
            ToggleState::Indeterminate
        };
        let review_all_tooltip = if review_all_state == ToggleState::Selected {
            "Unreview All Files"
        } else {
            "Review All Files"
        };
        let is_tree_view = self.files_view_mode == PrFilesViewMode::Tree;
        let view_mode_text = if is_tree_view {
            "Flat View"
        } else {
            "Tree View"
        };
        let view_mode_icon = if is_tree_view {
            IconName::ListCollapse
        } else {
            IconName::ListTree
        };

        let entries: Vec<_> = match self.files_view_mode {
            PrFilesViewMode::Flat => files
                .iter()
                .enumerate()
                .map(|(idx, file)| self.render_file_entry(idx, file, 0, cx))
                .collect(),
            PrFilesViewMode::Tree => self
                .build_tree_entries()
                .into_iter()
                .map(|entry| match entry {
                    PrFileTreeEntry::Directory {
                        path,
                        name,
                        depth,
                        is_collapsed,
                    } => self.render_directory_entry(&path, &name, depth, is_collapsed, cx),
                    PrFileTreeEntry::File { idx, depth } => {
                        let file = files[idx].clone();
                        self.render_file_entry(idx, &file, depth, cx)
                    }
                })
                .collect(),
        };

        v_flex()
            .w_full()
            .child(
                h_flex()
                    .pl_3()
                    .pr_1()
                    .py_1p5()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(format!("{} Changes", file_count))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("toggle-files-view", view_mode_text)
                                    .size(ButtonSize::Compact)
                                    .label_size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .start_icon(
                                        Icon::new(view_mode_icon)
                                            .size(IconSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.toggle_files_view_mode(cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("review-all-wrapper")
                                    .flex_none()
                                    .occlude()
                                    .cursor_pointer()
                                    .child(
                                        Checkbox::new("review-all", review_all_state)
                                            .fill()
                                            .elevation(ElevationIndex::Surface)
                                            .tooltip(Tooltip::text(review_all_tooltip))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.toggle_all_files_reviewed(cx);
                                            })),
                                    ),
                            ),
                    ),
            )
            .children(entries)
    }

    fn render_directory_entry(
        &self,
        path: &str,
        name: &str,
        depth: usize,
        is_collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::Name(format!("pr_dir_{}", path).into());
        let checkbox_id = ElementId::Name(format!("pr_dir_review_{}", path).into());
        let checkbox_wrapper_id = ElementId::Name(format!("pr_dir_review_wrapper_{}", path).into());
        let path_for_toggle = path.to_string();
        let path_for_checkbox = path.to_string();
        let expanded = !is_collapsed;
        let folder_icon = FileIcons::get_folder_icon(expanded, std::path::Path::new(path), cx);
        let fallback_folder_icon = if expanded {
            IconName::FolderOpen
        } else {
            IconName::Folder
        };
        let toggle_state = self.directory_review_state(path);

        let name_row = h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_1()
            .child(Self::render_indent_guides(depth, cx))
            .child(
                folder_icon
                    .map(|folder_icon| {
                        Icon::from_path(folder_icon)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                    })
                    .unwrap_or_else(|| {
                        Icon::new(fallback_folder_icon)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                    }),
            )
            .child(
                Label::new(SharedString::from(name.to_string()))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            );

        h_flex()
            .id(id)
            .w_full()
            .h(rems(1.75))
            .pl_3()
            .pr_1()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.toggle_directory(path_for_toggle.clone(), cx);
            }))
            .child(name_row)
            .child(
                div()
                    .id(checkbox_wrapper_id)
                    .flex_none()
                    .occlude()
                    .cursor_pointer()
                    .child(
                        Checkbox::new(checkbox_id, toggle_state)
                            .fill()
                            .elevation(ElevationIndex::Surface)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_directory_reviewed(&path_for_checkbox, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_indent_guides(depth: usize, cx: &App) -> impl IntoElement {
        let color = cx.theme().colors().panel_indent_guide;
        h_flex().flex_none().h_full().children((0..depth).map(|_| {
            h_flex()
                .flex_none()
                .w(px(TREE_INDENT))
                .h_full()
                .pl(px(TREE_GUIDE_OFFSET))
                .child(div().w(px(1.0)).h_full().bg(color))
        }))
    }

    fn render_file_entry(
        &self,
        idx: usize,
        file: &GitHubPullRequestFile,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = ElementId::Name(format!("file_entry_{}", idx).into());
        let is_tree_view = self.files_view_mode == PrFilesViewMode::Tree;

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

        let name_row = h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .gap_1p5()
            .when(is_tree_view, |this| {
                this.child(Self::render_indent_guides(depth, cx))
            })
            .child(
                Icon::new(status_icon)
                    .size(IconSize::Small)
                    .color(status_color),
            )
            .child(
                div()
                    .flex_none()
                    .child(Label::new(format!("{} ", file_name)).size(LabelSize::Small)),
            )
            .when(!is_tree_view && !parent_path.is_empty(), |this| {
                this.child(
                    Label::new(parent_path)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate_start(),
                )
            });

        h_flex()
            .id(id)
            .w_full()
            .h(rems(1.75))
            .pl_3()
            .pr_1()
            .gap_1p5()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.handle_file_click(&filename_for_click, window, cx);
            }))
            .child(name_row)
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
                        div()
                            .id(ElementId::Name(
                                format!("file_review_wrapper_{}", idx).into(),
                            ))
                            .flex_none()
                            .occlude()
                            .cursor_pointer()
                            .child(
                                Checkbox::new(checkbox_id, toggle_state)
                                    .fill()
                                    .elevation(ElevationIndex::Surface)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.toggle_file_reviewed(&filename, cx);
                                    })),
                            ),
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
