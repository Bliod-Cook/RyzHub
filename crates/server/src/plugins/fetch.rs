//! Requests to the open internet on a member's behalf: plugin packages, MCP servers, OAuth
//! servers. Only HTTPS to public addresses. The host is resolved here and the connection
//! pinned to the address that was checked, so DNS cannot point it elsewhere afterwards; every
//! redirect is checked the same way, and bodies are capped.
use anyhow::{Context, ensure};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::{Method, StatusCode, header::HeaderMap};
use std::{net::SocketAddr, time::Duration};
use url::Url;

use crate::{config::PluginsConfig, error::Code};

/// Redirects followed at most.
const MAX_REDIRECTS: usize = 3;

#[derive(Clone)]
pub struct Fetch {
    insecure: Vec<String>,
    timeout: Duration,
}

impl Fetch {
    pub fn new(config: &PluginsConfig) -> Self {
        Self {
            insecure: config.insecure_hosts.clone(),
            timeout: Duration::from_secs(config.fetch_timeout_secs.max(1)),
        }
    }

    /// The address to connect to for `url`, once it is known to be allowed.
    pub async fn check(&self, url: &Url) -> anyhow::Result<SocketAddr> {
        let host = url.host_str().context(Code::UnsafeDestination)?;
        let insecure = self.insecure.iter().any(|h| h == host);
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && insecure),
            Code::UnsafeDestination
        );
        ensure!(
            url.username().is_empty() && url.password().is_none(),
            Code::UnsafeDestination
        );
        let port = url
            .port_or_known_default()
            .context(Code::UnsafeDestination)?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let addresses: Vec<SocketAddr> = match bare.parse::<std::net::IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::time::timeout(self.timeout, tokio::net::lookup_host((bare, port)))
                .await
                .context(Code::PluginFetchFailed)?
                .context(Code::PluginFetchFailed)?
                .collect(),
        };
        ensure!(!addresses.is_empty(), Code::PluginFetchFailed);
        // Every address must be public: a name that also resolves privately is refused.
        ensure!(
            insecure
                || addresses
                    .iter()
                    .all(|a| ryzhub_execution::egress::public(a.ip())),
            Code::UnsafeDestination
        );
        Ok(addresses[0])
    }

    /// A client for `url` alone, connecting to the checked address.
    pub async fn client(&self, url: &Url) -> anyhow::Result<reqwest::Client> {
        let address = self.check(url).await?;
        let host = url.host_str().context(Code::UnsafeDestination)?;
        Ok(reqwest::Client::builder()
            .resolve(host, address)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.timeout)
            .timeout(self.timeout * 4)
            .user_agent(concat!("RyzHub/", env!("CARGO_PKG_VERSION")))
            .build()?)
    }

    /// Send a request and read at most `max` bytes of its successful answer. GETs follow
    /// redirects, each checked again; other methods only follow 307 and 308.
    pub async fn send(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        body: Option<Bytes>,
        max: usize,
    ) -> anyhow::Result<Bytes> {
        let mut url = Url::parse(url).context(Code::UnsafeDestination)?;
        for _ in 0..=MAX_REDIRECTS {
            let response = self
                .open(method.clone(), &url, headers.clone(), body.clone())
                .await?;
            let status = response.status();
            if status.is_redirection()
                && (method == Method::GET
                    || matches!(
                        status,
                        StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
                    ))
                && let Some(location) = response.headers().get("location")
            {
                url = url
                    .join(location.to_str().context(Code::PluginFetchFailed)?)
                    .context(Code::PluginFetchFailed)?;
                continue;
            }
            ensure!(status.is_success(), Code::PluginFetchFailed);
            return read(response, max).await;
        }
        anyhow::bail!(Code::PluginFetchFailed)
    }

    /// Send a request and return the answer unread, for streams. No redirects.
    pub async fn open(
        &self,
        method: Method,
        url: &Url,
        headers: HeaderMap,
        body: Option<Bytes>,
    ) -> anyhow::Result<reqwest::Response> {
        let client = self.client(url).await?;
        let mut request = client.request(method, url.clone()).headers(headers);
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send().await.context(Code::PluginFetchFailed)
    }
}

/// A body, up to `max` bytes.
pub async fn read(response: reqwest::Response, max: usize) -> anyhow::Result<Bytes> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context(Code::PluginFetchFailed)?;
        ensure!(body.len() + chunk.len() <= max, Code::PluginTooLarge);
        body.extend_from_slice(&chunk);
    }
    Ok(body.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(insecure: &[&str]) -> Fetch {
        Fetch {
            insecure: insecure.iter().map(|h| h.to_string()).collect(),
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn only_https_to_public_addresses() {
        let refused = |url: &str| {
            let url = Url::parse(url).unwrap();
            async move { fetch(&[]).check(&url).await.is_err() }
        };
        for url in [
            "http://93.184.216.34/plugin.zip",
            "https://127.0.0.1/",
            "https://10.0.0.8/",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/",
            "https://[fd00::1]/",
            "https://user:pass@93.184.216.34/",
            "https://localhost/",
            "ftp://93.184.216.34/",
        ] {
            assert!(refused(url).await, "{url}");
        }
        assert!(
            fetch(&[])
                .check(&Url::parse("https://93.184.216.34/x").unwrap())
                .await
                .is_ok()
        );
        // Hosts named for development may be plain HTTP and private.
        let local = fetch(&["127.0.0.1"]);
        assert!(
            local
                .check(&Url::parse("http://127.0.0.1:9/x").unwrap())
                .await
                .is_ok()
        );
    }
}
