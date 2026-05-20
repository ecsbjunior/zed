use gpui::{Context, Entity, EventEmitter, IntoElement, Render, WeakEntity, Window};
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

        h_flex()
            .gap_1()
            .child(self.render_diff_stat(&pull_request_view, cx))
            .child(self.render_toggle_comments(&pull_request_view, cx))
            .child(self.render_buffer_search())
            .child(self.render_view_on_provider(&pull_request_view, cx))
    }
}

impl GitPullRequestViewToolbar {
    fn render_diff_stat(
        &self,
        pull_request_view: &Entity<GitPullRequestView>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let (additions, deletions) = pull_request_view.read(cx).calculate_changed_lines(cx);

        h_flex()
            .gap_2()
            .child(DiffStat::new(
                "toolbar-diff-stat",
                additions as usize,
                deletions as usize,
            ))
            .child(Divider::vertical())
    }

    fn render_toggle_comments(
        &self,
        pull_request_view: &Entity<GitPullRequestView>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let is_visible = pull_request_view.read(cx).review_comments_visible();

        let (icon, tooltip) = if is_visible {
            (IconName::Eye, "Hide Review Comments")
        } else {
            (IconName::EyeOff, "Show Review Comments")
        };

        let weak_pull_request_view = pull_request_view.downgrade();

        IconButton::new("toggle-review-comments-visibility", icon)
            .icon_size(IconSize::Small)
            .tooltip(Tooltip::text(tooltip))
            .on_click(move |_event, _window, cx| {
                weak_pull_request_view
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

    fn render_view_on_provider(
        &self,
        pull_request_view: &Entity<GitPullRequestView>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let view = pull_request_view.read(cx);
        let url = view.pull_request_url(cx).map(|url| url.to_string());
        let tooltip = format!("View on {}", view.provider_name());

        IconButton::new("view-on-provider", IconName::Github)
            .icon_size(IconSize::Small)
            .disabled(url.is_none())
            .tooltip(Tooltip::text(tooltip))
            .on_click(move |_event, _window, cx| {
                if let Some(url) = url.as_deref() {
                    cx.open_url(url);
                }
            })
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
}
