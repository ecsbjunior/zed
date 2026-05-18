use gpui::{Context, EventEmitter, IntoElement, Render, WeakEntity, Window};
use ui::{DiffStat, Divider, Tooltip, prelude::*};
use workspace::{ItemHandle, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView};

use crate::git_pull_request_view::GitPullRequestView;

pub struct GitPullRequestViewToolbar {
    git_pull_request_view: Option<WeakEntity<GitPullRequestView>>,
}

impl GitPullRequestViewToolbar {
    pub fn new() -> Self {
        Self {
            git_pull_request_view: None,
        }
    }
}

impl Render for GitPullRequestViewToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pull_request_view) = self
            .git_pull_request_view
            .as_ref()
            .and_then(|w| w.upgrade())
        else {
            return div();
        };

        let (additions, deletions) = pull_request_view.read(cx).calculate_changed_lines(cx);
        let comments_visible = pull_request_view.read(cx).review_comments_visible();

        h_flex()
            .gap_1()
            .when(additions > 0 || deletions > 0, |this| {
                self.render_diff_stat(this, additions, deletions)
            })
            .child(self.render_toggle_comments(&pull_request_view, comments_visible))
            .child(self.render_buffer_search())
            .child(self.render_view_on_provider())
    }
}

impl GitPullRequestViewToolbar {
    fn render_diff_stat(&self, this: Div, additions: u32, deletions: u32) -> Div {
        this.child(
            h_flex()
                .gap_2()
                .child(DiffStat::new(
                    "toolbar-diff-stat",
                    additions as usize,
                    deletions as usize,
                ))
                .child(Divider::vertical()),
        )
    }

    fn render_toggle_comments(
        &self,
        view: &gpui::Entity<GitPullRequestView>,
        visible: bool,
    ) -> impl IntoElement {
        let icon = if visible {
            IconName::Eye
        } else {
            IconName::EyeOff
        };
        let tooltip = if visible {
            "Hide Review Comments"
        } else {
            "Show Review Comments"
        };
        let weak_view = view.downgrade();
        IconButton::new("toggle-review-comments", icon)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text(tooltip))
            .on_click(move |_, _, cx| {
                weak_view
                    .update(cx, |this, cx| this.toggle_review_comments_visibility(cx))
                    .ok();
            })
    }

    fn render_buffer_search(&self) -> impl IntoElement {
        IconButton::new("buffer-search", IconName::MagnifyingGlass)
            .icon_size(IconSize::Small)
            .tooltip(move |_, cx| {
                Tooltip::for_action(
                    "Buffer Search",
                    &zed_actions::buffer_search::Deploy::find(),
                    cx,
                )
            })
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(zed_actions::buffer_search::Deploy::find()), cx);
            })
    }

    fn render_view_on_provider(&self) -> impl IntoElement {
        IconButton::new("view-on-provider", IconName::Github)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text("View on Github"))
    }
}

impl EventEmitter<ToolbarItemEvent> for GitPullRequestViewToolbar {}

impl ToolbarItemView for GitPullRequestViewToolbar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        if let Some(entity) = active_pane_item.and_then(|i| i.act_as::<GitPullRequestView>(cx)) {
            self.git_pull_request_view = Some(entity.downgrade());
            return ToolbarItemLocation::PrimaryRight;
        }
        self.git_pull_request_view = None;
        ToolbarItemLocation::Hidden
    }

    fn pane_focus_update(
        &mut self,
        _pane_focused: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }
}
