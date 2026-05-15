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
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub number: u32,
    pub title: String,
    pub base: GitHubPullRequestBase,
    pub user: GitHubPullRequestUser,
}

#[derive(Debug, Default, Deserialize)]
pub struct GitHubPullRequestBase {
    #[serde(rename = "ref")]
    pub ref_name: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct GitHubPullRequestUser {
    pub login: String,
}

pub async fn fetch_pull_requests(
    client: Arc<dyn HttpClient>,
    remote: &ParsedGitRemote,
    credentials_provider: Arc<dyn CredentialsProvider>,
    cx: &AsyncApp,
) -> anyhow::Result<Vec<GitHubPullRequest>> {
    let url: String = build_fetch_pull_requests_url(remote)
        .expect("can't build pull request url")
        .into();

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

fn build_fetch_pull_requests_url(remote: &ParsedGitRemote) -> Option<Url> {
    let ParsedGitRemote { owner, repo } = remote;

    base_url().join(&format!("{owner}/{repo}/pulls")).ok()
}

fn base_url() -> Url {
    Url::parse("https://api.github.com/repos/").unwrap()
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
