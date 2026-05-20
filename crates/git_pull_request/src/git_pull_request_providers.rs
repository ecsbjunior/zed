use std::sync::{Arc, LazyLock};

use anyhow::{Context, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use credentials_provider::CredentialsProvider;
use futures::AsyncReadExt;
use git::ParsedGitRemote;
use gpui::AsyncApp;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
use serde::Deserialize;
use url::Url;

pub const PULL_REQUESTS_PER_PAGE: u32 = 100;
pub const PULL_REQUEST_FILES_PER_PAGE: u32 = 100;

const GITHUB_CREDENTIALS_URL: &str = "https://api.github.com";
const GITHUB_GRAPHQL_URL: &str = "https://api.github.com/graphql";

static GITHUB_API_BASE: LazyLock<Url> = LazyLock::new(|| {
    Url::parse("https://api.github.com/repos/").expect("valid github api base url")
});

#[derive(Debug, Default, Deserialize)]
pub struct PullRequest {
    pub created_at: DateTime<Utc>,
    pub number: u32,
    pub node_id: String,
    pub title: String,
    pub head: PullRequestRef,
    pub base: PullRequestRef,
    pub user: PullRequestUser,
}

#[derive(Debug, Default, Deserialize)]
pub struct PullRequestRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub sha: String,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct PullRequestUser {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct PullRequestFile {
    pub filename: String,
    pub status: PullRequestFileStatus,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum PullRequestFileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
    #[serde(other)]
    Unknown,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct PullRequestReviewComment {
    pub id: u64,
    pub body: String,
    pub path: String,
    pub user: PullRequestUser,
    pub created_at: DateTime<Utc>,
    pub commit_id: String,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub original_line: Option<u32>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl PullRequestReviewEvent {
    fn as_github_api_value(self) -> &'static str {
        match self {
            PullRequestReviewEvent::Approve => "APPROVE",
            PullRequestReviewEvent::RequestChanges => "REQUEST_CHANGES",
            PullRequestReviewEvent::Comment => "COMMENT",
        }
    }
}

pub struct NewReviewComment {
    pub commit_id: String,
    pub path: String,
    pub line: u32,
    pub side: String,
    pub body: String,
    pub in_reply_to_id: Option<u64>,
}

#[async_trait(?Send)]
pub trait PullRequestProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn list_pull_requests(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        page: u32,
        per_page: u32,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<PullRequest>>;

    async fn list_review_comments(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<PullRequestReviewComment>>;

    async fn create_review_comment(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        comment: NewReviewComment,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<PullRequestReviewComment>;

    async fn list_pull_request_files(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        page: u32,
        per_page: u32,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<PullRequestFile>>;

    async fn submit_review(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        body: String,
        event: PullRequestReviewEvent,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<()>;

    async fn list_viewed_files(
        &self,
        client: Arc<dyn HttpClient>,
        pull_request_node_id: String,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<String>>;

    async fn set_file_viewed(
        &self,
        client: Arc<dyn HttpClient>,
        pull_request_node_id: String,
        file_path: String,
        viewed: bool,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<()>;

    async fn auth_token(
        &self,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> Option<String>;
}

pub struct GithubPullRequestProvider;

impl GithubPullRequestProvider {
    pub fn new() -> Self {
        Self
    }

    async fn authorize(
        &self,
        request: http_client::http::request::Builder,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> http_client::http::request::Builder {
        if let Some(token) = self.auth_token(credentials_provider, cx).await {
            request.header("Authorization", format!("Bearer {}", token))
        } else {
            log::warn!("no GitHub token available");
            request
        }
    }

    /// Authorizes and sends a request, returning the response body bytes after
    /// verifying a successful status. `context` is included in transport errors.
    async fn send(
        &self,
        client: &Arc<dyn HttpClient>,
        request: http_client::http::request::Builder,
        body: AsyncBody,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
        context: String,
    ) -> anyhow::Result<Vec<u8>> {
        let request = self.authorize(request, credentials_provider, cx).await;
        let mut response = client.send(request.body(body)?).await.context(context)?;
        let mut bytes = Vec::new();
        response.body_mut().read_to_end(&mut bytes).await?;
        if !response.status().is_success() {
            let text = String::from_utf8_lossy(&bytes);
            bail!("status error {}: {text:?}", response.status().as_u16());
        }
        Ok(bytes)
    }
}

impl Default for GithubPullRequestProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl PullRequestProvider for GithubPullRequestProvider {
    fn name(&self) -> &'static str {
        "GitHub"
    }

    async fn list_pull_requests(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        page: u32,
        per_page: u32,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<PullRequest>> {
        let ParsedGitRemote { owner, repo } = remote;
        let url = GITHUB_API_BASE
            .join(&format!(
                "{owner}/{repo}/pulls?state=open&per_page={per_page}&page={page}"
            ))?
            .to_string();

        let request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        let body = self
            .send(
                &client,
                request,
                AsyncBody::default(),
                credentials_provider,
                cx,
                format!("error fetching github pull requests at {url:?}"),
            )
            .await?;

        Ok(serde_json::from_slice(&body)?)
    }

    async fn list_review_comments(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<PullRequestReviewComment>> {
        let ParsedGitRemote { owner, repo } = remote;
        let url = GITHUB_API_BASE
            .join(&format!(
                "{owner}/{repo}/pulls/{pull_number}/comments?per_page=100"
            ))?
            .to_string();

        let request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        let body = self
            .send(
                &client,
                request,
                AsyncBody::default(),
                credentials_provider,
                cx,
                format!("error fetching pull request comments at {url:?}"),
            )
            .await?;

        Ok(serde_json::from_slice(&body)?)
    }

    async fn create_review_comment(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        comment: NewReviewComment,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<PullRequestReviewComment> {
        let ParsedGitRemote { owner, repo } = remote;
        let url = GITHUB_API_BASE
            .join(&format!("{owner}/{repo}/pulls/{pull_number}/comments"))?
            .to_string();

        let mut payload = serde_json::json!({
            "body": comment.body,
            "commit_id": comment.commit_id,
            "path": comment.path,
            "line": comment.line,
            "side": comment.side,
        });
        if let Some(reply_id) = comment.in_reply_to_id
            && let Some(map) = payload.as_object_mut()
        {
            map.insert("in_reply_to".into(), serde_json::json!(reply_id));
        }
        let payload_bytes = serde_json::to_vec(&payload)?;

        let request = Request::post(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        let body = self
            .send(
                &client,
                request,
                AsyncBody::from(payload_bytes),
                credentials_provider,
                cx,
                format!("error creating review comment at {url:?}"),
            )
            .await?;

        Ok(serde_json::from_slice(&body)?)
    }

    async fn list_pull_request_files(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        page: u32,
        per_page: u32,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<PullRequestFile>> {
        let ParsedGitRemote { owner, repo } = remote;
        let url = GITHUB_API_BASE
            .join(&format!(
                "{owner}/{repo}/pulls/{pull_number}/files?per_page={per_page}&page={page}"
            ))?
            .to_string();

        let request = Request::get(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        let body = self
            .send(
                &client,
                request,
                AsyncBody::default(),
                credentials_provider,
                cx,
                format!("error fetching pull request files at {url:?}"),
            )
            .await?;

        Ok(serde_json::from_slice(&body)?)
    }

    async fn submit_review(
        &self,
        client: Arc<dyn HttpClient>,
        remote: &ParsedGitRemote,
        pull_number: u32,
        body: String,
        event: PullRequestReviewEvent,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<()> {
        let ParsedGitRemote { owner, repo } = remote;
        let url = GITHUB_API_BASE
            .join(&format!("{owner}/{repo}/pulls/{pull_number}/reviews"))?
            .to_string();

        let payload = serde_json::json!({
            "body": body,
            "event": event.as_github_api_value(),
        });
        let payload_bytes = serde_json::to_vec(&payload)?;

        let request = Request::post(&url)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        self.send(
            &client,
            request,
            AsyncBody::from(payload_bytes),
            credentials_provider,
            cx,
            format!("error submitting review at {url:?}"),
        )
        .await?;

        Ok(())
    }

    async fn list_viewed_files(
        &self,
        client: Arc<dyn HttpClient>,
        pull_request_node_id: String,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<Vec<String>> {
        let query = r#"
            query($prId: ID!) {
                node(id: $prId) {
                    ... on PullRequest {
                        files(first: 100) {
                            nodes { path viewerViewedState }
                        }
                    }
                }
            }
        "#;
        let body = serde_json::json!({
            "query": query,
            "variables": { "prId": pull_request_node_id },
        });
        let body_bytes = serde_json::to_vec(&body)?;

        let request = Request::post(GITHUB_GRAPHQL_URL)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        let buf = self
            .send(
                &client,
                request,
                AsyncBody::from(body_bytes),
                credentials_provider,
                cx,
                "error fetching viewed files".to_string(),
            )
            .await?;

        #[derive(Deserialize)]
        struct Resp {
            data: Option<Data>,
            errors: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Data {
            node: Option<Node>,
        }
        #[derive(Deserialize)]
        struct Node {
            files: Files,
        }
        #[derive(Deserialize)]
        struct Files {
            nodes: Vec<FileNode>,
        }
        #[derive(Deserialize)]
        struct FileNode {
            path: String,
            #[serde(rename = "viewerViewedState")]
            viewed_state: String,
        }

        let parsed: Resp = serde_json::from_slice(&buf)?;
        if let Some(errors) = parsed.errors {
            bail!("graphql errors: {errors}");
        }

        let nodes = parsed
            .data
            .and_then(|d| d.node)
            .map(|n| n.files.nodes)
            .unwrap_or_default();

        Ok(nodes
            .into_iter()
            .filter(|node| node.viewed_state == "VIEWED")
            .map(|node| node.path)
            .collect())
    }

    async fn set_file_viewed(
        &self,
        client: Arc<dyn HttpClient>,
        pull_request_node_id: String,
        file_path: String,
        viewed: bool,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> anyhow::Result<()> {
        let mutation = if viewed {
            "markFileAsViewed"
        } else {
            "unmarkFileAsViewed"
        };
        let query = format!(
            "mutation Set($prId: ID!, $path: String!) {{ {mutation}(input: {{ pullRequestId: $prId, path: $path }}) {{ pullRequest {{ id }} }} }}"
        );
        let body = serde_json::json!({
            "query": query,
            "variables": {
                "prId": pull_request_node_id,
                "path": file_path,
            }
        });
        let body_bytes = serde_json::to_vec(&body)?;

        let request = Request::post(GITHUB_GRAPHQL_URL)
            .header("Content-Type", "application/json")
            .follow_redirects(http_client::RedirectPolicy::FollowAll);
        let buf = self
            .send(
                &client,
                request,
                AsyncBody::from(body_bytes),
                credentials_provider,
                cx,
                format!("error marking file viewed: {file_path:?}"),
            )
            .await?;

        #[derive(Deserialize)]
        struct GraphQlResponse {
            errors: Option<serde_json::Value>,
        }
        if let Ok(parsed) = serde_json::from_slice::<GraphQlResponse>(&buf) {
            if let Some(errors) = parsed.errors {
                bail!("graphql errors: {errors}");
            }
        }

        Ok(())
    }

    /// Resolves a GitHub token using a layered fallback chain:
    /// 1. `GITHUB_TOKEN` environment variable
    /// 2. System keychain via `CredentialsProvider`
    /// 3. `gh auth token` CLI command
    async fn auth_token(
        &self,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &AsyncApp,
    ) -> Option<String> {
        if let Ok(token) = std::env::var("GITHUB_TOKEN")
            && !token.is_empty()
        {
            return Some(token);
        }

        if let Ok(Some((_username, token_bytes))) = credentials_provider
            .read_credentials(GITHUB_CREDENTIALS_URL, cx)
            .await
            && let Ok(token) = String::from_utf8(token_bytes)
        {
            return Some(token);
        }

        if let Ok(output) = smol::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
            .await
            && output.status.success()
        {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }

        None
    }
}
