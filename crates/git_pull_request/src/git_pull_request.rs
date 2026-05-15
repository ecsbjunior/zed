mod git_pull_request_panel;
mod git_pull_request_providers;
mod git_pull_request_view;
mod git_pull_request_view_toolbar;

pub use git_pull_request_panel::*;
pub use git_pull_request_view_toolbar::*;

use gpui::App;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        register(workspace);
    })
    .detach();
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(
        |workspace, _: &git_pull_request_panel::ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<GitPullRequestPanel>(window, cx);
        },
    );
}
