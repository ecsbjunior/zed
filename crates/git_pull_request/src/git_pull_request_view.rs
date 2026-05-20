use anyhow::Context as _;
use buffer_diff::BufferDiff;
use editor::{
    Addon, Editor, EditorEvent, MultiBuffer, PathKey, SelectionEffects,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    multibuffer_context_lines,
    scroll::Autoscroll,
};
use git::{
    GitHostingProviderRegistry, ParsedGitRemote,
    repository::RepoPath,
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiffStatus},
};
use gpui::{
    AnyElement, App, AppContext, AsyncApp, Context, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton, ParentElement, Render,
    SharedString, Subscription, TaskExt, WeakEntity, Window, actions,
};
use language::{
    Buffer, BufferId, Capability, DiskState, LanguageRegistry, LineEnding, OffsetRangeExt,
    ReplicaId, Rope, TextBuffer, ToPoint,
};
use multi_buffer::ExcerptBoundaryInfo;
use project::{Project, WorktreeId, git_store::Repository};
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::Arc,
};
use ui::{
    ActiveTheme, ButtonCommon, Checkbox, Clickable, Color, Disableable, ElevationIndex,
    FluentBuilder, Icon, IconName, LabelCommon, LabelSize, Styled, StyledExt, ToggleState, Tooltip,
    div, h_flex,
};
use url::Url;
use util::{ResultExt, paths::PathStyle, rel_path::RelPath, truncate_and_trailoff};
use workspace::{
    Item, ItemHandle, ItemNavHistory, Workspace,
    item::TabTooltipContent,
    searchable::SearchableItemHandle,
    ui::{Label, v_flex},
};

use crate::{
    git_pull_request_panel::GitPullRequestPanel,
    git_pull_request_providers::{
        NewReviewComment, PullRequest, PullRequestProvider, PullRequestReviewComment,
    },
};

actions!(git, [GitPullRequestOpenFileAtHead]);
actions!(git_pull_request, [AddReviewComment]);

struct PrBlob {
    path: RepoPath,
    worktree_id: WorktreeId,
    is_deleted: bool,
    display_name: String,
}

struct PrDiffAddon {
    file_statuses: HashMap<BufferId, FileStatus>,
    workspace: WeakEntity<Workspace>,
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

    fn render_buffer_header_controls(
        &self,
        _excerpt_info: &ExcerptBoundaryInfo,
        buffer: &language::BufferSnapshot,
        _window: &Window,
        cx: &App,
    ) -> Option<AnyElement> {
        let file = buffer.file()?;
        let filename = file.path().as_unix_str().to_owned();
        let panel = self
            .workspace
            .upgrade()?
            .read(cx)
            .panel::<GitPullRequestPanel>(cx)?;
        let is_reviewed = panel.read(cx).is_file_reviewed(&filename);
        let toggle_state = if is_reviewed {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let checkbox_id =
            SharedString::from(format!("pr-file-reviewed-{}", file.path().as_unix_str()));
        let panel_handle = panel.downgrade();
        let filename_for_click = filename;

        Some(
            h_flex()
                .id("pr-file-header-controls")
                .text_lg()
                .child(
                    Checkbox::new(checkbox_id, toggle_state)
                        .fill()
                        .elevation(ElevationIndex::Surface)
                        .on_click(move |_, _, cx| {
                            panel_handle
                                .update(cx, |panel, cx| {
                                    panel.toggle_file_reviewed(&filename_for_click, cx);
                                    cx.stop_propagation();
                                })
                                .ok();
                        }),
                )
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .into_any_element(),
        )
    }
}

pub struct GitPullRequestView {
    active_repository: Option<Entity<Repository>>,
    editor: Entity<Editor>,
    project: Entity<Project>,
    provider: Arc<dyn PullRequestProvider>,
    pull_request: Arc<PullRequest>,
    multibuffer: Entity<MultiBuffer>,
    review_comments: Vec<Arc<PullRequestReviewComment>>,
    comment_block_ids: Vec<CustomBlockId>,
    show_review_comments: bool,
    _workspace: WeakEntity<Workspace>,
    _panel_subscription: Option<Subscription>,
}

impl GitPullRequestView {
    pub fn calculate_changed_lines(&self, cx: &App) -> (u32, u32) {
        self.multibuffer.read(cx).snapshot(cx).total_changed_lines()
    }

    pub fn pull_request_number(&self) -> u32 {
        self.pull_request.number
    }

    pub fn provider_name(&self) -> &'static str {
        self.provider.name()
    }

    pub fn pull_request_url(&self, cx: &App) -> Option<Url> {
        let repository = self.active_repository.as_ref()?;
        let remote_url = repository.read(cx).default_remote_url()?;
        let provider_registry = GitHostingProviderRegistry::global(cx);
        let (host, remote) = git::parse_git_remote_url(provider_registry, &remote_url)?;
        let mut url = host.base_url();
        url.set_path(&format!(
            "/{}/{}/pull/{}",
            remote.owner, remote.repo, self.pull_request.number
        ));
        Some(url)
    }

    pub fn move_to_path(
        &mut self,
        path: Arc<RelPath>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path_key = PathKey::with_sort_prefix(1u64, path);
        let Some(position) = self.multibuffer.read(cx).location_for_path(&path_key, cx) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::focused()),
                window,
                cx,
                |s| {
                    s.select_ranges([position..position]);
                },
            );
        });
    }

    pub fn open(
        active_repository: Option<Entity<Repository>>,
        provider: Arc<dyn PullRequestProvider>,
        pull_request: Arc<PullRequest>,
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
                        let panel = workspace.panel::<GitPullRequestPanel>(cx);

                        let pull_request_view = cx.new(|cx| {
                            GitPullRequestView::new(
                                active_repository,
                                provider,
                                pull_request,
                                project.clone(),
                                workspace_handle,
                                panel,
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
        provider: Arc<dyn PullRequestProvider>,
        pull_request: Arc<PullRequest>,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        panel: Option<Entity<GitPullRequestPanel>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|_| MultiBuffer::new(Capability::ReadWrite));

        let weak_view = cx.weak_entity();
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);

            editor.disable_inline_diagnostics();
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_diff_review_button(true, cx);
            editor.set_expand_all_diff_hunks(cx);

            let view_for_hunk = weak_view.clone();
            editor.set_render_diff_hunk_controls(
                Arc::new(
                    move |row,
                          status,
                          hunk_range,
                          _is_created,
                          line_height,
                          _editor,
                          _window,
                          cx| {
                        render_pr_hunk_comment_button(
                            view_for_hunk.clone(),
                            row,
                            status,
                            hunk_range,
                            line_height,
                            cx,
                        )
                    },
                ),
                cx,
            );

            editor
        });

        let panel_subscription = panel.map(|panel| {
            cx.observe(&panel, |this, _, cx| {
                this.editor.update(cx, |_, cx| cx.notify());
            })
        });

        let mut this = Self {
            active_repository,
            editor,
            project: project.clone(),
            provider,
            pull_request,
            multibuffer,
            review_comments: Vec::new(),
            comment_block_ids: Vec::new(),
            show_review_comments: true,
            _workspace: workspace,
            _panel_subscription: panel_subscription,
        };

        this.fetch_pull_request_ref(cx);

        this
    }

    fn fetch_pull_request_ref(&mut self, cx: &mut Context<Self>) {
        let Some(repository) = self.active_repository.as_ref() else {
            return;
        };

        let pull_request_number = self.pull_request.number;
        let base_ref_name = self.pull_request.base.ref_name.clone();

        let work_directory = repository.read(cx).snapshot().work_directory_abs_path;
        let head_refspec = format!("pull/{n}/head:refs/{n}/head", n = pull_request_number);
        let base_refspec = format!("{base_ref_name}:refs/{n}/base", n = pull_request_number);

        cx.spawn(async move |this, cx| {
            let output = smol::process::Command::new("git")
                .current_dir(work_directory.as_ref())
                .args(["fetch", "--force", "origin", &head_refspec, &base_refspec])
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "failed to fetch PR #{} refs (head + base {}): {}",
                    pull_request_number,
                    base_ref_name,
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

        let pull_request_number = self.pull_request.number;
        let head_ref: SharedString = format!("refs/{}/head", pull_request_number).into();
        let base_ref: SharedString = format!("refs/{}/base", pull_request_number).into();

        let work_directory = repository.read(cx).snapshot().work_directory_abs_path;

        let diff_rx = repository.update(cx, |repository, cx| {
            repository.diff_tree(
                DiffTreeType::MergeBase {
                    base: base_ref.clone(),
                    head: head_ref.clone(),
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
                    let revision_path = format!("{head_ref}:{path_str}");
                    let output = smol::process::Command::new("git")
                        .current_dir(work_directory.as_ref())
                        .args(["show", &revision_path])
                        .output()
                        .await?;
                    if output.status.success() {
                        String::from_utf8_lossy(&output.stdout).into_owned()
                    } else {
                        log::warn!("failed to get file content from {revision_path}");
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
                let workspace = this._workspace.clone();
                this.editor.update(cx, |editor, _cx| {
                    editor.register_addon(PrDiffAddon {
                        file_statuses,
                        workspace,
                    });
                });
                this.load_review_comments(cx);
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn get_git_remote(&self, cx: &App) -> Option<ParsedGitRemote> {
        let repository = self.active_repository.as_ref()?;
        let remote_url = repository.read(cx).default_remote_url()?;
        let provider_registry = GitHostingProviderRegistry::global(cx);
        let (_, git_remote) = git::parse_git_remote_url(provider_registry, &remote_url)?;
        Some(git_remote)
    }

    fn load_review_comments(&mut self, cx: &mut Context<Self>) {
        let Some(git_remote) = self.get_git_remote(cx) else {
            return;
        };
        let pull_number = self.pull_request.number;
        let client = cx.http_client();
        let credentials = zed_credentials_provider::global(cx);
        let provider = self.provider.clone();

        cx.spawn(async move |this, cx| -> anyhow::Result<()> {
            let comments = provider
                .list_review_comments(client, &git_remote, pull_number, credentials, cx)
                .await?;

            this.update(cx, |this, cx| {
                this.review_comments = comments.into_iter().map(Arc::new).collect();
                this.render_review_comment_blocks(cx);
            })?;
            Ok(())
        })
        .detach_and_log_err(cx);
    }

    pub fn review_comments_visible(&self) -> bool {
        self.show_review_comments
    }

    pub fn toggle_review_comments_visibility(&mut self, cx: &mut Context<Self>) {
        self.show_review_comments = !self.show_review_comments;
        self.render_review_comment_blocks(cx);
    }

    fn render_review_comment_blocks(&mut self, cx: &mut Context<Self>) {
        let block_ids = std::mem::take(&mut self.comment_block_ids);
        if !block_ids.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(block_ids.into_iter().collect(), None, cx);
            });
        }

        if !self.show_review_comments {
            cx.notify();
            return;
        }

        let mut grouped: HashMap<(String, u32), Vec<Arc<PullRequestReviewComment>>> =
            HashMap::new();
        for comment in &self.review_comments {
            let Some(line) = comment.line.or(comment.original_line) else {
                continue;
            };
            grouped
                .entry((comment.path.clone(), line))
                .or_default()
                .push(comment.clone());
        }
        for thread in grouped.values_mut() {
            thread.sort_by_key(|c| c.created_at);
        }

        let multibuffer = self.multibuffer.read(cx);
        let multibuffer_snapshot = multibuffer.snapshot(cx);
        let mut buffer_by_path: HashMap<String, Entity<Buffer>> = HashMap::new();
        for buffer in multibuffer.all_buffers_iter() {
            if let Some(file) = buffer.read(cx).file() {
                buffer_by_path.insert(file.path().as_unix_str().to_owned(), buffer);
            }
        }

        let mut new_blocks: Vec<BlockProperties<multi_buffer::Anchor>> = Vec::new();
        for ((path, line), comments) in grouped {
            let Some(buffer) = buffer_by_path.get(&path) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let row = line.saturating_sub(1);
            let max_row = buffer_snapshot.max_point().row;
            let row = row.min(max_row);
            let text_anchor = buffer_snapshot.anchor_before(language::Point::new(row, 0));
            let Some(anchor) = multibuffer_snapshot.anchor_in_excerpt(text_anchor) else {
                continue;
            };

            let height = (comments.len() as u32 * 3 + 1).max(3);
            let comments_for_render = comments.clone();
            new_blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(height),
                style: BlockStyle::Flex,
                render: Arc::new(move |cx| {
                    render_comment_thread_block(&comments_for_render, cx).into_any_element()
                }),
                priority: 0,
            });
        }

        if new_blocks.is_empty() {
            cx.notify();
            return;
        }

        let new_ids = self
            .editor
            .update(cx, |editor, cx| editor.insert_blocks(new_blocks, None, cx));
        self.comment_block_ids = new_ids;
        cx.notify();
    }

    fn open_comment_modal_at(
        &mut self,
        anchor: multi_buffer::Anchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let commit_id = self.pull_request.head.sha.clone();
        if commit_id.is_empty() {
            log::warn!("cannot add review comment: PR head SHA is unknown");
            return;
        }
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let Some((text_anchor, buffer_snapshot)) = snapshot.anchor_to_buffer_anchor(anchor) else {
            return;
        };
        let Some(file) = buffer_snapshot.file() else {
            return;
        };
        let path = file.path().as_unix_str().to_owned();
        let row = text_anchor.to_point(buffer_snapshot).row;
        let line = (row + 1).max(1);

        let Some(workspace) = self._workspace.upgrade() else {
            return;
        };
        let weak_view = cx.weak_entity();
        workspace.update(cx, |workspace, cx| {
            workspace.toggle_modal(window, cx, move |window, cx| {
                PullRequestCommentModal::new(weak_view, path, line, "RIGHT", commit_id, window, cx)
            });
        });
    }

    fn add_review_comment(
        &mut self,
        _: &AddReviewComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let anchor = self.editor.read(cx).selections.newest_anchor().head();
        self.open_comment_modal_at(anchor, window, cx);
    }

    fn hunk_end_anchor(
        &self,
        hunk_range: &Range<multi_buffer::Anchor>,
        cx: &App,
    ) -> Option<multi_buffer::Anchor> {
        let snapshot = self.multibuffer.read(cx).snapshot(cx);
        let (text_anchor, buffer_snapshot) = snapshot.anchor_to_buffer_anchor(hunk_range.end)?;
        let row = text_anchor.to_point(buffer_snapshot).row;
        let last_row = row.saturating_sub(1);
        let new_text_anchor = buffer_snapshot.anchor_before(language::Point::new(last_row, 0));
        snapshot.anchor_in_excerpt(new_text_anchor)
    }

    fn submit_review_comment(
        &mut self,
        path: String,
        line: u32,
        side: &'static str,
        commit_id: String,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let Some(git_remote) = self.get_git_remote(cx) else {
            log::error!("no git remote to submit review comment");
            return;
        };
        let pull_number = self.pull_request.number;
        let client = cx.http_client();
        let credentials = zed_credentials_provider::global(cx);
        let provider = self.provider.clone();

        cx.spawn(async move |this, cx| -> anyhow::Result<()> {
            let comment = NewReviewComment {
                commit_id,
                path,
                line,
                side: side.to_string(),
                body,
                in_reply_to_id: None,
            };
            let result = provider
                .create_review_comment(client, &git_remote, pull_number, comment, credentials, cx)
                .await;

            this.update(cx, |this, cx| match result {
                Ok(_) => this.load_review_comments(cx),
                Err(err) => log::error!("failed to create review comment: {err:?}"),
            })?;
            Ok(())
        })
        .detach_and_log_err(cx);
    }
}

fn render_comment_thread_block(
    comments: &[Arc<PullRequestReviewComment>],
    cx: &mut editor::display_map::BlockContext,
) -> impl IntoElement {
    let colors = cx.theme().colors();
    v_flex()
        .w_full()
        .px_4()
        .py_2()
        .gap_2()
        .bg(colors.editor_subheader_background)
        .border_l_2()
        .border_color(colors.border)
        .children(comments.iter().map(|comment| {
            let created_at = ui::utils::FormatDistance::from_now(ui::utils::DateTimeType::Local(
                comment.created_at.into(),
            ))
            .add_suffix(true)
            .to_string();
            v_flex()
                .gap_0p5()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Label::new(SharedString::from(comment.user.login.clone()))
                                .size(LabelSize::Small)
                                .color(Color::Default),
                        )
                        .child(
                            Label::new(SharedString::from(created_at))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    Label::new(SharedString::from(comment.body.clone()))
                        .size(LabelSize::Small)
                        .color(Color::Default),
                )
        }))
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
            .on_action(cx.listener(Self::add_review_comment))
            .when(!self.editor.read(cx).is_empty(cx), |this| {
                this.child(div().flex_grow().child(self.editor.clone()))
            })
    }
}

fn render_pr_hunk_comment_button(
    view: WeakEntity<GitPullRequestView>,
    row: u32,
    status: &buffer_diff::DiffHunkStatus,
    hunk_range: Range<multi_buffer::Anchor>,
    line_height: gpui::Pixels,
    cx: &mut App,
) -> AnyElement {
    if status.is_deleted() {
        return gpui::Empty.into_any_element();
    }
    let colors = cx.theme().colors();
    h_flex()
        .h(line_height)
        .mr_1()
        .gap_1()
        .px_0p5()
        .pb_1()
        .border_x_1()
        .border_b_1()
        .border_color(colors.border_variant)
        .rounded_b_lg()
        .bg(colors.editor_background)
        .shadow_md()
        .child(
            ui::Button::new(("pr-hunk-comment", row as u64), "Comment")
                .size(ui::ButtonSize::Compact)
                .label_size(LabelSize::Small)
                .tooltip(Tooltip::text("Leave a review comment"))
                .on_click(move |_, window, cx| {
                    let hunk_range = hunk_range.clone();
                    view.update(cx, |view, cx| {
                        let selection = view.editor.read(cx).selections.newest_anchor().clone();
                        let snapshot = view.multibuffer.read(cx).snapshot(cx);
                        let selection_is_empty = selection.start.cmp(&selection.end, &snapshot)
                            == std::cmp::Ordering::Equal;
                        let anchor = if selection_is_empty {
                            view.hunk_end_anchor(&hunk_range, cx)
                                .unwrap_or(hunk_range.start)
                        } else {
                            selection.head()
                        };
                        view.open_comment_modal_at(anchor, window, cx);
                    })
                    .ok();
                }),
        )
        .into_any_element()
}

pub struct PullRequestCommentModal {
    view: WeakEntity<GitPullRequestView>,
    editor: Entity<Editor>,
    path: String,
    line: u32,
    side: &'static str,
    commit_id: String,
    is_submitting: bool,
}

impl PullRequestCommentModal {
    fn new(
        view: WeakEntity<GitPullRequestView>,
        path: String,
        line: u32,
        side: &'static str,
        commit_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(3, 10, window, cx);
            editor.set_placeholder_text("Write a review comment…", window, cx);
            editor.set_show_gutter(false, cx);
            editor
        });
        window.focus(&editor.focus_handle(cx), cx);

        Self {
            view,
            editor,
            path,
            line,
            side,
            commit_id,
            is_submitting: false,
        }
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.is_submitting {
            return;
        }
        let body = self.editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let Some(view) = self.view.upgrade() else {
            cx.emit(DismissEvent);
            return;
        };

        let path = self.path.clone();
        let line = self.line;
        let side = self.side;
        let commit_id = self.commit_id.clone();
        view.update(cx, |view, cx| {
            view.submit_review_comment(path, line, side, commit_id, body, cx);
        });
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

impl Focusable for PullRequestCommentModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for PullRequestCommentModal {}
impl workspace::ModalView for PullRequestCommentModal {}

impl Render for PullRequestCommentModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let body_empty = self.editor.read(cx).is_empty(cx);
        let disabled = self.is_submitting || body_empty;

        v_flex()
            .w(gpui::px(480.))
            .elevation_3(cx)
            .gap_2()
            .p_3()
            .child(
                Label::new(SharedString::from(format!(
                    "Comment on {}:{}",
                    self.path, self.line
                )))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .child(
                div()
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.editor_background)
                    .child(self.editor.clone()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .justify_end()
                    .child(
                        ui::Button::new("pr-comment-modal-cancel", "Cancel")
                            .size(ui::ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        ui::Button::new("pr-comment-modal-submit", "Comment")
                            .size(ui::ButtonSize::Compact)
                            .label_size(LabelSize::Small)
                            .disabled(disabled)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
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
