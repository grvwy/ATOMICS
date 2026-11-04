//! Ticket reads over a REST API: the request behind an operation mapped with `http`. Nothing here
//! knows any particular system or follows anything a server says about where to go next.
//!
//! A request's address is `[tickets.http] url` plus the operation's path, with values encoded into
//! their segments, and it is checked again right before each send. Redirects are refused, and a
//! token goes only in the `Authorization` header to the origin it belongs to.

use std::time::Duration;

use atomics_core::tickets::{Cleaner, HttpCall, HttpMethod, HttpRequest, McpServer};
use serde_json::{Map, Value, json};
use url::Url;

use super::McpError;
use super::oauth::{OAuth, Session, agent, parse_secure};

/// The largest answer read. A page of tickets is far smaller; more than this is not a ticket API.
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
/// How much of a refusal is read. Its code is near the start, and only 300 characters show.
const MAX_REFUSAL_BYTES: u64 = 64 * 1024;
const MAX_REFUSAL_CHARS: usize = 300;

/// The address a client reaches, by the rules every request is held to: https, or http to this
/// machine, with no user info, query or fragment. Refusals never repeat the address, which may hold
/// a credential or a value.
pub fn http_base(url: &str) -> Result<Url, McpError> {
    let mut base = secure(url)?;
    if base.query().is_some() || base.fragment().is_some() {
        return Err(McpError::Server("[tickets.http] url must hold no query or fragment".into()));
    }
    let path = base.path().trim_end_matches('/').to_string();
    base.set_path(&path);
    Ok(base)
}

fn secure(url: &str) -> Result<Url, McpError> {
    let url = parse_secure(url)
        .map_err(|_| McpError::Server("[tickets.http] url must be https, or http to this machine".into()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpError::Server("[tickets.http] url must hold no user name or password".into()));
    }
    Ok(url)
}

/// Whether two addresses have one origin. An address that does not parse has none.
fn same_origin(one: &str, other: &str) -> bool {
    match (Url::parse(one.trim()), Url::parse(other.trim())) {
        (Ok(one), Ok(other)) => one.origin().is_tuple() && one.origin() == other.origin(),
        _ => false,
    }
}

/// What `http` operations may authenticate with of `server`'s `token_env` value: nothing, unless
/// the API has the origin of `server.url`, which the token was issued for.
pub fn shared_token(server: &McpServer, api: &McpServer, token: Option<String>) -> Option<String> {
    let (own, api) = (server.url.as_deref()?, api.url.as_deref()?);
    token.filter(|_| server.bearer_env().is_some() && same_origin(own, api))
}

/// Base plus path and query, and nothing else: anything that parses to another origin, another path
/// or a fragment is refused.
fn request_url(base: &Url, request: &HttpRequest) -> Result<Url, McpError> {
    let leaves = || McpError::Server("the request would leave [tickets.http] url; it was not sent".into());
    if !request.path.starts_with('/') {
        return Err(leaves());
    }
    let path = format!("{}{}", base.path().trim_end_matches('/'), request.path);
    let mut url = secure(&format!("{}{path}", base.origin().ascii_serialization()))?;
    if !request.query.is_empty() {
        url.query_pairs_mut().extend_pairs(&request.query);
    }
    if url.origin() != base.origin() || url.path() != path || url.fragment().is_some() {
        return Err(leaves());
    }
    Ok(url)
}

/// The REST API `http` operations call. Holding one sends nothing: each `call` is one request.
pub struct HttpClient {
    agent: ureq::Agent,
    base: Url,
    /// `token_env`'s value, when it belongs to this origin (`shared_token`).
    token: Option<String>,
    /// This resource's token from the server's browser sign-in. It replaces `token`.
    oauth: Option<Box<Session>>,
}

impl HttpClient {
    /// `api` is the resource from `TicketConfig::http_server`, and `sign_in` the keychain and the server
    /// whose sign-in covers it. A sign-in that does not cover it is `SignInRequired`.
    pub fn connect(
        api: &McpServer,
        token: Option<String>,
        timeout: Duration,
        sign_in: Option<(&OAuth, &McpServer)>,
    ) -> Result<HttpClient, McpError> {
        let base = http_base(api.url.as_deref().unwrap_or_default())?;
        let oauth = if api.uses_oauth() {
            let (oauth, server) = sign_in.ok_or_else(|| McpError::transport("OAuth sign-in is unavailable".into()))?;
            Some(Box::new(oauth.session_for(server, base.as_str(), timeout)?))
        } else {
            None
        };
        Ok(HttpClient { agent: agent(timeout), base, token, oauth })
    }

    /// The OAuth access token in use, if any. It changes on refresh.
    pub fn oauth_token(&self) -> Option<&str> {
        self.oauth.as_ref().map(|session| session.access_token())
    }

    /// Whether this client's token comes from `server`'s sign-in, or from no sign-in at all.
    pub fn signs_in_with(&self, server: &McpServer) -> bool {
        self.oauth.as_ref().is_none_or(|session| session.belongs_to(server))
    }

    /// `cleaner` also covering what this client authenticates with right now.
    fn cleaner(&self, cleaner: &Cleaner) -> Cleaner {
        cleaner.clone().with(self.token.clone()).with(self.oauth_token().map(str::to_string))
    }

    /// Makes one request and returns its JSON answer. `label` names the operation in errors, which
    /// never carry the address with its values. A request holding this client's token or one of
    /// `tokens`, those of the job's other resources, in its address or body is not sent: `cleaner`
    /// also knows values that only look secret by name, which a person may well search for. A
    /// refusal's words are cleaned before they are cut, since a cut credential could not be found.
    pub fn call(
        &mut self,
        call: &HttpCall,
        label: &str,
        request: &HttpRequest,
        tokens: &Cleaner,
        cleaner: &Cleaner,
    ) -> Result<Value, McpError> {
        let url = request_url(&self.base, request)?;
        let body = request.body.as_ref().map(Value::to_string);
        if let Some(oauth) = &mut self.oauth {
            oauth.refresh_if_due()?;
        }
        let mut retried = false;
        let mut response = loop {
            let tokens = self.cleaner(tokens);
            if tokens.holds_secret(url.as_str()) || body.as_deref().is_some_and(|body| tokens.holds_secret(body)) {
                return Err(McpError::Server(format!("{label}: the request holds a credential; it was not sent")));
            }
            let response = self
                .send(call.method, &url, body.as_deref())
                .map_err(|err| McpError::request_error(format!("{label} failed"), err))?;
            // The server no longer accepts the token (revoked or expired early): refresh once and
            // retry. A refresh that fails is SignInRequired, so this cannot loop.
            if response.status().as_u16() == 401
                && !retried
                && let Some(oauth) = &mut self.oauth
            {
                retried = true;
                oauth.refresh()?;
                continue;
            }
            break response;
        };
        let status = response.status().as_u16();
        if status == 401
            && let Some(oauth) = &mut self.oauth
        {
            return Err(oauth.refused_by_resource());
        }
        if !(200..300).contains(&status) {
            return Err(self.refusal(label, status, &mut response, &self.cleaner(cleaner)));
        }
        if let Some(oauth) = &mut self.oauth {
            oauth.accepted_by_resource();
        }
        let headers: Map<String, Value> = call
            .response_headers
            .iter()
            .filter_map(|(name, header)| {
                let value = response.headers().get(header.as_str())?.to_str().ok()?;
                Some((name.clone(), Value::from(value)))
            })
            .collect();
        let bytes = response.body_mut().with_config().limit(MAX_BODY_BYTES).read_to_vec().map_err(|err| match err {
            ureq::Error::BodyExceedsLimit(_) => McpError::transport(format!(
                "{label}: the answer is larger than {} MiB",
                MAX_BODY_BYTES / (1024 * 1024)
            )),
            err => McpError::request_error(format!("reading the answer to {label} failed"), err),
        })?;
        let body: Value = serde_json::from_slice(&bytes)
            .map_err(|_| McpError::Server(format!("{label} did not answer with JSON")))?;
        Ok(if call.response_headers.is_empty() { body } else { json!({"body": body, "headers": headers}) })
    }

    fn send(
        &self,
        method: HttpMethod,
        url: &Url,
        body: Option<&str>,
    ) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
        let bearer = self.oauth_token().or(self.token.as_deref()).map(|token| format!("Bearer {token}"));
        match method {
            HttpMethod::Get => {
                let mut request = self.agent.get(url.as_str()).header("Accept", "application/json");
                if let Some(bearer) = bearer {
                    request = request.header("Authorization", bearer);
                }
                request.call()
            }
            HttpMethod::Post => {
                let mut request = self
                    .agent
                    .post(url.as_str())
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                if let Some(bearer) = bearer {
                    request = request.header("Authorization", bearer);
                }
                request.send(body.unwrap_or("{}"))
            }
        }
    }

    /// Why a non-2xx answer is an error. Only a lost connection, a timeout, a 429 or a 5xx says the
    /// API is down: a 404 is one missing ticket.
    fn refusal(
        &self,
        label: &str,
        status: u16,
        response: &mut ureq::http::Response<ureq::Body>,
        cleaner: &Cleaner,
    ) -> McpError {
        if (300..400).contains(&status) {
            return McpError::Server(format!("{label} answered a redirect (HTTP {status}), which is not followed"));
        }
        let challenged = response
            .headers()
            .get_all("www-authenticate")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .any(|value| value.to_ascii_lowercase().contains("resource_metadata"));
        if status == 401 && self.token.is_none() && challenged {
            return McpError::SignInRequired(format!(
                "{} asks for a sign-in: set oauth = true on the server and sign in",
                self.base
            ));
        }
        let text = response.body_mut().with_config().limit(MAX_REFUSAL_BYTES).read_to_string().unwrap_or_default();
        let shown: String = cleaner.error(&text).trim().chars().take(MAX_REFUSAL_CHARS).collect();
        let hint = if matches!(status, 401 | 403) { " (check server.token_env)" } else { "" };
        let separator = if shown.is_empty() { "" } else { ": " };
        let message = format!("{label} answered HTTP {status}{hint}{separator}{shown}");
        if status == 429 || (500..600).contains(&status) {
            McpError::http(status, message)
        } else {
            McpError::Tool { message, details: cleaner.text(&text) }
        }
    }
}

#[cfg(test)]
mod tests;
