use std::sync::Arc;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use credentials_provider::CredentialsProvider;
use futures::AsyncReadExt;
use git::ParsedGitRemote;
use gpui::AsyncApp;
use http_client::{AsyncBody, HttpClient, HttpRequestExt, Request};
use serde::Deserialize;
use url::Url;

#[derive(Debug, Default, Deserialize)]
pub struct GitHubPullRequest {
    pub created_at: DateTime<Utc>,
    pub number: u32,
    pub node_id: String,
    pub title: String,
    pub head: GitHubPullRequestRef,
    pub base: GitHubPullRequestRef,
    pub user: GitHubPullRequestUser,
}

#[derive(Debug, Default, Deserialize)]
pub struct GitHubPullRequestRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(default)]
    pub sha: String,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct GitHubPullRequestUser {
    pub login: String,
}

pub const PULL_REQUESTS_PER_PAGE: u32 = 100;

pub async fn fetch_pull_requests_page(
    client: Arc<dyn HttpClient>,
    remote: &ParsedGitRemote,
    page: u32,
    per_page: u32,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> anyhow::Result<Vec<GitHubPullRequest>> {
    let ParsedGitRemote { owner, repo } = remote;
    let url = base_url()
        .join(&format!(
            "{owner}/{repo}/pulls?state=open&per_page={per_page}&page={page}"
        ))
        .expect("can't build pull request url")
        .to_string();

    let mut request = Request::get(&url)
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::default())?)
        .await
        .with_context(|| format!("error fetching github pull requests at {:?}", url))?;

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    if response.status().is_client_error() {
        let text = String::from_utf8_lossy(body.as_slice());
        bail!(
            "status error {}, response: {text:?}",
            response.status().as_u16()
        );
    }

    Ok(serde_json::from_slice(&body)?)
}

fn base_url() -> Url {
    Url::parse("https://api.github.com/repos/").unwrap()
}

#[derive(Debug, Deserialize)]
pub struct GitHubPullRequestFile {
    pub filename: String,
    pub status: GitHubPullRequestFileStatus,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Deserialize, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum GitHubPullRequestFileStatus {
    Added,
    Removed,
    Modified,
    Renamed,
    Copied,
    Changed,
    Unchanged,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
pub struct GitHubPullRequestReviewComment {
    pub id: u64,
    pub body: String,
    pub path: String,
    pub user: GitHubPullRequestUser,
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

pub async fn fetch_pull_request_review_comments(
    client: Arc<dyn HttpClient>,
    remote: &ParsedGitRemote,
    pull_number: u32,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> anyhow::Result<Vec<GitHubPullRequestReviewComment>> {
    let ParsedGitRemote { owner, repo } = remote;
    let url = base_url()
        .join(&format!(
            "{owner}/{repo}/pulls/{pull_number}/comments?per_page=100"
        ))
        .expect("can't build pull request comments url")
        .to_string();

    let mut request = Request::get(&url)
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(github_token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", github_token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::default())?)
        .await
        .with_context(|| format!("error fetching pull request comments at {:?}", url))?;

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&body);
        bail!("status error {}: {text:?}", response.status().as_u16());
    }

    Ok(serde_json::from_slice(&body)?)
}

pub async fn create_pull_request_review_comment(
    client: Arc<dyn HttpClient>,
    remote: &ParsedGitRemote,
    pull_number: u32,
    commit_id: String,
    path: String,
    line: u32,
    side: &str,
    body: String,
    in_reply_to_id: Option<u64>,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> anyhow::Result<GitHubPullRequestReviewComment> {
    let ParsedGitRemote { owner, repo } = remote;
    let url = base_url()
        .join(&format!("{owner}/{repo}/pulls/{pull_number}/comments"))
        .expect("can't build pull request comment url")
        .to_string();

    let mut payload = serde_json::json!({
        "body": body,
        "commit_id": commit_id,
        "path": path,
        "line": line,
        "side": side,
    });
    if let Some(reply_id) = in_reply_to_id
        && let Some(map) = payload.as_object_mut()
    {
        map.insert("in_reply_to".into(), serde_json::json!(reply_id));
    }
    let payload_bytes = serde_json::to_vec(&payload)?;

    let mut request = Request::post(&url)
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(github_token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", github_token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::from(payload_bytes))?)
        .await
        .with_context(|| format!("error creating review comment at {:?}", url))?;

    let mut body_buf = Vec::new();
    response.body_mut().read_to_end(&mut body_buf).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&body_buf);
        bail!("status error {}: {text:?}", response.status().as_u16());
    }

    Ok(serde_json::from_slice(&body_buf)?)
}

pub const PULL_REQUEST_FILES_PER_PAGE: u32 = 100;

pub async fn fetch_pull_request_files_page(
    client: Arc<dyn HttpClient>,
    remote: &ParsedGitRemote,
    pull_number: u32,
    page: u32,
    per_page: u32,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> anyhow::Result<Vec<GitHubPullRequestFile>> {
    let ParsedGitRemote { owner, repo } = remote;
    let url = base_url()
        .join(&format!(
            "{owner}/{repo}/pulls/{pull_number}/files?per_page={per_page}&page={page}"
        ))
        .expect("can't build pull request files url")
        .to_string();

    let mut request = Request::get(&url)
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::default())?)
        .await
        .with_context(|| format!("error fetching pull request files at {:?}", url))?;

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    if response.status().is_client_error() {
        let text = String::from_utf8_lossy(body.as_slice());
        bail!(
            "status error {}, response: {text:?}",
            response.status().as_u16()
        );
    }

    Ok(serde_json::from_slice(&body)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullRequestReviewEvent {
    Approve,
    RequestChanges,
    Comment,
}

impl PullRequestReviewEvent {
    fn as_api_value(self) -> &'static str {
        match self {
            PullRequestReviewEvent::Approve => "APPROVE",
            PullRequestReviewEvent::RequestChanges => "REQUEST_CHANGES",
            PullRequestReviewEvent::Comment => "COMMENT",
        }
    }
}

pub async fn submit_pull_request_review(
    client: Arc<dyn HttpClient>,
    remote: &ParsedGitRemote,
    pull_number: u32,
    body: String,
    event: PullRequestReviewEvent,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> anyhow::Result<()> {
    let ParsedGitRemote { owner, repo } = remote;
    let url = base_url()
        .join(&format!("{owner}/{repo}/pulls/{pull_number}/reviews"))
        .expect("can't build review url")
        .to_string();

    let payload = serde_json::json!({
        "body": body,
        "event": event.as_api_value(),
    });
    let payload_bytes = serde_json::to_vec(&payload)?;

    let mut request = Request::post(&url)
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(github_token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", github_token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::from(payload_bytes))?)
        .await
        .with_context(|| format!("error submitting review at {url:?}"))?;

    let mut buf = Vec::new();
    response.body_mut().read_to_end(&mut buf).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&buf);
        bail!("status error {}: {text:?}", response.status().as_u16());
    }

    Ok(())
}

pub async fn fetch_pull_request_viewed_files(
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

    let mut request = Request::post("https://api.github.com/graphql")
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(github_token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", github_token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::from(body_bytes))?)
        .await
        .context("error fetching viewed files")?;

    let mut buf = Vec::new();
    response.body_mut().read_to_end(&mut buf).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&buf);
        bail!("status error {}: {text:?}", response.status().as_u16());
    }

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

pub async fn set_pull_request_file_viewed(
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

    let mut request = Request::post("https://api.github.com/graphql")
        .header("Content-Type", "application/json")
        .follow_redirects(http_client::RedirectPolicy::FollowAll);

    if let Some(github_token) = resolve_github_token(credentials_provider, cx).await {
        request = request.header("Authorization", format!("Bearer {}", github_token));
    } else {
        log::warn!("GITHUB_TOKEN is not set");
    }

    let mut response = client
        .send(request.body(AsyncBody::from(body_bytes))?)
        .await
        .with_context(|| format!("error marking file viewed: {file_path:?}"))?;

    let mut buf = Vec::new();
    response.body_mut().read_to_end(&mut buf).await?;

    if !response.status().is_success() {
        let text = String::from_utf8_lossy(&buf);
        bail!("status error {}: {text:?}", response.status().as_u16());
    }

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

const GITHUB_CREDENTIALS_URL: &str = "https://api.github.com";

/// Resolves a GitHub token using a layered fallback chain:
/// 1. `GITHUB_TOKEN` environment variable
/// 2. System keychain via `CredentialsProvider`
/// 3. `gh auth token` CLI command
pub async fn resolve_github_token(
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> Option<String> {
    //First env var
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Some(token);
        }
    }

    //system keychain
    if let Ok(Some((_username, token_bytes))) = credentials_provider
        .read_credentials(GITHUB_CREDENTIALS_URL, cx)
        .await
    {
        if let Ok(token) = String::from_utf8(token_bytes) {
            return Some(token);
        }
    }

    //gh cli

    if let Ok(output) = smol::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
    {
        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }

    None
}
