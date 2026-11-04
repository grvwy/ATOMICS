//! A scripted server on loopback: it records every request it gets and answers with what a test
//! wrote, so each rule can be checked on the wire.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use atomics_core::tickets::{Operation, Vars};

use super::*;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    target: String,
    authorization: Option<String>,
    content_type: Option<String>,
    body: String,
}

/// A loopback server answering each request with the next reply, repeating the last. A reply is the
/// whole HTTP response.
struct Script {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Script {
    fn start(replies: Vec<String>) -> Script {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (at, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut parts = first.split_whitespace();
                let (method, target) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
                let (mut length, mut authorization, mut content_type) = (0, None, None);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap();
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap(),
                        "authorization" => authorization = Some(value.trim().to_string()),
                        "content-type" => content_type = Some(value.trim().to_string()),
                        _ => {}
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body = String::from_utf8(body).unwrap();
                log.lock().unwrap().push(Seen { method, target, authorization, content_type, body });
                let reply = replies.get(at).or(replies.last()).unwrap();
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Script { url, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn reply(status: u16, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())
}

fn ok(body: &str) -> String {
    reply(200, "Content-Type: application/json\r\n", body)
}

fn api(url: &str) -> McpServer {
    McpServer { url: Some(url.to_string()), ..McpServer::default() }
}

fn operation(value: Value) -> Operation {
    serde_json::from_value(value).unwrap()
}

/// One request through the client, as the worker makes it.
fn request_with(
    url: &str,
    token: Option<&str>,
    cleaner: &Cleaner,
    operation: &Operation,
    vars: &Vars,
) -> Result<Value, McpError> {
    request_holding(url, token, &Cleaner::default(), cleaner, operation, vars)
}

fn request_holding(
    url: &str,
    token: Option<&str>,
    tokens: &Cleaner,
    cleaner: &Cleaner,
    operation: &Operation,
    vars: &Vars,
) -> Result<Value, McpError> {
    let mut client = HttpClient::connect(&api(url), token.map(str::to_string), TIMEOUT, None)?;
    let arguments = operation.arguments(vars).map_err(McpError::Server)?;
    let http = operation.http.as_deref().unwrap();
    let request = operation.http_request(http, vars, arguments).map_err(McpError::Server)?;
    client.call(http, &operation.label(), &request, tokens, cleaner)
}

fn request(url: &str, operation: &Operation, vars: &Vars) -> Result<Value, McpError> {
    request_with(url, None, &Cleaner::default(), operation, vars)
}

fn vars() -> Vars {
    Vars::new("demo").with("id", Some("DEMO-1")).with("query", Some("a b&c=d"))
}

#[test]
fn a_get_sends_its_arguments_as_a_query_and_reads_the_json_answer() {
    let script = Script::start(vec![ok(r#"{"id":"DEMO-1"}"#)]);
    let get = operation(json!({
        "http":{"method":"GET", "path":"/items/{id}"},
        "arguments":{"q":"{query}", "status":["open","done"], "take":25, "exact":true, "missing":null},
    }));
    assert_eq!(request(&format!("{}/v1/", script.url), &get, &vars()).unwrap(), json!({"id":"DEMO-1"}));
    let seen = &script.seen()[0];
    assert_eq!(seen.method, "GET");
    let (path, query) = seen.target.split_once('?').unwrap();
    assert_eq!(path, "/v1/items/DEMO-1");
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let pair = |name: &str, value: &str| (name.to_string(), value.to_string());
    assert_eq!(
        pairs,
        [
            pair("exact", "true"),
            pair("q", "a b&c=d"),
            pair("status", "open"),
            pair("status", "done"),
            pair("take", "25")
        ],
        "an array repeats its key, and null is left out"
    );
    assert_eq!(seen.body, "");
    assert!(seen.content_type.is_none());
}

#[test]
fn an_object_in_a_query_is_an_error_and_nothing_is_sent() {
    let script = Script::start(vec![ok("{}")]);
    for arguments in [json!({"filter":{"a":1}}), json!({"filter":[{"a":1}]}), json!({"filter":[[1]]})] {
        let get = operation(json!({"http":{"method":"GET", "path":"/items"}, "arguments":arguments}));
        let error = request(&script.url, &get, &vars()).unwrap_err().to_string();
        assert!(error.contains("query string cannot carry"), "{error}");
    }
    assert!(script.seen().is_empty());
}

#[test]
fn a_post_sends_a_json_body_and_only_the_named_arguments_in_its_query() {
    let script = Script::start(vec![ok(r#"{"tickets":[]}"#)]);
    let post = operation(json!({
        "http":{"method":"POST", "path":"/search", "query":["tenant"]},
        "arguments":{"query":"{query}", "tenant":"t1", "status":["open"], "nested":{"a":1}},
    }));
    assert_eq!(request(&script.url, &post, &vars()).unwrap(), json!({"tickets":[]}));
    let seen = &script.seen()[0];
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.target, "/search?tenant=t1");
    assert_eq!(seen.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        serde_json::from_str::<Value>(&seen.body).unwrap(),
        json!({"query":"a b&c=d", "status":["open"], "nested":{"a":1}})
    );
}

#[test]
fn response_headers_join_the_body_in_the_output() {
    let script = Script::start(vec![reply(200, "X-State: open\r\n", r#"{"id":"1"}"#)]);
    let get = operation(json!({
        "http":{"method":"GET", "path":"/items", "response_headers":{"state":"x-state", "gone":"x-absent"}},
        "result":{"state":"headers.state", "id":"body.id"},
    }));
    let output = request(&script.url, &get, &vars()).unwrap();
    assert_eq!(output, json!({"body":{"id":"1"}, "headers":{"state":"open"}}));
    let ticket = get.ticket(&output, "demo").unwrap();
    assert_eq!((ticket.id.as_str(), ticket.state.as_deref()), ("1", Some("open")));
}

#[test]
fn a_value_that_would_change_the_path_is_refused_and_nothing_is_sent() {
    let script = Script::start(vec![ok("{}")]);
    let get = operation(json!({"http":{"method":"GET", "path":"/items/{id}/comments"}}));
    for value in [
        "",
        " ",
        ".",
        "..",
        "...",
        " .. ",
        "../../admin",
        "a/b",
        "/etc/passwd",
        "a\\b",
        "..\\admin",
        "a;b",
        "..;",
        "%2e",
        "%2e%2e",
        "%2E%2E",
        "%252e%252e",
        "%25252e%25252e",
        "%2f",
        "a%2Fb",
        "a%252Fb",
        "..%2fadmin",
        "a%5cb",
        "line\nbreak",
        "%0a",
    ] {
        let vars = Vars::new("demo").with("id", Some(value));
        let error = request(&script.url, &get, &vars).unwrap_err().to_string();
        assert!(error.contains("{id} is not a single path segment"), "{value:?}: {error}");
    }
    assert!(script.seen().is_empty(), "{:?}", script.seen());
}

#[test]
fn a_value_is_encoded_into_its_one_segment() {
    let script = Script::start(vec![ok("{}")]);
    let get = operation(json!({"http":{"method":"GET", "path":"/items/{id}/x-{key}"}}));
    let vars = Vars::new("demo").with("id", Some("a b?c=d#e&f")).with("key", Some("é.1"));
    request(&script.url, &get, &vars).unwrap();
    assert_eq!(script.seen()[0].target, "/items/a%20b%3Fc%3Dd%23e%26f/x-%C3%A9.1");
}

#[test]
fn only_the_base_plus_the_path_is_an_address() {
    let base = http_base("http://127.0.0.1:9/v1/").unwrap();
    assert_eq!(base.as_str(), "http://127.0.0.1:9/v1");
    let at = |path: &str| request_url(&base, &HttpRequest { path: path.into(), query: Vec::new(), body: None });
    assert_eq!(at("/items/1").unwrap().as_str(), "http://127.0.0.1:9/v1/items/1");
    assert_eq!(
        request_url(&base, &HttpRequest { path: "/items".into(), query: vec![("a".into(), "b c".into())], body: None })
            .unwrap()
            .as_str(),
        "http://127.0.0.1:9/v1/items?a=b+c"
    );
    for path in [
        "/../admin",
        "/a/../../admin",
        "/./x",
        "/%2e%2e/admin",
        "@evil.example/",
        ".evil.example/",
        "x",
        "/a#fragment",
        "/a?b=c",
        "/a\\b",
        "/a\nb",
    ] {
        assert!(at(path).is_err(), "{path}");
    }
    // A host change spelled through the path stays this origin's path, or is refused.
    assert!(at("//evil.example/x").is_ok_and(|url| url.host_str() == Some("127.0.0.1")));
    for url in [
        "ftp://example.com/v1",
        "http://example.com/v1",
        "not a url",
        "",
        "https://user:secret@example.com/v1",
        "https://token@example.com/v1",
        "https://example.com/v1?token=abcdef",
        "https://example.com/v1#here",
    ] {
        let error = http_base(url).unwrap_err().to_string();
        assert!(error.contains("[tickets.http] url"), "{url}: {error}");
        assert!(!error.contains("secret") && !error.contains("abcdef"), "{url}: {error}");
    }
    assert!(http_base("https://example.com").is_ok());
    assert!(http_base("http://localhost:1234").is_ok());
    assert!(http_base("http://[::1]:1234/v1").is_ok());
}

#[test]
fn a_redirect_is_an_error_and_never_followed() {
    let elsewhere = Script::start(vec![ok(r#"{"stolen":true}"#)]);
    for status in [301, 302, 303, 307, 308] {
        let script = Script::start(vec![reply(status, &format!("Location: {}/stolen\r\n", elsewhere.url), "")]);
        let get = operation(json!({"http":{"method":"GET", "path":"/items"}}));
        let error =
            request_with(&script.url, Some("own-token-123456"), &Cleaner::default(), &get, &vars()).unwrap_err();
        assert!(matches!(error, McpError::Server(_)), "{error:?}");
        assert!(error.to_string().contains("redirect"), "{error}");
        assert!(!error.to_string().contains(&elsewhere.url), "{error}");
        assert_eq!(script.seen().len(), 1);
    }
    assert!(elsewhere.seen().is_empty());
}

#[test]
fn nothing_from_an_answer_becomes_an_address() {
    let elsewhere = Script::start(vec![ok("{}")]);
    let next = format!("{}/stolen?page=2", elsewhere.url);
    let script = Script::start(vec![ok(&json!({"items":[], "next":next}).to_string())]);
    let list = operation(json!({
        "http":{"method":"GET", "path":"/items"}, "items":"items", "paging":{"cursor":"cursor", "next":"next"},
    }));
    let first = request(&script.url, &list, &vars()).unwrap();
    let cursor = list.next_cursor(&first).unwrap();
    assert_eq!(cursor, next, "a cursor is an opaque value");

    let mut client = HttpClient::connect(&api(&script.url), None, TIMEOUT, None).unwrap();
    let extra = [("cursor".to_string(), Value::from(cursor.clone()))].into_iter().collect();
    let arguments = list.arguments_with(&vars(), &extra).unwrap();
    let http = list.http.as_deref().unwrap();
    let request = list.http_request(http, &vars(), arguments).unwrap();
    client.call(http, "list", &request, &Cleaner::default(), &Cleaner::default()).unwrap();

    let last = script.seen().pop().unwrap();
    let (path, query) = last.target.split_once('?').unwrap();
    assert_eq!(path, "/items");
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    assert_eq!(pairs, [("cursor".to_string(), cursor)]);
    assert!(elsewhere.seen().is_empty());
}

#[test]
fn a_token_goes_only_in_the_header_to_its_own_origin() {
    let own = Script::start(vec![ok("{}")]);
    let other = Script::start(vec![ok("{}")]);
    let server = |url: &str| McpServer {
        url: Some(url.to_string()),
        token_env: Some("TICKETS_TOKEN".into()),
        ..McpServer::default()
    };
    let token = || Some("own-token-123456".to_string());
    let get = operation(json!({"http":{"method":"GET", "path":"/items"}, "arguments":{"q":"{query}"}}));

    // token_env belongs to server.url's origin: the same origin under another path gets it.
    let shared = shared_token(&server(&format!("{}/mcp", own.url)), &api(&format!("{}/v1", own.url)), token());
    assert_eq!(shared, token());
    request_with(&format!("{}/v1", own.url), shared.as_deref(), &Cleaner::default(), &get, &vars()).unwrap();
    let seen = &own.seen()[0];
    assert_eq!(seen.authorization.as_deref(), Some("Bearer own-token-123456"));
    assert!(!seen.target.contains("own-token"), "{}", seen.target);

    // Another host, port or scheme does not.
    let shared = shared_token(&server(&format!("{}/mcp", own.url)), &api(&other.url), token());
    assert_eq!(shared, None);
    request_with(&other.url, shared.as_deref(), &Cleaner::default(), &get, &vars()).unwrap();
    assert_eq!(other.seen()[0].authorization, None);
    for (mcp, rest) in [
        ("https://a.example/mcp", "https://b.example/v1"),
        ("https://a.example/mcp", "http://a.example/v1"),
        ("https://a.example:444/mcp", "https://a.example/v1"),
        ("not a url", "not a url"),
    ] {
        assert_eq!(shared_token(&server(mcp), &api(rest), token()), None, "{mcp} {rest}");
    }
    // Nor is there a token to share from a stdio server, or without token_env.
    let stdio = McpServer { command: vec!["x".into()], ..server("https://a.example/mcp") };
    assert_eq!(shared_token(&stdio, &api("https://a.example/v1"), token()), None);
    assert_eq!(shared_token(&api("https://a.example/mcp"), &api("https://a.example/v1"), token()), None);
}

#[test]
fn a_request_holding_a_credential_is_not_sent() {
    let script = Script::start(vec![ok("{}")]);
    let secret = "own-token-123456";
    let vars = |text: &str| Vars::new("demo").with("id", Some(text)).with("query", Some(text));
    let clean = Cleaner::default();
    let get = operation(json!({"http":{"method":"GET", "path":"/items"}, "arguments":{"q":"{query}"}}));
    let post = operation(json!({"http":{"method":"POST", "path":"/items"}, "arguments":{"q":"{query}"}}));
    let at = operation(json!({"http":{"method":"GET", "path":"/items/{id}"}}));
    for op in [&get, &post, &at] {
        // The client's own token, and another resource's, in the query, body or path.
        let error = request_with(&script.url, Some(secret), &clean, op, &vars(secret)).unwrap_err().to_string();
        assert!(error.contains("holds a credential"), "{error}");
        assert!(!error.contains(secret), "{error}");
        let other = Cleaner::new(["other-token-654321".to_string()]);
        let error = request_holding(&script.url, None, &other, &clean, op, &vars("other-token-654321"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("holds a credential"), "{error}");
    }
    assert!(script.seen().is_empty(), "{:?}", script.seen());

    // What the cleaner only suspects by a setting's name is not a token: a person may search for it.
    let suspected = Cleaner::new(["browser".to_string()]);
    request_with(&script.url, None, &suspected, &get, &vars("browser")).unwrap();
    assert_eq!(script.seen().len(), 1);
}

#[test]
fn an_answer_over_the_cap_is_refused() {
    let big = "x".repeat(MAX_BODY_BYTES as usize + 1);
    let script = Script::start(vec![ok(&format!("\"{big}\""))]);
    let get = operation(json!({"http":{"method":"GET", "path":"/items"}}));
    let error = request(&script.url, &get, &vars()).unwrap_err();
    assert!(matches!(error, McpError::Transport { retryable: false, .. }), "{error:?}");
    assert!(error.to_string().contains("larger than 8 MiB"), "{error}");
}

#[test]
fn an_answer_that_is_not_json_is_refused() {
    for body in ["<html>Sign in</html>", "", "{\"truncated\":"] {
        let script = Script::start(vec![reply(200, "Content-Type: text/html\r\n", body)]);
        let get = operation(json!({"http":{"method":"GET", "path":"/items"}}));
        let error = request(&script.url, &get, &vars()).unwrap_err();
        assert!(matches!(error, McpError::Server(_)), "{error:?}");
        assert_eq!(error.to_string(), "GET /items did not answer with JSON");
    }
}

#[test]
fn a_refusal_is_cleaned_cut_and_says_whether_it_may_pass() {
    let secret = "job-secret-654321";
    let get = operation(json!({"http":{"method":"GET", "path":"/items"}}));
    let long = format!("{} {secret} {}", "word ".repeat(58), "more ".repeat(200));
    let cleaner = Cleaner::new([secret.to_string()]);
    let script = Script::start(vec![reply(404, "", &long)]);
    let error = request_with(&script.url, None, &cleaner, &get, &vars()).unwrap_err();
    let McpError::Tool { message, details } = &error else { panic!("{error:?}") };
    assert!(message.starts_with("GET /items answered HTTP 404: word"), "{message}");
    let shown = message.split_once(": ").unwrap().1;
    assert!(shown.chars().count() <= MAX_REFUSAL_CHARS, "{}", shown.chars().count());
    assert!(!message.contains("job-secret") && !details.contains(secret), "{message} {details}");

    // A credential the cut would have split is masked before the cut, not after.
    let split = format!("{}{secret}", "a ".repeat(146));
    let script = Script::start(vec![reply(400, "", &split)]);
    let error = request_with(&script.url, None, &cleaner, &get, &vars()).unwrap_err().to_string();
    assert!(!error.contains("job-sec"), "{error}");

    // The code of a refusal stays for telling refusals apart.
    let coded = json!({"code":"unreadable", "message":"no"}).to_string();
    let script = Script::start(vec![reply(403, "", &coded)]);
    let error = request(&script.url, &get, &vars()).unwrap_err();
    let McpError::Tool { message, details } = &error else { panic!("{error:?}") };
    assert!(message.contains("check server.token_env"), "{message}");
    assert_eq!(details, &coded);

    for (status, retryable) in [(429, true), (500, true), (503, true), (400, false), (404, false), (410, false)] {
        let script = Script::start(vec![reply(status, "", "no")]);
        let error = request(&script.url, &get, &vars()).unwrap_err();
        assert_eq!(matches!(error, McpError::Transport { retryable: true, .. }), retryable, "{status}: {error:?}");
        assert!(error.to_string().contains(&format!("HTTP {status}")), "{error}");
    }
}

#[test]
fn a_401_without_a_sign_in_says_what_would_fix_it() {
    let get = operation(json!({"http":{"method":"GET", "path":"/items"}}));
    // A static token cannot sign in: the person checks the token.
    let script = Script::start(vec![reply(401, "", "no")]);
    let error = request_with(&script.url, Some("own-token-123456"), &Cleaner::default(), &get, &vars()).unwrap_err();
    assert!(matches!(error, McpError::Tool { .. }), "{error:?}");
    assert!(error.to_string().contains("check server.token_env"), "{error}");
    assert_eq!(script.seen().len(), 1, "no second try");
    // No token, and the API asks for OAuth.
    let challenge = "WWW-Authenticate: Bearer resource_metadata=\"https://x/.well-known/y\"\r\n";
    let script = Script::start(vec![reply(401, challenge, "no")]);
    let error = request(&script.url, &get, &vars()).unwrap_err();
    assert!(matches!(error, McpError::SignInRequired(_)), "{error:?}");
    assert!(super::super::is_sign_in_required(&error.to_string()), "{error}");
}

#[test]
fn a_connection_that_fails_may_pass() {
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let get = operation(json!({"http":{"method":"GET", "path":"/items"}}));
    let error = request(&url, &get, &vars()).unwrap_err();
    assert!(matches!(error, McpError::Transport { retryable: true, .. }), "{error:?}");
    assert!(error.to_string().starts_with("GET /items failed"), "{error}");
}
