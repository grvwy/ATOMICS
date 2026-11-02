//! Azure DevOps against an in-process fake of the REST endpoints the controls use, with the fake
//! Azure CLI from the relay's tests. The JSON shapes are those dev.azure.com returns.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};

use super::*;

pub const ME: &str = "11111111-1111-4111-8111-111111111111";
const TEAM: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";

#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: String,
    pub body: String,
}

#[derive(Default)]
struct Data {
    pulls: Vec<Value>,
    evaluations: BTreeMap<u64, Vec<Value>>,
    bypass: bool,
    changes: Vec<Value>,
    blobs: BTreeMap<String, Vec<u8>>,
    builds: BTreeMap<u64, Value>,
    refuse: bool,
    seen: Vec<Seen>,
}

pub struct FakeAzure {
    pub url: String,
    data: Arc<Mutex<Data>>,
}

impl FakeAzure {
    pub fn start() -> FakeAzure {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let data = Arc::new(Mutex::new(Data::default()));
        let shared = data.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let data = shared.clone();
                std::thread::spawn(move || serve(stream, &data));
            }
        });
        FakeAzure { url, data }
    }

    pub fn add_pull(&self, pull: Value, evaluations: Vec<Value>) {
        let mut data = self.data.lock().unwrap();
        data.evaluations.insert(pull["pullRequestId"].as_u64().unwrap(), evaluations);
        data.pulls.push(pull);
    }

    pub fn push(&self, number: u64, sha: &str) {
        let mut data = self.data.lock().unwrap();
        let pull = data.pulls.iter_mut().find(|pull| pull["pullRequestId"] == number).unwrap();
        pull["lastMergeSourceCommit"]["commitId"] = json!(sha);
    }

    pub fn bypass(&self, allowed: bool) {
        self.data.lock().unwrap().bypass = allowed;
    }

    pub fn refuse(&self) {
        self.data.lock().unwrap().refuse = true;
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.data.lock().unwrap().seen.clone()
    }

    fn file(&self, change: Value, blobs: &[(&str, &str)]) {
        let mut data = self.data.lock().unwrap();
        data.changes.push(change);
        for (id, text) in blobs {
            data.blobs.insert(id.to_string(), text.as_bytes().to_vec());
        }
    }
}

/// A pull request as the list returns it. `reviewers` are (id, name, vote).
pub fn pull(number: u64, author: (&str, &str), reviewers: &[(&str, &str, i64)], merge_status: &str) -> Value {
    json!({
        "pullRequestId": number,
        "title": format!("#ENG-{number} change {number}"),
        "status": "active",
        "isDraft": false,
        "mergeStatus": merge_status,
        "sourceRefName": format!("refs/heads/feature/{number}"),
        "targetRefName": "refs/heads/main",
        "createdBy": {"id": author.0, "displayName": author.1},
        "lastMergeSourceCommit": {"commitId": format!("head-{number}")},
        "lastMergeTargetCommit": {"commitId": "base-main"},
        "reviewers": reviewers.iter().map(|(id, name, vote)| json!({
            "id": id, "displayName": name, "vote": vote, "isContainer": id.len() == TEAM.len() && *id == TEAM,
        })).collect::<Vec<_>>(),
        "repository": {"id": "44444444-4444-4444-8444-444444444444", "project": {"id": PROJECT_ID}},
    })
}

fn evaluation(kind: &str, name: &str, status: &str, blocking: bool, settings: Value, context: Value) -> Value {
    json!({
        "status": status,
        "configuration": {"isEnabled": true, "isBlocking": blocking, "type": {"id": kind, "displayName": name}, "settings": settings},
        "context": context,
    })
}

/// The evaluations of a pull request waiting for its reviewer while its build runs, on a branch
/// that allows squash only.
pub fn waiting() -> Vec<Value> {
    vec![
        evaluation(
            REVIEWERS_POLICY,
            "Minimum number of reviewers",
            "queued",
            true,
            json!({"minimumApproverCount": 1}),
            Value::Null,
        ),
        evaluation(
            "55555555-5555-4555-8555-555555555555",
            "Comment requirements",
            "approved",
            true,
            json!({}),
            Value::Null,
        ),
        evaluation(
            MERGE_STRATEGY_POLICY,
            "Require a merge strategy",
            "rejected",
            true,
            json!({"allowSquash": true}),
            Value::Null,
        ),
        evaluation(
            BUILD_POLICY,
            "Build",
            "running",
            true,
            json!({"buildDefinitionId": 28, "displayName": "PR validation"}),
            json!({"buildId": 901, "buildDefinitionName": "api"}),
        ),
    ]
}

pub fn ready() -> Vec<Value> {
    waiting()
        .into_iter()
        .map(|mut e| {
            if e["configuration"]["type"]["id"] != MERGE_STRATEGY_POLICY {
                e["status"] = json!("approved");
            }
            e
        })
        .collect()
}

fn serve(stream: TcpStream, data: &Mutex<Data>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or_default().to_string());
    let (mut length, mut authorization) = (0, String::new());
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or_default();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap_or(0),
            "authorization" => authorization = value.trim().to_string(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let body = String::from_utf8_lossy(&body).to_string();
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut data = data.lock().unwrap();
    data.seen.push(Seen {
        method: method.clone(),
        path: path.into(),
        authorization: authorization.clone(),
        body: body.clone(),
    });
    let (status, reply): (u16, Vec<u8>) = if data.refuse || !authorization.starts_with("Bearer AZURE-REAL-") {
        (401, Vec::new())
    } else {
        route(&mut data, &method, path, query, &body)
    };
    let mut stream = stream;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.len()
    );
    let _ = stream.write_all(&reply);
}

fn route(data: &mut Data, method: &str, path: &str, query: &str, body: &str) -> (u16, Vec<u8>) {
    let ok = |value: Value| (200, value.to_string().into_bytes());
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let param = |name: &str| {
        query.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}="))).unwrap_or_default().to_string()
    };
    match (method, segments.as_slice()) {
        ("GET", [_, "_apis", "connectionData"]) => {
            ok(json!({"authenticatedUser": {"id": ME, "providerDisplayName": "Alex Doe"}}))
        }
        ("GET", [_, "_apis", "projects", _, "teams"]) => ok(json!({"value": [{"id": TEAM, "name": "Backend"}]})),
        ("GET", [_, "_apis", "permissions", ..]) => ok(json!({"count": 1, "value": [data.bypass]})),
        ("GET", [_, _, "_apis", "git", "repositories", _, "pullrequests"]) => {
            let skip: usize = param("$skip").parse().unwrap_or(0);
            ok(json!({"value": data.pulls.iter().skip(skip).take(100).cloned().collect::<Vec<_>>()}))
        }
        (_, [_, _, "_apis", "git", "repositories", _, "pullrequests", number]) => {
            let number: u64 = number.parse().unwrap();
            let Some(pull) = data.pulls.iter_mut().find(|pull| pull["pullRequestId"] == number) else {
                return (
                    404,
                    json!({"message": "TF401180: The requested pull request was not found."}).to_string().into_bytes(),
                );
            };
            if method == "PATCH" {
                let request: Value = serde_json::from_str(body).unwrap();
                if request["lastMergeSourceCommit"]["commitId"] != pull["lastMergeSourceCommit"]["commitId"] {
                    let message = "TF401192: The pull request was updated; refresh and try again.";
                    return (409, json!({"message": message}).to_string().into_bytes());
                }
                pull["status"] = json!("completed");
            }
            ok(pull.clone())
        }
        ("GET", [_, _, "_apis", "policy", "evaluations"]) => {
            let number: u64 = param("artifactId").rsplit('/').next().unwrap().parse().unwrap();
            ok(json!({"value": data.evaluations.get(&number).cloned().unwrap_or_default()}))
        }
        ("GET", [_, _, "_apis", "git", "repositories", _, "diffs", "commits"]) => ok(json!({"changes": data.changes})),
        ("GET", [_, _, "_apis", "git", "repositories", _, "blobs", id]) => match data.blobs.get(*id) {
            Some(bytes) => (200, bytes.clone()),
            None => (404, Vec::new()),
        },
        ("GET", [_, _, "_apis", "git", "repositories", _]) => ok(json!({"defaultBranch": "refs/heads/main"})),
        ("POST", [_, _, "_apis", "pipelines", id, "runs"]) => {
            let id: u64 = id.parse().unwrap();
            data.builds.insert(
                id,
                json!({"status": "inProgress", "result": null, "_links": {"web": {"href": format!("https://dev.azure.com/run/{id}")}}}),
            );
            ok(json!({"id": 1}))
        }
        ("GET", [_, _, "_apis", "build", "builds"]) => {
            let id: u64 = param("definitions").parse().unwrap();
            ok(json!({"value": data.builds.get(&id).cloned().into_iter().collect::<Vec<_>>()}))
        }
        _ => (404, json!({"message": format!("no route for {method} {path}")}).to_string().into_bytes()),
    }
}

pub fn repo() -> RepoId {
    RepoId::azure("contoso", "Platform", "api").unwrap()
}

fn viewer() -> Viewer {
    Viewer { id: ME.into(), name: "Alex Doe".into() }
}

fn client(fake: &FakeAzure, dir: &std::path::Path) -> Client {
    let program = crate::relay::azure::tests::fake(dir);
    Client::open(&Shared::default(), Some(&fake.url), Some(program)).unwrap().expect("signed in")
}

#[test]
fn azure_pulls_name_the_viewer_when_they_or_their_team_review_and_when_they_wrote_it() {
    let teams: BTreeSet<String> = [TEAM.to_string()].into();
    let direct =
        pull_of(&pull(1, ("x", "Ana"), &[(ME, "Alex Doe", 0)], "succeeded"), &repo(), &viewer(), &teams).unwrap();
    let team =
        pull_of(&pull(2, ("x", "Ana"), &[(TEAM, "[Platform]\\Backend", 0)], "succeeded"), &repo(), &viewer(), &teams)
            .unwrap();
    let mine =
        pull_of(&pull(3, (ME, "Alex Doe (old name)"), &[("y", "Bo", 0)], "succeeded"), &repo(), &viewer(), &teams)
            .unwrap();
    let other = pull_of(&pull(4, ("x", "Ana"), &[("y", "Bo", 10)], "succeeded"), &repo(), &viewer(), &teams).unwrap();
    use atomics_core::github::Group;
    let groups: Vec<Group> = [&direct, &team, &mine, &other].iter().map(|pull| Group::of(pull, "Alex Doe")).collect();
    assert_eq!(groups, [Group::Assigned, Group::Assigned, Group::Mine, Group::Other]);
    assert_eq!(direct.url, "https://dev.azure.com/contoso/Platform/_git/api/pullrequest/1");
    assert_eq!((direct.head.as_str(), direct.base.as_str(), direct.head_sha.as_str()), ("feature/1", "main", "head-1"));
}

#[test]
fn azure_policies_become_merge_state_review_and_checks() {
    let value = pull(1, ("x", "Ana"), &[(ME, "Alex Doe", 0)], "succeeded");
    let blocked = details_of(&value, Some(&waiting()), false);
    assert_eq!(blocked.merge_state, "BLOCKED");
    assert_eq!(blocked.review_label(), "review required");
    assert_eq!(blocked.checks, CheckState::Pending);
    assert_eq!(blocked.check_runs, [Check { name: "PR validation".into(), state: CheckState::Pending }]);
    assert_eq!(merge_methods(&waiting()), Some(vec![MergeMethod::Squash]));
    assert!(!blocked.buttons(false).merge && !blocked.buttons(false).admin);
    assert!(details_of(&value, Some(&waiting()), true).buttons(false).admin, "a bypass right offers admin merge");

    let clean = details_of(&value, Some(&ready()), false);
    assert_eq!(
        (clean.merge_state.as_str(), clean.review_label(), clean.checks),
        ("CLEAN", "approved", CheckState::Passing)
    );
    assert!(clean.buttons(false).merge);

    let mut expired = ready();
    expired[3]["status"] = json!("queued");
    expired[3]["context"]["isExpired"] = json!(true);
    let stale = details_of(&value, Some(&expired), false);
    assert_eq!(stale.check_runs, [Check { name: "PR validation (expired)".into(), state: CheckState::None }]);
    assert_eq!((stale.checks, stale.merge_state.as_str()), (CheckState::None, "BLOCKED"));
    let mut failing = ready();
    failing[3]["status"] = json!("rejected");
    assert_eq!(details_of(&value, Some(&failing), false).checks, CheckState::Failing);
    let rejected = pull(1, ("x", "Ana"), &[(ME, "Alex Doe", -10)], "succeeded");
    assert_eq!(details_of(&rejected, Some(&ready()), false).review_label(), "changes requested");
    let conflicts = pull(1, ("x", "Ana"), &[], "conflicts");
    assert_eq!(details_of(&conflicts, Some(&ready()), false).merge_state, "DIRTY");
    assert!(!details_of(&conflicts, Some(&ready()), false).buttons(false).merge);
    // Without policies read, nothing is offered.
    assert_eq!(details_of(&value, None, true).merge_state, "UNKNOWN");
    assert!(!details_of(&value, None, true).buttons(false).merge);
    assert_eq!(merge_methods(&[]), None);
}

#[test]
fn azure_file_changes_print_as_git_diffs() {
    use atomics_core::diff::{FileStatus, parse_unified};
    let change = |kind: &str, path: &str, extra: Value| {
        let mut item = json!({"gitObjectType": "blob", "path": path, "objectId": "new", "originalObjectId": "old"});
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                item[key] = value.clone();
            }
        }
        json!({"changeType": kind, "item": item})
    };
    let text = |s: &str| Some(Some(s.to_string()));
    let edit = FileChange::of(&change("edit", "/src/a.rs", json!({})));
    let added = FileChange::of(&change("add", "/src/b.rs", json!({"originalObjectId": null})));
    let deleted = FileChange::of(&change("delete", "/src/c.rs", json!({})));
    let mut renamed_change = change("edit, rename", "/src/e.rs", json!({}));
    renamed_change["sourceServerItem"] = json!("/src/d.rs");
    let renamed = FileChange::of(&renamed_change);
    let mut out = String::new();
    out += &edit.unified(text("one\ntwo\nthree\n").as_ref(), text("one\n2\nthree\n").as_ref());
    out += &added.unified(None, text("fn b() {}\n").as_ref());
    out += &deleted.unified(text("gone\n").as_ref(), None);
    out += &renamed.unified(text("same\n").as_ref(), text("same\nmore\n").as_ref());
    out += &edit.unified(Some(&None), text("x").as_ref());
    let files = parse_unified(&out);
    let statuses: Vec<FileStatus> = files.iter().map(|file| file.status).collect();
    assert_eq!(
        statuses,
        [FileStatus::Modified, FileStatus::Added, FileStatus::Deleted, FileStatus::Renamed, FileStatus::Modified]
    );
    assert_eq!(files[0].path(), "src/a.rs");
    assert_eq!(files[3].old_path.as_deref(), Some("src/d.rs"));
    let lines: Vec<&str> = files[0].hunks[0].lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(lines, ["one", "two", "2", "three"]);
    assert!(files[4].binary || files[4].hunks.is_empty(), "a binary side shows no hunks");
}

#[test]
fn azure_lists_merges_diffs_and_runs_pipelines_against_the_fake() {
    let dir = crate::test_support::tempdir().unwrap();
    let fake = FakeAzure::start();
    fake.add_pull(pull(4102, ("x", "Robin Roe"), &[(ME, "Alex Doe", 0)], "succeeded"), ready());
    fake.add_pull(pull(4101, (ME, "Alex Doe"), &[("y", "Sam Poe", 0)], "succeeded"), waiting());
    for number in 0..150 {
        fake.add_pull(pull(20000 + number, ("x", "Ana"), &[], "succeeded"), Vec::new());
    }
    let client = client(&fake, dir.path());
    let (pulls, details) = client.listing(&repo(), true).unwrap();
    assert_eq!(pulls.len(), 152, "every page is read");
    let details = details.unwrap();
    assert_eq!(details.methods, [MergeMethod::Squash]);
    assert_eq!(details.pulls[&4102].merge_state, "CLEAN");
    assert_eq!(details.pulls[&4101].merge_state, "BLOCKED");
    assert_eq!(details.pulls[&20000].merge_state, "UNKNOWN");
    let evaluated = fake.seen().iter().filter(|seen| seen.path.ends_with("/policy/evaluations")).count();
    assert_eq!(evaluated, 2, "policies are read for the viewer's own and assigned pull requests only");
    assert!(!details.pulls[&4101].admin);
    // The bypass permission is read per session and offers admin merge on a blocked pull request.
    fake.bypass(true);
    let other = crate::test_support::tempdir().unwrap();
    let (_, bypassing) = self::client(&fake, other.path()).listing(&repo(), true).unwrap();
    assert!(bypassing.unwrap().pulls[&4101].buttons(false).admin);
    assert!(fake.seen().iter().all(|seen| seen.authorization.starts_with("Bearer AZURE-REAL-")));

    // A push after the reviewed head is refused, and the reviewed head completes.
    fake.push(4102, "head-new");
    let refused = client.merge(&repo(), 4102, MergeMethod::Squash, "head-4102", false).unwrap_err();
    assert!(refused.to_string().starts_with("Azure DevOps answered 409: TF401192"), "{refused}");
    assert_eq!(client.head(&repo(), 4102).unwrap(), "head-new");
    client.merge(&repo(), 4102, MergeMethod::Squash, "head-new", false).unwrap();
    let completed = fake.seen().into_iter().rfind(|seen| seen.method == "PATCH").unwrap();
    let sent: Value = serde_json::from_str(&completed.body).unwrap();
    assert_eq!(sent["status"], "completed");
    assert_eq!(sent["lastMergeSourceCommit"]["commitId"], "head-new");
    assert_eq!(sent["completionOptions"]["mergeStrategy"], "squash");
    assert_eq!(sent["completionOptions"]["bypassPolicy"], Value::Null);
    client.merge(&repo(), 4101, MergeMethod::Merge, "head-4101", true).unwrap();
    let bypassed = fake.seen().into_iter().rfind(|seen| seen.method == "PATCH").unwrap();
    let sent: Value = serde_json::from_str(&bypassed.body).unwrap();
    assert_eq!(
        (sent["completionOptions"]["mergeStrategy"].as_str(), sent["completionOptions"]["bypassPolicy"].as_bool()),
        (Some("noFastForward"), Some(true))
    );

    fake.file(
        json!({"changeType": "edit", "item": {"gitObjectType": "blob", "path": "/README.md", "objectId": "b2", "originalObjectId": "b1"}}),
        &[("b1", "# api\nold\n"), ("b2", "# api\nnew\n")],
    );
    fake.file(json!({"changeType": "edit", "item": {"gitObjectType": "tree", "path": "/src", "objectId": "t2"}}), &[]);
    let diff = client.diff(&repo(), 4101).unwrap();
    assert!(diff.starts_with("diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@"), "{diff}");
    assert!(diff.contains("-old\n+new\n"));

    assert_eq!(client.latest_run(&repo(), "42").unwrap(), None);
    let inputs = BTreeMap::from([("environment".to_string(), "beta".to_string())]);
    client.dispatch(&repo(), "42", "main", &inputs).unwrap();
    let run = client.latest_run(&repo(), "42").unwrap().unwrap();
    assert_eq!((run.label(), run.state()), ("in progress".to_string(), CheckState::Pending));
    let dispatched = fake.seen().into_iter().rfind(|seen| seen.method == "POST").unwrap();
    assert_eq!(dispatched.path, "/contoso/Platform/_apis/pipelines/42/runs");
    let sent: Value = serde_json::from_str(&dispatched.body).unwrap();
    assert_eq!(sent["resources"]["repositories"]["self"]["refName"], "refs/heads/main");
    assert_eq!(sent["templateParameters"]["environment"], "beta");
    assert!(client.dispatch(&repo(), "release.yml", "main", &inputs).unwrap_err().to_string().contains("numeric id"));
    assert_eq!(client.default_branch(&repo()).unwrap(), "main");

    fake.refuse();
    assert_eq!(
        client.head(&repo(), 4101).unwrap_err(),
        ApiError::Azure(401, "refused the Azure CLI's token; run az login, then check again".into())
    );
}

#[test]
fn azure_without_a_signed_in_cli_is_signed_out_not_an_error() {
    let dir = crate::test_support::tempdir().unwrap();
    let program = crate::relay::azure::tests::fake(dir.path());
    std::fs::write(dir.path().join("az-fail"), "").unwrap();
    assert!(Client::open(&Shared::default(), Some("http://127.0.0.1:9"), Some(program)).unwrap().is_none());
}

/// Reads a real repository with the signed-in Azure CLI, without changing anything:
/// `ATOMICS_LIVE_AZURE_REPO=org/project/repo cargo test -p atomics live_azure -- --ignored --nocapture`.
#[test]
#[ignore = "reaches dev.azure.com with the host's az login"]
fn live_azure_lists_details_and_a_diff() {
    let Some(repo) = std::env::var("ATOMICS_LIVE_AZURE_REPO").ok().and_then(|text| RepoId::parse(&text)) else {
        panic!("set ATOMICS_LIVE_AZURE_REPO=org/project/repo");
    };
    let client = Client::open(&Shared::default(), None, None).unwrap().expect("az login first");
    let viewer = client.viewer(&repo).unwrap();
    let (pulls, details) = client.listing(&repo, true).unwrap();
    let details = details.unwrap();
    use atomics_core::github::Group;
    for group in [Group::Assigned, Group::Mine, Group::Other] {
        let listed: Vec<&Pull> = pulls.iter().filter(|pull| Group::of(pull, &viewer.name) == group).collect();
        println!("{} ({})", group.label(), listed.len());
        for pull in listed.iter().take(5) {
            let d = &details.pulls[&pull.number];
            println!(
                "  #{} {} · {} · {} · {} {:?}",
                pull.number,
                pull.author,
                d.review_label(),
                d.merge_state,
                d.checks.symbol(),
                d.check_runs.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
        }
    }
    println!("methods {:?}", details.methods);
    if let Some(pull) = pulls.iter().find(|pull| Group::of(pull, &viewer.name) != Group::Other) {
        let diff = client.diff(&repo, pull.number).unwrap();
        let files = atomics_core::diff::parse_unified(&diff);
        println!("diff of #{}: {} files, {} bytes", pull.number, files.len(), diff.len());
        assert!(!files.is_empty());
    }
}
