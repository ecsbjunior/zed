use anyhow::Context as _;
use buffer_diff::BufferDiff;
use editor::{
    Addon, Editor, EditorEvent, ExcerptRange, MultiBuffer, PathKey,
    display_map::{BlockPlacement, BlockProperties, BlockStyle},
    multibuffer_context_lines,
};
use git::{
    repository::RepoPath,
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiffStatus},
};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, TaskExt, WeakEntity, Window, actions,
};
use language::{
    Anchor, Buffer, BufferId, Capability, DiskState, LanguageRegistry, LineEnding, OffsetRangeExt,
    Point, ReplicaId, Rope, TextBuffer,
};
use project::{Project, WorktreeId, git_store::Repository};
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
};
use ui::{ActiveTheme, Color, FluentBuilder, Icon, IconName, Styled, Tooltip, div};
use util::{ResultExt, paths::PathStyle, rel_path::RelPath, truncate_and_trailoff};
use workspace::{
    Item, ItemHandle, ItemNavHistory, Workspace,
    item::TabTooltipContent,
    searchable::SearchableItemHandle,
    ui::{Label, v_flex},
};

use crate::git_pull_request_providers::GitHubPullRequest;

actions!(git, [GitPullRequestOpenFileAtHead]);

struct PrBlob {
    path: RepoPath,
    worktree_id: WorktreeId,
    is_deleted: bool,
    display_name: String,
}

struct PrDiffAddon {
    file_statuses: HashMap<BufferId, FileStatus>,
}

impl Addon for PrDiffAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn override_status_for_buffer_id(
        &self,
        buffer_id: language::BufferId,
        _cx: &App,
    ) -> Option<FileStatus> {
        self.file_statuses.get(&buffer_id).copied()
    }
}

pub struct GitPullRequestView {
    active_repository: Option<Entity<Repository>>,
    editor: Entity<Editor>,
    project: Entity<Project>,
    // TODO: change GitHubPullRequest, use an abstraction
    pull_request: Arc<GitHubPullRequest>,
    multibuffer: Entity<MultiBuffer>,
    _workspace: WeakEntity<Workspace>,
}

impl GitPullRequestView {
    pub fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    pub fn open(
        active_repository: Option<Entity<Repository>>,
        pull_request: Arc<GitHubPullRequest>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        window
            .spawn(cx, async move |cx| {
                workspace
                    .update_in(cx, |workspace, window, cx| {
                        let project = workspace.project();
                        let workspace_handle = cx.weak_entity();
                        let pull_request_number = pull_request.number;

                        let pull_request_view = cx.new(|cx| {
                            GitPullRequestView::new(
                                active_repository,
                                pull_request,
                                project.clone(),
                                workspace_handle,
                                window,
                                cx,
                            )
                        });

                        let pane = workspace.active_pane();

                        pane.update(cx, |pane, cx| {
                            let idx = pane.items().position(|item| {
                                item.downcast::<GitPullRequestView>().is_some_and(|this| {
                                    this.read(cx).pull_request.number == pull_request_number
                                })
                            });

                            if let Some(idx) = idx {
                                pane.activate_item(idx, true, true, window, cx);
                            } else {
                                pane.add_item(
                                    Box::new(pull_request_view),
                                    true,
                                    true,
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        })
                    })
                    .log_err()
            })
            .detach();
    }

    fn new(
        active_repository: Option<Entity<Repository>>,
        pull_request: Arc<GitHubPullRequest>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadOnly));

        let message_buffer = cx.new(|cx| {
            let mut buffer =
                Buffer::local(format!("{}\n{}", pull_request.title, pull_request.body), cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });

        multibuffer.update(cx, |multibuffer, cx| {
            let snapshot = message_buffer.read(cx).snapshot();
            let full_range = Point::zero()..snapshot.max_point();
            let range = ExcerptRange {
                context: full_range.clone(),
                primary: full_range,
            };
            multibuffer.set_excerpt_ranges_for_path(
                PathKey::with_sort_prefix(0u64, RelPath::unix("commit message").unwrap().into()),
                message_buffer.clone(),
                &snapshot,
                vec![range],
                cx,
            )
        });

        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);

            editor.disable_inline_diagnostics();
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_diff_review_button(true, cx);
            editor.set_expand_all_diff_hunks(cx);
            editor.disable_header_for_buffer(message_buffer.read(cx).remote_id(), cx);
            editor.disable_indent_guides_for_buffer(message_buffer.read(cx).remote_id(), cx);

            editor.insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Above(editor::Anchor::Min),
                    height: Some(1),
                    style: BlockStyle::Sticky,
                    render: Arc::new(|_| gpui::Empty.into_any_element()),
                    priority: 0,
                }]
                .into_iter()
                .chain(
                    editor
                        .buffer()
                        .read(cx)
                        .snapshot(cx)
                        .anchor_in_buffer(Anchor::max_for_buffer(
                            message_buffer.read(cx).remote_id(),
                        ))
                        .map(|anchor| BlockProperties {
                            placement: BlockPlacement::Below(anchor),
                            height: Some(1),
                            style: BlockStyle::Sticky,
                            render: Arc::new(|_| gpui::Empty.into_any_element()),
                            priority: 0,
                        }),
                ),
                None,
                cx,
            );

            editor
        });

        let mut this = Self {
            active_repository,
            editor,
            project: project.clone(),
            pull_request,
            multibuffer,
            _workspace: workspace,
        };

        this.fetch_pull_request_ref(cx);

        this
    }

    fn fetch_pull_request_ref(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.active_repository.as_ref() else {
            return;
        };

        let pull_request_number = self.pull_request.number;

        let work_directory = repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .clone();
        let refspec = format!("pull/{}/head", pull_request_number);

        cx.spawn(async move |this, cx| {
            let output = smol::process::Command::new("git")
                .current_dir(work_directory.as_ref())
                .args(["fetch", "origin", &refspec])
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "failed to fetch PR #{} ref: {}",
                    pull_request_number,
                    stderr
                );
            }

            this.update(cx, |this, cx| {
                this.load_diff(cx);
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn load_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.active_repository.clone() else {
            return;
        };

        let base_branch_ref: SharedString = self.pull_request.base.ref_name.clone().into();

        let work_directory = repository
            .read(cx)
            .snapshot()
            .work_directory_abs_path
            .clone();

        let diff_rx = repository.update(cx, |repository, cx| {
            repository.diff_tree(
                DiffTreeType::MergeBase {
                    base: base_branch_ref.into(),
                    head: "FETCH_HEAD".into(),
                },
                cx,
            )
        });

        let project = self.project.clone();
        let language_registry = project.read(cx).languages().clone();

        let first_worktree_id = project
            .read(cx)
            .worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).id());

        cx.spawn(async move |this, cx| {
            let tree_diff = diff_rx.await??;

            let mut file_statuses: HashMap<BufferId, FileStatus> = HashMap::new();

            for (repository_path, tree_diff_status) in &tree_diff.entries {
                let is_deleted = matches!(tree_diff_status, TreeDiffStatus::Deleted { .. });

                let new_text = if is_deleted {
                    String::new()
                } else {
                    let path_str = repository_path.as_unix_str().to_string();
                    let output = smol::process::Command::new("git")
                        .current_dir(work_directory.as_ref())
                        .args(["show", &format!("FETCH_HEAD:{}", path_str)])
                        .output()
                        .await?;
                    if output.status.success() {
                        String::from_utf8_lossy(&output.stdout).into_owned()
                    } else {
                        log::warn!("failed to get file content from FETCH_HEAD:{}", path_str);
                        String::new()
                    }
                };

                let old_oid = match *tree_diff_status {
                    TreeDiffStatus::Deleted { old } => Some(old),
                    TreeDiffStatus::Modified { old } => Some(old),
                    TreeDiffStatus::Added => None,
                };

                let old_text = if let Some(oid) = old_oid {
                    let output = smol::process::Command::new("git")
                        .current_dir(work_directory.as_ref())
                        .args(["cat-file", "blob", &oid.to_string()])
                        .output()
                        .await?;
                    if output.status.success() {
                        Some(String::from_utf8_lossy(&output.stdout).into_owned())
                    } else {
                        log::warn!("failed to read blob {}", oid);
                        None
                    }
                } else {
                    None
                };

                let worktree_id = repository
                    .update(cx, |repo, cx| {
                        repo.repo_path_to_project_path(repository_path, cx)
                            .map(|path| path.worktree_id)
                            .or(first_worktree_id)
                    })
                    .context("project has no worktrees")?;

                let display_name = repository_path
                    .file_name()
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| repository_path.display(PathStyle::local()).to_string());

                let pr_blob = Arc::new(PrBlob {
                    path: repository_path.clone(),
                    worktree_id,
                    is_deleted,
                    display_name,
                }) as Arc<dyn language::File>;

                let buffer = build_buffer(new_text, pr_blob, &language_registry, cx).await?;
                let buffer_diff =
                    build_buffer_diff(old_text, &buffer, &language_registry, cx).await?;

                let file_status = match tree_diff_status {
                    TreeDiffStatus::Added => FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Added,
                        worktree_status: StatusCode::Unmodified,
                    }),
                    TreeDiffStatus::Modified { .. } => FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Modified,
                        worktree_status: StatusCode::Unmodified,
                    }),
                    TreeDiffStatus::Deleted { .. } => FileStatus::Tracked(TrackedStatus {
                        index_status: StatusCode::Deleted,
                        worktree_status: StatusCode::Unmodified,
                    }),
                };

                this.update(cx, |this, cx| {
                    let buffer_id = buffer.read(cx).remote_id();
                    file_statuses.insert(buffer_id, file_status);

                    this.multibuffer.update(cx, |multibuffer, cx| {
                        let snapshot = buffer.read(cx).snapshot();
                        let diff_snapshot = buffer_diff.read(cx).snapshot(cx);
                        let mut hunks = diff_snapshot.hunks(&snapshot).peekable();
                        let excerpt_ranges = if hunks.peek().is_none() {
                            vec![language::Point::zero()..snapshot.max_point()]
                        } else {
                            hunks
                                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                                .collect::<Vec<_>>()
                        };
                        multibuffer.set_excerpts_for_path(
                            PathKey::with_sort_prefix(
                                1u64,
                                snapshot.file().unwrap().path().clone(),
                            ),
                            buffer,
                            excerpt_ranges,
                            multibuffer_context_lines(cx),
                            cx,
                        );
                        multibuffer.add_diff(buffer_diff, cx);
                    });
                })?;
            }

            this.update(cx, |this, cx| {
                this.editor.update(cx, |editor, _cx| {
                    editor.register_addon(PrDiffAddon { file_statuses });
                });
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

impl language::File for PrBlob {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.is_deleted,
        }
    }

    fn path(&self) -> &Arc<RelPath> {
        self.path.as_ref()
    }

    fn full_path(&self, _cx: &App) -> PathBuf {
        self.path.as_std_path().to_path_buf()
    }

    fn path_style(&self, _cx: &App) -> PathStyle {
        PathStyle::local()
    }

    fn file_name<'a>(&'a self, _cx: &'a App) -> &'a str {
        self.display_name.as_ref()
    }

    fn worktree_id(&self, _cx: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        unimplemented!()
    }

    fn is_private(&self) -> bool {
        false
    }
}

async fn build_buffer(
    mut text: String,
    blob: Arc<dyn language::File>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> anyhow::Result<Entity<Buffer>> {
    let line_ending = LineEnding::detect(&text);
    LineEnding::normalize(&mut text);
    let text = Rope::from(text);
    let language = cx.update(|cx| language_registry.language_for_file(&blob, Some(&text), cx));
    let language = if let Some(language) = language {
        language_registry
            .load_language(&language)
            .await
            .ok()
            .and_then(|e| e.log_err())
    } else {
        None
    };
    let buffer = cx.new(|cx| {
        let buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            text,
        );
        let mut buffer = Buffer::build(buffer, Some(blob), Capability::ReadWrite);
        buffer.set_language_async(language, cx);
        buffer
    });
    Ok(buffer)
}

async fn build_buffer_diff(
    mut old_text: Option<String>,
    buffer: &Entity<Buffer>,
    language_registry: &Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> anyhow::Result<Entity<BufferDiff>> {
    if let Some(old_text) = &mut old_text {
        LineEnding::normalize(old_text);
    }

    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let buffer = cx.update(|cx| buffer.read(cx).snapshot());

    let diff = cx.new(|cx| BufferDiff::new(&buffer.text, cx));

    let update = diff
        .update(cx, |diff, cx| {
            diff.update_diff(
                buffer.text.clone(),
                old_text.map(|old_text| Arc::from(old_text.as_str())),
                Some(true),
                language.clone(),
                cx,
            )
        })
        .await;

    diff.update(cx, |diff, cx| {
        diff.language_changed(language, Some(language_registry.clone()), cx);
        diff.set_snapshot(update, &buffer.text, cx)
    })
    .await;

    Ok(diff)
}

impl EventEmitter<EditorEvent> for GitPullRequestView {}

impl Focusable for GitPullRequestView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for GitPullRequestView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        let title = truncate_and_trailoff(&self.pull_request.title, 20);
        format!("#{} — {}", self.pull_request.number, title).into()
    }

    fn tab_tooltip_content(&self, _cx: &App) -> Option<TabTooltipContent> {
        Some(TabTooltipContent::Custom(Box::new(Tooltip::element({
            let title = self.pull_request.title.clone();
            let number = self.pull_request.number;
            let tooltip = format!("#{} — {}", number, title);
            move |_window, _cx| {
                v_flex()
                    .child(Label::new(tooltip.clone()))
                    .into_any_element()
            }
        }))))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Pull Request View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for GitPullRequestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .when(!self.editor.read(cx).is_empty(cx), |this| {
                this.child(div().flex_grow().child(self.editor.clone()))
            })
    }
}

// .child(
//     h_flex()
//         .py_2()
//         .w_full()
//         .border_b_1()
//         .border_color(cx.theme().colors().border_variant)
//         .child(
//             h_flex().child(h_flex().w(px(64.))).child(
//                 v_flex()
//                     .child(Label::new(&self.pull_request.user.login))
//                     .child(
//                         h_flex().gap_1p5().child(
//                             Label::new(
//                                 FormatDistance::from_now(DateTimeType::Local(
//                                     self.pull_request.created_at.into(),
//                                 ))
//                                 .add_suffix(true)
//                                 .to_string(),
//                             )
//                             .color(Color::Muted)
//                             .size(LabelSize::Small),
//                         ),
//                     ),
//             ),
//         ),
// )
