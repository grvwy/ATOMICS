//! Azure DevOps repositories in the pull request controls. The same list, details, merge, diff and
//! action calls as GitHub's, through the Azure DevOps REST API. The token is the host Azure CLI's
//! (`az account get-access-token`), kept in memory until shortly before it expires and sent only as
//! the `Authorization` header to dev.azure.com; ATOMICS stores nothing.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomics_core::github::{AZURE_HOST, Check, CheckState, Details, MergeMethod, Pull, RepoId, percent_encode};
use serde_json::{Value, json};

use super::{ApiError, RepoDetails, Run};
use crate::relay::azure::Azure;

/// Azure DevOps's resource id, the token audience `az account get-access-token` is asked for.
pub const RESOURCE: &str = "499b84ac-1321-427f-aa17-267ca6975798";
const VERSION: &str = "api-version=7.1";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: u64 = 8 * 1024 * 1024;
/// Blobs larger than this are shown as binary rather than diffed.
const MAX_BLOB: u64 = 2 * 1024 * 1024;
const MAX_DIFF: usize = 16 * 1024 * 1024;
/// Open pull requests read per repository, at most.
const MAX_PULLS: usize = 1000;
/// Policy evaluations read in parallel.
const EVALUATIONS_AT_ONCE: usize = 8;
/// Team memberships and the bypass permission change rarely; they are read again after this.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

/// Policy types the details read, by their fixed ids.
const BUILD_POLICY: &str = "0609b952-1397-4640-95ec-e00a01b2c241";
const REVIEWERS_POLICY: &str = "fa4e907d-c16b-4a4c-9dfa-4906e5d171dd";
const MERGE_STRATEGY_POLICY: &str = "fa4e907d-c16b-4a4c-9dfa-4916e5d171ab";
/// The Git Repositories security namespace and its "Bypass policies when completing pull requests" bit.
const GIT_NAMESPACE: &str = "2e9eb7ed-3c0a-47d4-87c1-0ffdd275fd87";
const BYPASS_BIT: u32 = 32768;

/// The signed-in user, as connectionData names them.
#[derive(Debug, Clone, PartialEq)]
pub struct Viewer {
    pub id: String,
    pub name: String,
}

/// What successive polls and actions share: the Azure CLI and its token, who is signed in, their
/// teams per project and the bypass permission per repository.
#[derive(Default)]
pub struct Session {
    azure: Option<Arc<Azure>>,
    viewer: Option<Viewer>,
    teams: BTreeMap<(String, String), (Instant, BTreeSet<String>)>,
    bypass: BTreeMap<RepoId, (Instant, bool)>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("viewer", &self.viewer).finish_non_exhaustive()
    }
}

pub type Shared = Arc<Mutex<Session>>;

/// One signed-in client for dev.azure.com.
pub struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
    session: Shared,
}

impl Client {
    /// A client with the Azure CLI's token; `None` when az is not installed or not logged in.
    /// `base` replaces `https://dev.azure.com` (tests), and `az` the program found on the login
    /// shell's PATH.
    pub fn open(session: &Shared, base: Option<&str>, az: Option<PathBuf>) -> Result<Option<Client>, ApiError> {
        let azure = {
            let mut guard = session.lock().unwrap();
            if guard.azure.is_none() {
                let Some(program) = az.or_else(|| crate::setup::find_program("az")) else { return Ok(None) };
                let config = atomics_core::isolation::AzureConfig {
                    enabled: true,
                    query_read_exception: false,
                    resources: [(AZURE_HOST.to_string(), RESOURCE.to_string())].into(),
                };
                guard.azure = Some(Arc::new(Azure::new(program, config)));
            }
            guard.azure.clone().expect("set above")
        };
        // The session is not locked while az runs.
        let token = match azure.token(AZURE_HOST) {
            Ok(token) => token,
            Err(error) if error.contains("az login") => return Ok(None),
            Err(error) => return Err(failure(0, error)),
        };
        let local = base.is_some();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("atomics/", env!("CARGO_PKG_VERSION")));
        let config = if local { config.proxy(None) } else { config };
        Ok(Some(Client {
            base: base.unwrap_or("https://dev.azure.com").trim_end_matches('/').to_string(),
            token,
            agent: config.build().into(),
            session: session.clone(),
        }))
    }

    fn send(&self, method: &str, url: &str, body: Option<Value>, limit: u64) -> Result<Vec<u8>, ApiError> {
        let request = ureq::http::Request::builder()
            .method(method)
            .uri(url)
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {}", self.token));
        let sent = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(body.to_string())
                .map(|request| self.agent.run(request)),
            None => request.body(()).map(|request| self.agent.run(request)),
        };
        let mut response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return Err(failure(0, &format!("could not be reached: {}", super::transport(&error)))),
            Err(_) => return Err(failure(0, "the request could not be built")),
        };
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|error| failure(0, &format!("could not be read: {}", super::transport(&error))))?;
        match status {
            // A sign-in page (203) instead of JSON means the token was refused, too.
            401 | 203 => Err(failure(401, "refused the Azure CLI's token; run az login, then check again")),
            200..=299 => Ok(body),
            429 => Err(failure(429, "is throttling requests; the next poll tries again")),
            _ => Err(failure(status, &super::message(&String::from_utf8_lossy(&body)))),
        }
    }

    fn get(&self, url: &str) -> Result<Value, ApiError> {
        let body = self.send("GET", url, None, MAX_BODY)?;
        serde_json::from_slice(&body).map_err(|_| failure(200, "answered with something other than JSON"))
    }

    fn repo_url(&self, repo: &RepoId) -> Result<String, ApiError> {
        let (organization, project) =
            repo.azure_parts().ok_or_else(|| failure(0, "cannot reach a repository that is not on dev.azure.com"))?;
        Ok(format!(
            "{}/{}/{}/_apis/git/repositories/{}",
            self.base,
            percent_encode(organization),
            percent_encode(project),
            percent_encode(&repo.name)
        ))
    }

    fn project_url(&self, repo: &RepoId) -> Result<String, ApiError> {
        let (organization, project) =
            repo.azure_parts().ok_or_else(|| failure(0, "cannot reach a repository that is not on dev.azure.com"))?;
        Ok(format!("{}/{}/{}", self.base, percent_encode(organization), percent_encode(project)))
    }

    /// The signed-in user, read once per session from the first repository's organization.
    pub fn viewer(&self, repo: &RepoId) -> Result<Viewer, ApiError> {
        if let Some(viewer) = self.session.lock().unwrap().viewer.clone() {
            return Ok(viewer);
        }
        let (organization, _) =
            repo.azure_parts().ok_or_else(|| failure(0, "cannot reach a repository that is not on dev.azure.com"))?;
        let value = self.get(&format!("{}/{}/_apis/connectionData", self.base, percent_encode(organization)))?;
        let user = &value["authenticatedUser"];
        let viewer = Viewer {
            id: user["id"].as_str().unwrap_or_default().to_string(),
            name: user["providerDisplayName"].as_str().unwrap_or_default().to_string(),
        };
        if viewer.id.is_empty() || viewer.name.is_empty() {
            return Err(failure(200, "did not say who is signed in"));
        }
        self.session.lock().unwrap().viewer = Some(viewer.clone());
        Ok(viewer)
    }

    /// The ids of the viewer's teams in the repository's project; a team listed as a reviewer
    /// assigns the pull request to each of its members.
    fn my_teams(&self, repo: &RepoId) -> BTreeSet<String> {
        let Some((organization, project)) = repo.azure_parts() else { return BTreeSet::new() };
        let key = (organization.to_string(), project.to_string());
        if let Some((at, teams)) = self.session.lock().unwrap().teams.get(&key)
            && at.elapsed() < CACHE_TTL
        {
            return teams.clone();
        }
        let url = format!(
            "{}/{}/_apis/projects/{}/teams?$mine=true&$top=1000&{VERSION}",
            self.base,
            percent_encode(organization),
            percent_encode(project)
        );
        // Without teams, only direct assignments count; a later poll tries again.
        let Ok(value) = self.get(&url) else { return BTreeSet::new() };
        let teams: BTreeSet<String> = value["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|team| team["id"].as_str())
            .map(str::to_string)
            .collect();
        self.session.lock().unwrap().teams.insert(key, (Instant::now(), teams.clone()));
        teams
    }

    /// The active pull requests, newest first, with details when `details` is set.
    pub fn listing(&self, repo: &RepoId, details: bool) -> Result<(Vec<Pull>, Option<RepoDetails>), ApiError> {
        let viewer = self.viewer(repo)?;
        let teams = self.my_teams(repo);
        let mut raw = Vec::new();
        while raw.len() < MAX_PULLS {
            let url = format!(
                "{}/pullrequests?searchCriteria.status=active&$top=100&$skip={}&{VERSION}",
                self.repo_url(repo)?,
                raw.len()
            );
            let page = self.get(&url)?;
            let items = page["value"].as_array().cloned().unwrap_or_default();
            let done = items.len() < 100;
            raw.extend(items);
            if done {
                break;
            }
        }
        let pulls: Vec<Pull> = raw.iter().filter_map(|pull| pull_of(pull, repo, &viewer, &teams)).collect();
        if !details {
            return Ok((pulls, None));
        }
        let admin = self.bypass(repo, raw.first());
        // Policies are read for the viewer's own and assigned pull requests, a few at a time; the
        // others show what the list says, which never offers a merge.
        let wanted: Vec<usize> = (0..pulls.len())
            .filter(|&i| atomics_core::github::Group::of(&pulls[i], &viewer.name) != atomics_core::github::Group::Other)
            .collect();
        let mut evaluations: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
        for chunk in wanted.chunks(EVALUATIONS_AT_ONCE) {
            let raw = &raw;
            let read: Vec<(usize, Result<Vec<Value>, ApiError>)> = std::thread::scope(|scope| {
                let jobs: Vec<_> =
                    chunk.iter().map(|&i| (i, scope.spawn(move || self.evaluations(repo, &raw[i])))).collect();
                jobs.into_iter()
                    .map(|(i, job)| (i, job.join().unwrap_or_else(|_| Err(failure(0, "policy read failed")))))
                    .collect()
            });
            for (i, result) in read {
                evaluations.insert(i, result?);
            }
        }
        let mut repo_details = RepoDetails::default();
        let mut methods = None;
        for (i, (pull, value)) in pulls.iter().zip(&raw).enumerate() {
            let evaluations = evaluations.get(&i).map(Vec::as_slice);
            if methods.is_none() {
                methods = evaluations.and_then(merge_methods);
            }
            repo_details.pulls.insert(pull.number, details_of(value, evaluations, admin));
        }
        repo_details.methods =
            methods.unwrap_or_else(|| vec![MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase]);
        Ok((pulls, Some(repo_details)))
    }

    /// The policies that apply to a pull request, each with its configuration and status.
    fn evaluations(&self, repo: &RepoId, pull: &Value) -> Result<Vec<Value>, ApiError> {
        let project = pull["repository"]["project"]["id"].as_str().unwrap_or_default();
        let id = pull["pullRequestId"].as_u64().unwrap_or_default();
        let url = format!(
            "{}/_apis/policy/evaluations?artifactId=vstfs:///CodeReview/CodeReviewId/{project}/{id}&api-version=7.1-preview.1",
            self.project_url(repo)?
        );
        Ok(self.get(&url)?["value"].as_array().cloned().unwrap_or_default())
    }

    /// Whether the viewer may complete pull requests past their policies, cached per repository.
    fn bypass(&self, repo: &RepoId, sample: Option<&Value>) -> bool {
        if let Some((at, allowed)) = self.session.lock().unwrap().bypass.get(repo)
            && at.elapsed() < CACHE_TTL
        {
            return *allowed;
        }
        let Some(sample) = sample else { return false };
        let (Some(project), Some(id)) =
            (sample["repository"]["project"]["id"].as_str(), sample["repository"]["id"].as_str())
        else {
            return false;
        };
        let Some((organization, _)) = repo.azure_parts() else { return false };
        let url = format!(
            "{}/{}/_apis/permissions/{GIT_NAMESPACE}/{BYPASS_BIT}?tokens=repoV2/{project}/{id}&{VERSION}",
            self.base,
            percent_encode(organization)
        );
        // A failed lookup offers no bypass, and is asked again on the next poll.
        let Ok(value) = self.get(&url) else { return false };
        let allowed = value["value"][0].as_bool().unwrap_or(false);
        self.session.lock().unwrap().bypass.insert(repo.clone(), (Instant::now(), allowed));
        allowed
    }

    fn pull(&self, repo: &RepoId, number: u64) -> Result<Value, ApiError> {
        self.get(&format!("{}/pullrequests/{number}?{VERSION}", self.repo_url(repo)?))
    }

    /// The pull request's head commit now.
    pub fn head(&self, repo: &RepoId, number: u64) -> Result<String, ApiError> {
        self.pull(repo, number)?["lastMergeSourceCommit"]["commitId"]
            .as_str()
            .filter(|sha| !sha.is_empty())
            .map(str::to_string)
            .ok_or_else(|| failure(200, "has no head commit for the pull request"))
    }

    /// Completes the pull request with `method`, only while its head is still `sha`; with `admin`,
    /// past its policies, which Azure DevOps allows only with the bypass permission.
    pub fn merge(
        &self,
        repo: &RepoId,
        number: u64,
        method: MergeMethod,
        sha: &str,
        admin: bool,
    ) -> Result<String, ApiError> {
        let strategy = match method {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "noFastForward",
            MergeMethod::Rebase => "rebase",
        };
        let mut options =
            json!({ "mergeStrategy": strategy, "deleteSourceBranch": false, "transitionWorkItems": false });
        if admin {
            options["bypassPolicy"] = json!(true);
            options["bypassReason"] = json!("Completed past its policies from ATOMICS");
        }
        let body = json!({
            "status": "completed",
            "lastMergeSourceCommit": { "commitId": sha },
            "completionOptions": options,
        });
        let url = format!("{}/pullrequests/{number}?{VERSION}", self.repo_url(repo)?);
        let reply = self.send("PATCH", &url, Some(body), MAX_BODY)?;
        let value: Value = serde_json::from_slice(&reply).unwrap_or_default();
        Ok(match value["status"].as_str() {
            Some("completed") => "Pull request completed".into(),
            _ => "Completion started".into(),
        })
    }

    /// The pull request's changes against the merge-base, as a unified diff built from the two
    /// sides' blobs; nothing is fetched into a local repository.
    pub fn diff(&self, repo: &RepoId, number: u64) -> Result<String, ApiError> {
        let pull = self.pull(repo, number)?;
        let commit = |side: &str| pull[side]["commitId"].as_str().unwrap_or_default().to_string();
        let (base, head) = (commit("lastMergeTargetCommit"), commit("lastMergeSourceCommit"));
        if base.is_empty() || head.is_empty() {
            return Err(failure(200, "has no commits to compare for the pull request yet"));
        }
        let mut changes = Vec::new();
        loop {
            let url = format!(
                "{}/diffs/commits?baseVersion={base}&baseVersionType=commit&targetVersion={head}&targetVersionType=commit&diffCommonCommit=true&$top=2000&$skip={}&{VERSION}",
                self.repo_url(repo)?,
                changes.len()
            );
            let page = self.get(&url)?;
            let items = page["changes"].as_array().cloned().unwrap_or_default();
            let done = items.len() < 2000 || changes.len() >= 10_000;
            changes.extend(items);
            if done {
                break;
            }
        }
        let mut out = String::new();
        for change in changes.iter().filter(|change| change["item"]["gitObjectType"] == "blob") {
            let file = FileChange::of(change);
            let old = file.old_blob.as_deref().map(|id| self.blob(repo, id)).transpose()?;
            let new = file.new_blob.as_deref().map(|id| self.blob(repo, id)).transpose()?;
            out.push_str(&file.unified(old.as_ref(), new.as_ref()));
            if out.len() > MAX_DIFF {
                return Err(failure(200, "has a pull request diff too large to show; open it in the browser"));
            }
        }
        Ok(out)
    }

    /// A blob's text, or `None` when it is binary or too large to diff.
    fn blob(&self, repo: &RepoId, id: &str) -> Result<Option<String>, ApiError> {
        let url = format!("{}/blobs/{id}?$format=octetstream&{VERSION}", self.repo_url(repo)?);
        match self.send("GET", &url, None, MAX_BLOB) {
            Ok(bytes) if !bytes.contains(&0) => Ok(String::from_utf8(bytes).ok()),
            Ok(_) => Ok(None),
            Err(ApiError::Azure(0, message)) if message.starts_with("could not be read") => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn default_branch(&self, repo: &RepoId) -> Result<String, ApiError> {
        let value = self.get(&format!("{}?{VERSION}", self.repo_url(repo)?))?;
        value["defaultBranch"]
            .as_str()
            .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name).to_string())
            .ok_or_else(|| failure(200, "has no default branch for the repository"))
    }

    /// Runs pipeline `pipeline` (its numeric id) on `git_ref`, with `inputs` as its template parameters.
    pub fn dispatch(
        &self,
        repo: &RepoId,
        pipeline: &str,
        git_ref: &str,
        inputs: &BTreeMap<String, String>,
    ) -> Result<(), ApiError> {
        let pipeline = pipeline_id(pipeline)?;
        let ref_name = if git_ref.starts_with("refs/") { git_ref.to_string() } else { format!("refs/heads/{git_ref}") };
        let body = json!({
            "resources": { "repositories": { "self": { "refName": ref_name } } },
            "templateParameters": inputs,
        });
        self.send(
            "POST",
            &format!("{}/_apis/pipelines/{pipeline}/runs?{VERSION}", self.project_url(repo)?),
            Some(body),
            MAX_BODY,
        )?;
        Ok(())
    }

    /// The latest run of pipeline `pipeline`, if it ever ran.
    pub fn latest_run(&self, repo: &RepoId, pipeline: &str) -> Result<Option<Run>, ApiError> {
        let pipeline = pipeline_id(pipeline)?;
        let value = self.get(&format!(
            "{}/_apis/build/builds?definitions={pipeline}&$top=1&queryOrder=queueTimeDescending&{VERSION}",
            self.project_url(repo)?
        ))?;
        let build = &value["value"][0];
        if build.is_null() {
            return Ok(None);
        }
        Ok(Some(run_of(build)))
    }
}

/// An Azure DevOps failure, worded to follow "Azure DevOps " ("Azure DevOps answered 409: …").
fn failure(status: u16, message: &str) -> ApiError {
    ApiError::Azure(status, message.trim().chars().take(300).collect())
}

fn pipeline_id(pipeline: &str) -> Result<u64, ApiError> {
    pipeline
        .trim()
        .parse()
        .map_err(|_| failure(0, "runs pipelines by their numeric id: set workflow to the pipeline's id"))
}

fn branch(name: &str) -> String {
    name.strip_prefix("refs/heads/").unwrap_or(name).to_string()
}

/// A listed pull request. `requested` holds the reviewers by name, and the viewer's name when one of
/// their teams is a reviewer, so the counter's `review` filter and the groups see team assignments.
fn pull_of(value: &Value, repo: &RepoId, viewer: &Viewer, teams: &BTreeSet<String>) -> Option<Pull> {
    let number = value["pullRequestId"].as_u64()?;
    let mut requested: Vec<String> = Vec::new();
    for reviewer in value["reviewers"].as_array().into_iter().flatten() {
        let id = reviewer["id"].as_str().unwrap_or_default();
        let name = reviewer["displayName"].as_str().unwrap_or_default();
        if id == viewer.id || teams.contains(id) {
            requested.push(viewer.name.clone());
        } else if !name.is_empty() {
            requested.push(name.to_string());
        }
    }
    requested.dedup();
    let author_id = value["createdBy"]["id"].as_str().unwrap_or_default();
    let author = if author_id == viewer.id {
        viewer.name.clone()
    } else {
        value["createdBy"]["displayName"].as_str().unwrap_or_default().to_string()
    };
    Some(Pull {
        number,
        title: value["title"].as_str().unwrap_or_default().into(),
        url: format!("{}/pullrequest/{number}", repo.web_url()),
        author,
        draft: value["isDraft"].as_bool().unwrap_or(false),
        head: branch(value["sourceRefName"].as_str().unwrap_or_default()),
        head_sha: value["lastMergeSourceCommit"]["commitId"].as_str().unwrap_or_default().into(),
        base: branch(value["targetRefName"].as_str().unwrap_or_default()),
        requested,
    })
}

/// The controls' view of a pull request: its merge state in GitHub's terms, review, checks from
/// build policies, and whether the viewer may bypass policies. Without `evaluations` the merge
/// state stays UNKNOWN, which offers no merge.
fn details_of(pull: &Value, evaluations: Option<&[Value]>, bypass: bool) -> Details {
    let merge_status = pull["mergeStatus"].as_str().unwrap_or_default();
    let draft = pull["isDraft"].as_bool().unwrap_or(false);
    let votes: Vec<i64> =
        pull["reviewers"].as_array().into_iter().flatten().filter_map(|r| r["vote"].as_i64()).collect();
    let mut details = Details {
        mergeable: if merge_status == "conflicts" { "CONFLICTING" } else { "MERGEABLE" }.into(),
        ..Details::default()
    };
    let enabled = evaluations
        .unwrap_or_default()
        .iter()
        .filter(|e| e["configuration"]["isEnabled"].as_bool().unwrap_or(true))
        .filter(|e| e["status"] != "notApplicable");
    let mut blocked = false;
    let mut reviewers_policy = None;
    for evaluation in enabled {
        let kind = evaluation["configuration"]["type"]["id"].as_str().unwrap_or_default();
        let status = evaluation["status"].as_str().unwrap_or_default();
        // The merge strategy is chosen on completion; its evaluation says nothing yet.
        if kind == MERGE_STRATEGY_POLICY {
            continue;
        }
        if evaluation["configuration"]["isBlocking"].as_bool().unwrap_or(false) && status != "approved" {
            blocked = true;
        }
        if kind == REVIEWERS_POLICY {
            reviewers_policy = Some(status == "approved");
        }
        if kind == BUILD_POLICY {
            let settings = &evaluation["configuration"]["settings"];
            let name = settings["displayName"]
                .as_str()
                .filter(|name| !name.is_empty())
                .or(evaluation["context"]["buildDefinitionName"].as_str())
                .unwrap_or("Build");
            // An expired build waits for someone to queue it again; it is neither running nor failing.
            let expired = evaluation["context"]["isExpired"].as_bool().unwrap_or(false);
            let state = match status {
                _ if expired => CheckState::None,
                "approved" => CheckState::Passing,
                "rejected" | "broken" => CheckState::Failing,
                "queued" | "running" => CheckState::Pending,
                _ => CheckState::None,
            };
            let name = if expired { format!("{name} (expired)") } else { name.to_string() };
            details.check_runs.push(Check { name, state });
        }
    }
    details.checks = [CheckState::Failing, CheckState::Pending, CheckState::Passing]
        .into_iter()
        .find(|state| details.check_runs.iter().any(|check| check.state == *state))
        .unwrap_or_default();
    details.review = if votes.iter().any(|vote| *vote < 0) {
        Some("CHANGES_REQUESTED".into())
    } else {
        match reviewers_policy {
            Some(true) => Some("APPROVED".into()),
            Some(false) => Some("REVIEW_REQUIRED".into()),
            None if votes.iter().any(|vote| *vote >= 5) => Some("APPROVED".into()),
            None => None,
        }
    };
    details.merge_state = match (draft, merge_status, evaluations.is_some()) {
        (true, _, _) => "DRAFT",
        (_, "conflicts", _) => "DIRTY",
        (_, "rejectedByPolicy" | "failure", true) => "BLOCKED",
        (_, "succeeded", true) if blocked => "BLOCKED",
        (_, "succeeded", true) => "CLEAN",
        _ => "UNKNOWN",
    }
    .into();
    details.admin = bypass;
    details
}

/// The methods the branch's merge strategy policy allows, when one applies.
fn merge_methods(evaluations: &[Value]) -> Option<Vec<MergeMethod>> {
    let policy = evaluations.iter().find(|e| {
        e["configuration"]["type"]["id"] == MERGE_STRATEGY_POLICY
            && e["configuration"]["isEnabled"].as_bool().unwrap_or(true)
    })?;
    let settings = &policy["configuration"]["settings"];
    let allowed = |key: &str| settings[key].as_bool().unwrap_or(false);
    let mut methods = Vec::new();
    if allowed("allowSquash") || allowed("useSquashMerge") {
        methods.push(MergeMethod::Squash);
    }
    if allowed("allowNoFastForward") {
        methods.push(MergeMethod::Merge);
    }
    if allowed("allowRebase") {
        methods.push(MergeMethod::Rebase);
    }
    Some(methods)
}

fn run_of(build: &Value) -> Run {
    let status = match build["status"].as_str().unwrap_or_default() {
        "completed" => "completed",
        "inProgress" | "cancelling" => "in_progress",
        _ => "queued",
    };
    let conclusion = build["result"].as_str().map(|result| {
        match result {
            "succeeded" => "success",
            "failed" => "failure",
            "canceled" => "cancelled",
            "partiallySucceeded" => "partially_succeeded",
            other => other,
        }
        .to_string()
    });
    Run { status: status.into(), conclusion, url: build["_links"]["web"]["href"].as_str().unwrap_or_default().into() }
}

/// One changed file of a commit diff: its paths and the blobs of both sides.
#[derive(Debug, PartialEq)]
struct FileChange {
    old_path: Option<String>,
    new_path: Option<String>,
    old_blob: Option<String>,
    new_blob: Option<String>,
}

impl FileChange {
    fn of(change: &Value) -> FileChange {
        let kind = change["changeType"].as_str().unwrap_or_default();
        let item = &change["item"];
        let path = item["path"].as_str().unwrap_or_default().trim_start_matches('/').to_string();
        let blob = |key: &str| item[key].as_str().filter(|id| !id.is_empty()).map(str::to_string);
        let kinds: Vec<&str> = kind.split(',').map(str::trim).collect();
        let original = change["sourceServerItem"]
            .as_str()
            .or(change["originalPath"].as_str())
            .map(|path| path.trim_start_matches('/').to_string());
        if kinds.contains(&"add") {
            FileChange { old_path: None, new_path: Some(path), old_blob: None, new_blob: blob("objectId") }
        } else if kinds.contains(&"delete") {
            let old = blob("originalObjectId").or_else(|| blob("objectId"));
            FileChange { old_path: Some(path), new_path: None, old_blob: old, new_blob: None }
        } else {
            FileChange {
                old_path: Some(original.unwrap_or_else(|| path.clone())),
                new_path: Some(path),
                old_blob: blob("originalObjectId"),
                new_blob: blob("objectId"),
            }
        }
    }

    /// The file as `git diff` prints it, which `diff::parse_unified` reads.
    fn unified(&self, old: Option<&Option<String>>, new: Option<&Option<String>>) -> String {
        let old_path = self.old_path.as_deref().or(self.new_path.as_deref()).unwrap_or_default();
        let new_path = self.new_path.as_deref().or(self.old_path.as_deref()).unwrap_or_default();
        let mut out = format!("diff --git a/{old_path} b/{new_path}\n");
        match (&self.old_path, &self.new_path) {
            (None, _) => out.push_str("new file mode 100644\n"),
            (_, None) => out.push_str("deleted file mode 100644\n"),
            (Some(old), Some(new)) if old != new => out.push_str(&format!("rename from {old}\nrename to {new}\n")),
            _ => {}
        }
        let text = |side: Option<&Option<String>>| match side {
            None => Some(String::new()),
            Some(text) => text.clone(),
        };
        let (Some(before), Some(after)) = (text(old), text(new)) else {
            out.push_str(&format!("Binary files a/{old_path} and b/{new_path} differ\n"));
            return out;
        };
        if before == after {
            return out;
        }
        out.push_str(&format!(
            "--- {}\n+++ {}\n",
            self.old_path.as_ref().map_or("/dev/null".into(), |path| format!("a/{path}")),
            self.new_path.as_ref().map_or("/dev/null".into(), |path| format!("b/{path}")),
        ));
        let diff = similar::TextDiff::from_lines(&before, &after);
        for hunk in diff.unified_diff().context_radius(3).iter_hunks() {
            out.push_str(&hunk.to_string());
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests;
