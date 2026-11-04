//! A fake authorization server, a fake MCP HTTP server and a fake REST API, all in-process, and a
//! "browser" that is an HTTP GET to the authorization URL followed by a GET to the redirect it answers
//! with. The authorization server keeps a real server's rules for a grant of several resources: every
//! token names one audience, a refused resource spends nothing, and a rotated refresh token used twice
//! ends the whole grant.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::*;
use crate::mcp_client::McpClient;

struct Req {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn query(&self) -> HashMap<String, String> {
        let query = self.path.split_once('?').map(|(_, q)| q).unwrap_or_default();
        url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
    }

    /// Every value of a query parameter that may repeat.
    fn query_all(&self, name: &str) -> Vec<String> {
        let query = self.path.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let pairs = url::form_urlencoded::parse(query.as_bytes()).into_owned();
        pairs.filter(|(key, _)| key == name).map(|(_, value)| value).collect()
    }

    fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes()).into_owned().collect()
    }
}

struct Resp(u16, Vec<(&'static str, String)>, String);

fn json_resp(status: u16, value: Value) -> Resp {
    Resp(status, vec![("Content-Type", "application/json".into())], value.to_string())
}

fn bind() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

fn serve(listener: TcpListener, handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) {
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let handler = handler.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut parts = first.split_whitespace();
                let (method, path) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
                let (mut headers, mut length) = (Vec::new(), 0);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        let (k, v) = (k.to_ascii_lowercase(), v.trim().to_string());
                        if k == "content-length" {
                            length = v.parse().unwrap_or(0);
                        }
                        headers.push((k, v));
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = Req { method, path, headers, body: String::from_utf8(body).unwrap() };
                let Resp(status, headers, body) = handler(&request);
                let mut out = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                for (k, v) in headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                let _ = stream.write_all(format!("{out}\r\n{body}").as_bytes());
            });
        }
    });
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) registrations: Vec<Value>,
    pub(crate) clients: HashMap<String, String>,
    pub(crate) codes: HashMap<String, (String, String, String)>,
    pub(crate) access: HashSet<String>,
    pub(crate) refresh: HashSet<String>,
    pub(crate) counter: u32,
    pub(crate) expires_in: u64,
    pub(crate) authorize_urls: Vec<String>,
    pub(crate) token_requests: Vec<HashMap<String, String>>,
    pub(crate) list_auth: Vec<String>,
    /// The bearer each `tools/call` came with, in order.
    pub(crate) call_auth: Vec<String>,
    pub(crate) state_override: Option<String>,
    pub(crate) iss_override: Option<String>,
    pub(crate) metadata_issuer: Option<String>,
    pub(crate) oidc_only: bool,
    pub(crate) no_challenge: bool,
    pub(crate) revoked: Vec<HashMap<String, String>>,
    pub(crate) revoke_status: Option<u16>,
    /// How long the token endpoint takes to answer.
    pub(crate) token_delay: Duration,
    /// The resource refuses every token, even one just issued.
    pub(crate) refuse_access: bool,
    /// The MCP server answers 403 to its own valid tokens, as to a person without the right.
    pub(crate) mcp_forbids: bool,
    /// What each code and refresh token's grant holds.
    pub(crate) code_resources: HashMap<String, Vec<String>>,
    pub(crate) grant_resources: HashMap<String, Vec<String>>,
    /// The one audience each access token names.
    pub(crate) audience: HashMap<String, String>,
    /// Refresh tokens already rotated away, and how often one came back.
    pub(crate) spent: HashSet<String>,
    pub(crate) reused: u32,
    /// The token endpoint refuses the API: the grant does not hold it, or no longer may.
    pub(crate) refuse_api_tokens: bool,
    /// The API answers 401 to its own valid tokens, as an endpoint not open to them does.
    pub(crate) api_refuses: bool,
    /// The API answers 403 to its own valid tokens, as to a person without the right.
    pub(crate) api_forbids: bool,
    /// What the API's protected-resource metadata says, when not the truth.
    pub(crate) api_metadata_issuer: Option<String>,
    pub(crate) api_metadata_resource: Option<String>,
    /// What the authorization server's metadata names as its token endpoint, when not its own.
    pub(crate) token_endpoint_override: Option<String>,
    /// Every request the API got, metadata included, and the bearers it was shown.
    pub(crate) api_requests: Vec<String>,
    pub(crate) api_auth: Vec<String>,
    /// Every bearer the MCP server was shown, on any call.
    pub(crate) mcp_auth: Vec<String>,
    pub(crate) mcp_bodies: Vec<String>,
}

pub(crate) struct Fake {
    as_url: String,
    pub(crate) mcp_url: String,
    pub(crate) api_url: String,
    pub(crate) state: Mutex<State>,
    tickets: Option<Mutex<McpClient>>,
}

impl Fake {
    pub(crate) fn start() -> Arc<Fake> {
        Self::start_with(None)
    }

    /// Protect the real stdio ticket fixture with this server's OAuth flow. The demo must use
    /// the sign-in's bearer for its searches and reads, rather than switching servers after login.
    pub(crate) fn start_with_tickets(client: McpClient) -> Arc<Fake> {
        Self::start_with(Some(Mutex::new(client)))
    }

    fn start_with(tickets: Option<Mutex<McpClient>>) -> Arc<Fake> {
        let ((as_listener, as_url), (mcp_listener, mcp_base), (api_listener, api_base)) = (bind(), bind(), bind());
        let fake = Arc::new(Fake {
            as_url,
            mcp_url: format!("{mcp_base}/mcp"),
            api_url: format!("{api_base}/api"),
            state: Mutex::new(State { expires_in: 3600, ..State::default() }),
            tickets,
        });
        let a = fake.clone();
        serve(as_listener, move |r| a.authorization_server(r));
        let m = fake.clone();
        serve(mcp_listener, move |r| m.mcp(r));
        let a = fake.clone();
        serve(api_listener, move |r| a.serve_api(r));
        fake
    }

    pub(crate) fn server(&self) -> McpServer {
        McpServer { url: Some(self.mcp_url.clone()), oauth: true, ..McpServer::default() }
    }

    /// The server as a profile with the API sees it: one sign-in for both.
    pub(crate) fn server_with_api(&self) -> McpServer {
        McpServer { resources: vec![self.api_url.clone()], ..self.server() }
    }

    /// The API as `TicketConfig::http_server` names it.
    pub(crate) fn api(&self) -> McpServer {
        McpServer { url: Some(self.api_url.clone()), oauth: true, ..McpServer::default() }
    }

    fn known(&self, resource: &str) -> bool {
        resource == self.mcp_url || resource == self.api_url
    }

    fn authorization_server(&self, r: &Req) -> Resp {
        if r.path == "/token" {
            let delay = self.state.lock().unwrap().token_delay;
            std::thread::sleep(delay);
        }
        let mut st = self.state.lock().unwrap();
        let path = r.path.split('?').next().unwrap();
        match (r.method.as_str(), path) {
            ("GET", "/.well-known/oauth-authorization-server") | ("GET", "/.well-known/openid-configuration") => {
                if st.oidc_only && path.contains("oauth-authorization-server") {
                    return Resp(404, vec![], String::new());
                }
                json_resp(
                    200,
                    json!({
                        "issuer": st.metadata_issuer.clone().unwrap_or(self.as_url.clone()),
                        "authorization_endpoint": format!("{}/authorize", self.as_url),
                        "token_endpoint": st.token_endpoint_override.clone().unwrap_or(format!("{}/token", self.as_url)),
                        "registration_endpoint": format!("{}/register", self.as_url),
                        "revocation_endpoint": format!("{}/revoke", self.as_url),
                        "code_challenge_methods_supported": ["S256"],
                        "authorization_response_iss_parameter_supported": true,
                    }),
                )
            }
            ("POST", "/register") => {
                let body: Value = serde_json::from_str(&r.body).unwrap();
                let redirect = body["redirect_uris"][0].as_str().unwrap().to_string();
                assert_eq!(body["token_endpoint_auth_method"], "none");
                assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"), "{redirect}");
                let id = format!("client-{}", st.clients.len() + 1);
                st.clients.insert(id.clone(), redirect);
                st.registrations.push(body);
                json_resp(201, json!({ "client_id": id }))
            }
            ("GET", "/authorize") => {
                let q = r.query();
                let resources = r.query_all("resource");
                let ok = q["response_type"] == "code"
                    && q["code_challenge_method"] == "S256"
                    && resources.first() == Some(&self.mcp_url)
                    && resources.iter().all(|resource| self.known(resource))
                    && st.clients.get(&q["client_id"]) == Some(&q["redirect_uri"]);
                if !ok {
                    return Resp(400, vec![], "bad authorization request".into());
                }
                st.authorize_urls.push(r.path.clone());
                // A scope must belong to every resource named: one for the MCP server alone does not.
                if resources.len() > 1 && q.contains_key("scope") {
                    let mut target = Url::parse(&q["redirect_uri"]).unwrap();
                    target
                        .query_pairs_mut()
                        .append_pair("error", "invalid_scope")
                        .append_pair("state", &q["state"])
                        .append_pair("iss", &self.as_url);
                    return Resp(302, vec![("Location", target.to_string())], String::new());
                }
                st.counter += 1;
                let code = format!("code-{}", st.counter);
                st.code_resources.insert(code.clone(), resources);
                st.codes.insert(
                    code.clone(),
                    (q["code_challenge"].clone(), q["client_id"].clone(), q["redirect_uri"].clone()),
                );
                let state = st.state_override.clone().unwrap_or(q["state"].clone());
                let iss = st.iss_override.clone().unwrap_or(self.as_url.clone());
                let mut target = Url::parse(&q["redirect_uri"]).unwrap();
                target
                    .query_pairs_mut()
                    .append_pair("code", &code)
                    .append_pair("state", &state)
                    .append_pair("iss", &iss);
                Resp(302, vec![("Location", target.to_string())], String::new())
            }
            ("POST", "/revoke") => {
                let form = r.form();
                st.access.remove(&form["token"]);
                st.refresh.remove(&form["token"]);
                st.revoked.push(form);
                Resp(st.revoke_status.unwrap_or(200), vec![], String::new())
            }
            ("POST", "/token") => {
                let form = r.form();
                st.token_requests.push(form.clone());
                let invalid = |error: &str| json_resp(400, json!({ "error": error }));
                let Some(resource) = form.get("resource").filter(|resource| self.known(resource)).cloned() else {
                    return invalid("invalid_target");
                };
                // What the grant holds; a resource it does not hold costs nothing.
                let held = match form["grant_type"].as_str() {
                    "authorization_code" => st.code_resources.get(&form["code"]).cloned(),
                    _ => st.grant_resources.get(&form["refresh_token"]).cloned(),
                };
                let refused = resource == self.api_url && st.refuse_api_tokens;
                if refused || held.as_ref().is_some_and(|held| !held.contains(&resource)) {
                    return invalid("invalid_target");
                }
                let resources = match form["grant_type"].as_str() {
                    "authorization_code" => {
                        let Some((challenge, client, redirect)) = st.codes.remove(&form["code"]) else {
                            return invalid("invalid_grant");
                        };
                        let proven = super::challenge(&form["code_verifier"]);
                        if proven != challenge || form["client_id"] != client || form["redirect_uri"] != redirect {
                            return invalid("invalid_grant");
                        }
                        st.code_resources.remove(&form["code"]).unwrap_or_default()
                    }
                    "refresh_token" => {
                        // Rotation: a refresh token works once, and using it twice ends the grant.
                        if st.spent.contains(&form["refresh_token"]) {
                            st.reused += 1;
                            st.access.clear();
                            st.refresh.clear();
                            return invalid("invalid_grant");
                        }
                        if !st.refresh.remove(&form["refresh_token"]) {
                            return invalid("invalid_grant");
                        }
                        st.spent.insert(form["refresh_token"].clone());
                        st.grant_resources.remove(&form["refresh_token"]).unwrap_or_default()
                    }
                    _ => return invalid("unsupported_grant_type"),
                };
                st.counter += 1;
                let (access, refresh) =
                    (format!("access-token-{}", st.counter), format!("refresh-token-{}", st.counter));
                st.access.insert(access.clone());
                st.audience.insert(access.clone(), resource);
                st.refresh.insert(refresh.clone());
                st.grant_resources.insert(refresh.clone(), resources);
                json_resp(
                    200,
                    json!({ "access_token": access, "token_type": "Bearer", "refresh_token": refresh, "expires_in": st.expires_in }),
                )
            }
            _ => Resp(404, vec![], String::new()),
        }
    }

    fn mcp(&self, r: &Req) -> Resp {
        let mut st = self.state.lock().unwrap();
        if r.method == "GET" {
            return if r.path == "/.well-known/oauth-protected-resource/mcp" {
                json_resp(200, json!({ "resource": self.mcp_url, "authorization_servers": [self.as_url] }))
            } else {
                Resp(404, vec![], String::new())
            };
        }
        let token = r.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
        // The discovery probe carries no token.
        if !token.is_empty() {
            st.mcp_auth.push(token.to_string());
        }
        st.mcp_bodies.push(r.body.clone());
        let for_this_server = st.audience.get(token).is_some_and(|audience| *audience == self.mcp_url);
        if !st.access.contains(token) || !for_this_server || st.refuse_access {
            let mut headers = vec![];
            if !st.no_challenge {
                let metadata = self.mcp_url.replace("/mcp", "/.well-known/oauth-protected-resource/mcp");
                headers.push((
                    "WWW-Authenticate",
                    format!("Bearer error=\"invalid_token\", resource_metadata=\"{metadata}\", scope=\"tickets.read\""),
                ));
            }
            return Resp(401, headers, "unauthorized".into());
        }
        if st.mcp_forbids {
            return Resp(403, vec![], "forbidden".into());
        }
        let body: Value = serde_json::from_str(&r.body).unwrap();
        if let Some(tickets) = &self.tickets {
            // Authentication above applies to every forwarded request. Serialize calls because the
            // fixture records them in one state file, just as the ticket worker serializes jobs.
            drop(st);
            let mut client = tickets.lock().unwrap();
            let result = match body["method"].as_str().unwrap() {
                "tools/list" => client.request("tools/list", body["params"].clone()),
                "tools/call" => client.request("tools/call", body["params"].clone()),
                "initialize" => Ok(json!({})),
                _ => return Resp(202, vec![], String::new()),
            };
            return match result {
                Ok(result) => json_resp(200, json!({"jsonrpc":"2.0", "id":body["id"], "result":result})),
                Err(error) => json_resp(
                    200,
                    json!({"jsonrpc":"2.0", "id":body["id"],
                    "error":{"code":-32603, "message":error.to_string()}}),
                ),
            };
        }
        match body["method"].as_str().unwrap() {
            "initialize" => json_resp(200, json!({ "jsonrpc": "2.0", "id": body["id"], "result": {} })),
            "tools/list" => {
                st.list_auth.push(r.header("authorization").unwrap().to_string());
                json_resp(
                    200,
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "tools": [{ "name": "search" }] } }),
                )
            }
            "tools/call" => {
                st.call_auth.push(token.to_string());
                // A `whoami` tool answers who signed in, so a profile that maps it reads an identity.
                let text = if body["params"]["name"] == "whoami" {
                    json!({ "userId": "agent-1", "kind": "agent", "displayName": "Example agent" }).to_string()
                } else {
                    "{}".to_string()
                };
                json_resp(
                    200,
                    json!({ "jsonrpc": "2.0", "id": body["id"], "result": { "content": [{ "type": "text", "text": text }] } }),
                )
            }
            _ => Resp(202, vec![], String::new()),
        }
    }
}

impl Fake {
    /// The REST API: its metadata by GET, and one ticket for a token naming it as the audience.
    fn serve_api(&self, r: &Req) -> Resp {
        let mut st = self.state.lock().unwrap();
        st.api_requests.push(format!("{} {}", r.method, r.path));
        if r.path.starts_with("/.well-known/") {
            if r.method != "GET" || r.path != "/.well-known/oauth-protected-resource/api" {
                return Resp(404, vec![], String::new());
            }
            let resource = st.api_metadata_resource.clone().unwrap_or(self.api_url.clone());
            let issuer = st.api_metadata_issuer.clone().unwrap_or(self.as_url.clone());
            return json_resp(200, json!({ "resource": resource, "authorization_servers": [issuer] }));
        }
        let token = r.header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default().to_string();
        st.api_auth.push(token.clone());
        let for_this_api = st.audience.get(&token).is_some_and(|audience| *audience == self.api_url);
        if !st.access.contains(&token) || !for_this_api || st.api_refuses {
            // A server's words for where to go next are never followed.
            let challenge = "Bearer resource_metadata=\"http://127.0.0.1:1/.well-known/oauth-protected-resource\"";
            return Resp(401, vec![("WWW-Authenticate", challenge.into())], "unauthorized".into());
        }
        if st.api_forbids {
            return Resp(403, vec![], "forbidden".into());
        }
        json_resp(200, json!({ "id": "DEMO-1", "key": "DEMO-1", "title": "Read over the API", "state": "open" }))
    }
}

/// The browser: follow the authorization URL's redirect to the loopback callback.
pub(crate) fn browse(url: &str) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).max_redirects(0).build().into();
    let response = agent.get(url).call().unwrap();
    if let Some(location) = response.headers().get("location").and_then(|v| v.to_str().ok()) {
        let _ = agent.get(location).call();
    }
}

pub(crate) fn open_in_browser(url: &str) -> anyhow::Result<()> {
    let url = url.to_string();
    std::thread::spawn(move || browse(&url));
    Ok(())
}

pub(crate) fn machine() -> (crate::test_support::TempDir, OAuth) {
    let (dir, keychain, _) = crate::credentials::tests::machine(false);
    let oauth = OAuth::new(keychain, Browser::at("unused-browser")).with_callback_timeout(Duration::from_secs(10));
    (dir, oauth)
}

fn sign_in(oauth: &OAuth, server: &McpServer) -> Result<(), McpError> {
    oauth.sign_in_via(server, Duration::from_secs(10), &open_in_browser)
}

fn items(dir: &std::path::Path, kind: &str) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&format!("keychain-oauth-{kind}-")))
        .count()
}

fn stored_refresh_token(oauth: &OAuth, server: &McpServer) -> String {
    stored(oauth, server).refresh_token.unwrap()
}

fn stored(oauth: &OAuth, server: &McpServer) -> Tokens {
    let key = server_key(server).unwrap();
    oauth.read_json::<Tokens>(&slot("tokens", &key)).unwrap().unwrap()
}

const TIMEOUT: Duration = Duration::from_secs(5);

fn refresh_requests(fake: &Fake) -> Vec<HashMap<String, String>> {
    let st = fake.state.lock().unwrap();
    st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").cloned().collect()
}

/// `GET /items/DEMO-1`, as the worker makes it.
fn read_over_the_api(client: &mut crate::mcp_client::HttpClient) -> Result<Value, McpError> {
    use atomics_core::tickets::{Cleaner, HttpCall, HttpMethod, HttpRequest};
    let call = HttpCall {
        method: HttpMethod::Get,
        path: String::new(),
        query: Vec::new(),
        response_headers: Default::default(),
    };
    let request = HttpRequest { path: "/items/DEMO-1".into(), query: Vec::new(), body: None };
    client.call(&call, "GET /items/{id}", &request, &Cleaner::default(), &Cleaner::default())
}

fn api_client(fake: &Fake, oauth: &OAuth, server: &McpServer) -> Result<crate::mcp_client::HttpClient, McpError> {
    crate::mcp_client::HttpClient::connect(&fake.api(), None, TIMEOUT, Some((oauth, server)))
}

#[test]
fn pkce_challenge_is_rfc_7636_s256() {
    assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let (a, b) = (random_token(32).unwrap(), random_token(32).unwrap());
    assert_ne!(a, b);
    assert_eq!(a.len(), 43);
}

#[test]
fn challenges_are_parsed_for_their_parameters() {
    let header = r#"Bearer realm="x", error="invalid_token", resource_metadata="https://a.example/.well-known/oauth-protected-resource", scope="a b""#;
    assert_eq!(
        auth_param(header, "resource_metadata").unwrap(),
        "https://a.example/.well-known/oauth-protected-resource"
    );
    assert_eq!(auth_param(header, "scope").unwrap(), "a b");
    assert_eq!(auth_param("bearer resource_metadata=https://x/y", "resource_metadata").unwrap(), "https://x/y");
    assert_eq!(auth_param("Basic realm=\"x\"", "realm"), None);
    assert_eq!(auth_param("Bearer realm=\"x\"", "resource_metadata"), None);
}

#[test]
fn signing_in_discovers_registers_proves_pkce_and_uses_the_bearer() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server();
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    // A connection never signs in by itself.
    let unsigned = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(&unsigned, McpError::SignInRequired(_)), "{unsigned:?}");
    assert!(is_sign_in_required(&unsigned.to_string()));
    assert!(!is_sign_in_refused(&unsigned.to_string()), "nothing was saved to refuse: {unsigned}");

    sign_in(&oauth, &server).unwrap();
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    {
        let st = fake.state.lock().unwrap();
        // Registration: one public client with a loopback redirect.
        assert_eq!(st.registrations.len(), 1);
        assert_eq!(st.registrations[0]["grant_types"], json!(["authorization_code", "refresh_token"]));
        // The authorization request: S256, state, resource, and the scope the 401 named.
        let url = Url::parse(&format!("http://x{}", st.authorize_urls[0])).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["resource"], fake.mcp_url);
        assert_eq!(q["scope"], "tickets.read");
        assert!(q["state"].len() >= 43);
        // The token endpoint verified the verifier against the challenge (it refuses otherwise).
        assert_eq!(st.token_requests[0]["grant_type"], "authorization_code");
        assert_eq!(st.token_requests[0]["resource"], fake.mcp_url);
        assert_eq!(st.token_requests[0]["code_verifier"].len(), 43);
    }

    let mut client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
    assert_eq!(client.tool_names().unwrap(), ["search"]);
    let token = client.oauth_token().unwrap().to_string();
    assert_eq!(fake.state.lock().unwrap().list_auth, [format!("Bearer {token}")]);

    // Tokens are in the keychain only: hex-encoded items, nothing readable in the tool's arguments.
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access"), items(dir.path(), "client")), (1, 1, 1));
    let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
    assert!(!argv.contains(&token));
    for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("keychain-oauth-") {
            assert!(!std::fs::read_to_string(entry.path()).unwrap().contains(&token));
        }
    }

    // Signing out removes the items; the registered client stays for the next sign-in.
    oauth.sign_out(&server).unwrap();
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access"), items(dir.path(), "client")), (0, 0, 1));
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    oauth.sign_out(&server).unwrap();
    assert!(McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).is_err());
    sign_in(&oauth, &server).unwrap();
    assert_eq!(fake.state.lock().unwrap().registrations.len(), 1, "the client_id was cached");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
}

#[test]
fn discovery_falls_back_to_well_known_urls_and_openid_configuration() {
    let fake = Fake::start();
    {
        let mut st = fake.state.lock().unwrap();
        st.no_challenge = true;
        st.oidc_only = true;
    }
    let (_dir, oauth) = machine();
    sign_in(&oauth, &fake.server()).unwrap();
    assert_eq!(oauth.status(&fake.server()), SignInStatus::SignedIn);
}

#[test]
fn an_unconfigured_server_that_asks_for_a_sign_in_says_so() {
    let fake = Fake::start();
    let plain = McpServer { oauth: false, ..fake.server() };
    let error = McpClient::connect(&plain, Duration::from_secs(5), |_| None).err().unwrap();
    assert!(matches!(&error, McpError::SignInRequired(m) if m.contains("oauth = true")), "{error:?}");
}

#[test]
fn a_token_near_expiry_refreshes_with_rotation_and_a_401_refreshes_once() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    // The first token expires inside the refresh skew.
    fake.state.lock().unwrap().expires_in = 30;
    sign_in(&oauth, &server).unwrap();
    let first = stored_refresh_token(&oauth, &server);
    fake.state.lock().unwrap().expires_in = 3600;

    let mut client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
    let second = stored_refresh_token(&oauth, &server);
    assert_ne!(first, second, "the rotated refresh token replaced the old one");
    {
        let st = fake.state.lock().unwrap();
        assert!(!st.refresh.contains(&first), "the old refresh token is spent");
        let refreshes: Vec<_> = st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").collect();
        assert_eq!(refreshes.len(), 1);
        assert_eq!(refreshes[0]["refresh_token"], first);
        assert_eq!(refreshes[0]["resource"], fake.mcp_url);
    }

    // The server stops accepting the access token: one refresh, then the call goes through.
    fake.state.lock().unwrap().access.clear();
    assert_eq!(client.tool_names().unwrap(), ["search"]);
    assert_ne!(stored_refresh_token(&oauth, &server), second);
    let st = fake.state.lock().unwrap();
    assert_eq!(st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").count(), 2);
    assert_eq!(st.list_auth, [format!("Bearer {}", client.oauth_token().unwrap())]);
}

#[test]
fn a_rest_read_refreshes_once_on_a_401_and_then_says_what_to_do() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();

    // The first read gets the API's token from the refresh token.
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    assert_eq!(read_over_the_api(&mut client).unwrap()["id"], "DEMO-1");
    assert_eq!(refresh_requests(&fake).len(), 1);

    // The API stops accepting the token: one refresh, and the same request goes through.
    let first = client.oauth_token().unwrap().to_string();
    fake.state.lock().unwrap().access.clear();
    assert_eq!(read_over_the_api(&mut client).unwrap()["id"], "DEMO-1");
    assert_eq!(refresh_requests(&fake).len(), 2);
    assert_ne!(client.oauth_token().unwrap(), first);

    // A refresh that does not help: the next 401 asks for a person, with no second refresh.
    fake.state.lock().unwrap().api_refuses = true;
    let error = read_over_the_api(&mut client).unwrap_err();
    assert!(matches!(error, McpError::SignInRequired(_)), "{error:?}");
    assert!(is_sign_in_required(&error.to_string()), "{error}");
    assert_eq!(refresh_requests(&fake).len(), 3);
    assert!(
        fake.state.lock().unwrap().api_requests.iter().all(|r| !r.contains("127.0.0.1:1")),
        "the challenge's address was not followed"
    );

    // No saved sign-in: a person has to act, and nothing is sent.
    let (_third, signed_out) = machine();
    let error = api_client(&fake, &signed_out, &server).err().unwrap();
    assert!(matches!(error, McpError::SignInRequired(_)), "{error:?}");
    assert_eq!(refresh_requests(&fake).len(), 3);
}

#[test]
fn a_rest_read_refused_with_a_403_says_the_sign_in_lacks_the_right() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    assert_eq!(read_over_the_api(&mut client).unwrap()["id"], "DEMO-1");
    let refreshes = refresh_requests(&fake).len();

    fake.state.lock().unwrap().api_forbids = true;
    let error = read_over_the_api(&mut client).unwrap_err();
    assert!(matches!(error, McpError::Tool { .. }), "another sign-in would not help: {error:?}");
    let text = error.to_string();
    assert!(text.contains("HTTP 403") && text.contains("sign-in lacks the right"), "{text}");
    assert!(!text.contains("token_env"), "{text}");
    assert!(!is_sign_in_required(&text), "{text}");
    assert_eq!(refresh_requests(&fake).len(), refreshes, "a new token would not help");
}

#[test]
fn an_mcp_request_refused_with_a_403_says_the_sign_in_lacks_the_right() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    sign_in(&oauth, &server).unwrap();

    fake.state.lock().unwrap().mcp_forbids = true;
    let error = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(!error.needs_sign_in(), "another sign-in would not help: {error:?}");
    let text = error.to_string();
    assert!(text.contains("HTTP 403 (the sign-in lacks the right to this)"), "{text}");
    assert!(!text.contains("token_env"), "{text}");
}

#[test]
fn a_revoked_refresh_token_needs_a_new_sign_in_and_is_not_retried() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server();
    sign_in(&oauth, &server).unwrap();
    {
        let mut st = fake.state.lock().unwrap();
        st.access.clear();
        st.refresh.clear();
    }
    let error = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    // The saved sign-in was refused, which the error says apart from having none.
    assert!(matches!(&error, McpError::SignInRefused(m) if m.contains("refused the saved sign-in")), "{error:?}");
    assert!(error.to_string().starts_with(SIGN_IN_REQUIRED));
    assert!(is_sign_in_required(&error.to_string()) && is_sign_in_refused(&error.to_string()), "{error}");
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access")), (0, 0));
    // Asking again goes straight to SignInRequired without another refresh attempt: nothing is saved now.
    let again = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(again, McpError::SignInRequired(_)), "{again:?}");
    assert!(!is_sign_in_refused(&again.to_string()), "{again}");
    let refreshes =
        fake.state.lock().unwrap().token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").count();
    assert_eq!(refreshes, 1);
}

#[test]
fn a_state_mismatch_an_issuer_mismatch_and_foreign_metadata_are_rejected() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();

    // A forged state is not this sign-in's answer: it is ignored, and the wait runs out.
    fake.state.lock().unwrap().state_override = Some("forged-state".into());
    let error = sign_in(&oauth.clone().with_callback_timeout(Duration::from_millis(500)), &server).unwrap_err();
    assert!(
        matches!(&error, McpError::Server(m) if m.contains("timed out") && m.contains("state mismatch")),
        "{error:?}"
    );

    {
        let mut st = fake.state.lock().unwrap();
        st.state_override = None;
        st.iss_override = Some("http://evil.example".into());
    }
    let error = sign_in(&oauth, &server).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("issuer mismatch")), "{error:?}");

    {
        let mut st = fake.state.lock().unwrap();
        st.iss_override = None;
        st.metadata_issuer = Some("http://127.0.0.1:1".into());
    }
    let error = sign_in(&oauth, &server).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("different issuer")), "{error:?}");

    // Nothing was exchanged or stored along the way.
    assert!(fake.state.lock().unwrap().token_requests.is_empty());
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
}

#[test]
fn a_stray_callback_and_a_silent_connection_do_not_end_the_sign_in() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    let strays = Arc::new(Mutex::new(Vec::new()));
    let seen = strays.clone();
    let open = move |url: &str| -> anyhow::Result<()> {
        let authorize = Url::parse(url).unwrap();
        let redirect = authorize.query_pairs().find(|(k, _)| k == "redirect_uri").unwrap().1.into_owned();
        let (url, seen) = (url.to_string(), seen.clone());
        std::thread::spawn(move || {
            // A connection that never sends a byte, held open for the whole sign-in.
            let port = Url::parse(&redirect).unwrap().port().unwrap();
            let _silent = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
            for stray in [format!("{redirect}?code=stolen&state=wrong"), format!("{redirect}?code=stolen")] {
                let mut response = agent.get(&stray).call().unwrap();
                let body = response.body_mut().read_to_string().unwrap();
                seen.lock().unwrap().push((response.status().as_u16(), body));
            }
            browse(&url);
        });
        Ok(())
    };
    let started = Instant::now();
    oauth.sign_in_via(&server, Duration::from_secs(10), &open).unwrap();
    assert!(started.elapsed() < Duration::from_secs(8), "the silent connection held the listener");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    let strays = strays.lock().unwrap();
    assert_eq!(strays.len(), 2);
    assert!(strays.iter().all(|(status, body)| *status == 400 && body.contains("newest sign-in link")), "{strays:?}");
    // Only the real answer's code was exchanged.
    let st = fake.state.lock().unwrap();
    assert!(st.token_requests.iter().all(|r| r["code"] != "stolen"));
}

#[test]
fn concurrent_refreshers_spend_a_rotating_refresh_token_once() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server();
    fake.state.lock().unwrap().expires_in = 30;
    sign_in(&oauth, &server).unwrap();
    let first = stored_refresh_token(&oauth, &server);
    {
        let mut st = fake.state.lock().unwrap();
        st.expires_in = 3600;
        // Without the lock both refreshers would be inside the token request together.
        st.token_delay = Duration::from_millis(300);
    }
    // Two ATOMICS processes: separate clients over the one keychain, each holding its own lock file.
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let refreshers: Vec<_> = (0..2)
        .map(|_| {
            let (oauth, server, barrier) = (oauth.clone(), server.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                oauth.session(&server, Duration::from_secs(5)).map(|session| session.access_token().to_string())
            })
        })
        .collect();
    let tokens: Vec<String> = refreshers.into_iter().map(|t| t.join().unwrap().unwrap()).collect();
    assert_eq!(tokens[0], tokens[1], "the second refresher used the first one's tokens");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    let st = fake.state.lock().unwrap();
    let refreshes: Vec<_> = st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").collect();
    assert_eq!(refreshes.len(), 1, "one refresh token, spent once");
    assert_eq!(refreshes[0]["refresh_token"], first);
    assert!(st.access.contains(&tokens[0]));
    drop(st);
    assert_ne!(stored_refresh_token(&oauth, &server), first);
}

#[test]
fn a_browser_that_never_comes_back_times_out() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let oauth = oauth.with_callback_timeout(Duration::from_millis(300));
    let started = Instant::now();
    let error = oauth.sign_in_via(&fake.server(), Duration::from_secs(5), &|_| Ok(())).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("timed out")), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn metadata_over_plain_http_to_another_host_is_refused() {
    assert!(parse_secure("https://auth.example/x").is_ok());
    assert!(parse_secure("http://127.0.0.1:9/x").is_ok());
    assert!(parse_secure("http://auth.example/x").is_err());
}

#[test]
fn trace_cleaner_reads_access_and_refresh_secrets_without_refreshing() {
    let (_dir, oauth) = machine();
    let server = McpServer { url: Some("https://tickets.example/mcp".into()), ..Default::default() };
    let key = server_key(&server).unwrap();
    oauth
        .save(
            &key,
            &Tokens {
                issuer: "https://tickets.example".into(),
                client_id: "public-client".into(),
                token_endpoint: "https://tickets.example/token".into(),
                resource: server.url.clone().unwrap(),
                refresh_token: Some("refresh-trace-secret".into()),
                revocation_endpoint: None,
                extra_resources: Vec::new(),
                refused_resources: Vec::new(),
            },
            &slot("access", &key),
            &Access { token: "access-trace-secret".into(), expires_at: 0 },
        )
        .unwrap();
    let secrets = oauth.secrets(&server).unwrap();
    assert_eq!(secrets.len(), 2);
    let cleaner = atomics_core::tickets::Cleaner::new(secrets);
    let clean = cleaner.redacted("access-trace-secret refresh-trace-secret");
    assert!(!clean.contains("access-trace-secret"));
    assert!(!clean.contains("refresh-trace-secret"));
    oauth.sign_out(&server).unwrap();
    assert!(oauth.secrets(&server).unwrap().is_empty());
}

#[test]
fn an_error_without_state_finishes_only_for_the_expected_issuer() {
    let (listener, base) = bind();
    let issuer = "https://auth.example";
    let auth = AuthServer {
        issuer: issuer.into(),
        authorization_endpoint: Url::parse(issuer).unwrap(),
        token_endpoint: Url::parse(issuer).unwrap(),
        registration_endpoint: None,
        revocation_endpoint: None,
        iss_in_response: true,
    };
    let sender = std::thread::spawn(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
        for query in [
            "error=access_denied&state=wrong&iss=https://auth.example",
            "error=access_denied",
            "error=access_denied&iss=https://other.example",
            "code=stolen&iss=https://auth.example",
            "error=access_denied&iss=https://auth.example",
        ] {
            let response = agent.get(format!("{base}/callback?{query}")).call().unwrap();
            assert_eq!(response.status(), 400);
        }
    });
    let cancel = AtomicBool::new(false);
    let (_tx, paste) = std::sync::mpsc::channel();
    let control =
        SignInControl { cancel: &cancel, paste: &paste, notify: &|_| {}, open_browser: false, can_paste: false };
    let error = await_callback(&listener, "expected-state", &auth, Duration::from_secs(10), &control).unwrap_err();
    sender.join().unwrap();
    assert!(
        matches!(&error, McpError::Server(message) if message == "the authorization server answered access_denied"),
        "{error:?}"
    );
}

#[test]
fn trace_secret_fingerprints_detect_other_process_sign_in_and_access_only_rotation() {
    let (dir, oauth) = machine();
    let fake = Fake::start();
    let server = fake.server();
    assert!(oauth.secrets(&server).unwrap().is_empty(), "cache the initially absent sign-in");
    // An independent handle models a process that cannot invalidate this handle's cache.
    let other = OAuth { trace_secrets: Default::default(), ..oauth.clone() };
    sign_in(&other, &server).unwrap();
    let old = oauth.secrets(&server).unwrap();
    assert_eq!(old.len(), 2);
    let argv = dir.path().join("keychain-argv");
    std::fs::write(&argv, "").unwrap();
    assert_eq!(oauth.secrets(&server).unwrap(), old);
    let calls = std::fs::read_to_string(&argv).unwrap();
    assert_eq!(calls.lines().filter(|line| *line == "lookup").count(), 2, "cached reads inspect heads only");
    let key = server_key(&server).unwrap();
    other
        .write_json(
            &slot("access", &key),
            &Access { token: "access-only-rotation".into(), expires_at: unix_now() + 3600 },
        )
        .unwrap();
    let cleaner = atomics_core::tickets::Cleaner::new(oauth.secrets(&server).unwrap());
    assert_eq!(cleaner.redacted("access-only-rotation"), "***");
    other.sign_out(&server).unwrap();
    assert!(oauth.secrets(&server).unwrap().is_empty());
}

// One sign-in, a token per resource.

fn authorize_query(fake: &Fake, at: usize) -> Vec<(String, String)> {
    let path = fake.state.lock().unwrap().authorize_urls[at].clone();
    Url::parse(&format!("http://x{path}")).unwrap().query_pairs().into_owned().collect()
}

#[test]
fn one_sign_in_yields_a_token_per_resource_from_the_refresh_token() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);

    // The request names both resources, one parameter each, and no scope: the 401 named one for the
    // MCP server alone, which a server refuses for a request naming two.
    let query = authorize_query(&fake, 0);
    let resources: Vec<_> = query.iter().filter(|(k, _)| k == "resource").map(|(_, v)| v.as_str()).collect();
    assert_eq!(resources, [fake.mcp_url.as_str(), fake.api_url.as_str()]);
    assert!(!query.iter().any(|(k, _)| k == "scope"), "{query:?}");

    // The API's metadata was read with a GET, and nothing else reached the API.
    assert_eq!(fake.state.lock().unwrap().api_requests, ["GET /.well-known/oauth-protected-resource/api"]);

    // The consent is recorded, and the code spent once, for the server's own resource.
    assert_eq!(stored(&oauth, &server).extra_resources, vec![fake.api_url.clone()]);
    {
        let st = fake.state.lock().unwrap();
        let codes: Vec<_> = st.token_requests.iter().filter(|r| r["grant_type"] == "authorization_code").collect();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0]["resource"], fake.mcp_url);
        assert!(st.token_requests.iter().all(|r| !r.contains_key("scope")));
    }

    // The second token comes from the refresh token, naming the API, and the rotated refresh token
    // lands in the one shared item.
    let spent = stored_refresh_token(&oauth, &server);
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    assert_eq!(read_over_the_api(&mut client).unwrap()["title"], "Read over the API");
    let refreshes = refresh_requests(&fake);
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0]["resource"], fake.api_url);
    assert_eq!(refreshes[0]["refresh_token"], spent);
    assert_ne!(stored_refresh_token(&oauth, &server), spent);
    assert_eq!(items(dir.path(), "tokens"), 1, "the API has no sign-in of its own");
    assert_eq!(items(dir.path(), "access"), 2, "one token per resource");

    // Each resource keeps its token: no refresh is needed to read over either again.
    let mut mcp = McpClient::connect_with(&server, TIMEOUT, |_| None, Some(&oauth)).unwrap();
    assert_eq!(mcp.tool_names().unwrap(), ["search"]);
    let mut again = api_client(&fake, &oauth, &server).unwrap();
    assert_eq!(read_over_the_api(&mut again).unwrap()["id"], "DEMO-1");
    assert_eq!(refresh_requests(&fake).len(), 1);
    assert_eq!(again.oauth_token(), client.oauth_token());
    assert_ne!(mcp.oauth_token(), client.oauth_token());
}

#[test]
fn the_extra_slot_fits_the_keychain_and_keeps_a_long_token_whole() {
    let (_dir, oauth) = machine();
    let server = McpServer { url: Some("https://tickets.example/mcp".into()), ..Default::default() };
    let key = server_key(&server).unwrap();
    let name = extra_slot(&key, "https://api.example/v1");
    assert_eq!(name.len(), "access-".len() + 64 + 1 + 16);
    assert_ne!(name, extra_slot(&key, "https://api.example/v2"));
    // A JWT passes 2 KiB, so it takes chunks, whose names must not meet another resource's slot.
    let long = Access { token: "j".repeat(4 * 1024), expires_at: 7 };
    oauth.write_json(&name, &long).unwrap();
    assert_eq!(oauth.read_json::<Access>(&name).unwrap().unwrap().token, long.token);
    assert!(oauth.read_json::<Access>(&slot("access", &key)).unwrap().is_none());
}

#[test]
fn the_server_key_ignores_resources_so_a_saved_sign_in_keeps_its_slot() {
    let fake = Fake::start();
    assert_eq!(server_key(&fake.server()), server_key(&fake.server_with_api()));
    // An item written before this field existed reads as consenting to nothing further.
    let old = r#"{"issuer":"https://a","client_id":"c","token_endpoint":"https://a/t","resource":"https://a/mcp"}"#;
    let tokens: Tokens = serde_json::from_str(old).unwrap();
    assert!(tokens.extra_resources.is_empty() && tokens.refused_resources.is_empty());
}

#[test]
fn a_resource_the_person_did_not_consent_to_gets_nothing_and_a_new_sign_in_adds_it() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    // A sign-in from before the profile named the API.
    sign_in(&oauth, &fake.server()).unwrap();
    let server = fake.server_with_api();
    assert_eq!(oauth.status(&fake.server()), SignInStatus::SignedIn);
    assert_eq!(oauth.status(&server), SignInStatus::Partial);
    let before = stored_refresh_token(&oauth, &server);

    let error = api_client(&fake, &oauth, &server).err().unwrap();
    assert!(matches!(&error, McpError::SignInRequired(m) if m.contains("sign in once more")), "{error:?}");
    assert!(refresh_requests(&fake).is_empty(), "no token is minted silently");
    assert!(fake.state.lock().unwrap().api_auth.is_empty());
    assert_eq!(stored_refresh_token(&oauth, &server), before);

    // Nothing about the sign-in it does hold changed.
    let mut mcp = McpClient::connect_with(&server, TIMEOUT, |_| None, Some(&oauth)).unwrap();
    assert_eq!(mcp.tool_names().unwrap(), ["search"]);

    sign_in(&oauth, &server).unwrap();
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    assert_eq!(read_over_the_api(&mut client).unwrap()["id"], "DEMO-1");
}

#[test]
fn a_resource_dropped_from_the_consent_is_refused_even_by_a_live_session() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    read_over_the_api(&mut client).unwrap();
    // Another process signs in without the API, while this one still holds its token.
    sign_in(&oauth, &fake.server()).unwrap();
    let requests = refresh_requests(&fake).len();
    fake.state.lock().unwrap().access.clear();
    let error = read_over_the_api(&mut client).unwrap_err();
    assert!(matches!(&error, McpError::SignInRequired(m) if m.contains("sign in once more")), "{error:?}");
    assert_eq!(refresh_requests(&fake).len(), requests, "nothing is spent on a resource out of the consent");
}

#[test]
fn a_refused_second_resource_keeps_the_sign_in_and_offers_a_new_one_only_once() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    fake.state.lock().unwrap().refuse_api_tokens = true;
    let before = stored_refresh_token(&oauth, &server);

    // Refused once: a person is asked, the sign-in stays and the refresh token was not spent.
    let error = api_client(&fake, &oauth, &server).err().unwrap();
    assert!(matches!(&error, McpError::SignInRequired(_)), "{error:?}");
    assert_eq!(stored_refresh_token(&oauth, &server), before, "a refused resource spends nothing");
    assert!(fake.state.lock().unwrap().refresh.contains(&before));
    assert_eq!(oauth.status(&fake.server()), SignInStatus::SignedIn, "the first sign-in is in place");
    assert_eq!(oauth.status(&server), SignInStatus::Partial, "and the banner has something to offer");
    let mut mcp = McpClient::connect_with(&server, TIMEOUT, |_| None, Some(&oauth)).unwrap();
    assert_eq!(mcp.tool_names().unwrap(), ["search"]);
    // Out of the consent now: asking again sends nothing.
    let requests = refresh_requests(&fake).len();
    let again = api_client(&fake, &oauth, &server).err().unwrap();
    assert!(matches!(again, McpError::SignInRequired(_)), "{again:?}");
    assert_eq!(refresh_requests(&fake).len(), requests);

    // Signed in once more, and refused again: an error, not a third offer, and the banner stays down.
    sign_in(&oauth, &server).unwrap();
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    for _ in 0..2 {
        let error = api_client(&fake, &oauth, &server).err().unwrap();
        assert!(!is_sign_in_required(&error.to_string()), "{error}");
        assert!(error.to_string().contains("signing in again did not change that"), "{error}");
        assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    }
    let mut mcp = McpClient::connect_with(&server, TIMEOUT, |_| None, Some(&oauth)).unwrap();
    assert_eq!(mcp.tool_names().unwrap(), ["search"]);
    assert_eq!(fake.state.lock().unwrap().reused, 0);

    // Once the server allows it the same sign-in works, and a later refusal starts over.
    fake.state.lock().unwrap().refuse_api_tokens = false;
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    assert_eq!(read_over_the_api(&mut client).unwrap()["id"], "DEMO-1");
    assert!(stored(&oauth, &server).refused_resources.is_empty());
}

#[test]
fn an_api_answering_401_to_its_own_token_asks_for_a_sign_in_once_not_forever() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    fake.state.lock().unwrap().api_refuses = true;

    let mut client = api_client(&fake, &oauth, &server).unwrap();
    let error = read_over_the_api(&mut client).unwrap_err();
    assert!(matches!(error, McpError::SignInRequired(_)), "{error:?}");
    assert_eq!(oauth.status(&server), SignInStatus::Partial);

    sign_in(&oauth, &server).unwrap();
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    let error = read_over_the_api(&mut client).unwrap_err();
    assert!(!is_sign_in_required(&error.to_string()), "{error}");
    assert!(error.to_string().contains("signing in again did not change that"), "{error}");
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn, "the sign-in stays, and no banner comes back");

    // The API accepts it after all: the refusal is over.
    fake.state.lock().unwrap().api_refuses = false;
    assert_eq!(read_over_the_api(&mut client).unwrap()["id"], "DEMO-1");
    assert!(stored(&oauth, &server).refused_resources.is_empty());
}

#[test]
fn a_grant_without_a_refresh_token_cannot_reach_a_further_resource_and_stays() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    let key = server_key(&server).unwrap();
    sign_in(&oauth, &server).unwrap();
    let mut tokens = stored(&oauth, &server);
    tokens.refresh_token = None;
    oauth.write_json(&slot("tokens", &key), &tokens).unwrap();
    let error = api_client(&fake, &oauth, &server).err().unwrap();
    assert!(matches!(error, McpError::SignInRequired(_)), "{error:?}");
    assert!(oauth.read_json::<Tokens>(&slot("tokens", &key)).unwrap().is_some(), "still signed in");
}

#[test]
fn a_further_resource_must_name_the_issuer_in_use_or_the_person_is_not_asked() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    let opened = Arc::new(Mutex::new(0));
    let count = opened.clone();
    let open = move |url: &str| -> anyhow::Result<()> {
        *count.lock().unwrap() += 1;
        open_in_browser(url)
    };

    sign_in(&oauth, &fake.server()).unwrap();
    let kept = stored_refresh_token(&oauth, &fake.server());
    fake.state.lock().unwrap().api_metadata_issuer = Some("http://127.0.0.1:1".into());
    let error = oauth.sign_in_via(&server, TIMEOUT, &open).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("does not name the authorization server")), "{error:?}");

    {
        let mut st = fake.state.lock().unwrap();
        st.api_metadata_issuer = None;
        st.api_metadata_resource = Some("http://127.0.0.1:1/other".into());
    }
    let error = oauth.sign_in_via(&server, TIMEOUT, &open).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("is for another resource")), "{error:?}");

    // Metadata nobody publishes: the fake's API answers 404 below another path.
    let missing = McpServer { resources: vec![format!("{}/elsewhere", fake.api_url)], ..fake.server() };
    let error = oauth.sign_in_via(&missing, TIMEOUT, &open).unwrap_err();
    assert!(matches!(&error, McpError::Server(m) if m.contains("does not publish")), "{error:?}");

    // An address no token may go to, in words that do not repeat it.
    let unsafe_api = McpServer { resources: vec!["https://user:pw@api.example/v1".into()], ..fake.server() };
    let error = oauth.sign_in_via(&unsafe_api, TIMEOUT, &open).unwrap_err().to_string();
    assert!(!error.contains("pw"), "{error}");

    // The browser never opened, and the sign-in already saved is untouched.
    assert_eq!(*opened.lock().unwrap(), 0);
    assert_eq!(stored_refresh_token(&oauth, &fake.server()), kept);
    assert!(stored(&oauth, &fake.server()).extra_resources.is_empty());
}

#[test]
fn a_scope_is_still_asked_for_when_a_sign_in_names_one_resource() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    sign_in(&oauth, &fake.server()).unwrap();
    let query = authorize_query(&fake, 0);
    assert!(query.iter().any(|(k, v)| k == "scope" && v == "tickets.read"), "{query:?}");
    assert_eq!(query.iter().filter(|(k, _)| k == "resource").count(), 1);
}

#[test]
fn an_api_at_the_servers_own_address_shares_its_token() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = McpServer { resources: vec![format!("{}/", fake.mcp_url)], ..fake.server() };
    sign_in(&oauth, &server).unwrap();
    assert!(stored(&oauth, &server).extra_resources.is_empty());
    assert_eq!(oauth.status(&server), SignInStatus::SignedIn);
    let session = oauth.session_for(&server, &fake.mcp_url, TIMEOUT).unwrap();
    assert!(!session.access_token().is_empty());
    assert!(refresh_requests(&fake).is_empty());
    assert_eq!(items(dir.path(), "access"), 1);
}

#[test]
fn sessions_refreshing_together_spend_each_refresh_token_once() {
    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    // The MCP token is due, and the API has none: every session below must refresh or wait.
    fake.state.lock().unwrap().expires_in = 30;
    sign_in(&oauth, &server).unwrap();
    {
        let mut st = fake.state.lock().unwrap();
        st.expires_in = 3600;
        // Without the lock the refreshers would be inside the token request together.
        st.token_delay = Duration::from_millis(150);
    }
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let sessions: Vec<_> = (0..4)
        .map(|at| {
            let (oauth, server, barrier, api) = (oauth.clone(), server.clone(), barrier.clone(), fake.api_url.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let session = if at % 2 == 0 {
                    oauth.session(&server, TIMEOUT)
                } else {
                    oauth.session_for(&server, &api, TIMEOUT)
                };
                session.map(|session| session.access_token().to_string())
            })
        })
        .collect();
    let tokens: Vec<String> = sessions.into_iter().map(|t| t.join().unwrap().unwrap()).collect();

    assert_eq!(tokens[0], tokens[2], "both MCP sessions use one token");
    assert_eq!(tokens[1], tokens[3], "both API sessions use one token");
    let st = fake.state.lock().unwrap();
    assert_eq!(st.reused, 0, "a refresh token used twice would end the whole grant");
    assert_eq!(st.audience[&tokens[0]], fake.mcp_url);
    assert_eq!(st.audience[&tokens[1]], fake.api_url);
    assert!(st.access.contains(&tokens[0]) && st.access.contains(&tokens[1]));
    let refreshes: Vec<_> = st.token_requests.iter().filter(|r| r["grant_type"] == "refresh_token").collect();
    assert_eq!(refreshes.len(), 2, "one refresh per resource");
    assert_ne!(refreshes[0]["refresh_token"], refreshes[1]["refresh_token"]);
    drop(st);
    assert!(fake.state.lock().unwrap().refresh.contains(&stored_refresh_token(&oauth, &server)));
}

#[test]
fn a_refresh_token_goes_only_to_the_saved_token_endpoint() {
    let fake = Fake::start();
    let other = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    // What a server says after the sign-in does not move the refresh token.
    {
        let mut st = fake.state.lock().unwrap();
        st.token_endpoint_override = Some(format!("{}/token", other.as_url));
        st.api_metadata_issuer = Some(other.as_url.clone());
    }
    let mut client = api_client(&fake, &oauth, &server).unwrap();
    read_over_the_api(&mut client).unwrap();
    assert_eq!(refresh_requests(&fake).len(), 1);
    assert!(other.state.lock().unwrap().token_requests.is_empty());
    assert_eq!(stored(&oauth, &server).token_endpoint, format!("{}/token", fake.as_url));
}

#[test]
fn further_tokens_are_masked_and_a_rotation_of_one_is_noticed_from_its_head_alone() {
    let (dir, oauth) = machine();
    let fake = Fake::start();
    let server = fake.server_with_api();
    let other = OAuth { trace_secrets: Default::default(), ..oauth.clone() };
    sign_in(&other, &server).unwrap();
    let session = other.session_for(&server, &fake.api_url, TIMEOUT).unwrap();
    let api_token = session.access_token().to_string();

    let secrets = oauth.secrets(&server).unwrap();
    assert_eq!(secrets.len(), 3, "the refresh token and one access token per resource");
    let cleaner = atomics_core::tickets::Cleaner::new(secrets.clone());
    assert_eq!(cleaner.redacted(&format!("sent {api_token}")), "sent ***");

    // A second read only compares heads: tokens, the MCP token and the API's.
    let argv = dir.path().join("keychain-argv");
    std::fs::write(&argv, "").unwrap();
    assert_eq!(oauth.secrets(&server).unwrap(), secrets);
    let calls = std::fs::read_to_string(&argv).unwrap();
    assert_eq!(calls.lines().filter(|line| *line == "lookup").count(), 3, "{calls}");
    // A server without the API listed still masks what the sign-in holds.
    assert_eq!(oauth.secrets(&fake.server()).unwrap(), secrets);

    // The API's token alone rotates in another process.
    let key = server_key(&server).unwrap();
    let rotated = Access { token: "further-access-only-rotation".into(), expires_at: unix_now() + 3600 };
    other.write_json(&extra_slot(&key, &fake.api_url), &rotated).unwrap();
    let cleaner = atomics_core::tickets::Cleaner::new(oauth.secrets(&server).unwrap());
    assert_eq!(cleaner.redacted("further-access-only-rotation"), "***");

    // A consent gained in another process names a slot this one has not seen.
    let (_second, fresh) = machine();
    let second = OAuth { trace_secrets: Default::default(), ..fresh.clone() };
    sign_in(&second, &fake.server()).unwrap();
    assert_eq!(fresh.secrets(&fake.server()).unwrap().len(), 2);
    sign_in(&second, &server).unwrap();
    let session = second.session_for(&server, &fake.api_url, TIMEOUT).unwrap();
    assert!(fresh.secrets(&fake.server()).unwrap().contains(&session.access_token().to_string()));
}

#[test]
fn the_secrets_cache_keeps_one_entry_per_server() {
    let (dir, oauth) = machine();
    let (one, two) = (Fake::start(), Fake::start());
    sign_in(&oauth, &one.server()).unwrap();
    sign_in(&oauth, &two.server()).unwrap();
    let (first, second) = (oauth.secrets(&one.server()).unwrap(), oauth.secrets(&two.server()).unwrap());
    let argv = dir.path().join("keychain-argv");
    std::fs::write(&argv, "").unwrap();
    for _ in 0..3 {
        assert_eq!(oauth.secrets(&one.server()).unwrap(), first);
        assert_eq!(oauth.secrets(&two.server()).unwrap(), second);
    }
    let calls = std::fs::read_to_string(&argv).unwrap();
    assert_eq!(calls.lines().filter(|line| *line == "lookup").count(), 12, "heads only: {calls}");
}

#[test]
fn signing_out_clears_and_revokes_every_token_of_every_resource() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    let api_token = oauth.session_for(&server, &fake.api_url, TIMEOUT).unwrap().access_token().to_string();
    let mcp_token = oauth.session(&server, TIMEOUT).unwrap().access_token().to_string();
    let refresh = stored_refresh_token(&oauth, &server);
    assert_eq!(items(dir.path(), "access"), 2);

    // A sign-out knows what the sign-in holds: this server value lists nothing further.
    assert_eq!(oauth.sign_out(&fake.server()).unwrap(), Revocation::Revoked);
    {
        let st = fake.state.lock().unwrap();
        let revoked: HashSet<&str> = st.revoked.iter().map(|form| form["token"].as_str()).collect();
        assert_eq!(revoked, HashSet::from([api_token.as_str(), mcp_token.as_str(), refresh.as_str()]));
        let hints = st.revoked.iter().filter(|form| form["token_type_hint"] == "access_token").count();
        assert_eq!(hints, 2);
    }
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access")), (0, 0));
    assert_eq!(oauth.status(&server), SignInStatus::NeedsSignIn);
    assert!(matches!(api_client(&fake, &oauth, &server).err().unwrap(), McpError::SignInRequired(_)));
    assert!(oauth.secrets(&server).unwrap().is_empty());
}

#[test]
fn a_refresh_the_server_refuses_for_its_own_resource_ends_the_further_tokens_too() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    oauth.session_for(&server, &fake.api_url, TIMEOUT).unwrap();
    assert_eq!(items(dir.path(), "access"), 2);
    {
        let mut st = fake.state.lock().unwrap();
        st.refresh.clear();
        st.access.clear();
        st.expires_in = 3600;
    }
    let error = McpClient::connect_with(&server, TIMEOUT, |_| None, Some(&oauth)).err().unwrap();
    assert!(matches!(error, McpError::SignInRefused(_)), "{error:?}");
    assert_eq!((items(dir.path(), "tokens"), items(dir.path(), "access")), (0, 0));
}

#[test]
fn a_new_sign_in_drops_the_last_ones_further_tokens() {
    let fake = Fake::start();
    let (dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    let old = oauth.session_for(&server, &fake.api_url, TIMEOUT).unwrap().access_token().to_string();
    assert_eq!(items(dir.path(), "access"), 2);
    sign_in(&oauth, &server).unwrap();
    assert_eq!(items(dir.path(), "access"), 1, "the old grant's API token is gone");
    let new = oauth.session_for(&server, &fake.api_url, TIMEOUT).unwrap().access_token().to_string();
    assert_ne!(new, old);
}

#[test]
fn each_token_goes_only_to_its_own_resource() {
    use atomics_core::tickets::{Cleaner, HttpCall, HttpMethod, HttpRequest};

    let fake = Fake::start();
    let (_dir, oauth) = machine();
    let server = fake.server_with_api();
    sign_in(&oauth, &server).unwrap();
    let mut mcp = McpClient::connect_with(&server, TIMEOUT, |_| None, Some(&oauth)).unwrap();
    let mut api = api_client(&fake, &oauth, &server).unwrap();
    mcp.tool_names().unwrap();
    read_over_the_api(&mut api).unwrap();
    let (mcp_token, api_token) = (mcp.oauth_token().unwrap().to_string(), api.oauth_token().unwrap().to_string());
    assert_ne!(mcp_token, api_token);

    // Both refresh on a 401, and each still shows only its own token to its own resource.
    fake.state.lock().unwrap().access.clear();
    mcp.tool_names().unwrap();
    read_over_the_api(&mut api).unwrap();
    let (mcp_new, api_new) = (mcp.oauth_token().unwrap().to_string(), api.oauth_token().unwrap().to_string());
    {
        let st = fake.state.lock().unwrap();
        assert!(!st.mcp_auth.is_empty() && !st.api_auth.is_empty());
        for token in &st.mcp_auth {
            assert_eq!(st.audience.get(token), Some(&fake.mcp_url), "{token} reached the MCP server");
        }
        for token in &st.api_auth {
            assert_eq!(st.audience.get(token), Some(&fake.api_url), "{token} reached the API");
        }
        assert!(st.mcp_auth.contains(&mcp_new) && !st.mcp_auth.contains(&api_new));
        assert!(st.api_auth.contains(&api_new) && !st.api_auth.contains(&mcp_new));
    }

    // In a request's words too: the API is not sent one holding the MCP server's token.
    let call = HttpCall {
        method: HttpMethod::Post,
        path: String::new(),
        query: Vec::new(),
        response_headers: Default::default(),
    };
    let holding = HttpRequest { path: "/search".into(), query: Vec::new(), body: Some(json!({ "text": mcp_new })) };
    let seen = fake.state.lock().unwrap().api_auth.len();
    let error =
        api.call(&call, "POST /search", &holding, &Cleaner::new([mcp_new.clone()]), &Cleaner::default()).unwrap_err();
    assert!(error.to_string().contains("holds a credential"), "{error}");
    assert_eq!(fake.state.lock().unwrap().api_auth.len(), seen);
}
